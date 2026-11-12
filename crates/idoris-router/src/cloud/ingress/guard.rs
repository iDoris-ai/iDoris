//! 云入口的中间件（计算服务 S5）：一个请求从公网进来，要依次过这几道门。
//!
//! 数据面（`/api/v1/*`）的顺序，**最外层在前**：
//!
//! ```text
//! strip_untrusted_headers   删掉 X-iDoris-Tenant：云入口不读、不信（设计稿 §9 数据面的租户）
//!   record_id_middleware    给每个响应（包括被拒的）一个 X-iDoris-Record-Id
//!     request_lifecycle     TCP 对端地址、客户端断开 -> 取消
//!       edge_guard          客户端来源（受信代理）、CORS 预检与响应头
//!         ── 路由：没有的路径 / 方法在这里就是 404，不认证、不碰库 ──
//!           data_plane_guard  认证 -> 成员 id -> 付款上下文与租户 -> 三级上限 -> 读请求体 -> 处理函数
//! ```
//!
//! `data_plane_guard` 里面的顺序是有讲究的（一步一步，前一步不过就不做后一步）：
//!
//! 1. **凭证格式**：没有 / 格式不对的凭证 -> 只扣「没有凭证」的限流器（来源软配额 + 全局总配额），401；不碰库。
//! 2. **失败前缀预检**：这个凭证前缀最近认证失败太多 -> 429 `auth_throttled`；不碰库。
//! 3. **认证**（虚拟 key + S2 的凭证闸门，每次请求读库、不缓存）：失败 -> 记一次失败、401（不透明：不告诉是哪种失败）。
//! 4. **成员 id**（`X-iDoris-Member`）格式：不合法 -> 400。
//! 5. **付款上下文 + 租户 + 数据处理要求**（`caller`）：失败 -> 401 / 403 / 503，**不回落**。
//! 6. **三级上限**（成员 -> 租户 -> 平台）：被限 -> 429，带 `Retry-After` 和 `X-iDoris-Limit-Scope`。
//! 7. **读请求体**（有上限、有期限）：太大 -> 413，太慢 -> 408。放在认证和限流**之后**：没有资格的请求不会让我们为它缓冲一兆字节。
//! 8. 交给处理函数；响应体持有并发名额，直到整个响应（含流）发完或客户端断开。
//!
//! 控制面（`/control/v1`，只有配置里 `control_plane.public: true` 才挂）不经过上面这些：它自己认证、自己授权。
//! 只给它加了一道请求体读取期限（慢请求），其它一概不动。

use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use futures_util::{Stream, StreamExt};
use idoris_compute_contracts::ids::{CredentialId, PrincipalId};
use idoris_compute_contracts::reason::ReasonCode;

use super::caller::Refusal;
use super::limits::{Denied, LimitKind, Permit};
use super::net::ClientAddr;
use super::reason::IngressReason;
use super::{CloudIngress, HEADER_LIMIT_SCOPE, HEADER_MEMBER, abuse};
use crate::auth::VirtualKeyAuthError;
use crate::{HEADER_REASON, error_envelope, virtual_key_unauthorized_response};

// ---------------------------------------------------------------------------
// 响应
// ---------------------------------------------------------------------------

/// 一个云入口自己的拒绝：状态码、`error.type` = `reason_code` = `X-iDoris-Reason`。
fn reject(reason: IngressReason, remediation: &'static str) -> Response {
    let mut response = error_envelope(reason.status(), reason.as_str(), remediation);
    set_reason_header(&mut response, reason.as_str());
    response
}

fn set_reason_header(response: &mut Response, code: &'static str) {
    response
        .headers_mut()
        .insert(HEADER_REASON, HeaderValue::from_static(code));
}

/// 限流类拒绝：429，`Retry-After`（至少 1 秒），哪一级拦的（限额类才有）。
fn throttle(reason: IngressReason, scope: Option<&'static str>, retry_after_secs: u64) -> Response {
    let remediation = match reason {
        IngressReason::AuthThrottled => {
            "too many failed authentications for this credential; wait and retry"
        }
        IngressReason::AnonymousRateLimited => {
            "too many requests without a valid credential; wait and retry"
        }
        IngressReason::ConcurrencyLimited => {
            "too many requests are already in flight; wait for one to finish and retry"
        }
        _ => "request rate limit reached; wait and retry",
    };
    let mut response = reject(reason, remediation);
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.max(1).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    if let Some(scope) = scope {
        response
            .headers_mut()
            .insert(HEADER_LIMIT_SCOPE, HeaderValue::from_static(scope));
    }
    response
}

fn limited(denied: Denied) -> Response {
    let reason = match denied.kind {
        LimitKind::Rate => IngressReason::RequestRateLimited,
        LimitKind::Concurrency => IngressReason::ConcurrencyLimited,
    };
    throttle(reason, Some(denied.scope.as_str()), denied.retry_after_secs)
}

fn unavailable() -> Response {
    reject(
        IngressReason::CloudUnavailable,
        "the service cannot accept this request right now",
    )
}

/// 付款上下文建立不了：403，S0 的稳定原因码。`member_sent` 只用来把「没带成员 id」说清楚（调用方自己知道发没发）。
fn payment_denied(code: ReasonCode, member_sent: bool) -> Response {
    let remediation = if code == ReasonCode::PaymentContextInvalid && !member_sent {
        "this credential must send the pseudonymous member id in X-iDoris-Member"
    } else {
        "the payment context for this request could not be established"
    };
    let mut response = error_envelope(StatusCode::FORBIDDEN, code.as_str(), remediation);
    set_reason_header(&mut response, code.as_str());
    response
}

fn refusal_response(refusal: Refusal, member_sent: bool) -> Response {
    match refusal {
        Refusal::Unauthorized => virtual_key_unauthorized_response(),
        Refusal::Payment(code) => payment_denied(code, member_sent),
        Refusal::TenantNotProvisioned => reject(
            IngressReason::TenantNotProvisioned,
            "this tenant has no data-processing declaration yet; contact the platform operator",
        ),
        Refusal::Unavailable => unavailable(),
    }
}

// ---------------------------------------------------------------------------
// 最外层：不信任的头
// ---------------------------------------------------------------------------

/// 云入口不读、不信 `X-iDoris-Tenant`：租户、付款账户、数据类别都从凭证推导。头在最外层就被删掉，
/// 后面的任何一层（包括审计）都看不到它。本地租户模式的 `X-iDoris-Tenant` 契约不变——那是另一个路由器。
pub(super) async fn strip_untrusted_headers(mut request: Request, next: Next) -> Response {
    request.headers_mut().remove("x-idoris-tenant");
    next.run(request).await
}

// ---------------------------------------------------------------------------
// 边缘：客户端来源、CORS
// ---------------------------------------------------------------------------

pub(super) async fn edge_guard(
    State(ingress): State<Arc<CloudIngress>>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<crate::connection::RequestLifecycle>()
        .and_then(crate::connection::RequestLifecycle::peer)
        .map(|peer| peer.ip());
    let client =
        super::net::resolve_client(&ingress.config.trusted_proxies, peer, request.headers());
    request.extensions_mut().insert(client);

    let cors = &ingress.config.cors;
    if let Some(response) = super::cors::answer_preflight(cors, &request) {
        return response;
    }
    let origin = super::cors::request_origin(request.headers());
    let mut response = next.run(request).await;
    super::cors::decorate(cors, origin.as_deref(), &mut response);
    response
}

// ---------------------------------------------------------------------------
// 数据面
// ---------------------------------------------------------------------------

/// `X-iDoris-Member`：恰好零个或一个、合法的主体 id。多个、空、含非法字符、过长都是错误（不是「取第一个」）。
fn parse_member_header(headers: &HeaderMap) -> Result<Option<PrincipalId>, ()> {
    let mut values = headers.get_all(HEADER_MEMBER).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let text = first.to_str().map_err(|_| ())?;
    PrincipalId::new(text).map(Some).map_err(|_| ())
}

/// 没有资格的请求（没有凭证、凭证格式不对、认证失败）：扣来源软配额和全局总配额；配额还有就是 401，没有就是 429。
fn deny_unauthenticated(ingress: &CloudIngress, source: IpAddr, now_ms: i64) -> Response {
    match ingress.anonymous.admit(source, now_ms) {
        Ok(()) => virtual_key_unauthorized_response(),
        Err(retry_after) => throttle(IngressReason::AnonymousRateLimited, None, retry_after),
    }
}

pub(super) async fn data_plane_guard(
    State(ingress): State<Arc<CloudIngress>>,
    request: Request,
    next: Next,
) -> Response {
    let now = ingress.clock.now();
    let source = request
        .extensions()
        .get::<ClientAddr>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |client| client.ip);

    // 1. 凭证格式。没有凭证的请求没有任何人需要保护：只扣没有凭证的限流器，不碰库。
    let secret = match crate::auth::bearer_secret(request.headers()) {
        Ok(secret) => secret,
        Err(_) => return deny_unauthenticated(&ingress, source, now.0),
    };
    let prefix = abuse::credential_prefix(secret.expose_secret());

    // 2. 失败前缀预检（内存里，不碰库）。键是凭证前缀，**不是**来源地址：同一个出口后面别的学生的凭证前缀不同，不受影响。
    if let Some(prefix) = prefix
        && let Err(retry_after) = ingress.failures.check(prefix, now.0)
    {
        return throttle(IngressReason::AuthThrottled, None, retry_after);
    }

    // 3. 认证。
    let identity = match ingress.authenticator.authenticate(request.headers()).await {
        Ok(identity) => identity,
        Err(VirtualKeyAuthError::Unauthorized) => {
            if let Some(prefix) = prefix {
                ingress.failures.record_failure(prefix, now.0);
            }
            return deny_unauthenticated(&ingress, source, now.0);
        }
        Err(VirtualKeyAuthError::Unavailable) => return unavailable(),
    };
    if let Some(prefix) = prefix {
        ingress.failures.record_success(prefix);
    }

    // 4. 成员 id 的格式。
    let member_sent = request.headers().contains_key(HEADER_MEMBER);
    let member = match parse_member_header(request.headers()) {
        Ok(member) => member,
        Err(()) => {
            return reject(
                IngressReason::MemberIdInvalid,
                "X-iDoris-Member must be exactly one valid pseudonymous member id",
            );
        }
    };

    // 5. 付款上下文、租户、数据处理要求。会读库：放进 spawn_blocking。
    let Ok(credential) = CredentialId::new(identity.key_id.as_str()) else {
        return virtual_key_unauthorized_response();
    };
    let resolver = Arc::clone(&ingress.resolver);
    let resolved =
        tokio::task::spawn_blocking(move || resolver.resolve(&credential, member.as_ref(), now))
            .await;
    let resolved = match resolved {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(refusal)) => return refusal_response(refusal, member_sent),
        Err(_) => return unavailable(),
    };

    // 6. 三级上限（成员 -> 租户 -> 平台）。
    let permit = match ingress
        .limiter
        .admit(&resolved.keys, &resolved.limits, now.0)
    {
        Ok(permit) => permit,
        Err(denied) => return limited(denied),
    };

    // 7. 请求体：认证和限流都过了才读。
    let mut request = match buffer_body(
        request,
        ingress.config.body_max_bytes,
        ingress.config.body_read_timeout,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };

    // 8. 处理函数。云入口推导出来的调用方放进扩展：请求自己没有任何办法放东西进去。
    request.extensions_mut().insert(identity);
    request.extensions_mut().insert(resolved.policy);
    request.extensions_mut().insert(resolved.context);
    let response = next.run(request).await;
    hold_permit(response, permit)
}

/// 读完请求体：有上限、有期限。`Content-Length` 先查（诚实的客户端不用读一个字节就被拒绝）；分块上传边读边数。
async fn buffer_body(
    request: Request,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Request, Response> {
    let (parts, body) = request.into_parts();
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > u64::try_from(max_bytes).unwrap_or(u64::MAX)) {
        return Err(reject(
            IngressReason::BodyTooLarge,
            "the request body is larger than this service accepts",
        ));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream = body.into_data_stream();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => {
                return Err(reject(
                    IngressReason::RequestTimeout,
                    "the request body was not received in time",
                ));
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                return Err(error_envelope(
                    StatusCode::BAD_REQUEST,
                    "invalid_body",
                    "the request body could not be read",
                ));
            }
            Ok(Some(Ok(chunk))) => {
                if buffer.len().saturating_add(chunk.len()) > max_bytes {
                    return Err(reject(
                        IngressReason::BodyTooLarge,
                        "the request body is larger than this service accepts",
                    ));
                }
                buffer.extend_from_slice(&chunk);
            }
        }
    }
    Ok(Request::from_parts(parts, Body::from(Bytes::from(buffer))))
}

// ---------------------------------------------------------------------------
// 并发名额跟着响应体走
// ---------------------------------------------------------------------------

struct PermitBody<S> {
    inner: S,
    _permit: Permit,
}

impl<S: Stream + Unpin> Stream for PermitBody<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// 把并发名额挂在响应体上：响应体被完整发完、出错、或者客户端断开后被丢弃时才释放。
/// 流式响应因此在整个流持续期间都占着名额。
fn hold_permit(response: Response, permit: Permit) -> Response {
    let (parts, body) = response.into_parts();
    let guarded = PermitBody {
        inner: body.into_data_stream(),
        _permit: permit,
    };
    Response::from_parts(parts, Body::from_stream(guarded))
}

// ---------------------------------------------------------------------------
// 控制面：只加请求体读取期限
// ---------------------------------------------------------------------------

/// 控制面请求体必须在期限内读完（慢请求）。只包装请求体，不改控制面任何别的行为。
pub(super) async fn control_body_deadline(
    State(ingress): State<Arc<CloudIngress>>,
    request: Request,
    next: Next,
) -> Response {
    let deadline = tokio::time::Instant::now() + ingress.config.body_read_timeout;
    let (parts, body) = request.into_parts();
    let guarded = futures_util::stream::unfold(
        (body.into_data_stream(), false),
        move |(mut stream, done)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(chunk))) => Some((Ok(chunk), (stream, false))),
                Ok(Some(Err(error))) => Some((Err(std::io::Error::other(error)), (stream, true))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "request body read timed out",
                    )),
                    (stream, true),
                )),
            }
        },
    );
    next.run(Request::from_parts(parts, Body::from_stream(guarded)))
        .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(HEADER_MEMBER, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn the_member_header_is_zero_or_one_valid_id_and_never_the_first_of_several() {
        assert_eq!(parse_member_header(&headers(&[])), Ok(None));
        assert_eq!(
            parse_member_header(&headers(&["kid-1"]))
                .unwrap()
                .unwrap()
                .as_str(),
            "kid-1"
        );
        for bad in [
            vec!["kid-1", "kid-2"],
            vec![""],
            vec!["has space"],
            vec!["a@b.example"],
            vec!["a/b"],
            vec![&"x".repeat(129)[..]],
        ] {
            assert_eq!(parse_member_header(&headers(&bad)), Err(()), "{bad:?}");
        }
        let mut non_ascii = HeaderMap::new();
        non_ascii.append(HEADER_MEMBER, HeaderValue::from_bytes(b"kid\xff").unwrap());
        assert_eq!(parse_member_header(&non_ascii), Err(()));
    }

    #[test]
    fn throttle_responses_carry_a_retry_after_of_at_least_one_second_and_the_scope() {
        let response = throttle(IngressReason::RequestRateLimited, Some("tenant"), 0);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(response.headers()[HEADER_LIMIT_SCOPE], "tenant");
        assert_eq!(response.headers()[HEADER_REASON], "request_rate_limited");
        let response = throttle(IngressReason::AuthThrottled, None, 42);
        assert_eq!(response.headers()[header::RETRY_AFTER], "42");
        assert!(!response.headers().contains_key(HEADER_LIMIT_SCOPE));
    }

    #[test]
    fn payment_denials_use_the_s0_codes_as_403() {
        for code in [
            ReasonCode::PaymentContextInvalid,
            ReasonCode::PrincipalRevoked,
            ReasonCode::MembershipRevoked,
            ReasonCode::AccountInactive,
        ] {
            let response = payment_denied(code, true);
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response.headers()[HEADER_REASON], code.as_str());
        }
    }
}

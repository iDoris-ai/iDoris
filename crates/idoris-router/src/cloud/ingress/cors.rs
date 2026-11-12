//! 跨域（CORS）。P0 默认**不开放**。
//!
//! 设计稿 §2：eduai 前端永远不持有 iDoris 凭证，P0 一律走 eduai 后端代理，浏览器不直连。所以云入口默认不发任何
//! CORS 头，预检请求（`OPTIONS`）也和任何别的未知路由一样是 404。将来要让浏览器直连（后端签发的短期、限定用途
//! 凭证）时，才在云入口配置的 `cors.allowed_origins` 里**逐个**列出允许的来源：
//!
//! - 只做**精确匹配**（`https://app.example.com`）：不支持通配符，`*`、`null`、带路径的写法配置时就报错；
//!   非回环地址必须是 `https://`；
//! - 永远不发 `Access-Control-Allow-Credentials`（凭证是 `Authorization` 头里的 bearer，不是 cookie）；
//! - 来源在名单里：响应带 `Access-Control-Allow-Origin: <那个来源>` 和 `Access-Control-Expose-Headers`，
//!   暴露的头 = S3 的 [`EXPOSED_HEADERS`]（体验分级的原因码、降级、级别头……）加上入口自己的
//!   `Retry-After` 和 `X-iDoris-Limit-Scope`——**S3 的列表只有一份**，这里是在它后面追加，不另列；
//! - 来源不在名单里：响应不带任何 CORS 头（浏览器会拦住），预检直接 403 `origin_not_allowed`；
//! - 配置了 CORS 之后，每个响应都带 `Vary: Origin`（共享缓存不会把给 A 来源的头发给 B 来源）。
//!
//! CORS 只作用于数据面 `/api/v1/*`。控制面（`/control/v1`）永远不发 CORS 头：它是后端对后端的接口。

use std::collections::BTreeSet;

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::reason::IngressReason;
use crate::cloud::EXPOSED_HEADERS;

/// 配置里最长的预检缓存时间（秒）：一天。
pub const MAX_PREFLIGHT_AGE_SECS: u64 = 86_400;

/// 入口自己的、除了 S3 的 [`EXPOSED_HEADERS`] 之外还要让浏览器读到的响应头。
pub const INGRESS_EXPOSED_HEADERS: &[&str] = &["Retry-After", super::HEADER_LIMIT_SCOPE];

/// 预检时允许浏览器带的请求头：只有认证和内容类型（成员 id 头是后端代理用的，浏览器不发）。
const ALLOWED_REQUEST_HEADERS: &str = "authorization, content-type";
const ALLOWED_METHODS: &str = "GET, POST, OPTIONS";

/// 暴露给浏览器的响应头：S3 的列表加上入口自己的。
pub fn exposed_headers() -> String {
    EXPOSED_HEADERS
        .iter()
        .chain(INGRESS_EXPOSED_HEADERS)
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}

/// CORS 策略：允许的来源（精确匹配）和预检缓存时间。空名单 = 不开放。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsPolicy {
    origins: BTreeSet<String>,
    max_age_secs: u64,
}

impl CorsPolicy {
    /// 校验并构造。每个来源必须是 `https://host[:port]`（回环地址允许 `http://`），小写，没有路径、查询、
    /// 用户信息、通配符。重复的来源是错误（几乎一定是复制粘贴的失误）。
    pub fn new(origins: &[String], max_age_secs: u64) -> Result<Self, String> {
        if max_age_secs > MAX_PREFLIGHT_AGE_SECS {
            return Err(format!(
                "cors.max_age_secs must be at most {MAX_PREFLIGHT_AGE_SECS}"
            ));
        }
        let mut set = BTreeSet::new();
        for origin in origins {
            validate_origin(origin)?;
            if !set.insert(origin.clone()) {
                return Err(format!("cors.allowed_origins lists {origin:?} twice"));
            }
        }
        Ok(Self {
            origins: set,
            max_age_secs,
        })
    }

    /// 是否开放跨域。默认（空名单）不开放。
    pub fn is_enabled(&self) -> bool {
        !self.origins.is_empty()
    }

    /// 来源在名单里吗（精确匹配，区分大小写）。
    pub fn allows(&self, origin: &str) -> bool {
        self.origins.contains(origin)
    }

    pub fn max_age_secs(&self) -> u64 {
        self.max_age_secs
    }

    pub fn origins(&self) -> impl Iterator<Item = &str> {
        self.origins.iter().map(String::as_str)
    }
}

fn validate_origin(origin: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("cors.allowed_origins entry {origin:?}: {why}"));
    let (scheme, authority) = if let Some(rest) = origin.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = origin.strip_prefix("http://") {
        ("http", rest)
    } else {
        return bad("must start with https:// (or http:// for a loopback host)");
    };
    if authority.is_empty() {
        return bad("has no host");
    }
    if origin != origin.to_ascii_lowercase() {
        return bad("must be lowercase, exactly as a browser sends it");
    }
    if authority.contains(['/', '?', '#', '@', '*', ' ', '\t', '\\']) {
        return bad("must be a bare origin: scheme, host and optional port only");
    }
    let (host, port) = match authority.strip_prefix('[') {
        // [::1] 或 [::1]:8443
        Some(rest) => {
            let Some((inside, tail)) = rest.split_once(']') else {
                return bad("has an unterminated IPv6 literal");
            };
            let port = match tail {
                "" => None,
                tail => match tail.strip_prefix(':') {
                    Some(port) => Some(port),
                    None => return bad("has junk after the IPv6 literal"),
                },
            };
            (inside, port)
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if host.is_empty() {
        return bad("has no host");
    }
    if let Some(port) = port
        && (port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return bad("has an invalid port");
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if scheme == "http" && !loopback {
        return bad("plain http is only allowed for localhost, 127.0.0.1 and [::1]");
    }
    Ok(())
}

/// 请求是不是预检：`OPTIONS`，带 `Origin` 和 `Access-Control-Request-Method`。
fn is_preflight(request: &Request) -> bool {
    request.method() == Method::OPTIONS
        && request.headers().contains_key(header::ORIGIN)
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

fn origin_of(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::ORIGIN).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    first.to_str().ok()
}

/// 如果这是一个预检请求并且 CORS 已经开放，直接给出答案（允许 204 / 不允许 403）；否则 `None`，请求照常往下走。
/// CORS 没开放时预检**不特殊处理**：它会落到「未知路由 / 方法」，也就是 404，和不存在 CORS 这个功能一样。
pub fn answer_preflight(policy: &CorsPolicy, request: &Request) -> Option<Response> {
    if !policy.is_enabled() || !is_preflight(request) {
        return None;
    }
    let allowed = origin_of(request.headers()).filter(|origin| policy.allows(origin));
    let Some(origin) = allowed else {
        let mut response = crate::error_envelope(
            StatusCode::FORBIDDEN,
            IngressReason::OriginNotAllowed.as_str(),
            "this origin is not allowed to call the API from a browser",
        );
        mark_vary(response.headers_mut());
        return Some(response);
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    set_allow_origin(headers, origin);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(ALLOWED_METHODS),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(ALLOWED_REQUEST_HEADERS),
    );
    if let Ok(age) = HeaderValue::from_str(&policy.max_age_secs().to_string()) {
        headers.insert(header::ACCESS_CONTROL_MAX_AGE, age);
    }
    mark_vary(headers);
    Some(response)
}

/// 给一个已经生成的响应补上 CORS 头：来源在名单里才发。`origin` 是请求的 `Origin`（没有 = 不是跨域请求）。
pub fn decorate(policy: &CorsPolicy, origin: Option<&str>, response: &mut Response) {
    if !policy.is_enabled() {
        return;
    }
    mark_vary(response.headers_mut());
    let Some(origin) = origin.filter(|origin| policy.allows(origin)) else {
        return;
    };
    set_allow_origin(response.headers_mut(), origin);
    if let Ok(value) = HeaderValue::from_str(&exposed_headers()) {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
}

/// 请求头里的 `Origin`（只认恰好一个、合法 UTF-8 的值）。
pub fn request_origin(headers: &HeaderMap) -> Option<String> {
    origin_of(headers).map(str::to_string)
}

fn set_allow_origin(headers: &mut HeaderMap, origin: &str) {
    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
}

fn mark_vary(headers: &mut HeaderMap) {
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use axum::body::Body;

    use super::*;

    fn policy(origins: &[&str]) -> CorsPolicy {
        CorsPolicy::new(
            &origins.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            600,
        )
        .unwrap()
    }

    fn request(method: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().method(method).uri("/api/v1/models");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn the_default_policy_is_closed() {
        let closed = CorsPolicy::default();
        assert!(!closed.is_enabled());
        assert!(!closed.allows("https://app.example.com"));
        let preflight = request(
            "OPTIONS",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
            ],
        );
        assert!(answer_preflight(&closed, &preflight).is_none());
        let mut response = StatusCode::OK.into_response();
        decorate(&closed, Some("https://app.example.com"), &mut response);
        assert!(response.headers().is_empty(), "{:?}", response.headers());
    }

    #[test]
    fn only_exact_bare_origins_are_accepted_at_configuration_time() {
        for good in [
            "https://app.example.com",
            "https://app.example.com:8443",
            "http://localhost:5173",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
            "https://[2001:db8::1]",
        ] {
            assert!(
                CorsPolicy::new(&[good.to_string()], 0).is_ok(),
                "{good} should be accepted"
            );
        }
        for bad in [
            "*",
            "null",
            "https://*.example.com",
            "https://example.com/",
            "https://example.com/app",
            "https://user@example.com",
            "https://example.com?x=1",
            "https://EXAMPLE.com",
            "http://example.com",
            "ftp://example.com",
            "example.com",
            "https://",
            "https://example.com:",
            "https://example.com:99999x",
            "https://[::1",
            "",
        ] {
            assert!(
                CorsPolicy::new(&[bad.to_string()], 0).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(
            CorsPolicy::new(
                &[
                    "https://a.example.com".to_string(),
                    "https://a.example.com".to_string()
                ],
                0
            )
            .is_err(),
            "duplicates are a typo"
        );
        assert!(CorsPolicy::new(&[], MAX_PREFLIGHT_AGE_SECS + 1).is_err());
    }

    #[test]
    fn a_listed_origin_gets_the_headers_and_an_unlisted_one_gets_none() {
        let policy = policy(&["https://app.example.com"]);
        let mut allowed = StatusCode::OK.into_response();
        decorate(&policy, Some("https://app.example.com"), &mut allowed);
        let headers = allowed.headers();
        assert_eq!(
            headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example.com"
        );
        assert_eq!(headers[header::VARY], "Origin");
        assert!(
            !headers.contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            "credentials are bearer tokens, never cookies"
        );
        let exposed = headers[header::ACCESS_CONTROL_EXPOSE_HEADERS]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        // S3 的列表一个不少，再加上入口自己的两个
        for name in EXPOSED_HEADERS.iter().chain(INGRESS_EXPOSED_HEADERS) {
            assert!(exposed.contains(&name.to_ascii_lowercase()), "{name}");
        }
        // 另一个来源：不发 Allow-Origin，也不发 Expose（浏览器会拦），但有 Vary
        let mut denied = StatusCode::OK.into_response();
        decorate(&policy, Some("https://evil.example.net"), &mut denied);
        assert!(
            !denied
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
        assert!(
            !denied
                .headers()
                .contains_key(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        );
        assert_eq!(denied.headers()[header::VARY], "Origin");
        // 不是跨域请求（没有 Origin）：同上
        let mut plain = StatusCode::OK.into_response();
        decorate(&policy, None, &mut plain);
        assert!(
            !plain
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
        // 区分大小写、不做子串匹配
        for sneaky in [
            "https://app.example.com.evil.net",
            "https://APP.example.com",
            "https://app.example.com:443",
            "http://app.example.com",
        ] {
            let mut response = StatusCode::OK.into_response();
            decorate(&policy, Some(sneaky), &mut response);
            assert!(
                !response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
                "{sneaky}"
            );
        }
    }

    #[test]
    fn the_exposed_list_is_s3s_list_plus_two_and_not_a_second_copy() {
        let exposed = exposed_headers();
        let parts: Vec<&str> = exposed.split(", ").collect();
        assert_eq!(
            parts.len(),
            EXPOSED_HEADERS.len() + INGRESS_EXPOSED_HEADERS.len()
        );
        assert_eq!(&parts[..EXPOSED_HEADERS.len()], EXPOSED_HEADERS);
        assert_eq!(
            INGRESS_EXPOSED_HEADERS,
            ["Retry-After", "X-iDoris-Limit-Scope"]
        );
    }

    #[test]
    fn preflight_is_answered_for_a_listed_origin_and_refused_for_the_rest() {
        let policy = policy(&["https://app.example.com"]);
        let allowed = request(
            "OPTIONS",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "authorization"),
            ],
        );
        let response = answer_preflight(&policy, &allowed).unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let h = response.headers();
        assert_eq!(
            h[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example.com"
        );
        assert_eq!(
            h[header::ACCESS_CONTROL_ALLOW_METHODS],
            "GET, POST, OPTIONS"
        );
        assert_eq!(
            h[header::ACCESS_CONTROL_ALLOW_HEADERS],
            "authorization, content-type"
        );
        assert_eq!(h[header::ACCESS_CONTROL_MAX_AGE], "600");
        assert!(!h.contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS));

        let denied = request(
            "OPTIONS",
            &[
                ("origin", "https://evil.example.net"),
                ("access-control-request-method", "POST"),
            ],
        );
        let response = answer_preflight(&policy, &denied).unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );

        // 不是预检：没有 Access-Control-Request-Method，或者不是 OPTIONS
        let not_preflight = request("OPTIONS", &[("origin", "https://app.example.com")]);
        assert!(answer_preflight(&policy, &not_preflight).is_none());
        let get = request(
            "GET",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
            ],
        );
        assert!(answer_preflight(&policy, &get).is_none());
        // 重复的 Origin 头：当作没有可用的来源，不允许
        let mut doubled = request(
            "OPTIONS",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
            ],
        );
        doubled
            .headers_mut()
            .append("origin", "https://evil.example.net".parse().unwrap());
        assert_eq!(
            answer_preflight(&policy, &doubled).unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}

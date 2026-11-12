//! 云入口自己的稳定字符串（计算服务 S5）。
//!
//! 这些码出现在三个地方，值永远相同：HTTP 响应体的 `error.type`、`error.reason_code`，以及响应头
//! `X-iDoris-Reason`（浏览器读得到，见 `cors`）。和 S4 留下的路由层字符串（`cloud_unavailable`、
//! `streaming_not_allowed`……）同一个做法：它们不在 S0 的 `ReasonCode` 里，因为那是「钱 / 付款上下文」
//! 的契约，而这里是「入口把请求挡在门外」的原因，和钱没有关系。
//!
//! # 线上字符串不可变
//!
//! 调用方（eduai 后端、浏览器里的 JS、Agent24）按字符串匹配，所以和 S0 的原因码一样：只能加新变体、
//! 加新字符串，不能改旧的、不能复用旧的。`tests` 里把每一个字符串和状态码钉死了。
//!
//! # 不在这里的
//!
//! - 401：凭证有问题（不认识、撤销、冻结、过期、成员关系失效……）一律是**同一个**不透明的 401，
//!   沿用本地数据面的 `unauthorized` / `VIRTUAL_KEY_UNAUTHORIZED`，不告诉调用方是哪一种（不给探测凭证的口子）。
//! - 付款上下文无法确定时（成员 id 越界、主体被撤销……）的 403 用 S0 的 `ReasonCode`
//!   （`payment_context_invalid` / `principal_revoked` / `membership_revoked` / `account_inactive`），
//!   因为那正是 S0 为它定的码。
//! - 花费闸门（S3）的 `rate_limited` / `in_flight_limited` / `subsidy_pool_exhausted` / `budget_exhausted`：
//!   那是「钱」的上限，由准入账本给出。这里的 `request_rate_limited` / `concurrency_limited` 是「请求个数 / 并发」
//!   的上限，两套闸门互补、互不替代，所以码也不同：调用方一眼看得出是哪道闸门拦的。

use axum::http::StatusCode;

/// 云入口拒绝一个请求的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IngressReason {
    /// 同一个凭证前缀在窗口内认证失败太多次（防爆破，也防一个带着已撤销凭证死循环重试的客户端）。429。
    AuthThrottled,
    /// 没有带凭证（或带的格式不对）的请求太快了。只作用于「没有凭证」的请求，永远不会拦住带着有效凭证的请求。429。
    AnonymousRateLimited,
    /// 请求速率上限（成员 / 租户 / 平台，哪一级见 `X-iDoris-Limit-Scope`）。429，带 `Retry-After`。
    RequestRateLimited,
    /// 并发（在途请求数）上限（成员 / 租户 / 平台，哪一级见 `X-iDoris-Limit-Scope`）。429，带 `Retry-After`。
    ConcurrencyLimited,
    /// 请求体超过上限。413。
    BodyTooLarge,
    /// 请求体在期限内没有读完（慢请求）。408。
    RequestTimeout,
    /// `X-iDoris-Member` 不合法（重复、空、含非法字符、过长）。400。
    MemberIdInvalid,
    /// 凭证的租户没有在云入口配置里声明数据处理要求：不知道该用什么标准处理这些数据，所以不处理。403。
    TenantNotProvisioned,
    /// 跨域请求的来源不在配置的白名单里（只在配置了 CORS 之后才可能出现）。403。
    OriginNotAllowed,
    /// 云入口自己出了问题（读不了账户库……），失败即关闭。503。沿用 S4 的同名字符串。
    CloudUnavailable,
}

impl IngressReason {
    /// 全部原因码，顺序固定（= 变体声明顺序）。
    pub const ALL: [IngressReason; 10] = [
        IngressReason::AuthThrottled,
        IngressReason::AnonymousRateLimited,
        IngressReason::RequestRateLimited,
        IngressReason::ConcurrencyLimited,
        IngressReason::BodyTooLarge,
        IngressReason::RequestTimeout,
        IngressReason::MemberIdInvalid,
        IngressReason::TenantNotProvisioned,
        IngressReason::OriginNotAllowed,
        IngressReason::CloudUnavailable,
    ];

    /// 线上字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            IngressReason::AuthThrottled => "auth_throttled",
            IngressReason::AnonymousRateLimited => "anonymous_rate_limited",
            IngressReason::RequestRateLimited => "request_rate_limited",
            IngressReason::ConcurrencyLimited => "concurrency_limited",
            IngressReason::BodyTooLarge => "request_body_too_large",
            IngressReason::RequestTimeout => "request_timeout",
            IngressReason::MemberIdInvalid => "member_id_invalid",
            IngressReason::TenantNotProvisioned => "tenant_not_provisioned",
            IngressReason::OriginNotAllowed => "origin_not_allowed",
            IngressReason::CloudUnavailable => "cloud_unavailable",
        }
    }

    /// 对应的 HTTP 状态码。**不可变**。
    pub fn status(self) -> StatusCode {
        match self {
            IngressReason::AuthThrottled
            | IngressReason::AnonymousRateLimited
            | IngressReason::RequestRateLimited
            | IngressReason::ConcurrencyLimited => StatusCode::TOO_MANY_REQUESTS,
            IngressReason::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            IngressReason::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
            IngressReason::MemberIdInvalid => StatusCode::BAD_REQUEST,
            IngressReason::TenantNotProvisioned | IngressReason::OriginNotAllowed => {
                StatusCode::FORBIDDEN
            }
            IngressReason::CloudUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// 是不是限流类（带 `Retry-After`）。
    pub fn is_throttle(self) -> bool {
        matches!(
            self,
            IngressReason::AuthThrottled
                | IngressReason::AnonymousRateLimited
                | IngressReason::RequestRateLimited
                | IngressReason::ConcurrencyLimited
        )
    }
}

impl std::fmt::Display for IngressReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// 线上契约的权威清单：字符串和状态码。改这里就是在改契约，不允许。
    const PINNED: [(IngressReason, &str, u16); 10] = [
        (IngressReason::AuthThrottled, "auth_throttled", 429),
        (
            IngressReason::AnonymousRateLimited,
            "anonymous_rate_limited",
            429,
        ),
        (
            IngressReason::RequestRateLimited,
            "request_rate_limited",
            429,
        ),
        (
            IngressReason::ConcurrencyLimited,
            "concurrency_limited",
            429,
        ),
        (IngressReason::BodyTooLarge, "request_body_too_large", 413),
        (IngressReason::RequestTimeout, "request_timeout", 408),
        (IngressReason::MemberIdInvalid, "member_id_invalid", 400),
        (
            IngressReason::TenantNotProvisioned,
            "tenant_not_provisioned",
            403,
        ),
        (IngressReason::OriginNotAllowed, "origin_not_allowed", 403),
        (IngressReason::CloudUnavailable, "cloud_unavailable", 503),
    ];

    #[test]
    fn every_wire_string_and_status_is_pinned() {
        let in_table: Vec<IngressReason> = PINNED.iter().map(|(code, _, _)| *code).collect();
        assert_eq!(in_table, IngressReason::ALL.to_vec());
        for (code, text, status) in PINNED {
            assert_eq!(code.as_str(), text);
            assert_eq!(code.to_string(), text);
            assert_eq!(code.status().as_u16(), status, "{text}");
        }
    }

    #[test]
    fn strings_are_unique_snake_case_and_never_collide_with_the_s0_codes() {
        let strings: BTreeSet<&str> = PINNED.iter().map(|(_, text, _)| *text).collect();
        assert_eq!(strings.len(), PINNED.len());
        for (_, text, _) in PINNED {
            assert!(
                !text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{text}"
            );
            // S0 的码是「钱 / 付款上下文」的契约，这里的码不能和它们同名（同名就分不清是哪道闸门）。
            if text != "cloud_unavailable" {
                assert!(
                    text.parse::<idoris_compute_contracts::reason::ReasonCode>()
                        .is_err(),
                    "{text} collides with an S0 reason code"
                );
            }
        }
    }

    #[test]
    fn only_the_four_throttles_carry_retry_after() {
        let throttles: Vec<&str> = IngressReason::ALL
            .into_iter()
            .filter(|code| code.is_throttle())
            .map(IngressReason::as_str)
            .collect();
        assert_eq!(
            throttles,
            [
                "auth_throttled",
                "anonymous_rate_limited",
                "request_rate_limited",
                "concurrency_limited"
            ]
        );
        for code in IngressReason::ALL {
            assert_eq!(
                code.is_throttle(),
                code.status() == StatusCode::TOO_MANY_REQUESTS
            );
        }
    }
}

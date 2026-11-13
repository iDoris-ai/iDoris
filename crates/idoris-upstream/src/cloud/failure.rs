//! 云上游调用的失败分类：稳定的代码，**不含**响应正文、请求内容或密钥。
//!
//! 这些代码会写进尝试记录（`failure_kind`）、日志和给客户端的错误体，所以只能是固定字符串。
//! 它们描述的是「这次调用在哪一步、怎么失败的」，是否能证明上游没有执行由契约的状态机裁决
//! （只有 4xx 能证明，见 `idoris_compute_contracts::attempt::is_refusal_status`），这里不下结论。

use super::wire::CompletionError;

/// 一次上游调用的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudFailure {
    // ---- 请求发出之前 / 发出时 ----
    /// 连接阶段失败（拒绝连接、DNS、TLS 握手）。
    ConnectFailed,
    /// 连接阶段超时。
    ConnectTimeout,
    /// 连上之后、响应头回来之前出错（连接被重置、写失败）。
    SendFailed,
    /// 响应头在期限内没有回来。
    ResponseHeadTimeout,
    // ---- 响应头 ----
    /// 非 2xx 的状态码（含 3xx：不跟随重定向）。
    Status(u16),
    // ---- 响应体（非流式）----
    BodyReadFailed,
    BodyTimeout,
    BodyTooLarge,
    /// 2xx，但响应体不可用。
    Response(CompletionError),
    // ---- 流 ----
    /// 流在没有 `[DONE]`（或没有以空行结束的最后一个事件）时结束了。
    StreamTruncated,
    StreamIdleTimeout,
    /// 流的总时长超过了上限（一直在涓涓细流地发，但永远不结束）。
    StreamTotalTimeout,
    StreamEventTooLarge,
    /// 上游在流里用一个 `error` 事件报错。
    StreamErrorEvent,
    /// 2xx，但不是 `text/event-stream`，或第一个事件就不是合法的 chunk。
    StreamMalformed,
}

impl CloudFailure {
    /// 稳定的代码。
    pub fn code(self) -> &'static str {
        match self {
            CloudFailure::ConnectFailed => "connect_failed",
            CloudFailure::ConnectTimeout => "connect_timeout",
            CloudFailure::SendFailed => "send_failed",
            CloudFailure::ResponseHeadTimeout => "response_head_timeout",
            CloudFailure::Status(_) => "http_status",
            CloudFailure::BodyReadFailed => "body_read_failed",
            CloudFailure::BodyTimeout => "body_timeout",
            CloudFailure::BodyTooLarge => "body_too_large",
            CloudFailure::Response(error) => error.code(),
            CloudFailure::StreamTruncated => "stream_truncated",
            CloudFailure::StreamIdleTimeout => "stream_idle_timeout",
            CloudFailure::StreamTotalTimeout => "stream_total_timeout",
            CloudFailure::StreamEventTooLarge => "stream_event_too_large",
            CloudFailure::StreamErrorEvent => "stream_error_event",
            CloudFailure::StreamMalformed => "stream_malformed",
        }
    }

    /// 带状态码的失败。
    pub fn http_status(self) -> Option<u16> {
        match self {
            CloudFailure::Status(status) => Some(status),
            _ => None,
        }
    }

    /// 超时类失败（给客户端的状态码用 504 而不是 502）。
    pub fn is_timeout(self) -> bool {
        matches!(
            self,
            CloudFailure::ConnectTimeout
                | CloudFailure::ResponseHeadTimeout
                | CloudFailure::BodyTimeout
                | CloudFailure::StreamIdleTimeout
                | CloudFailure::StreamTotalTimeout
        )
    }
}

impl std::fmt::Display for CloudFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloudFailure::Status(status) => write!(f, "upstream HTTP status {status}"),
            other => f.write_str(other.code()),
        }
    }
}

impl std::error::Error for CloudFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_pairwise_distinct() {
        let all = [
            CloudFailure::ConnectFailed,
            CloudFailure::ConnectTimeout,
            CloudFailure::SendFailed,
            CloudFailure::ResponseHeadTimeout,
            CloudFailure::Status(503),
            CloudFailure::BodyReadFailed,
            CloudFailure::BodyTimeout,
            CloudFailure::BodyTooLarge,
            CloudFailure::Response(CompletionError::NotJson),
            CloudFailure::Response(CompletionError::NoChoices),
            CloudFailure::StreamTruncated,
            CloudFailure::StreamIdleTimeout,
            CloudFailure::StreamTotalTimeout,
            CloudFailure::StreamEventTooLarge,
            CloudFailure::StreamErrorEvent,
            CloudFailure::StreamMalformed,
        ];
        let codes: Vec<&str> = all.iter().map(|f| f.code()).collect();
        let mut unique = codes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "{codes:?}");
        // 线上字符串：改了就是改了对账用的代码
        assert_eq!(
            codes,
            vec![
                "connect_failed",
                "connect_timeout",
                "send_failed",
                "response_head_timeout",
                "http_status",
                "body_read_failed",
                "body_timeout",
                "body_too_large",
                "response_not_json",
                "response_no_choices",
                "stream_truncated",
                "stream_idle_timeout",
                "stream_total_timeout",
                "stream_event_too_large",
                "stream_error_event",
                "stream_malformed",
            ]
        );
    }

    #[test]
    fn only_status_failures_carry_a_status_and_the_timeouts_are_flagged() {
        assert_eq!(CloudFailure::Status(429).http_status(), Some(429));
        assert_eq!(CloudFailure::ConnectFailed.http_status(), None);
        assert!(CloudFailure::ResponseHeadTimeout.is_timeout());
        assert!(CloudFailure::StreamIdleTimeout.is_timeout());
        assert!(!CloudFailure::ConnectFailed.is_timeout());
        assert!(!CloudFailure::Status(504).is_timeout());
        assert_eq!(
            CloudFailure::Status(503).to_string(),
            "upstream HTTP status 503"
        );
    }
}

//! 流式响应里一个 SSE 事件的含义：结束标记、注释、带内容的 JSON chunk、上游在流里报的错。
//!
//! 只读取我们要用的结构化字段，**不解读、不记录消息内容**：
//! - `id`：供应商请求号（OpenAI 兼容各家把它放在每个 chunk 的顶层 `id`），对账用；
//! - `usage`：`stream_options.include_usage` 之后，最后一个 chunk（`choices` 为空数组）带的用量；
//! - `error`：有些上游在 HTTP 200 之后用一个 `{"error": ...}` 事件报错。

use serde_json::Value;

use super::sse::SseEvent;
use super::usage::{UsageParse, parse_usage};

/// 供应商请求号的最大长度（字节）。更长的当作没有（对账键不会有这么长）。
pub const MAX_SUPPLIER_ID_BYTES: usize = 256;

/// 一个 JSON chunk 里我们关心的字段。
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkInfo {
    /// 顶层 `id`（字符串且不超过 [`MAX_SUPPLIER_ID_BYTES`]）。
    pub id: Option<String>,
    /// 顶层 `usage` 的判定。
    pub usage: UsageParse,
    /// 顶层有非空的 `error`：上游在流里报了错。
    pub error: bool,
    /// 顶层 `choices` 是数组。
    pub has_choices: bool,
    /// `choices` 是空数组（usage 单独成块时的样子）。
    pub choices_empty: bool,
    /// `choices` 是数组，而且每个元素都是对象（一个合法的 completion chunk 的最低要求）。
    pub choices_well_formed: bool,
    /// 某个 choice 带了非空的 `finish_reason`：这个 choice 的生成已经结束（「最后一块内容」的标志）。
    pub has_finish_reason: bool,
}

/// 一个 SSE 事件的分类。
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkKind {
    /// `data: [DONE]`。
    Done,
    /// 注释或没有 `data` 的事件（keep-alive）：不带任何响应内容。
    Ignorable,
    /// `data:` 是一个 JSON 对象。
    Json(ChunkInfo),
    /// `data:` 不是 JSON 对象（解析失败、数组、标量）。
    Opaque,
}

fn trimmed(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// 分类一个事件。
pub fn classify(event: &SseEvent) -> ChunkKind {
    let Some(data) = event.data.as_deref() else {
        return ChunkKind::Ignorable;
    };
    let data = trimmed(data);
    if data == b"[DONE]" {
        return ChunkKind::Done;
    }
    match serde_json::from_slice::<Value>(data) {
        Ok(Value::Object(object)) => {
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= MAX_SUPPLIER_ID_BYTES)
                .map(str::to_owned);
            let choices = object.get("choices").and_then(Value::as_array);
            ChunkKind::Json(ChunkInfo {
                id,
                usage: parse_usage(object.get("usage")),
                error: object.get("error").is_some_and(|e| !e.is_null()),
                has_choices: choices.is_some(),
                choices_empty: choices.is_some_and(Vec::is_empty),
                choices_well_formed: choices.is_some_and(|list| list.iter().all(Value::is_object)),
                has_finish_reason: choices.is_some_and(|list| {
                    list.iter().any(|choice| {
                        choice
                            .get("finish_reason")
                            .is_some_and(|reason| !reason.is_null())
                    })
                }),
            })
        }
        _ => ChunkKind::Opaque,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn event(data: &str) -> SseEvent {
        SseEvent::from_lines(&[format!("data: {data}").as_bytes()])
    }

    #[test]
    fn done_comment_and_opaque() {
        assert_eq!(classify(&event("[DONE]")), ChunkKind::Done);
        assert_eq!(classify(&event("  [DONE]  ")), ChunkKind::Done);
        assert_eq!(
            classify(&SseEvent::from_lines(&[b": keep-alive"])),
            ChunkKind::Ignorable
        );
        assert_eq!(
            classify(&SseEvent::from_lines(&[b"event: ping"])),
            ChunkKind::Ignorable
        );
        assert_eq!(classify(&event("not json")), ChunkKind::Opaque);
        assert_eq!(classify(&event("[1,2]")), ChunkKind::Opaque);
        assert_eq!(classify(&event("42")), ChunkKind::Opaque);
        assert_eq!(classify(&event("\"x\"")), ChunkKind::Opaque);
    }

    #[test]
    fn a_content_chunk_exposes_id_and_choices_but_not_content() {
        let ChunkKind::Json(info) = classify(&event(
            r#"{"id":"chatcmpl-9","object":"chat.completion.chunk","choices":[{"delta":{"content":"hi"}}]}"#,
        )) else {
            panic!("expected a JSON chunk");
        };
        assert_eq!(info.id.as_deref(), Some("chatcmpl-9"));
        assert!(info.has_choices);
        assert!(!info.choices_empty);
        assert!(!info.error);
        assert_eq!(info.usage, UsageParse::Absent);
    }

    #[test]
    fn the_usage_only_final_chunk_has_empty_choices() {
        let ChunkKind::Json(info) = classify(&event(
            r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#,
        )) else {
            panic!("expected a JSON chunk");
        };
        assert!(info.has_choices && info.choices_empty);
        assert_eq!(info.usage.valid_usage().unwrap().completion_tokens, 4);
    }

    #[test]
    fn choices_shape_and_finish_reason_are_exposed_for_the_final_chunk_decision() {
        let info = |data: &str| match classify(&event(data)) {
            ChunkKind::Json(info) => info,
            other => panic!("expected a JSON chunk, got {other:?}"),
        };
        // 内容块：choices 非空、合法、没有 finish_reason
        let content = info(r#"{"choices":[{"delta":{"content":"x"},"finish_reason":null}]}"#);
        assert!(content.choices_well_formed && !content.has_finish_reason);
        // 最后一块内容：带 finish_reason
        let last = info(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#);
        assert!(last.choices_well_formed && last.has_finish_reason);
        // 单独的 usage 块：choices 为空，也是合法的
        let usage_only =
            info(r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#);
        assert!(usage_only.choices_well_formed && usage_only.choices_empty);
        assert!(!usage_only.has_finish_reason);
        // 畸形：没有 choices、choices 不是数组、元素不是对象
        for data in [r#"{}"#, r#"{"choices":"x"}"#, r#"{"choices":[1]}"#] {
            let bad = info(data);
            assert!(!bad.choices_well_formed, "{data}");
        }
    }

    #[test]
    fn an_error_event_is_flagged() {
        let ChunkKind::Json(info) = classify(&event(r#"{"error":{"message":"overloaded"}}"#))
        else {
            panic!("expected a JSON chunk");
        };
        assert!(info.error);
        assert!(!info.has_choices);
        // `error: null` 不算
        let ChunkKind::Json(info) = classify(&event(r#"{"error":null,"choices":[]}"#)) else {
            panic!("expected a JSON chunk");
        };
        assert!(!info.error);
    }

    #[test]
    fn oversized_or_non_string_ids_are_ignored() {
        let long = "x".repeat(MAX_SUPPLIER_ID_BYTES + 1);
        for data in [
            format!(r#"{{"id":"{long}","choices":[]}}"#),
            r#"{"id":42,"choices":[]}"#.to_string(),
            r#"{"id":"","choices":[]}"#.to_string(),
        ] {
            let ChunkKind::Json(info) = classify(&event(&data)) else {
                panic!("expected a JSON chunk");
            };
            assert_eq!(info.id, None, "{data}");
        }
    }
}

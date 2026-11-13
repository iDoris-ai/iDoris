//! 流式响应里一个 SSE 事件的含义：结束标记、注释、带内容的 JSON chunk、上游在流里报的错。
//!
//! 只读取我们要用的结构化字段，**不解读、不记录消息内容**：
//! - `id`：供应商请求号（OpenAI 兼容各家把它放在每个 chunk 的顶层 `id`），对账用；
//! - `usage`：`stream_options.include_usage` 之后，最后一个 chunk（`choices` 为空数组）带的用量；
//! - `error`：有些上游在 HTTP 200 之后用一个 `{"error": ...}` 事件报错；
//! - `choices` 的形状、有没有 `finish_reason`、`delta` 里有没有**新的生成内容**（只看「有没有」，不读内容本身）：
//!   调用方据此判断一个 chunk 是不是合法的 completion chunk，以及一个 usage 之后生成有没有继续（见 `ChunkInfo`）。

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
    /// `choices` 是数组，而且每个元素都是**像样的 choice**（一个合法的 completion chunk 的最低要求）：对象，并且带 `delta`
    /// 对象（内容块、空 delta 的块都行），或者带已结束的 `finish_reason`（有的实现最后一块没有 `delta`）。
    /// `{"choices":[{}]}` 这种空 choice 不是（R3 Medium-3）：它不带任何东西，却会被当成回答转发、跨过发布边界。
    /// 空数组是合法的（单独的 usage 块的样子）。
    pub choices_well_formed: bool,
    /// 某个 choice 带了已结束的 `finish_reason`（非 `null`、也不是空字符串；有的兼容实现在没结束的块里写 `""`）：
    /// 这个 choice 的生成已经结束。
    pub has_finish_reason: bool,
    /// 某个 choice 的 `delta` 带了**新的生成内容**：除顶层 `role` 之外的任何地方出现非空字符串（`content`、`refusal`、
    /// `reasoning_content`、工具调用的名字和参数……）。`role: assistant`、`content: ""`、`null`、空数组、空对象、
    /// 空 delta 的结束块都不算。故意不按字段名列白名单：不认识的新字段里有文字也当内容——误判的方向是「不信 usage」
    /// （按预留价），不是「少收」。
    pub has_content: bool,
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

/// 这个 choice 的 `finish_reason` 表示生成已经结束：非 `null`，也不是空字符串。
fn choice_finished(choice: &Value) -> bool {
    match choice.get("finish_reason") {
        None | Some(Value::Null) => false,
        Some(Value::String(reason)) => !reason.is_empty(),
        Some(_) => true,
    }
}

/// 一个像样的 choice：是对象，并且带 `delta` 对象，或者带已结束的 `finish_reason`。
fn choice_is_well_formed(choice: &Value) -> bool {
    choice.is_object()
        && (choice.get("delta").is_some_and(Value::is_object) || choice_finished(choice))
}

/// `delta` 里有没有新的生成内容（见 [`ChunkInfo::has_content`]）。
fn delta_carries_content(delta: &Value) -> bool {
    fn has_text(value: &Value) -> bool {
        match value {
            Value::String(text) => !text.is_empty(),
            Value::Array(items) => items.iter().any(has_text),
            Value::Object(fields) => fields.values().any(has_text),
            _ => false,
        }
    }
    delta.as_object().is_some_and(|fields| {
        fields
            .iter()
            .any(|(key, value)| key != "role" && has_text(value))
    })
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
                choices_well_formed: choices
                    .is_some_and(|list| list.iter().all(choice_is_well_formed)),
                has_finish_reason: choices.is_some_and(|list| list.iter().any(choice_finished)),
                has_content: choices.is_some_and(|list| {
                    list.iter()
                        .any(|choice| choice.get("delta").is_some_and(delta_carries_content))
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

    /// R3 Medium-3：一个 choice 必须像样（带 `delta` 对象，或已结束的 `finish_reason`），`{"choices":[{}]}` 之类的空 choice
    /// 不是合法的 completion chunk（只检查「元素是对象」会把它当回答转发、让它跨过发布边界）。
    #[test]
    fn a_choice_with_nothing_in_it_is_not_a_completion_chunk() {
        let info = |data: &str| match classify(&event(data)) {
            ChunkKind::Json(info) => info,
            other => panic!("expected a JSON chunk, got {other:?}"),
        };
        for data in [
            r#"{"choices":[{}]}"#,
            r#"{"choices":[{"index":0}]}"#,
            r#"{"choices":[{"delta":null}]}"#,
            r#"{"choices":[{"delta":"text"}]}"#,
            r#"{"choices":[{"delta":[]}]}"#,
            r#"{"choices":[{"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"finish_reason":""}]}"#,
            // 一个像样的、一个空的：整体不合法
            r#"{"choices":[{"delta":{"content":"x"}},{}]}"#,
        ] {
            let parsed = info(data);
            assert!(parsed.has_choices, "{data}");
            assert!(!parsed.choices_well_formed, "{data}");
        }
        for data in [
            r#"{"choices":[{"delta":{}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            // 有的实现最后一块没有 delta，只有 finish_reason
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"finish_reason":"length","index":0}]}"#,
            // 单独的 usage 块：空数组
            r#"{"choices":[]}"#,
        ] {
            assert!(info(data).choices_well_formed, "{data}");
        }
    }

    /// 空字符串的 `finish_reason` 不是「已结束」（有的兼容实现在没结束的块里写 `""`）。
    #[test]
    fn an_empty_finish_reason_does_not_mean_the_choice_finished() {
        let info = |data: &str| match classify(&event(data)) {
            ChunkKind::Json(info) => info,
            other => panic!("expected a JSON chunk, got {other:?}"),
        };
        for data in [
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":""}]}"#,
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"content":"x"}}]}"#,
        ] {
            assert!(!info(data).has_finish_reason, "{data}");
        }
        for data in [
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#,
        ] {
            assert!(info(data).has_finish_reason, "{data}");
        }
    }

    /// R3 N3：只有**新的生成内容**算内容；空 delta 的结束块、只带 `role` 的块、空字符串 / `null` / 空容器都不算。
    #[test]
    fn only_generated_text_counts_as_new_content() {
        let content = |data: &str| match classify(&event(data)) {
            ChunkKind::Json(info) => info.has_content,
            other => panic!("expected a JSON chunk, got {other:?}"),
        };
        for data in [
            r#"{"choices":[]}"#,
            r#"{"choices":[{"delta":{}}]}"#,
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"content":null,"refusal":null,"tool_calls":[]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"function_call":{"name":"","arguments":""}}}]}"#,
            // 没有 choices 的单独 usage 块
            r#"{"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ] {
            assert!(!content(data), "{data}");
        }
        for data in [
            r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            r#"{"choices":[{"delta":{"content":" "}}]}"#,
            r#"{"choices":[{"delta":{"refusal":"no"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\""}}]}}]}"#,
            // 最后一块内容 + finish_reason：有内容
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
            // 不认识的新字段里有文字：当内容（宁可不信 usage，不能少收）
            r#"{"choices":[{"delta":{"some_new_field":{"deep":["text"]}}}]}"#,
        ] {
            assert!(content(data), "{data}");
        }
        // 内容跟着 role 一起来：有内容
        assert!(content(
            r#"{"choices":[{"delta":{"role":"assistant","content":"x"}}]}"#
        ));
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

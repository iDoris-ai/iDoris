//! 流式响应里一个 SSE 事件的含义：结束标记、注释、带内容的 JSON chunk、上游在流里报的错。
//!
//! 只读取我们要用的结构化字段，**不解读、不记录消息内容**：
//! - `id`：供应商请求号（OpenAI 兼容各家把它放在每个 chunk 的顶层 `id`），对账用；
//! - `usage`：`stream_options.include_usage` 之后，最后一个 chunk（`choices` 为空数组）带的用量；
//! - `error`：有些上游在 HTTP 200 之后用一个 `{"error": ...}` 事件报错；
//! - `choices` 的形状、每个 choice 的 `index` / `finish_reason` / `delta` 的**类型**、有没有 `finish_reason`、`delta` 里有没有
//!   **新的生成内容**（只看「有没有」，不读内容本身）：调用方据此判断一个 chunk 是不是合法的 completion chunk，
//!   以及一个 usage 之后生成有没有继续、每个 choice 的结束标志是不是比它最近的新内容更晚（见 [`ChunkInfo`]）。

use serde_json::Value;

use super::sse::SseEvent;
use super::usage::{UsageParse, parse_usage};

/// 供应商请求号的最大长度（字节）。更长的当作没有（对账键不会有这么长）。
pub const MAX_SUPPLIER_ID_BYTES: usize = 256;

/// 一个 choice 里我们关心的事实（见 [`ChunkInfo::choices`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChoiceFacts {
    /// choice 的 `index`；没写（或写 `null`）就取它在这个 chunk 的 `choices` 数组里的位置。
    /// 我们从不请求 `n > 1`，正常的上游只有一个 choice（`index` 0）；调用方按它区分各个 choice，
    /// 好让「某个 choice 结束」不会被当成「所有 choice 都结束」。
    pub index: u64,
    /// `finish_reason` 是非空字符串：这个 choice 的生成已经结束。
    pub finished: bool,
    /// 这个 choice 的 `delta` 带了**新的生成内容**（定义见 [`ChunkInfo::has_content`]）。
    pub has_content: bool,
}

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
    /// `choices` 是数组，而且每个元素都是**像样的 choice**（一个合法的 completion chunk 的最低要求）：对象，字段类型都对，
    /// 并且带 `delta` 对象（内容块、空 delta 的块都行），或者带已结束的 `finish_reason`（有的实现最后一块没有 `delta`）。
    /// 字段类型：`delta` 缺省、`null` 或对象；`finish_reason` 缺省、`null` 或字符串；`index` 缺省、`null` 或非负整数。
    /// `{"choices":[{}]}` 这种空 choice 不是（R3 Medium-3）：它不带任何东西，却会被当成回答转发、跨过发布边界；
    /// `{"choices":[{"finish_reason":false}]}` 这种类型不对的也不是（第六轮：原来任何非 `null`、非空字符串的值都算「已结束」）。
    /// 空数组是合法的（单独的 usage 块的样子）。
    pub choices_well_formed: bool,
    /// 每个 choice 的事实（按它们在 `choices` 里的顺序；不是对象的元素没有）。**只有 `choices_well_formed` 为真时才有意义**。
    pub choices: Vec<ChoiceFacts>,
    /// 某个 choice 带了已结束的 `finish_reason`（**非空字符串**；`null`、空字符串——有的兼容实现在没结束的块里写 `""`——
    /// 和任何别的类型都不算）：这个 choice 的生成已经结束。
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

/// 解析一个 choice：事实，以及它是不是「像样的 choice」。不是对象返回 `None`。
fn parse_choice(position: usize, choice: &Value) -> Option<(ChoiceFacts, bool)> {
    let fields = choice.as_object()?;
    // `finish_reason`：缺省 / `null` / 字符串。别的类型（布尔值、数字、对象、数组）是类型错误，不算「已结束」
    let (finish_typed, finished) = match fields.get("finish_reason") {
        None | Some(Value::Null) => (true, false),
        Some(Value::String(reason)) => (true, !reason.is_empty()),
        Some(_) => (false, false),
    };
    // `delta`：缺省 / `null` / 对象
    let (delta_typed, delta) = match fields.get("delta") {
        None | Some(Value::Null) => (true, None),
        Some(delta @ Value::Object(_)) => (true, Some(delta)),
        Some(_) => (false, None),
    };
    // `index`：缺省 / `null` / 非负整数
    let fallback = u64::try_from(position).unwrap_or(u64::MAX);
    let (index_typed, index) = match fields.get("index") {
        None | Some(Value::Null) => (true, fallback),
        Some(value) => match value.as_u64() {
            Some(index) => (true, index),
            None => (false, fallback),
        },
    };
    let facts = ChoiceFacts {
        index,
        finished,
        has_content: delta.is_some_and(delta_carries_content),
    };
    // 像样：字段类型都对，并且带 `delta` 对象或已结束的 `finish_reason`
    let well_formed = finish_typed && delta_typed && index_typed && (delta.is_some() || finished);
    Some((facts, well_formed))
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
            let parsed: Vec<Option<(ChoiceFacts, bool)>> = choices.map_or_else(Vec::new, |list| {
                list.iter()
                    .enumerate()
                    .map(|(position, choice)| parse_choice(position, choice))
                    .collect()
            });
            let facts: Vec<ChoiceFacts> = parsed
                .iter()
                .flatten()
                .map(|(facts, _well_formed)| *facts)
                .collect();
            ChunkKind::Json(ChunkInfo {
                id,
                usage: parse_usage(object.get("usage")),
                error: object.get("error").is_some_and(|e| !e.is_null()),
                has_choices: choices.is_some(),
                choices_empty: choices.is_some_and(Vec::is_empty),
                choices_well_formed: choices.is_some()
                    && parsed
                        .iter()
                        .all(|choice| choice.is_some_and(|(_, well_formed)| well_formed)),
                has_finish_reason: facts.iter().any(|choice| choice.finished),
                has_content: facts.iter().any(|choice| choice.has_content),
                choices: facts,
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

    /// 第六轮（R4 Medium-3 残留）：`finish_reason` 只有字符串才是合法的结束原因。布尔值、数字、对象、数组都不是——
    /// 这样的 choice 字段类型不对，整个 chunk 不是合法的 completion chunk，更不算「已结束」。
    /// 原来任何非 `null`、非空字符串的值都算结束：`{"choices":[{"finish_reason":false}]}` 被当成合法回答转发，
    /// 单独送出、客户端收到后再报错，就跨过了发布边界，还按未知 usage 收费。
    #[test]
    fn a_finish_reason_that_is_not_a_string_is_a_malformed_choice_not_a_finished_one() {
        let info = |data: &str| match classify(&event(data)) {
            ChunkKind::Json(info) => info,
            other => panic!("expected a JSON chunk, got {other:?}"),
        };
        for data in [
            r#"{"choices":[{"finish_reason":false}]}"#,
            r#"{"choices":[{"finish_reason":true}]}"#,
            r#"{"choices":[{"finish_reason":0}]}"#,
            r#"{"choices":[{"finish_reason":1}]}"#,
            r#"{"choices":[{"finish_reason":1.5}]}"#,
            r#"{"choices":[{"finish_reason":{}}]}"#,
            r#"{"choices":[{"finish_reason":{"reason":"stop"}}]}"#,
            r#"{"choices":[{"finish_reason":[]}]}"#,
            r#"{"choices":[{"finish_reason":["stop"]}]}"#,
            // 有 `delta` 对象也一样：类型不对的字段不会因为别的字段像样就被放过
            r#"{"choices":[{"delta":{},"finish_reason":false}]}"#,
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":{}}]}"#,
            // 一个像样的、一个类型不对的：整体不合法
            r#"{"choices":[{"delta":{"content":"x"}},{"delta":{},"finish_reason":0}]}"#,
        ] {
            let parsed = info(data);
            assert!(parsed.has_choices, "{data}");
            assert!(!parsed.choices_well_formed, "{data}");
            assert!(
                !parsed.has_finish_reason,
                "a non-string finish_reason must never count as the end of the generation: {data}"
            );
        }
        // 对照：`null`、缺省、空字符串（有的兼容实现在没结束的块里写 `""`）是「还没结束」，字符串是「已结束」
        for (data, finished) in [
            (r#"{"choices":[{"delta":{},"finish_reason":null}]}"#, false),
            (r#"{"choices":[{"delta":{}}]}"#, false),
            (r#"{"choices":[{"delta":{},"finish_reason":""}]}"#, false),
            (r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#, true),
            (r#"{"choices":[{"finish_reason":"length"}]}"#, true),
        ] {
            let parsed = info(data);
            assert!(parsed.choices_well_formed, "{data}");
            assert_eq!(parsed.has_finish_reason, finished, "{data}");
        }
    }

    /// 第六轮：choice 其它字段的类型也要对——`delta` 不是对象（也不是 `null`）、`index` 不是非负整数，整个 chunk 不合法。
    /// 缺省和 `null` 都是合法的写法（有的实现不写 `index`，最后一块不写 `delta`）。
    #[test]
    fn a_choice_with_a_mistyped_delta_or_index_is_malformed() {
        let well_formed = |data: &str| match classify(&event(data)) {
            ChunkKind::Json(info) => info.choices_well_formed,
            other => panic!("expected a JSON chunk, got {other:?}"),
        };
        for data in [
            // `delta` 不是对象：带着有效的 finish_reason 也不行（原来「有 finish_reason 就放行」）
            r#"{"choices":[{"delta":"text","finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":1,"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":[],"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":true}]}"#,
            // `index` 不是非负整数
            r#"{"choices":[{"index":"0","delta":{}}]}"#,
            r#"{"choices":[{"index":-1,"delta":{}}]}"#,
            r#"{"choices":[{"index":0.5,"delta":{}}]}"#,
            r#"{"choices":[{"index":false,"delta":{}}]}"#,
            r#"{"choices":[{"index":[0],"delta":{}}]}"#,
            r#"{"choices":[{"index":{},"delta":{}}]}"#,
        ] {
            assert!(!well_formed(data), "{data}");
        }
        for data in [
            r#"{"choices":[{"index":0,"delta":{}}]}"#,
            r#"{"choices":[{"index":3,"delta":{"content":"x"}}]}"#,
            r#"{"choices":[{"index":null,"delta":{}}]}"#,
            r#"{"choices":[{"delta":{}}]}"#,
            // 最后一块没有 `delta`，或者 `delta` 是 `null`，只有字符串的 `finish_reason`
            r#"{"choices":[{"index":0,"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":null,"finish_reason":"stop"}]}"#,
        ] {
            assert!(well_formed(data), "{data}");
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

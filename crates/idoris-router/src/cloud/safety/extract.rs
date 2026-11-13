//! 从请求和上游响应里取出**要检查的文本**，并为 edu 重建发给客户端的响应体。
//!
//! # 输入：检查的是「真正会送到模型的所有文本」
//!
//! 上游请求体由 `idoris_upstream::cloud::wire::build_chat_body` 用**白名单**构造（[`FORWARDED_FIELDS`]）。
//! 会被模型读到的文本只可能在 `messages`、`tools`、`tool_choice`、`response_format`、`stop` 里，其余白名单字段
//! 是数字或布尔。这里对这五个字段做**递归取串**（任何层级的字符串值都取，不只是 `content`）：
//! 孩子的话可能躲在 `name`、助手历史消息的 `tool_calls[].function.arguments`、`content` 数组的分段里。
//! 测试 `every_forwarded_field_is_classified` 保证：将来有人往白名单里加字段，不分类就编译不过测试，不会悄悄漏检。
//!
//! 每条消息按**来源**分类（[`Origin`]），规则检查器按来源用不同的规则集：
//!
//! | 来源 | 角色 | 为什么分开 |
//! |---|---|---|
//! | `User` | `user`、未知角色 | 孩子写的话，最严 |
//! | `Tool` | `tool`、`function` | 工具返回的引用内容（可能是网页摘录），不可信，最严 |
//! | `Operator` | `system`、`developer`，以及 `tools` / `tool_choice` / `response_format` / `stop` | 运营方写的指令，会复述要拒绝的话题（「如果孩子要求自伤……就拒绝」），只查越狱类规则 |
//! | `Assistant` | `assistant` | 对话历史里模型说过的话，按**输出**规则查（拒绝语里会提到有害词，不能把拒绝语当成有害请求） |
//!
//! 非文本内容（图片、音频、文件……）规则检查器无法检查：置 [`CheckedInput::has_uncheckable_content`]，
//! 由 gate 按失败即关闭拒绝。语音转写是 P1，不在这里。
//!
//! # 输出：重建，而不是原样转发
//!
//! 非 edu 的响应体原样交给客户端（S4 的行为，一字不变）。edu 的**不是**：上游的响应里有我们没有检查的文本字段
//! （`reasoning_content`、`annotations`、`provider`……），原样转发就等于把没检查的字节交给孩子。
//! [`rebuild_response`] 只取 `choices[0].message` 的 `content`、`refusal`、`tool_calls[].function.{name,arguments}`
//! ——这些全部会被检查——再加上只含数字的 `usage` 和经过格式校验的 `id`、`model`、`finish_reason`，
//! 重新序列化。**客户端收到的每一个字符串，要么是被检查过的文本，要么是被校验为短标识的字段。**

use serde_json::{Map, Value, json};

use super::text::Sensitive;

/// 一段文本的来源（决定规则检查器用哪套规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    User,
    Tool,
    Operator,
    Assistant,
}

/// 一段要检查的文本。
#[derive(Debug, Clone)]
pub struct Segment {
    pub origin: Origin,
    pub text: Sensitive,
}

/// 一个请求里所有要检查的文本。
#[derive(Debug, Clone, Default)]
pub struct CheckedInput {
    pub segments: Vec<Segment>,
    /// 请求里有无法检查的非文本内容（图片、音频、文件……）。
    pub has_uncheckable_content: bool,
}

impl CheckedInput {
    pub fn total_bytes(&self) -> usize {
        self.segments.iter().map(|s| s.text.len()).sum()
    }

    /// 孩子和工具写的文本（最严的那两种来源）。
    pub fn untrusted_texts(&self) -> impl Iterator<Item = &str> {
        self.segments
            .iter()
            .filter(|s| matches!(s.origin, Origin::User | Origin::Tool))
            .map(|s| s.text.expose())
    }

    /// 最后一条用户消息（给 LLM 检查器当输出检查的上下文）。
    pub fn last_user_text(&self) -> Option<&str> {
        self.segments
            .iter()
            .rev()
            .find(|s| s.origin == Origin::User)
            .map(|s| s.text.expose())
    }
}

/// 一次工具调用里会被检查的文本。
#[derive(Debug, Clone)]
pub struct ToolCallText {
    pub name: Sensitive,
    pub arguments: Sensitive,
}

/// 模型回答里所有要检查的文本。
#[derive(Debug, Clone, Default)]
pub struct CheckedOutput {
    pub content: Option<Sensitive>,
    pub refusal: Option<Sensitive>,
    pub tool_calls: Vec<ToolCallText>,
}

impl CheckedOutput {
    /// 所有文本（回答、拒绝语、工具名和参数）。
    pub fn texts(&self) -> Vec<&str> {
        let mut texts = Vec::new();
        if let Some(content) = &self.content {
            texts.push(content.expose());
        }
        if let Some(refusal) = &self.refusal {
            texts.push(refusal.expose());
        }
        for call in &self.tool_calls {
            texts.push(call.name.expose());
            texts.push(call.arguments.expose());
        }
        texts
    }

    pub fn total_bytes(&self) -> usize {
        self.texts().iter().map(|t| t.len()).sum()
    }
}

/// 取文本失败：请求或响应的结构不符合约定，没法可靠地取出要检查的内容。失败即关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractError {
    Malformed,
    TooLarge,
}

/// 递归取串的最大深度。
const MAX_DEPTH: usize = 48;

/// 白名单字段里**会装文本**的那几个（运营方写的，来源 `Operator`）；`messages` 单独处理。
const OPERATOR_TEXT_FIELDS: &[&str] = &["tools", "tool_choice", "response_format", "stop"];

/// 白名单字段里**只会是数字或布尔**的（不装文本，不检查）。只给 `every_forwarded_field_is_classified` 用：
/// 它存在的意义就是让「白名单里新增了字段却没人分类」变成一条失败的测试。
#[cfg(test)]
const NON_TEXT_FIELDS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "presence_penalty",
    "frequency_penalty",
    "repetition_penalty",
    "seed",
    "parallel_tool_calls",
];

fn collect_strings(value: &Value, out: &mut Vec<String>, depth: usize) -> Result<(), ExtractError> {
    if depth > MAX_DEPTH {
        return Err(ExtractError::Malformed);
    }
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, out, depth + 1)?;
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, out, depth + 1)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn origin_of(role: &str) -> Origin {
    match role {
        "system" | "developer" => Origin::Operator,
        "assistant" => Origin::Assistant,
        "tool" | "function" => Origin::Tool,
        // `user` 和任何未知角色：最严
        _ => Origin::User,
    }
}

/// `content`：字符串，或分段数组（只有 `text` 分段能检查）。
fn collect_content(
    content: &Value,
    out: &mut Vec<String>,
    uncheckable: &mut bool,
) -> Result<(), ExtractError> {
    match content {
        Value::Null => Ok(()),
        Value::String(text) => {
            out.push(text.clone());
            Ok(())
        }
        Value::Array(parts) => {
            for part in parts {
                match part {
                    Value::String(text) => out.push(text.clone()),
                    Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                        Some("text") => match object.get("text") {
                            Some(Value::String(text)) => out.push(text.clone()),
                            _ => return Err(ExtractError::Malformed),
                        },
                        _ => *uncheckable = true,
                    },
                    _ => return Err(ExtractError::Malformed),
                }
            }
            Ok(())
        }
        _ => Err(ExtractError::Malformed),
    }
}

/// 从客户端请求体里取出所有要检查的文本。`max_bytes` 是文本总量上限（超过返回 `TooLarge`）。
pub fn extract_input(
    body: &Map<String, Value>,
    max_bytes: usize,
) -> Result<CheckedInput, ExtractError> {
    let mut input = CheckedInput::default();
    let mut total = 0usize;

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ExtractError::Malformed)?;
    for message in messages {
        let object = message.as_object().ok_or(ExtractError::Malformed)?;
        let role = object
            .get("role")
            .and_then(Value::as_str)
            .ok_or(ExtractError::Malformed)?;
        let mut pieces: Vec<String> = Vec::new();
        for (key, value) in object {
            match key.as_str() {
                "role" => {}
                "content" => {
                    collect_content(value, &mut pieces, &mut input.has_uncheckable_content)?
                }
                _ => collect_strings(value, &mut pieces, 0)?,
            }
        }
        if pieces.is_empty() {
            continue;
        }
        let text = pieces.join("\n");
        total = total.saturating_add(text.len());
        if total > max_bytes {
            return Err(ExtractError::TooLarge);
        }
        input.segments.push(Segment {
            origin: origin_of(role),
            text: Sensitive::new(text),
        });
    }

    for field in OPERATOR_TEXT_FIELDS {
        let Some(value) = body.get(*field) else {
            continue;
        };
        let mut pieces: Vec<String> = Vec::new();
        collect_strings(value, &mut pieces, 0)?;
        if pieces.is_empty() {
            continue;
        }
        let text = pieces.join("\n");
        total = total.saturating_add(text.len());
        if total > max_bytes {
            return Err(ExtractError::TooLarge);
        }
        input.segments.push(Segment {
            origin: Origin::Operator,
            text: Sensitive::new(text),
        });
    }
    Ok(input)
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

/// `finish_reason` 的词表（OpenAI 兼容）。词表之外的值按 `stop` 处理。
const FINISH_REASONS: &[&str] = &["stop", "length", "tool_calls", "content_filter"];

/// 短标识：只含 ASCII 字母数字和 `._:-`（`model` 还允许 `/`、`~`），长度有限。
fn is_short_token(text: &str, allow_slash: bool) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'.' | b'_' | b':' | b'-')
                || (allow_slash && matches!(b, b'/' | b'~'))
        })
}

fn numeric_usage(usage: Option<&Value>) -> Option<Value> {
    let object = usage?.as_object()?;
    let mut out = Map::new();
    for key in ["prompt_tokens", "completion_tokens", "total_tokens"] {
        if let Some(n) = object.get(key).and_then(Value::as_u64) {
            out.insert(key.to_string(), json!(n));
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// 重建后的响应：要检查的文本，加上可以发给客户端的字节。
#[derive(Debug, Clone)]
pub struct RebuiltResponse {
    pub output: CheckedOutput,
    pub body: Vec<u8>,
}

/// 从上游的非流式响应里只取会被检查的字段，重建发给客户端的响应体。
/// `fallback_id` 在上游的 `id` 不是短标识时用。结构不符合约定返回 `Malformed`（失败即关闭）。
pub fn rebuild_response(
    upstream: &[u8],
    fallback_id: &str,
    max_bytes: usize,
) -> Result<RebuiltResponse, ExtractError> {
    let value: Value = serde_json::from_slice(upstream).map_err(|_| ExtractError::Malformed)?;
    let root = value.as_object().ok_or(ExtractError::Malformed)?;
    let choice = root
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(Value::as_object)
        .ok_or(ExtractError::Malformed)?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or(ExtractError::Malformed)?;
    if let Some(role) = message.get("role")
        && role.as_str() != Some("assistant")
    {
        return Err(ExtractError::Malformed);
    }

    // content：字符串、null，或分段数组（text 和 refusal 分段）
    let mut content_text: Option<String> = None;
    let mut refusal_text: Option<String> = None;
    match message.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => content_text = Some(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            let mut refusal = String::new();
            for part in parts {
                let object = part.as_object().ok_or(ExtractError::Malformed)?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => match object.get("text") {
                        Some(Value::String(piece)) => text.push_str(piece),
                        _ => return Err(ExtractError::Malformed),
                    },
                    Some("refusal") => match object.get("refusal") {
                        Some(Value::String(piece)) => refusal.push_str(piece),
                        _ => return Err(ExtractError::Malformed),
                    },
                    // 图片、音频……：没法检查，不发
                    _ => return Err(ExtractError::Malformed),
                }
            }
            content_text = Some(text);
            if !refusal.is_empty() {
                refusal_text = Some(refusal);
            }
        }
        Some(_) => return Err(ExtractError::Malformed),
    }
    match message.get("refusal") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => {
            refusal_text = Some(match refusal_text {
                Some(existing) => format!("{existing}{text}"),
                None => text.clone(),
            });
        }
        Some(_) => return Err(ExtractError::Malformed),
    }

    // tool_calls
    let mut calls: Vec<(String, String, String)> = Vec::new(); // (id, name, arguments)
    match message.get("tool_calls") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                let object = item.as_object().ok_or(ExtractError::Malformed)?;
                if let Some(kind) = object.get("type")
                    && kind.as_str() != Some("function")
                {
                    return Err(ExtractError::Malformed);
                }
                let function = object
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or(ExtractError::Malformed)?;
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(ExtractError::Malformed)?
                    .to_string();
                let arguments = match function.get("arguments") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Null) | None => String::new(),
                    // 有的上游把参数当对象返回：序列化成字符串（OpenAI 的形状），同样要检查
                    Some(other @ (Value::Object(_) | Value::Array(_))) => other.to_string(),
                    Some(_) => return Err(ExtractError::Malformed),
                };
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| is_short_token(id, false))
                    .map_or_else(|| format!("call_{index}"), str::to_string);
                calls.push((id, name, arguments));
            }
        }
        Some(_) => return Err(ExtractError::Malformed),
    }

    let output = CheckedOutput {
        content: content_text.clone().map(Sensitive::new),
        refusal: refusal_text.clone().map(Sensitive::new),
        tool_calls: calls
            .iter()
            .map(|(_, name, arguments)| ToolCallText {
                name: Sensitive::new(name.clone()),
                arguments: Sensitive::new(arguments.clone()),
            })
            .collect(),
    };
    if output.total_bytes() > max_bytes {
        return Err(ExtractError::TooLarge);
    }

    // ---- 重建 ----
    let mut rebuilt_message = Map::new();
    rebuilt_message.insert("role".into(), json!("assistant"));
    rebuilt_message.insert(
        "content".into(),
        content_text.map_or(Value::Null, Value::String),
    );
    if let Some(refusal) = refusal_text {
        rebuilt_message.insert("refusal".into(), Value::String(refusal));
    }
    if !calls.is_empty() {
        rebuilt_message.insert(
            "tool_calls".into(),
            Value::Array(
                calls
                    .iter()
                    .map(|(id, name, arguments)| {
                        json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments},
                        })
                    })
                    .collect(),
            ),
        );
    }
    let finish_reason = if calls.is_empty() {
        choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| FINISH_REASONS.contains(reason))
            .unwrap_or("stop")
    } else {
        "tool_calls"
    };
    let mut rebuilt = Map::new();
    rebuilt.insert(
        "id".into(),
        json!(
            root.get("id")
                .and_then(Value::as_str)
                .filter(|id| is_short_token(id, false))
                .unwrap_or(fallback_id)
        ),
    );
    rebuilt.insert("object".into(), json!("chat.completion"));
    rebuilt.insert(
        "created".into(),
        json!(root.get("created").and_then(Value::as_u64).unwrap_or(0)),
    );
    rebuilt.insert(
        "model".into(),
        json!(
            root.get("model")
                .and_then(Value::as_str)
                .filter(|model| is_short_token(model, true))
                .unwrap_or("unknown")
        ),
    );
    rebuilt.insert(
        "choices".into(),
        json!([{
            "index": 0,
            "message": Value::Object(rebuilt_message),
            "finish_reason": finish_reason,
        }]),
    );
    if let Some(usage) = numeric_usage(root.get("usage")) {
        rebuilt.insert("usage".into(), usage);
    }
    let body = serde_json::to_vec(&Value::Object(rebuilt)).map_err(|_| ExtractError::Malformed)?;
    Ok(RebuiltResponse { output, body })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_upstream::cloud::wire::{ChatCall, FORWARDED_FIELDS, build_chat_body};

    use super::*;

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn every_forwarded_field_is_classified() {
        // 白名单里的每一个字段都必须被明确分类：装文本的（检查）或只会是数字/布尔的（不检查）。
        // 将来有人给 `FORWARDED_FIELDS` 加字段，不在这里分类，这条测试就失败，不会悄悄漏检。
        for field in FORWARDED_FIELDS {
            let classified = *field == "messages"
                || OPERATOR_TEXT_FIELDS.contains(field)
                || NON_TEXT_FIELDS.contains(field);
            assert!(
                classified,
                "forwarded field {field:?} is neither checked nor known to carry no text"
            );
        }
        // 反过来：分类表里没有白名单已经不转发的字段
        for field in OPERATOR_TEXT_FIELDS.iter().chain(NON_TEXT_FIELDS) {
            assert!(
                FORWARDED_FIELDS.contains(field),
                "{field:?} is not forwarded"
            );
        }
    }

    #[test]
    fn every_string_anywhere_in_the_forwarded_text_fields_is_extracted() {
        let input = extract_input(
            &body(json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "SYS"},
                    {"role": "user", "content": "USER", "name": "NAME-FIELD"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": "lookup", "arguments": "{\"q\":\"ARGS\"}"}}]},
                    {"role": "tool", "tool_call_id": "c1", "content": "TOOL-RESULT"},
                    {"role": "user", "content": [{"type": "text", "text": "PART-1"}, {"type": "text", "text": "PART-2"}]},
                ],
                "tools": [{"type": "function", "function": {"name": "f", "description": "TOOL-DESC"}}],
                "response_format": {"type": "json_schema", "json_schema": {"name": "SCHEMA-NAME"}},
                "stop": ["STOP-SEQ"],
                "user": "NOT-FORWARDED",
                "metadata": {"x": "NOT-FORWARDED-EITHER"},
            })),
            1 << 20,
        )
        .unwrap();
        let all: String = input
            .segments
            .iter()
            .map(|s| s.text.expose().to_string())
            .collect::<Vec<_>>()
            .join("|");
        for expected in [
            "SYS",
            "USER",
            "NAME-FIELD",
            "lookup",
            "ARGS",
            "TOOL-RESULT",
            "PART-1",
            "PART-2",
            "TOOL-DESC",
            "SCHEMA-NAME",
            "STOP-SEQ",
        ] {
            assert!(all.contains(expected), "{expected} missing from {all}");
        }
        // 白名单不转发的字段不用检查（模型读不到）
        assert!(!all.contains("NOT-FORWARDED"));
        assert!(!input.has_uncheckable_content);
    }

    #[test]
    fn origins_follow_the_role() {
        let input = extract_input(
            &body(json!({"messages": [
                {"role": "system", "content": "a"},
                {"role": "developer", "content": "b"},
                {"role": "user", "content": "c"},
                {"role": "wizard", "content": "d"},
                {"role": "assistant", "content": "e"},
                {"role": "tool", "content": "f"},
                {"role": "function", "content": "g"},
            ], "tools": [{"type": "function"}], "stop": "END"})),
            1 << 20,
        )
        .unwrap();
        let origins: Vec<Origin> = input.segments.iter().map(|s| s.origin).collect();
        assert_eq!(
            origins,
            vec![
                Origin::Operator,
                Origin::Operator,
                Origin::User,
                Origin::User, // 未知角色按最严处理
                Origin::Assistant,
                Origin::Tool,
                Origin::Tool,
                Origin::Operator, // tools
                Origin::Operator, // stop
            ]
        );
        assert_eq!(input.last_user_text(), Some("d"));
        assert_eq!(
            input.untrusted_texts().collect::<Vec<_>>(),
            ["c", "d", "f", "g"]
        );
    }

    #[test]
    fn non_text_parts_are_flagged_not_silently_skipped() {
        for part in [
            json!({"type": "image_url", "image_url": {"url": "https://example.invalid/x.png"}}),
            json!({"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}),
            json!({"type": "file", "file": {"file_id": "f"}}),
            json!({"type": "something_new"}),
            json!({"no_type": true}),
        ] {
            let input = extract_input(
                &body(json!({"messages": [{"role": "user", "content": [
                    {"type": "text", "text": "hello"}, part]}]})),
                1 << 20,
            )
            .unwrap();
            assert!(input.has_uncheckable_content);
        }
    }

    #[test]
    fn malformed_requests_fail_closed() {
        for bad in [
            json!({"messages": "hi"}),
            json!({"model": "m"}),
            json!({"messages": ["hi"]}),
            json!({"messages": [{"content": "no role"}]}),
            json!({"messages": [{"role": 3, "content": "x"}]}),
            json!({"messages": [{"role": "user", "content": {"a": "b"}}]}),
            json!({"messages": [{"role": "user", "content": [7]}]}),
            json!({"messages": [{"role": "user", "content": [{"type": "text"}]}]}),
        ] {
            assert_eq!(
                extract_input(&body(bad.clone()), 1 << 20).unwrap_err(),
                ExtractError::Malformed,
                "{bad}"
            );
        }
        // 嵌套太深
        let mut deep = json!("x");
        for _ in 0..80 {
            deep = json!([deep]);
        }
        assert_eq!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": "a", "extra": deep}]})),
                1 << 20
            )
            .unwrap_err(),
            ExtractError::Malformed
        );
    }

    #[test]
    fn oversized_input_is_too_large() {
        let big = "x".repeat(100);
        assert_eq!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": big}]})),
                99
            )
            .unwrap_err(),
            ExtractError::TooLarge
        );
        assert!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": big}]})),
                100
            )
            .is_ok()
        );
    }

    fn completion(message: Value) -> Vec<u8> {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000u64,
            "model": "example-org/chat-70b",
            "choices": [{"index": 0, "finish_reason": "stop", "message": message}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42,
                      "prompt_tokens_details": {"cached_tokens": 0}, "cost": 0.0001},
            "provider": "SomeProvider",
            "system_fingerprint": "fp",
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn the_rebuilt_response_carries_only_checked_text_and_validated_tokens() {
        let upstream = completion(json!({
            "role": "assistant", "content": "ANSWER",
            "reasoning_content": "UNCHECKED-REASONING",
            "reasoning": "UNCHECKED-REASONING-2",
            "annotations": [{"type": "url_citation", "title": "UNCHECKED-TITLE"}],
            "tool_calls": [{"id": "call_a", "type": "function",
                            "function": {"name": "calc", "arguments": "{\"x\":1}"}}],
        }));
        let rebuilt = rebuild_response(&upstream, "fallback", 1 << 20).unwrap();
        let text = String::from_utf8(rebuilt.body.clone()).unwrap();
        for dropped in [
            "UNCHECKED",
            "SomeProvider",
            "system_fingerprint",
            "cached_tokens",
            "cost",
        ] {
            assert!(!text.contains(dropped), "{dropped} leaked into {text}");
        }
        let value: Value = serde_json::from_slice(&rebuilt.body).unwrap();
        assert_eq!(value["id"], "chatcmpl-1");
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["created"], 1_700_000_000u64);
        assert_eq!(value["model"], "example-org/chat-70b");
        assert_eq!(value["choices"][0]["message"]["content"], "ANSWER");
        assert_eq!(value["choices"][0]["message"]["role"], "assistant");
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        assert_eq!(value["usage"]["prompt_tokens"], 12);
        assert_eq!(value["usage"]["total_tokens"], 42);
        // 会被检查的文本和发给客户端的文本是同一份
        let texts = rebuilt.output.texts();
        assert!(
            texts.contains(&"ANSWER") && texts.contains(&"calc") && texts.contains(&"{\"x\":1}")
        );
    }

    #[test]
    fn identifiers_are_validated_and_replaced_when_they_are_not_short_tokens() {
        let upstream = json!({
            "id": "an id with spaces and Thai สวัสดี",
            "model": "<script>",
            "choices": [{"message": {"role": "assistant", "content": "x",
                "tool_calls": [{"id": "bad id!", "type": "function",
                                "function": {"name": "f", "arguments": {"a": 1}}}]},
                "finish_reason": "weird_reason"}],
        })
        .to_string();
        let rebuilt = rebuild_response(upstream.as_bytes(), "req-123", 1 << 20).unwrap();
        let value: Value = serde_json::from_slice(&rebuilt.body).unwrap();
        assert_eq!(value["id"], "req-123");
        assert_eq!(value["model"], "unknown");
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_0"
        );
        // 对象形式的参数被序列化成字符串，并且被检查
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert!(rebuilt.output.texts().contains(&"{\"a\":1}"));
        assert!(value.get("usage").is_none());
        // 有工具调用时 finish_reason 固定为 tool_calls
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        // 没有工具调用、词表之外的 finish_reason 按 stop
        let plain =
            json!({"choices": [{"message": {"content": "x"}, "finish_reason": "eos"}]}).to_string();
        let value: Value = serde_json::from_slice(
            &rebuild_response(plain.as_bytes(), "r", 1 << 20)
                .unwrap()
                .body,
        )
        .unwrap();
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn parts_refusals_and_empty_answers() {
        let upstream = completion(json!({"role": "assistant", "content": [
            {"type": "text", "text": "A"}, {"type": "text", "text": "B"},
            {"type": "refusal", "refusal": "NO"}], "refusal": "MORE"}));
        let rebuilt = rebuild_response(&upstream, "r", 1 << 20).unwrap();
        assert_eq!(rebuilt.output.content.as_ref().unwrap().expose(), "AB");
        assert_eq!(rebuilt.output.refusal.as_ref().unwrap().expose(), "NOMORE");
        // 空回答：没有什么要检查的，也不是错误
        let empty = completion(json!({"role": "assistant", "content": null}));
        let rebuilt = rebuild_response(&empty, "r", 1 << 20).unwrap();
        assert!(rebuilt.output.texts().is_empty());
    }

    #[test]
    fn unvettable_responses_fail_closed() {
        for bad in [
            "not json".to_string(),
            "[]".to_string(),
            json!({"choices": []}).to_string(),
            json!({"choices": [1]}).to_string(),
            json!({"choices": [{"text": "legacy completion"}]}).to_string(),
            json!({"choices": [{"message": {"role": "user", "content": "x"}}]}).to_string(),
            json!({"choices": [{"message": {"content": {"a": 1}}}]}).to_string(),
            json!({"choices": [{"message": {"content": [{"type": "image_url"}]}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "tool_calls": "nope"}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "tool_calls": [{"function": {}}]}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "tool_calls": [{"type": "code", "function": {"name": "f"}}]}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "refusal": 3}}]}).to_string(),
        ] {
            assert_eq!(
                rebuild_response(bad.as_bytes(), "r", 1 << 20).unwrap_err(),
                ExtractError::Malformed,
                "{bad}"
            );
        }
        let huge = completion(json!({"role": "assistant", "content": "y".repeat(50)}));
        assert_eq!(
            rebuild_response(&huge, "r", 49).unwrap_err(),
            ExtractError::TooLarge
        );
    }

    #[test]
    fn nothing_the_upstream_says_about_ids_or_the_model_reaches_the_client() {
        // r3 Medium#1：孩子输入里的电话号码被上游回显成顶层 `id`（或 `model`、工具调用 `id`）。
        // 它们都是合法的「短标识」，所以旧实现原样放行，而且绕过了 PII 输出检查。
        let upstream = json!({
            "id": "0812345678",
            "model": "CHILDINPUT-SENTINEL-4f81c2d7",
            "choices": [{"message": {"role": "assistant", "content": "ok",
                "tool_calls": [{"id": "0899999999", "type": "function",
                                "function": {"name": "f", "arguments": "{}"}}]}}],
        })
        .to_string();
        let rebuilt = rebuild_response(upstream.as_bytes(), "req-1", 1 << 20).unwrap();
        let text = String::from_utf8(rebuilt.body).unwrap();
        for echoed in ["0812345678", "CHILDINPUT-SENTINEL", "0899999999"] {
            assert!(!text.contains(echoed), "{echoed} leaked into {text}");
        }
    }

    // ---- 「检查的 = 转发的」：性质测试 ----

    /// 可复现的伪随机数（不引入依赖）。
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// 每个标签都独一无二，也不会撞上任何规则的词。
    struct Tags(usize);

    impl Tags {
        fn tag(&mut self) -> String {
            self.0 += 1;
            format!("zqx{}tag", self.0)
        }
    }

    fn random_value(rng: &mut Rng, tags: &mut Tags, depth: usize) -> Value {
        match if depth >= 3 {
            rng.below(4)
        } else {
            rng.below(7)
        } {
            0 => json!(tags.tag()),
            1 => json!(rng.below(100)),
            2 => json!(rng.below(2) == 0),
            3 => Value::Null,
            4 | 5 => Value::Array(
                (0..rng.below(3))
                    .map(|_| random_value(rng, tags, depth + 1))
                    .collect(),
            ),
            _ => Value::Object(
                (0..rng.below(3))
                    .map(|_| (tags.tag(), random_value(rng, tags, depth + 1)))
                    .collect(),
            ),
        }
    }

    fn random_message(rng: &mut Rng, tags: &mut Tags) -> Value {
        let role = match rng.below(8) {
            0 => "system".to_string(),
            1 => "developer".to_string(),
            2 | 3 => "user".to_string(),
            4 => "assistant".to_string(),
            5 => "tool".to_string(),
            6 => "function".to_string(),
            _ => tags.tag(), // 未知角色：它的名字也会被转发
        };
        let mut message = Map::new();
        message.insert("role".into(), json!(role));
        match rng.below(4) {
            0 => {
                message.insert("content".into(), json!(tags.tag()));
            }
            1 => {
                let mut part = Map::new();
                part.insert("type".into(), json!("text"));
                part.insert("text".into(), json!(tags.tag()));
                if rng.below(2) == 0 {
                    part.insert(tags.tag(), random_value(rng, tags, 1));
                }
                message.insert(
                    "content".into(),
                    json!([Value::Object(part), json!(tags.tag())]),
                );
            }
            2 => {
                message.insert("content".into(), Value::Null);
            }
            _ => {}
        }
        if rng.below(2) == 0 {
            message.insert("name".into(), json!(tags.tag()));
        }
        if rng.below(2) == 0 {
            let arguments = serde_json::to_string(&json!({tags.tag(): tags.tag()})).unwrap();
            message.insert(
                "tool_calls".into(),
                json!([{"id": tags.tag(), "type": "function",
                        "function": {"name": tags.tag(), "arguments": arguments}}]),
            );
        }
        if rng.below(3) == 0 {
            message.insert(tags.tag(), random_value(rng, tags, 1));
        }
        Value::Object(message)
    }

    /// 一个随机的客户端请求体：每个白名单字段都有可能出现，还有不会被转发的字段。
    fn random_client_body(seed: u64) -> Map<String, Value> {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut tags = Tags(0);
        let mut body = Map::new();
        body.insert("model".into(), json!("idoris/agent"));
        let messages: Vec<Value> = (0..=rng.below(4))
            .map(|_| random_message(&mut rng, &mut tags))
            .collect();
        body.insert("messages".into(), Value::Array(messages));
        if rng.below(2) == 0 {
            let tools: Vec<Value> = (0..=rng.below(2))
                .map(|_| {
                    json!({"type": "function", "function": {
                        "name": tags.tag(), "description": tags.tag(),
                        "parameters": {"type": "object", "properties": {
                            tags.tag(): {"type": "string", "description": tags.tag()}}}}})
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        if rng.below(2) == 0 {
            body.insert(
                "tool_choice".into(),
                if rng.below(2) == 0 {
                    json!(tags.tag())
                } else {
                    json!({"type": "function", "function": {"name": tags.tag()}})
                },
            );
        }
        if rng.below(2) == 0 {
            body.insert(
                "response_format".into(),
                json!({"type": "json_schema", "json_schema": {
                    "name": tags.tag(), "schema": random_value(&mut rng, &mut tags, 0)}}),
            );
        }
        if rng.below(2) == 0 {
            body.insert(
                "stop".into(),
                if rng.below(2) == 0 {
                    json!(tags.tag())
                } else {
                    json!([tags.tag(), tags.tag()])
                },
            );
        }
        for field in NUMBER_FIELDS {
            if rng.below(2) == 0 {
                body.insert((*field).into(), json!(rng.below(10)));
            }
        }
        if rng.below(2) == 0 {
            body.insert("parallel_tool_calls".into(), json!(true));
        }
        // 白名单之外的字段不会被转发，检查与否都不影响「检查的 = 转发的」
        body.insert("user".into(), json!(tags.tag()));
        body.insert("metadata".into(), json!({tags.tag(): tags.tag()}));
        body
    }

    const NUMBER_FIELDS: &[&str] = &[
        "temperature",
        "top_p",
        "top_k",
        "min_p",
        "presence_penalty",
        "frequency_penalty",
        "repetition_penalty",
        "seed",
    ];

    /// 固定的结构词：不是客户端能写的文本（白名单字段名、上游请求里由服务端写的字段、消息结构里固定的键和角色名）。
    /// 它们出现与否不影响检查，因为里面装不下任何内容。
    const STRUCTURAL: &[&str] = &[
        "model",
        "max_tokens",
        "stream",
        "role",
        "content",
        "type",
        "text",
        "system",
        "developer",
        "user",
        "assistant",
        "tool",
        "function",
    ];

    fn every_string(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| every_string(item, out)),
            Value::Object(map) => {
                for (key, item) in map {
                    out.push(key.clone());
                    every_string(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_string_in_the_forwarded_request_is_in_the_checked_text() {
        // 性质：`build_chat_body` 真正会发给上游的请求里，**每一个**字符串（值和键名，任何深度）都出现在检查内容里。
        // 随机的请求体里每个字符串都是独一无二的标签；上游请求由真正的 `build_chat_body` 构造。
        let none = Map::new();
        for seed in 1..=400u64 {
            let client = random_client_body(seed);
            let forwarded = build_chat_body(&ChatCall {
                client_body: &client,
                upstream_model: "upstream-model",
                max_tokens: 64,
                stream: false,
                extra_body: &none,
            })
            .unwrap();
            let forwarded: Value = serde_json::from_slice(&forwarded).unwrap();
            let mut strings = Vec::new();
            every_string(&forwarded, &mut strings);
            let checked = extract_input(&client, 1 << 22).unwrap();
            let all: String = checked
                .segments
                .iter()
                .map(|s| s.text.expose().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let tagged = strings.iter().filter(|s| s.starts_with("zqx")).count();
            assert!(tagged > 0, "seed {seed} generated nothing to check");
            for string in &strings {
                if STRUCTURAL.contains(&string.as_str())
                    || FORWARDED_FIELDS.contains(&string.as_str())
                {
                    continue;
                }
                assert!(
                    all.contains(string.as_str()),
                    "seed {seed}: the forwarded string {string:?} is not in the checked text\nbody: {}",
                    Value::Object(client.clone())
                );
            }
        }
    }

    #[test]
    fn only_the_first_choice_is_forwarded() {
        let upstream = json!({"choices": [
            {"message": {"role": "assistant", "content": "FIRST"}},
            {"message": {"role": "assistant", "content": "SECOND-UNCHECKED"}}]})
        .to_string();
        let rebuilt = rebuild_response(upstream.as_bytes(), "r", 1 << 20).unwrap();
        let text = String::from_utf8(rebuilt.body).unwrap();
        assert!(text.contains("FIRST") && !text.contains("SECOND"));
    }
}

//! OpenAI 兼容 Chat Completions 的请求体构造与非流式响应解析。
//!
//! # 请求体：白名单，不是黑名单
//!
//! 客户端的 JSON 里只有**明确支持**的字段会被带给上游；其它一律不转发。这是儿童数据与计费两方面的要求：
//!
//! - 不能把能识别用户的字段带出去：`user`、`metadata`、`store`、`service_tier`……都不转发
//!   （既有 `RemoteClient` 的「去关联」立场：只发 `model` 与 `messages`，这里在此基础上加上工具与结构化输出）；
//! - 不能让客户端指定上游的路由偏好：`provider`、`models`、`route`、`transforms`、`plugins` 之类被丢弃。
//!   端点许可表里的「具体端点」（比如聚合路由的 `zdr-route`）是靠**配置里的 `extra_body`** 落实的，
//!   由运维写死、盖在客户端字段之上，客户端改不了；**但 `extra_body` 自己也不能碰协议 / 计费 / 身份 / 留存字段**
//!   （[`RESERVED_EXTRA_BODY_FIELDS`]：`model`、`max_tokens`、`stream`、`n`、`messages`、`tools`、`user`、`store`……）：
//!   它们决定「发给哪个模型、最多花多少钱、带不带身份、上游留不留存」，是准入与许可检查的前提，配置加载时就拒绝
//!   （[`reserved_extra_body_field`]），构造请求时这些字段最后写入、永远是准入时算好的值；
//! - 不能让客户端改变计费倍数：`n` 必须是 1（调用方在构造前拒绝），`logprobs` 之类不转发。
//!
//! 一定覆盖的字段（**最后写入**，在客户端字段和 `extra_body` 之后）：`model`（端点自己的模型 id，不是客户端用的别名）、
//! `max_tokens`（**必须**是预留时算出的有效值，客户端的 `max_tokens` / `max_completion_tokens` 都不转发，否则上游可以
//! 输出超过预留的内容）、`stream`、`stream_options`（流式时固定 `{"include_usage": true}`：流内 usage 是付费流式请求的
//! 结算依据；非流式没有这个字段）。
//!
//! 工具调用：`tools` / `tool_choice` / `parallel_tool_calls` 原样透传；`messages` 原样透传，所以助手消息里的
//! `tool_calls` 与 `role: tool` 的回传消息自然带着。结构化输出：`response_format` 原样透传。
//!
//! # 非流式响应
//!
//! 只取 `id`（供应商请求号）与 `usage`，响应体本身由调用方原样转交给客户端，这里不解读也不记录内容。

use serde_json::{Map, Value, json};

use super::usage::{UsageParse, parse_usage};

/// 客户端字段里会被转发给上游的白名单。
pub const FORWARDED_FIELDS: &[&str] = &[
    "messages",
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "repetition_penalty",
    "seed",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "response_format",
];

/// 单次请求里 `tools` 的最大个数（防止把巨大的工具表原样转发）。
pub const MAX_TOOLS: usize = 128;

/// 端点级固定附加字段（配置里的 `extra_body`）**不允许出现**的字段：协议形状、计费倍数与上限、身份、留存。
///
/// 为什么：准入、许可表和预留都是按「这个候选、这个模型、这个 `max_tokens`」算好的。`extra_body` 盖在最上面，
/// 如果它能写 `model`，一个被许可处理儿童数据的候选 A 实际会把请求发给没被许可的模型 B；如果它能写 `max_tokens`，
/// 余额只够 16 个 token 时被压低的上限会被放大回 4096，造成超额成本；`n`、`service_tier`、`logprobs` 改变计费倍数；
/// `stream` / `stream_options` 决定结算依据（流内 usage）；`user`、`metadata`、`store` 是能识别用户、让上游留存的字段。
/// `extra_body` 的正当用途只是供应商自己的路由 / 策略参数（比如聚合路由的 `provider: {zdr: true}`）。
pub const RESERVED_EXTRA_BODY_FIELDS: &[&str] = &[
    // 协议形状与身份
    "model",
    "messages",
    "stream",
    "stream_options",
    // 上限与计费倍数
    "max_tokens",
    "max_completion_tokens",
    "n",
    "best_of",
    "service_tier",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "prediction",
    // 工具与输出形状（许可表按它们筛能力）
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "functions",
    "function_call",
    "response_format",
    // 能识别用户、让上游留存的字段
    "user",
    "metadata",
    "store",
    "safety_identifier",
];

/// `extra_body` 里第一个保留字段（按 [`RESERVED_EXTRA_BODY_FIELDS`] 的顺序）；没有返回 `None`。配置加载时用它拒绝。
pub fn reserved_extra_body_field(extra_body: &Map<String, Value>) -> Option<&'static str> {
    RESERVED_EXTRA_BODY_FIELDS
        .iter()
        .copied()
        .find(|field| extra_body.contains_key(*field))
}

/// 构造请求体的错误。信息里没有请求内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("`messages` is required")]
    MessagesMissing,
    #[error("`messages` must be a non-empty array of objects with a string `role`")]
    MessagesInvalid,
    #[error("field `{0}` has an invalid type")]
    InvalidField(&'static str),
}

/// 一次上游调用的输入。
#[derive(Debug, Clone, Copy)]
pub struct ChatCall<'a> {
    /// 客户端的请求体（已确认是 JSON 对象）。
    pub client_body: &'a Map<String, Value>,
    /// 端点自己的模型 id。
    pub upstream_model: &'a str,
    /// 预留时算出的有效 `max_tokens`。
    pub max_tokens: u64,
    pub stream: bool,
    /// 端点级的固定附加字段（运维写死；盖在其它所有字段之上）。
    pub extra_body: &'a Map<String, Value>,
}

fn require(ok: bool, field: &'static str) -> Result<(), BuildError> {
    if ok {
        Ok(())
    } else {
        Err(BuildError::InvalidField(field))
    }
}

/// 构造发给上游的请求体（JSON 字节）。
pub fn build_chat_body(call: &ChatCall<'_>) -> Result<Vec<u8>, BuildError> {
    let source = call.client_body;
    let messages = source
        .get("messages")
        .ok_or(BuildError::MessagesMissing)?
        .as_array()
        .ok_or(BuildError::MessagesInvalid)?;
    if messages.is_empty()
        || !messages.iter().all(|m| {
            m.as_object()
                .is_some_and(|o| o.get("role").is_some_and(Value::is_string))
        })
    {
        return Err(BuildError::MessagesInvalid);
    }

    let mut body = Map::new();
    for &field in FORWARDED_FIELDS {
        let Some(value) = source.get(field) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        match field {
            "temperature" | "top_p" | "top_k" | "min_p" | "presence_penalty"
            | "frequency_penalty" | "repetition_penalty" | "seed" => {
                require(value.is_number(), field)?;
            }
            "stop" => require(value.is_string() || value.is_array(), field)?,
            "tools" => {
                let tools = value.as_array();
                require(
                    tools.is_some_and(|t| t.len() <= MAX_TOOLS && t.iter().all(Value::is_object)),
                    field,
                )?;
            }
            "tool_choice" => require(value.is_string() || value.is_object(), field)?,
            "parallel_tool_calls" => require(value.is_boolean(), field)?,
            "response_format" => require(value.is_object(), field)?,
            _ => {}
        }
        body.insert(field.to_string(), value.clone());
    }
    // 端点级固定字段盖在客户端字段之上（比如聚合路由的「只走 ZDR」路由参数）。保留字段一个都不收：配置加载时
    // 已经拒绝（`reserved_extra_body_field`），这里再跳过一遍，绕过配置校验直接构造设置的调用方也改不了它们。
    for (key, value) in call.extra_body {
        if RESERVED_EXTRA_BODY_FIELDS.contains(&key.as_str()) {
            continue;
        }
        body.insert(key.clone(), value.clone());
    }
    // 协议、计费、身份字段**最后写入**：不论客户端或 `extra_body` 写了什么，发出去的永远是准入时算好的值
    body.insert(
        "model".to_string(),
        Value::String(call.upstream_model.to_string()),
    );
    body.insert("max_tokens".to_string(), json!(call.max_tokens));
    body.remove("max_completion_tokens");
    body.remove("n");
    body.insert("stream".to_string(), Value::Bool(call.stream));
    if call.stream {
        body.insert("stream_options".to_string(), json!({"include_usage": true}));
    } else {
        body.remove("stream_options");
    }
    // 序列化失败只可能是 Map 里有不能序列化的值；全是 serde_json::Value，不会发生
    serde_json::to_vec(&Value::Object(body)).map_err(|_| BuildError::MessagesInvalid)
}

/// 非流式响应里我们关心的部分。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCompletion {
    /// 顶层 `id`：供应商请求号。
    pub id: Option<String>,
    pub usage: UsageParse,
}

/// 非流式响应无法使用的原因（稳定的代码，不含响应内容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompletionError {
    #[error("the response is not valid JSON")]
    NotJson,
    #[error("the response is not a JSON object")]
    NotAnObject,
    /// HTTP 2xx 但响应体是一个错误对象。
    #[error("the response is an error object")]
    ErrorObject,
    #[error("the response has no choices")]
    NoChoices,
}

impl CompletionError {
    pub fn code(self) -> &'static str {
        match self {
            CompletionError::NotJson => "response_not_json",
            CompletionError::NotAnObject => "response_not_an_object",
            CompletionError::ErrorObject => "response_error_object",
            CompletionError::NoChoices => "response_no_choices",
        }
    }
}

/// 解析非流式响应体。
pub fn parse_completion(body: &[u8]) -> Result<ParsedCompletion, CompletionError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| CompletionError::NotJson)?;
    let object = value.as_object().ok_or(CompletionError::NotAnObject)?;
    if object.get("error").is_some_and(|e| !e.is_null()) {
        return Err(CompletionError::ErrorObject);
    }
    if object
        .get("choices")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(CompletionError::NoChoices);
    }
    Ok(ParsedCompletion {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= super::chunk::MAX_SUPPLIER_ID_BYTES)
            .map(str::to_owned),
        usage: parse_usage(object.get("usage")),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn build(client: Value, stream: bool, extra: Value) -> Value {
        let client = client.as_object().unwrap().clone();
        let extra = extra.as_object().unwrap().clone();
        let bytes = build_chat_body(&ChatCall {
            client_body: &client,
            upstream_model: "example-org/chat-70b",
            max_tokens: 321,
            stream,
            extra_body: &extra,
        })
        .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn model_and_max_tokens_are_always_the_endpoints_and_the_reserved_ones() {
        let body = build(
            json!({
                "model": "idoris/agent",
                "max_tokens": 999_999,
                "max_completion_tokens": 888_888,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            false,
            json!({}),
        );
        assert_eq!(body["model"], "example-org/chat-70b");
        assert_eq!(body["max_tokens"], 321);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["stream"], false);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn streaming_always_asks_for_in_stream_usage_whatever_the_client_said() {
        for client_options in [
            json!({"messages": [{"role":"user","content":"x"}]}),
            json!({"messages": [{"role":"user","content":"x"}], "stream_options": {"include_usage": false}}),
            json!({"messages": [{"role":"user","content":"x"}], "stream_options": {"continuous_usage_stats": true}}),
        ] {
            let body = build(client_options, true, json!({}));
            assert_eq!(body["stream"], true);
            assert_eq!(body["stream_options"], json!({"include_usage": true}));
        }
    }

    #[test]
    fn tools_tool_choice_structured_output_and_tool_messages_pass_through_verbatim() {
        let tools = json!([{"type": "function", "function": {"name": "get_weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]);
        let messages = json!([
            {"role": "user", "content": "weather in Bangkok?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Bangkok\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "31C"}
        ]);
        let response_format = json!({"type": "json_schema", "json_schema": {"name": "r", "schema": {"type": "object"}}});
        let body = build(
            json!({
                "messages": messages,
                "tools": tools,
                "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
                "parallel_tool_calls": false,
                "response_format": response_format,
                "temperature": 0.2,
                "stop": ["END"],
                "seed": 7
            }),
            false,
            json!({}),
        );
        assert_eq!(body["messages"], messages);
        assert_eq!(body["tools"], tools);
        assert_eq!(body["tool_choice"]["function"]["name"], "get_weather");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["response_format"], response_format);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["stop"], json!(["END"]));
        assert_eq!(body["seed"], 7);
    }

    #[test]
    fn identifying_and_routing_fields_are_never_forwarded() {
        let body = build(
            json!({
                "messages": [{"role": "user", "content": "x"}],
                "user": "student-4711",
                "metadata": {"tenant": "school-a"},
                "store": true,
                "service_tier": "priority",
                "provider": {"allow_fallbacks": true, "zdr": false},
                "models": ["a", "b"],
                "route": "fallback",
                "transforms": ["middle-out"],
                "plugins": [{"id": "web"}],
                "n": 1,
                "logprobs": true,
                "logit_bias": {"1": 100},
                "x_unknown": "whatever"
            }),
            false,
            json!({}),
        );
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut keys = keys;
        keys.sort_unstable();
        assert_eq!(keys, vec!["max_tokens", "messages", "model", "stream"]);
    }

    #[test]
    fn the_endpoints_fixed_extra_body_overrides_the_client_but_never_the_protocol_fields() {
        let body = build(
            json!({
                "messages": [{"role": "user", "content": "x"}],
                "provider": {"zdr": false}
            }),
            true,
            json!({"provider": {"zdr": true, "allow_fallbacks": false}, "max_tokens": 1}),
        );
        assert_eq!(
            body["provider"],
            json!({"zdr": true, "allow_fallbacks": false})
        );
        // R2 High#2：`extra_body` 盖得过客户端，但盖不过准入时算好的有效 `max_tokens`（旧行为：写死的 1 会赢，
        // 余额不足时被压低的上限同样会被它放大）
        assert_eq!(body["max_tokens"], 321);
    }

    /// R2 High#2：`extra_body` 里的保留字段（协议 / 计费 / 身份 / 留存）一个都不会出现在上游请求里；
    /// 模型、有效 `max_tokens`、`stream`、`stream_options` 永远是调用方给的值。
    #[test]
    fn extra_body_can_never_override_model_caps_stream_or_identity_fields() {
        let hostile = json!({
            "model": "unlicensed/other-model",
            "max_tokens": 4096,
            "max_completion_tokens": 4096,
            "stream": true,
            "stream_options": {"include_usage": false},
            "n": 8,
            "messages": [{"role": "user", "content": "swapped"}],
            "tools": [{"type": "function", "function": {"name": "injected"}}],
            "tool_choice": "required",
            "service_tier": "priority",
            "logprobs": true,
            "user": "student-4711",
            "metadata": {"tenant": "school-a"},
            "store": true,
            "provider": {"zdr": true}
        });
        for stream in [false, true] {
            let body = build(
                json!({"messages": [{"role": "user", "content": "real"}]}),
                stream,
                hostile.clone(),
            );
            assert_eq!(body["model"], "example-org/chat-70b", "stream={stream}");
            assert_eq!(body["max_tokens"], 321, "stream={stream}");
            assert_eq!(body["stream"], stream);
            if stream {
                assert_eq!(body["stream_options"], json!({"include_usage": true}));
            } else {
                assert!(body.get("stream_options").is_none());
            }
            assert_eq!(body["messages"][0]["content"], "real");
            for field in [
                "max_completion_tokens",
                "n",
                "tools",
                "tool_choice",
                "service_tier",
                "logprobs",
                "user",
                "metadata",
                "store",
            ] {
                assert!(
                    body.get(field).is_none(),
                    "{field} leaked (stream={stream})"
                );
            }
            // 正当用途（供应商自己的路由参数）照常带上
            assert_eq!(body["provider"], json!({"zdr": true}));
        }
    }

    #[test]
    fn the_reserved_field_check_names_the_first_offender_and_passes_provider_routing() {
        let extra = |value: Value| value.as_object().unwrap().clone();
        assert_eq!(
            reserved_extra_body_field(&extra(json!({"provider": {"zdr": true}, "top_k": 40}))),
            None
        );
        assert_eq!(
            reserved_extra_body_field(&extra(json!({"provider": {}, "model": "x"}))),
            Some("model")
        );
        for field in RESERVED_EXTRA_BODY_FIELDS {
            let mut one = Map::new();
            one.insert((*field).to_string(), json!(1));
            assert_eq!(reserved_extra_body_field(&one), Some(*field));
        }
        // 客户端能发的白名单字段里，协议 / 计费相关的都在保留表里
        for field in [
            "messages",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "response_format",
        ] {
            assert!(RESERVED_EXTRA_BODY_FIELDS.contains(&field), "{field}");
        }
    }

    #[test]
    fn invalid_shapes_are_rejected_before_anything_is_sent() {
        let call = |client: Value| {
            let client = client.as_object().unwrap().clone();
            let extra = Map::new();
            build_chat_body(&ChatCall {
                client_body: &client,
                upstream_model: "m",
                max_tokens: 1,
                stream: false,
                extra_body: &extra,
            })
        };
        assert_eq!(call(json!({})).unwrap_err(), BuildError::MessagesMissing);
        for messages in [
            json!("hi"),
            json!([]),
            json!(["hi"]),
            json!([{"content": "no role"}]),
            json!([{"role": 3}]),
        ] {
            assert_eq!(
                call(json!({"messages": messages})).unwrap_err(),
                BuildError::MessagesInvalid
            );
        }
        let ok = json!([{"role": "user", "content": "x"}]);
        let invalid: [(&'static str, Value); 7] = [
            ("temperature", json!("hot")),
            ("tools", json!("not an array")),
            ("tools", json!([1])),
            ("tool_choice", json!(3)),
            ("parallel_tool_calls", json!("yes")),
            ("response_format", json!([])),
            ("stop", json!(5)),
        ];
        for (field, value) in invalid {
            let mut body = json!({"messages": ok});
            body[field] = value;
            assert_eq!(
                call(body).unwrap_err(),
                BuildError::InvalidField(field),
                "{field}"
            );
        }
        // null 等于没写
        assert!(call(json!({"messages": ok, "tools": null, "temperature": null})).is_ok());
        // 工具表太大
        let many: Vec<Value> = (0..=MAX_TOOLS)
            .map(|_| json!({"type": "function"}))
            .collect();
        assert_eq!(
            call(json!({"messages": ok, "tools": many})).unwrap_err(),
            BuildError::InvalidField("tools")
        );
    }

    #[test]
    fn a_completion_exposes_the_id_and_usage_only() {
        let parsed = parse_completion(
            br#"{"id":"chatcmpl-1","choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]}}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        )
        .unwrap();
        assert_eq!(parsed.id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(parsed.usage.valid_usage().unwrap().prompt_tokens, 10);
        // 没有 usage
        let parsed = parse_completion(br#"{"id":"c","choices":[{"message":{}}]}"#).unwrap();
        assert_eq!(parsed.usage, UsageParse::Absent);
    }

    #[test]
    fn unusable_completions_are_classified_with_stable_codes() {
        assert_eq!(
            parse_completion(b"<html>bad gateway</html>").unwrap_err(),
            CompletionError::NotJson
        );
        assert_eq!(
            parse_completion(b"[]").unwrap_err(),
            CompletionError::NotAnObject
        );
        assert_eq!(
            parse_completion(br#"{"error":{"message":"overloaded"}}"#).unwrap_err(),
            CompletionError::ErrorObject
        );
        assert_eq!(
            parse_completion(br#"{"id":"c","choices":[]}"#).unwrap_err(),
            CompletionError::NoChoices
        );
        assert_eq!(
            parse_completion(br#"{"id":"c"}"#).unwrap_err(),
            CompletionError::NoChoices
        );
        let codes: Vec<&str> = [
            CompletionError::NotJson,
            CompletionError::NotAnObject,
            CompletionError::ErrorObject,
            CompletionError::NoChoices,
        ]
        .iter()
        .map(|e| e.code())
        .collect();
        assert_eq!(
            codes,
            vec![
                "response_not_json",
                "response_not_an_object",
                "response_error_object",
                "response_no_choices"
            ]
        );
    }
}

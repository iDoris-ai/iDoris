//! 客户端请求体的校验与计量：JSON 形状、`n`、`stream`、`max_tokens`，以及要用到的能力（工具调用、结构化输出）
//! 和输入 token 的保守估算。只看结构，**不记录也不转发**任何内容（转发是 `idoris-upstream::cloud::wire` 的事，
//! 白名单）。

use idoris_compute_contracts::permit::CapabilityNeeds;
use idoris_tenancy::budget::estimate_tokens;
use serde_json::{Map, Value};

/// 客户端请求体的问题（稳定的代码；信息里没有请求内容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestProblem {
    NotJson,
    NotAnObject,
    MissingModel,
    MessagesMissing,
    UnsupportedCompletionCount,
    InvalidStream,
    InvalidStreamOptions,
    InvalidMaxTokens(&'static str),
}

impl RequestProblem {
    pub(crate) fn code(self) -> &'static str {
        match self {
            RequestProblem::NotJson => "invalid_json",
            RequestProblem::NotAnObject => "invalid_body",
            RequestProblem::MissingModel => "unsupported_model",
            RequestProblem::MessagesMissing => "invalid_messages",
            RequestProblem::UnsupportedCompletionCount => "unsupported_completion_count",
            RequestProblem::InvalidStream => "invalid_stream",
            RequestProblem::InvalidStreamOptions => "invalid_stream_options",
            RequestProblem::InvalidMaxTokens(_) => "invalid_completion_cap",
        }
    }

    pub(crate) fn message(self) -> String {
        match self {
            RequestProblem::NotJson => "request body is not valid JSON".to_string(),
            RequestProblem::NotAnObject => "request body must be a JSON object".to_string(),
            RequestProblem::MissingModel => "`model` must be a non-empty string".to_string(),
            RequestProblem::MessagesMissing => "`messages` must be an array".to_string(),
            RequestProblem::UnsupportedCompletionCount => {
                "n must be the integer 1 or null; multiple completions are not supported"
                    .to_string()
            }
            RequestProblem::InvalidStream => "`stream` must be a boolean".to_string(),
            RequestProblem::InvalidStreamOptions => {
                "`stream_options` must be an object with a boolean `include_usage`".to_string()
            }
            RequestProblem::InvalidMaxTokens(field) => {
                format!("{field} must be an integer >= 1")
            }
        }
    }
}

/// 校验过的客户端请求。
#[derive(Debug)]
pub(crate) struct ClientRequest {
    pub(crate) body: Map<String, Value>,
    pub(crate) model: String,
    pub(crate) stream: bool,
    pub(crate) wants_usage_chunk: bool,
    pub(crate) requested_max_tokens: Option<u64>,
    pub(crate) input_tokens_estimate: u64,
    pub(crate) needs: CapabilityNeeds,
}

fn max_tokens_field(
    body: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<u64>, RequestProblem> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(n) if n >= 1 => Ok(Some(n)),
            _ => Err(RequestProblem::InvalidMaxTokens(field)),
        },
    }
}

pub(crate) fn parse_client_request(bytes: &[u8]) -> Result<ClientRequest, RequestProblem> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| RequestProblem::NotJson)?;
    let Value::Object(body) = value else {
        return Err(RequestProblem::NotAnObject);
    };
    let model = match body.get("model").and_then(Value::as_str) {
        Some(model) if !model.is_empty() && model.len() <= 256 => model.to_string(),
        _ => return Err(RequestProblem::MissingModel),
    };
    if !body.get("messages").is_some_and(Value::is_array) {
        return Err(RequestProblem::MessagesMissing);
    }
    // n：多个补全会让输出按倍数计费，预留是按一个算的
    match body.get("n") {
        None | Some(Value::Null) => {}
        Some(n) if n.as_u64() == Some(1) => {}
        Some(_) => return Err(RequestProblem::UnsupportedCompletionCount),
    }
    let stream = match body.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(RequestProblem::InvalidStream),
    };
    let wants_usage_chunk = match body.get("stream_options") {
        None | Some(Value::Null) => false,
        Some(Value::Object(options)) => match options.get("include_usage") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => return Err(RequestProblem::InvalidStreamOptions),
        },
        Some(_) => return Err(RequestProblem::InvalidStreamOptions),
    };
    // 两个字段都给了取小的（更严格的那个）
    let requested_max_tokens = match (
        max_tokens_field(&body, "max_tokens")?,
        max_tokens_field(&body, "max_completion_tokens")?,
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };

    let mut needs = CapabilityNeeds::default();
    if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        needs.tool_calling = true;
    }
    if body
        .get("response_format")
        .and_then(Value::as_object)
        .and_then(|format| format.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "text")
    {
        needs.structured_output = true;
    }

    // 输入估算：对 messages 与 tools 的整段 JSON 文本做保守估算（含键名与引号，只会多不会少）
    let mut text = String::new();
    for field in ["messages", "tools", "tool_choice", "response_format"] {
        if let Some(value) = body.get(field) {
            text.push_str(&value.to_string());
        }
    }
    let input_tokens_estimate = estimate_tokens(&text, "cloud");

    Ok(ClientRequest {
        body,
        model,
        stream,
        wants_usage_chunk,
        requested_max_tokens,
        input_tokens_estimate,
        needs,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    fn parse(value: Value) -> Result<ClientRequest, RequestProblem> {
        parse_client_request(value.to_string().as_bytes())
    }

    fn base() -> Value {
        json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "hi"}]})
    }

    #[test]
    fn a_plain_request() {
        let request = parse(base()).unwrap();
        assert_eq!(request.model, "idoris/agent");
        assert!(!request.stream);
        assert!(!request.wants_usage_chunk);
        assert_eq!(request.requested_max_tokens, None);
        assert_eq!(request.needs, CapabilityNeeds::default());
        assert!(request.input_tokens_estimate > 0);
    }

    #[test]
    fn shape_problems_have_stable_codes() {
        assert_eq!(
            parse_client_request(b"nope").unwrap_err(),
            RequestProblem::NotJson
        );
        assert_eq!(
            parse_client_request(b"[]").unwrap_err(),
            RequestProblem::NotAnObject
        );
        for model in [json!(null), json!(""), json!(3)] {
            let mut body = base();
            body["model"] = model;
            assert_eq!(parse(body).unwrap_err(), RequestProblem::MissingModel);
        }
        let mut body = base();
        body["messages"] = json!("hi");
        assert_eq!(parse(body).unwrap_err(), RequestProblem::MessagesMissing);
        for n in [json!(2), json!("1"), json!(0), json!(1.5)] {
            let mut body = base();
            body["n"] = n;
            assert_eq!(
                parse(body).unwrap_err(),
                RequestProblem::UnsupportedCompletionCount
            );
        }
        let mut body = base();
        body["n"] = json!(1);
        assert!(parse(body).is_ok());
        let mut body = base();
        body["stream"] = json!("yes");
        assert_eq!(parse(body).unwrap_err(), RequestProblem::InvalidStream);
        let mut body = base();
        body["stream_options"] = json!({"include_usage": "yes"});
        assert_eq!(
            parse(body).unwrap_err(),
            RequestProblem::InvalidStreamOptions
        );
        let codes: Vec<&str> = [
            RequestProblem::NotJson,
            RequestProblem::NotAnObject,
            RequestProblem::MissingModel,
            RequestProblem::MessagesMissing,
            RequestProblem::UnsupportedCompletionCount,
            RequestProblem::InvalidStream,
            RequestProblem::InvalidStreamOptions,
            RequestProblem::InvalidMaxTokens("max_tokens"),
        ]
        .iter()
        .map(|p| p.code())
        .collect();
        assert_eq!(
            codes,
            vec![
                "invalid_json",
                "invalid_body",
                "unsupported_model",
                "invalid_messages",
                "unsupported_completion_count",
                "invalid_stream",
                "invalid_stream_options",
                "invalid_completion_cap"
            ]
        );
    }

    #[test]
    fn max_tokens_takes_the_stricter_of_the_two_fields_and_rejects_zero_and_junk() {
        let mut body = base();
        body["max_tokens"] = json!(500);
        assert_eq!(parse(body.clone()).unwrap().requested_max_tokens, Some(500));
        body["max_completion_tokens"] = json!(100);
        assert_eq!(parse(body).unwrap().requested_max_tokens, Some(100));
        for bad in [json!(0), json!(-1), json!(1.5), json!("9"), json!(true)] {
            let mut body = base();
            body["max_tokens"] = bad;
            assert_eq!(
                parse(body).unwrap_err(),
                RequestProblem::InvalidMaxTokens("max_tokens")
            );
        }
        let mut body = base();
        body["max_completion_tokens"] = json!(0);
        assert_eq!(
            parse(body).unwrap_err(),
            RequestProblem::InvalidMaxTokens("max_completion_tokens")
        );
    }

    #[test]
    fn stream_usage_and_capability_needs() {
        let mut body = base();
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
        body["tools"] = json!([{"type": "function", "function": {"name": "f"}}]);
        body["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "r"}});
        let request = parse(body).unwrap();
        assert!(request.stream && request.wants_usage_chunk);
        assert!(request.needs.tool_calling && request.needs.structured_output);
        // 空 tools 与 text 格式不算需要
        let mut body = base();
        body["tools"] = json!([]);
        body["response_format"] = json!({"type": "text"});
        assert_eq!(parse(body).unwrap().needs, CapabilityNeeds::default());
    }

    #[test]
    fn the_input_estimate_covers_tools_and_never_undercounts_cjk() {
        let small = parse(base()).unwrap().input_tokens_estimate;
        let mut body = base();
        body["tools"] = json!([{"type": "function", "function": {"name": "f", "description": "x".repeat(300)}}]);
        assert!(parse(body).unwrap().input_tokens_estimate > small + 90);
        let thai = "สวัสดีครับ ".repeat(20);
        let mut body = base();
        body["messages"] = json!([{"role": "user", "content": thai.clone()}]);
        let estimate = parse(body).unwrap().input_tokens_estimate;
        assert!(
            estimate >= thai.len() as u64,
            "1 token per non-ASCII byte at least"
        );
    }
}

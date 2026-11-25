//! 客户端请求体的校验与计量：JSON 形状、`n`、`stream`、`max_tokens`，以及要用到的能力（工具调用、结构化输出）
//! 和输入 token 的保守估算。只看结构，**不记录也不转发**任何内容（转发是 `idoris-upstream::cloud::wire` 的事，
//! 白名单）。

use axum::http::HeaderMap;
use idoris_compute_contracts::permit::CapabilityNeeds;
use idoris_tenancy::budget::estimate_tokens;
use serde_json::{Map, Value};

use super::executor::CallPrivacy;

/// 隐私声明本身不合法（重复的头、空值、不认识的值……）。失败即关闭：不当作「没声明」放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidPrivacy;

/// 去掉首尾的 ASCII 空格和水平制表符：HTTP 头可选空白（OWS）的范围。`any` 是允许发往云端的那个值，识别它只认这两种空白；
/// 不换行空格、换行、回车、其它 Unicode 空白都不算（Codex 预审 M4）。
fn trim_ows(text: &str) -> &str {
    text.trim_matches(|c| c == ' ' || c == '\t')
}

/// 请求头 `X-iDoris-Privacy` 里的声明：没有头 = `Undeclared`；恰好一个值：`any` 或 `local_only`（`local_only` 不分大小写、
/// 去掉任何首尾空白，宁可多拒也不放过）；多个值、非 UTF-8、空白、别的值 = 不合法。
fn header_privacy(headers: &HeaderMap) -> Result<CallPrivacy, InvalidPrivacy> {
    let mut values = headers.get_all("x-idoris-privacy").iter();
    let Some(first) = values.next() else {
        return Ok(CallPrivacy::Undeclared);
    };
    if values.next().is_some() {
        return Err(InvalidPrivacy);
    }
    let text = first.to_str().map_err(|_| InvalidPrivacy)?;
    if trim_ows(text) == "any" {
        Ok(CallPrivacy::Any)
    } else if text.trim().eq_ignore_ascii_case("local_only") {
        Ok(CallPrivacy::LocalOnly)
    } else {
        Err(InvalidPrivacy)
    }
}

/// 请求体里的一处声明（顶层 `privacy` 或 `metadata.privacy`）：没写 / `null` / `any`（首尾只去掉 ASCII 空格和水平制表符，和请求头
/// 一样，见 [`trim_ows`]）不说明任何事（这个字段从来不转发给上游）；`local_only`（不分大小写、可带任何首尾空白）= 显式本地；
/// **写了别的任何东西**（`"localonly"`、带不换行空格或换行的 `any`、`true`、对象……）= 不合法，与请求头同样失败即关闭，
/// 不当作「没声明」放行。
fn body_field_privacy(value: Option<&Value>) -> Result<bool, InvalidPrivacy> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(text)) if trim_ows(text) == "any" => Ok(false),
        Some(Value::String(text)) if text.trim().eq_ignore_ascii_case("local_only") => Ok(true),
        Some(_) => Err(InvalidPrivacy),
    }
}

/// 请求体里的等价声明：顶层 `privacy`，或 `metadata.privacy`（调用方自己的标签里只有 `privacy` 这一个键被当作声明）。
fn body_declares_local_only(body: &Map<String, Value>) -> Result<bool, InvalidPrivacy> {
    let top = body_field_privacy(body.get("privacy"))?;
    let nested = body_field_privacy(
        body.get("metadata")
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get("privacy")),
    )?;
    Ok(top || nested)
}

/// 调用方对这次请求的隐私声明（头 + 请求体）。两处里有任何一处写了 `local_only` 就是 `LocalOnly`（取最严的）；
/// 头不合法返回 [`InvalidPrivacy`]。**没声明不等于 `local_only`**：见 [`CallPrivacy::Undeclared`]。
pub(crate) fn declared_privacy(
    headers: &HeaderMap,
    body: &Map<String, Value>,
) -> Result<CallPrivacy, InvalidPrivacy> {
    let from_header = header_privacy(headers)?;
    // 请求体的错写先于头里的取值判断：头写 `any` 不能掩盖它
    let body_local_only = body_declares_local_only(body)?;
    if from_header == CallPrivacy::LocalOnly || body_local_only {
        return Ok(CallPrivacy::LocalOnly);
    }
    Ok(from_header)
}

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

    use axum::http::HeaderValue;
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

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append("x-idoris-privacy", value.parse().unwrap());
        }
        map
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// R2 High#1：显式 `local_only`（头或请求体）是 `LocalOnly`；没声明不是；头不合法一律是错误，不是「没声明」。
    #[test]
    fn privacy_is_read_from_the_header_and_the_body_and_the_strictest_wins() {
        let plain = object(base());
        assert_eq!(
            declared_privacy(&headers(&[]), &plain),
            Ok(CallPrivacy::Undeclared),
            "an OpenAI client sends no privacy header; the cloud is where it chose to send its data"
        );
        assert_eq!(
            declared_privacy(&headers(&["any"]), &plain),
            Ok(CallPrivacy::Any)
        );
        for value in ["local_only", "LOCAL_ONLY", " local_only "] {
            assert_eq!(
                declared_privacy(&headers(&[value]), &plain),
                Ok(CallPrivacy::LocalOnly),
                "{value:?}"
            );
        }
        // 头不合法：重复、空白、不认识的值、非 UTF-8
        for bad in [
            vec!["any", "local_only"],
            vec!["any", "any"],
            vec![""],
            vec!["  "],
            vec!["bogus"],
            vec!["Any"],
        ] {
            assert_eq!(
                declared_privacy(&headers(&bad), &plain),
                Err(InvalidPrivacy),
                "{bad:?}"
            );
        }
        let mut non_ascii = HeaderMap::new();
        non_ascii.append(
            "x-idoris-privacy",
            HeaderValue::from_bytes(b"any\xff").unwrap(),
        );
        assert_eq!(declared_privacy(&non_ascii, &plain), Err(InvalidPrivacy));
        // 请求体里的等价声明
        let mut with_body = plain.clone();
        with_body.insert("privacy".into(), json!("local_only"));
        assert_eq!(
            declared_privacy(&headers(&[]), &with_body),
            Ok(CallPrivacy::LocalOnly)
        );
        let mut with_metadata = plain.clone();
        with_metadata.insert("metadata".into(), json!({"privacy": "Local_Only"}));
        assert_eq!(
            declared_privacy(&headers(&["any"]), &with_metadata),
            Ok(CallPrivacy::LocalOnly),
            "the strictest of header and body wins"
        );
        // 合法取值（`any`、`null` = 没写）行为不变：不说明任何事
        for other in [json!("any"), json!(null)] {
            let mut body = plain.clone();
            body.insert("privacy".into(), other);
            body.insert("metadata".into(), json!({"privacy": "any", "tag": "x"}));
            assert_eq!(
                declared_privacy(&headers(&[]), &body),
                Ok(CallPrivacy::Undeclared)
            );
        }
        // 没有 `privacy` 的 metadata 不受影响
        let mut tagged = plain.clone();
        tagged.insert("metadata".into(), json!({"tag": "x"}));
        assert_eq!(
            declared_privacy(&headers(&[]), &tagged),
            Ok(CallPrivacy::Undeclared)
        );
    }

    /// issue #603 N5：请求体里写了 `privacy` 却不是已定义的取值（`local-only`、`true`、对象……）
    /// 与请求头同样拒绝，不能被当成「没声明」而上云。`metadata.privacy` 同理（现有代码本来就读它）。
    #[test]
    fn an_unknown_body_privacy_value_is_rejected_like_a_bad_header() {
        let plain = object(base());
        let bad_values = [
            json!("local-only"),
            json!("localonly"),
            json!("Any"),
            json!(""),
            json!(" "),
            json!(true),
            json!(5),
            json!({"local_only": true}),
            json!(["local_only"]),
        ];
        for bad in bad_values {
            let mut body = plain.clone();
            body.insert("privacy".into(), bad.clone());
            assert_eq!(
                declared_privacy(&headers(&[]), &body),
                Err(InvalidPrivacy),
                "body privacy {bad}"
            );
            // 头写了 any 也不能掩盖请求体里的错写
            assert_eq!(
                declared_privacy(&headers(&["any"]), &body),
                Err(InvalidPrivacy),
                "body privacy {bad} with header any"
            );
            let mut body = plain.clone();
            body.insert("metadata".into(), json!({"privacy": bad.clone()}));
            assert_eq!(
                declared_privacy(&headers(&[]), &body),
                Err(InvalidPrivacy),
                "metadata.privacy {bad}"
            );
        }
        // 对照：合法取值不变
        let mut ok = plain.clone();
        ok.insert("privacy".into(), json!(" LOCAL_ONLY "));
        assert_eq!(
            declared_privacy(&headers(&[]), &ok),
            Ok(CallPrivacy::LocalOnly)
        );
    }

    /// PR-Daemon 在 #608 评审里指出：头里带空白的 ` any ` 被接受，请求体里的 ` any ` 却是 400。N5 的要求是「像请求头一样」，
    /// 两处对 `any` 的宽严要一致：都是去掉首尾空白之后精确匹配 `any`；`Any` / `ANY` 仍然不认（失败即关闭，头和请求体一样）。
    /// 请求体里的 `any` 和没写一样，不说明任何事（没有变成 `CallPrivacy::Any`）。
    #[test]
    fn a_padded_any_is_read_the_same_way_in_the_header_and_in_the_body() {
        let plain = object(base());
        for padded in [" any ", "  any", "any  "] {
            assert_eq!(
                declared_privacy(&headers(&[padded]), &plain),
                Ok(CallPrivacy::Any),
                "header {padded:?}"
            );
            let mut top = plain.clone();
            top.insert("privacy".into(), json!(padded));
            assert_eq!(
                declared_privacy(&headers(&[]), &top),
                Ok(CallPrivacy::Undeclared),
                "body privacy {padded:?}"
            );
            let mut nested = plain.clone();
            nested.insert("metadata".into(), json!({"privacy": padded}));
            assert_eq!(
                declared_privacy(&headers(&[]), &nested),
                Ok(CallPrivacy::Undeclared),
                "body metadata.privacy {padded:?}"
            );
        }
        // 大小写不对：头和请求体一样不认
        for wrong in ["Any", "ANY", " Any "] {
            assert_eq!(
                declared_privacy(&headers(&[wrong]), &plain),
                Err(InvalidPrivacy),
                "header {wrong:?}"
            );
            let mut top = plain.clone();
            top.insert("privacy".into(), json!(wrong));
            assert_eq!(
                declared_privacy(&headers(&[]), &top),
                Err(InvalidPrivacy),
                "body privacy {wrong:?}"
            );
        }
        // 空白本身仍然不是取值（头和请求体一样）
        for blank in ["", " ", "   "] {
            let mut top = plain.clone();
            top.insert("privacy".into(), json!(blank));
            assert_eq!(
                declared_privacy(&headers(&[]), &top),
                Err(InvalidPrivacy),
                "body privacy {blank:?}"
            );
        }
    }

    /// Codex 预审 M4：`any` 是允许发往云端的那个值，识别要比 `local_only` 严：首尾只去掉 ASCII 空格和水平制表符（HTTP 头里可选空白的
    /// 范围）。请求体里的不换行空格、转义的换行 / 回车、垂直制表、换页、别的 Unicode 空白都不是 `any`；去掉空白之后什么也没有的也不是。
    /// `local_only` 仍然宽松（去掉任何首尾空白、不分大小写，宁可多拒也不放过）。顶层 `privacy` 和 `metadata.privacy` 一样。
    #[test]
    fn any_is_trimmed_like_a_header_value_and_not_like_unicode_text() {
        let plain = object(base());
        let with = |value: &str, nested: bool| {
            let mut body = plain.clone();
            if nested {
                body.insert("metadata".into(), json!({"privacy": value}));
            } else {
                body.insert("privacy".into(), json!(value));
            }
            body
        };
        for nested in [false, true] {
            for ok in ["any", " any", "any ", "\tany", "any\t", " \t any \t "] {
                assert_eq!(
                    declared_privacy(&headers(&[]), &with(ok, nested)),
                    Ok(CallPrivacy::Undeclared),
                    "{ok:?} nested={nested}"
                );
            }
            for bad in [
                "\u{00A0}any",
                "any\u{00A0}",
                "any\n",
                "\nany",
                "any\r\n",
                "\u{000B}any",
                "any\u{000C}",
                "\u{2003}any",
                "\u{3000}any",
                " \t ",
                "\t",
                "",
                " ",
            ] {
                assert_eq!(
                    declared_privacy(&headers(&[]), &with(bad, nested)),
                    Err(InvalidPrivacy),
                    "{bad:?} nested={nested}"
                );
            }
            for local in [
                "local_only",
                "LOCAL_ONLY",
                " local_only ",
                "\u{00A0}local_only",
                "local_only\n",
            ] {
                assert_eq!(
                    declared_privacy(&headers(&[]), &with(local, nested)),
                    Ok(CallPrivacy::LocalOnly),
                    "{local:?} nested={nested}"
                );
            }
        }
        // 请求头里首尾的水平制表符同样认
        assert_eq!(
            declared_privacy(&headers(&["\tany"]), &plain),
            Ok(CallPrivacy::Any)
        );
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
        let non_ascii_bytes = thai.bytes().filter(|b| !b.is_ascii()).count() as u64;
        assert!(
            estimate >= non_ascii_bytes,
            "1 token per non-ASCII byte at least"
        );
    }
}

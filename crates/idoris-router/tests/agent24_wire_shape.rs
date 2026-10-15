//! Agent24 v0.3.1 原生 Supervisor 路径线缆形状探针（独立测试工装）。
//!
//! 与 `fix/idoris-native-compat-20261015` 上 `agent24_native_compat.rs` 的五项
//! 基线互补而非复制：那里用 `oneshot` 在进程内调用 router，只检查字段“存在”；
//! 这里把 router 真正绑定到 loopback TCP 端口，用 reqwest 走真实 HTTP，
//! 上游是隔离的 wiremock loopback 夹具（oMLX 形状），并检查：
//!
//! 1. `max_tokens` 的具体数值（含边界值）原样、按整数类型转发；
//! 2. 带嵌套 JSON Schema 的 `tools` 与 `tool_choice` / `parallel_tool_calls` 深度相等转发；
//! 3. 上游 `tool_calls` 响应的 id / name / arguments 原样回到客户端；
//! 4. `response_format` 的 `json_schema` 完整结构深度相等转发；
//! 5. `chat_template_kwargs.enable_thinking` 的 true 与 false 两个开关值都按原值转发；
//! 6. 非法参数值被拒绝（4xx），且夹具没有收到任何 chat POST。
//!
//! 不测超时（另一车道独占）。不使用 ignore/skip：当前产品未实现的项目会真实 RED。
//! 夹具只打印非敏感字段（被探测字段的值、POST 次数、HTTP 状态）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app, dispatch::BoundSupervisor};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// 上游 chat 响应：一个带 tool_calls 的 OpenAI 形状 completion。
fn tool_call_completion() -> Value {
    json!({
        "id": "chatcmpl-wire-fixture",
        "object": "chat.completion",
        "model": "omlx",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_wire_7f3a9c",
                    "type": "function",
                    "function": {
                        "name": "lookup_weather",
                        "arguments": "{\"city\":\"Bangkok\",\"unit\":\"celsius\",\"days\":3}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
}

fn text_completion() -> Value {
    json!({
        "id": "chatcmpl-wire-fixture",
        "object": "chat.completion",
        "model": "omlx",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "wire ok"}, "finish_reason": "stop"}]
    })
}

/// 隔离的 loopback oMLX 形状夹具 + 绑定在 loopback TCP 上的真实 router。
struct Harness {
    upstream: MockServer,
    endpoint: String,
    client: reqwest::Client,
    router_task: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.router_task.abort();
    }
}

impl Harness {
    async fn start(chat_response: Value) -> Self {
        // Each harness represents a distinct engine. A pooled server can reuse
        // an endpoint while the prior Supervisor still owns its load fence.
        let upstream = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "loaded_models": [],
                "model_memory_used": 0,
                "model_memory_max": 16,
                "pressure": "ok"
            })))
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/models/omlx/load"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"id": "omlx", "loaded": true, "pinned": false, "estimated_size": 1_073_741_824_u64}]
            })))
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response))
            .mount(&upstream)
            .await;

        // 原生（on_demand / Supervisor）卡片：不改成 Resident，避免走直通 proxy。
        let mut card: ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        card.endpoint = upstream.uri();
        let app = build_app(AppState {
            runtimes: Some(BoundSupervisor::spawn_omlx(&card).unwrap()).into(),
            cards: vec![card],
            ..AppState::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            upstream,
            endpoint: format!("http://{addr}/v1/chat/completions"),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            router_task,
        }
    }

    /// 通过真实 HTTP 发一次非流式请求；返回 (状态码, 响应 JSON, 夹具收到的 chat POST 体)。
    async fn post(&self, extra: &[(&str, Value)]) -> (u16, Value, Vec<Value>) {
        let mut body = json!({
            "model": "idoris/daily",
            "messages": [{"role": "user", "content": "probe"}],
            "stream": false
        });
        for (key, value) in extra {
            body[*key] = value.clone();
        }
        let response = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("X-iDoris-Privacy", "local_only")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.bytes().await.unwrap();
        let json: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"_raw": String::from_utf8_lossy(&bytes)}));
        let chats: Vec<Value> = self
            .upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/v1/chat/completions")
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect();
        (status, json, chats)
    }
}

fn reason(response: &Value) -> String {
    response["error"]["reason_code"]
        .as_str()
        .unwrap_or("-")
        .to_owned()
}

/// 对“应转发”的字段：期望 200、夹具恰好收到 1 次 chat POST、字段深度相等
/// （数值还要求类型一致，例如整数不能变成浮点）。返回失败描述而不 panic，
/// 便于多值探针把每个值的结果都记录下来再统一断言。
async fn check_forwarded(probe: &str, extra: &[(&str, Value)]) -> Result<(), String> {
    let harness = Harness::start(text_completion()).await;
    let (status, response, chats) = harness.post(extra).await;
    for (key, sent) in extra {
        let received = chats.first().map_or(&Value::Null, |c| &c[*key]);
        eprintln!(
            "WIRE probe={probe} field={key} http_status={status} reason_code={} chat_posts={} sent={sent} fixture_received={received}",
            reason(&response),
            chats.len()
        );
    }
    if status != 200 {
        return Err(format!(
            "probe={probe}: 原生 Supervisor 路径未接受该字段 (HTTP {status}, reason_code={}, chat_posts={})",
            reason(&response),
            chats.len()
        ));
    }
    if chats.len() != 1 {
        return Err(format!(
            "probe={probe}: 夹具应恰好收到 1 次 chat POST，实际 {}",
            chats.len()
        ));
    }
    for (key, sent) in extra {
        let received = &chats[0][*key];
        if received != sent
            || received.is_u64() != sent.is_u64()
            || received.is_f64() != sent.is_f64()
        {
            return Err(format!(
                "probe={probe}: 字段 {key} 未原样转发 (sent={sent}, received={received})"
            ));
        }
    }
    Ok(())
}

fn assert_all(results: Vec<Result<(), String>>) {
    let failures: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    assert!(
        failures.is_empty(),
        "线缆形状探针 RED:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn native_http_max_tokens_exact_values_are_forwarded_as_integers() {
    // 边界值 1、非整百值 1337、大值 32768：检测钳制、默认值覆盖或浮点化。
    let mut results = Vec::new();
    for value in [1_u64, 1337, 32768] {
        results.push(
            check_forwarded(
                &format!("max_tokens={value}"),
                &[("max_tokens", json!(value))],
            )
            .await,
        );
    }
    assert_all(results);
}

fn weather_tools() -> Value {
    json!([{
        "type": "function",
        "function": {
            "name": "lookup_weather",
            "description": "按城市查询天气（线缆探针，夹具不执行）",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string", "minLength": 1},
                    "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]},
                    "days": {"type": "integer", "minimum": 1, "maximum": 7}
                },
                "required": ["city", "unit"],
                "additionalProperties": false
            }
        }
    }, {
        "type": "function",
        "function": {
            "name": "noop_probe",
            "parameters": {"type": "object", "properties": {}}
        }
    }])
}

#[tokio::test]
async fn native_http_tools_json_with_nested_schema_is_forwarded_deep_equal() {
    assert_all(vec![
        check_forwarded(
            "tools+tool_choice+parallel_tool_calls",
            &[
                ("tools", weather_tools()),
                (
                    "tool_choice",
                    json!({"type": "function", "function": {"name": "lookup_weather"}}),
                ),
                ("parallel_tool_calls", json!(false)),
            ],
        )
        .await,
    ]);
}

#[tokio::test]
async fn native_http_tool_calls_response_preserves_id_name_and_arguments() {
    let harness = Harness::start(tool_call_completion()).await;
    let (status, response, chats) = harness.post(&[("tools", weather_tools())]).await;
    let call = &response["choices"][0]["message"]["tool_calls"][0];
    eprintln!(
        "WIRE probe=tool_calls_response http_status={status} reason_code={} chat_posts={} client_tool_call_id={} client_name={} client_arguments={} client_finish_reason={}",
        reason(&response),
        chats.len(),
        call["id"],
        call["function"]["name"],
        call["function"]["arguments"],
        response["choices"][0]["finish_reason"]
    );
    assert_eq!(
        status, 200,
        "原生 Supervisor 路径未接受 tools 请求; response={response}"
    );
    assert_eq!(chats.len(), 1);
    let expected = &tool_call_completion()["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], expected["id"], "tool_call id 未保真");
    assert_eq!(call["type"], "function");
    assert_eq!(
        call["function"]["name"], expected["function"]["name"],
        "tool_call name 未保真"
    );
    // arguments 必须是与上游逐字节一致的 JSON 字符串，而不是被解析/重排的对象。
    assert_eq!(
        call["function"]["arguments"], expected["function"]["arguments"],
        "tool_call arguments 未逐字保真"
    );
    assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn native_http_response_format_json_schema_structure_is_forwarded_deep_equal() {
    assert_all(vec![
        check_forwarded(
            "response_format=json_schema",
            &[(
                "response_format",
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "wire_probe_answer",
                        "strict": true,
                        "schema": {
                            "type": "object",
                            "properties": {
                                "answer": {"type": "string"},
                                "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                                "tags": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["answer", "confidence"],
                            "additionalProperties": false
                        }
                    }
                }),
            )],
        )
        .await,
    ]);
}

#[tokio::test]
async fn native_http_thinking_switch_true_and_false_both_forwarded_verbatim() {
    // 同时测 true 与 false：只测 false 时，“恒定注入 false”的实现也会假绿。
    let mut results = Vec::new();
    for enabled in [true, false] {
        results.push(
            check_forwarded(
                &format!("enable_thinking={enabled}"),
                &[("chat_template_kwargs", json!({"enable_thinking": enabled}))],
            )
            .await,
        );
    }
    assert_all(results);
}

#[tokio::test]
async fn native_http_invalid_parameter_values_are_rejected_without_chat_post() {
    let cases: [(&str, Value); 9] = [
        ("max_tokens", json!(-1)),
        ("max_tokens", json!(0)),
        ("max_tokens", json!("512")),
        ("max_tokens", json!(1.5)),
        ("tools", json!({"type": "function"})),
        (
            "tools",
            json!([{"type": "function", "function": {"parameters": {"type": "object"}}}]),
        ),
        ("response_format", json!({"type": "xml"})),
        ("response_format", json!("json_object")),
        ("chat_template_kwargs", json!({"enable_thinking": "yes"})),
    ];
    let harness = Harness::start(text_completion()).await;
    let mut accepted = Vec::new();
    for (key, value) in cases {
        let (status, response, chats) = harness.post(&[(key, value.clone())]).await;
        eprintln!(
            "WIRE probe=invalid field={key} sent={value} http_status={status} reason_code={} cumulative_chat_posts={}",
            reason(&response),
            chats.len()
        );
        if !(400..500).contains(&status) {
            accepted.push(format!("{key}={value} -> {status}"));
        }
    }
    let chats = harness.upstream.received_requests().await.unwrap();
    let chats: Vec<_> = chats
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .collect();
    assert!(accepted.is_empty(), "非法参数未被 4xx 拒绝: {accepted:?}");
    assert!(
        chats.is_empty(),
        "非法参数不得触发任何 chat POST，实际 {}",
        chats.len()
    );
}

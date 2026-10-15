#![allow(clippy::unwrap_used, clippy::expect_used)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app, dispatch::BoundSupervisor};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
async fn check(field: &str, value: Value) {
    let upstream = MockServer::start().await;
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
            "models": [{
                "id": "omlx",
                "loaded": true,
                "pinned": false,
                "estimated_size": 1024_u64 * 1024 * 1024
            }]
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "omlx",
            "choices": [{"message": {"content": "buffered answer"}}]
        })))
        .mount(&upstream)
        .await;
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = upstream.uri();
    card.extensions = Some(
        serde_json::from_value(serde_json::json!({
            "_degradation": "Removing this explicit fixture binding disables daily aliases.",
            "idoris_role_models": {"daily": "omlx"}
        }))
        .unwrap(),
    );
    let app = build_app(AppState {
        runtimes: Some(BoundSupervisor::spawn_omlx(&card).unwrap()).into(),
        cards: vec![card],
        ..AppState::default()
    });
    let mut body =
        json!({"model":"idoris/daily","messages":[{"role":"user","content":"hi"}],"stream":false});
    body[field] = value.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("X-iDoris-Privacy", "local_only")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response = response.into_body().collect().await.unwrap().to_bytes();
    let requests = upstream.received_requests().await.unwrap();
    let posts: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .collect();
    assert_eq!(
        status,
        StatusCode::OK,
        "field={field}; POSTs={}; response={}",
        posts.len(),
        String::from_utf8_lossy(&response)
    );
    assert_eq!(posts.len(), 1);
    let forwarded: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(forwarded[field], value);
    assert_eq!(forwarded["stream"], false);
}
#[tokio::test]
async fn max_tokens_reaches_native_backend() {
    check("max_tokens", json!(512)).await;
}
#[tokio::test]
async fn tools_reach_native_backend() {
    check(
        "tools",
        json!([{"type":"function","function":{"name":"probe","parameters":{"type":"object"}}}]),
    )
    .await;
}
#[tokio::test]
async fn response_format_reaches_native_backend() {
    check("response_format", json!({"type":"json_object"})).await;
}
#[tokio::test]
async fn thinking_reaches_native_backend() {
    check("chat_template_kwargs", json!({"enable_thinking":false})).await;
}

#[tokio::test]
async fn disable_thinking_reaches_native_backend() {
    check("disable_thinking", json!(true)).await;
}

async fn native_app(upstream: &MockServer) -> axum::Router {
    native_app_with_memory(upstream, 1073741824).await
}

async fn native_app_with_memory(upstream: &MockServer, estimated_size: u64) -> axum::Router {
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 16, "pressure": "ok"
        })))
        .mount(upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/omlx/load"))
        .respond_with(ResponseTemplate::new(200))
        .mount(upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
            "id":"omlx", "loaded":true, "pinned":false, "estimated_size":estimated_size
        }]})))
        .mount(upstream)
        .await;
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = upstream.uri();
    card.extensions = Some(
        serde_json::from_value(serde_json::json!({
            "_degradation": "Removing this explicit fixture binding disables daily aliases.",
            "idoris_role_models": {"daily": "omlx"}
        }))
        .unwrap(),
    );
    build_app(AppState {
        runtimes: Some(BoundSupervisor::spawn_omlx(&card).unwrap()).into(),
        cards: vec![card],
        ..AppState::default()
    })
}

fn request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("X-iDoris-Privacy", "local_only")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn tool_call_and_result_round_trip_preserves_native_model_and_usage() {
    let upstream = MockServer::start().await;
    let app = native_app(&upstream).await;
    let assistant = json!({"role":"assistant", "content":null, "tool_calls":[{
        "id":"call_probe", "type":"function", "function":{"name":"probe", "arguments":"{\"city\":\"Paris\"}"}
    }]});
    let completion = json!({"id":"native-call", "object":"chat.completion", "created":123,
        "model":"actual-native-model", "choices":[{"index":0,"message":assistant,"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":71,"completion_tokens":19,"total_tokens":90,"prompt_tokens_details":{"cached_tokens":12}}
    });
    let final_completion = json!({"id":"native-final", "object":"chat.completion", "created":124,
        "model":"actual-native-model", "choices":[{"index":0,"message":{"role":"assistant","content":"Paris is sunny"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":99,"completion_tokens":5,"total_tokens":104}
    });
    let tool_reply = completion.clone();
    let final_reply = final_completion.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let has_result = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            ResponseTemplate::new(200).set_body_json(if has_result {
                &final_reply
            } else {
                &tool_reply
            })
        })
        .mount(&upstream)
        .await;
    let tools =
        json!([{"type":"function", "function":{"name":"probe","parameters":{"type":"object"}}}]);
    let response = app
        .clone()
        .oneshot(request(json!({"model":"idoris/daily", "max_tokens":512,
        "tools":tools,"messages":[{"role":"user","content":"weather?"}]})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body, completion);
    let messages = json!([{"role":"user","content":"weather?"}, body["choices"][0]["message"].clone(),
        {"role":"tool","name":"probe", "tool_call_id":"call_probe", "content":"sunny"}]);
    let response = app
        .oneshot(request(
            json!({"model":"idoris/daily","tools":tools,"messages":messages}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let final_body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(final_body, final_completion);
    let requests = upstream.received_requests().await.unwrap();
    let posts: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .collect();
    assert_eq!(posts.len(), 2);
    let first: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(first["max_tokens"], 512);
    assert_eq!(first["tools"], tools);
    let second: Value = serde_json::from_slice(&posts[1].body).unwrap();
    assert_eq!(second["messages"], messages);
    assert_eq!(second["tools"], tools);
    assert_eq!(second["model"], "omlx");
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/v1/models/omlx/load")
    );
}

#[tokio::test]
async fn invalid_or_unknown_native_parameters_refuse_before_lifecycle_io() {
    let upstream = MockServer::start().await;
    let app = native_app(&upstream).await;
    for (field, value) in [
        ("max_tokens", Value::Null),
        ("max_tokens", json!(0)),
        ("max_tokens", json!(-1)),
        ("max_tokens", json!(1.5)),
        ("max_tokens", json!("512")),
        ("tools", json!({})),
        ("tools", json!([{"type":"function","function":{}}])),
        ("response_format", json!({"type":"unknown"})),
        ("chat_template_kwargs", json!(false)),
        ("disable_thinking", json!("true")),
        ("future_parameter", json!(true)),
    ] {
        let mut body = json!({"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]});
        body[field] = value;
        let response = app.clone().oneshot(request(body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{field}");
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn null_without_tool_calls_is_an_upstream_error() {
    let upstream = MockServer::start().await;
    let app = native_app(&upstream).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"model":"omlx","choices":[{"message":{"content":null}}]})),
        )
        .mount(&upstream)
        .await;
    let response = app
        .oneshot(request(
            json!({"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn parameters_cannot_bypass_supervisor_memory_admission() {
    let upstream = MockServer::start().await;
    let app = native_app_with_memory(&upstream, 64 * 1024 * 1024 * 1024).await;
    let response = app
        .oneshot(request(json!({"model":"idoris/daily", "max_tokens":512,
        "tools":[], "response_format":{"type":"json_object"},
        "chat_template_kwargs":{"enable_thinking":false},
        "messages":[{"role":"user","content":"hi"}]})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let requests = upstream.received_requests().await.unwrap();
    assert!(requests.iter().all(|request| request.method != "POST"));
}

#[tokio::test]
async fn parameters_cannot_bypass_spend_admission() {
    use idoris_tenancy::budget::{BudgetLedger, SpendGate};
    let upstream = MockServer::start().await;
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = upstream.uri();
    card.extensions = Some(
        serde_json::from_value(serde_json::json!({
            "_degradation": "Removing this explicit fixture binding disables daily aliases.",
            "idoris_role_models": {"daily": "omlx"}
        }))
        .unwrap(),
    );
    card.provider.cost = idoris_contracts::provider::Cost {
        input_per_m: 1_000_000.0,
        output_per_m: 2_000_000.0,
    };
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
    ledger
        .configure_tenant("personal", 1, "UTC", SpendGate::PaidOnly)
        .unwrap();
    let app = build_app(AppState {
        runtimes: Some(BoundSupervisor::spawn_omlx(&card).unwrap()).into(),
        cards: vec![card],
        budget_ledger: Some(std::sync::Arc::new(ledger)),
        ..AppState::default()
    });
    let response = app
        .oneshot(request(json!({"model":"idoris/daily", "max_tokens":512,
        "tools":[], "messages":[{"role":"user","content":"hi"}]})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

async fn assert_invalid_backend_model(model: Option<Value>) {
    let upstream = MockServer::start().await;
    let app = native_app(&upstream).await;
    let mut completion = json!({"choices":[{"message":{"role":"assistant","content":"answer"}}],
        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}});
    if let Some(model) = model {
        completion["model"] = model;
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completion))
        .mount(&upstream)
        .await;
    let response = app
        .oneshot(request(json!({"model":"idoris/daily", "max_tokens":512,
        "messages":[{"role":"user","content":"hi"}]})))
        .await
        .unwrap();
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/chat/completions")
            .count(),
        1,
        "response validation happens after the inference POST"
    );
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "backend={completion}; response={body}"
    );
    assert!(
        body.get("model").is_none(),
        "no inferred actual-model identity: {body}"
    );
    assert!(body.get("choices").is_none());
}

#[tokio::test]
async fn missing_backend_model_is_refused_after_inference() {
    assert_invalid_backend_model(None).await;
}

#[tokio::test]
async fn empty_backend_model_is_refused_after_inference() {
    assert_invalid_backend_model(Some(json!(""))).await;
}

#[tokio::test]
async fn nonstring_backend_model_is_refused_after_inference() {
    assert_invalid_backend_model(Some(json!(42))).await;
}

#[tokio::test]
async fn whitespace_backend_model_is_refused_after_inference() {
    assert_invalid_backend_model(Some(json!(" \t\n"))).await;
}

#[tokio::test]
async fn null_backend_model_is_refused_after_inference() {
    assert_invalid_backend_model(Some(Value::Null)).await;
}

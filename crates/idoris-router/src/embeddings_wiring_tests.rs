#![allow(clippy::unwrap_used)]

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::Capability;
use idoris_contracts::component_card::Egress;
use idoris_tenancy::budget::{BudgetLedger, SpendGate};
use idoris_tenancy::store::{RecordKind, TenantStore};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

fn embedding_card(id: &str, endpoint: &str) -> idoris_contracts::ComponentCard {
    let mut card = super::tests::resident_component_card(id, endpoint);
    card.provider.capabilities = vec![Capability::Embedding];
    card
}

fn request(body: serde_json::Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn embeddings_preserves_upstream_path_payload_and_response() {
    let server = MockServer::start().await;
    let payload = json!({
        "model": "embed-model",
        "input": ["hello", "world"],
        "encoding_format": "float"
    });
    let upstream_body = json!({
        "object": "list",
        "data": [{"object":"embedding","index":0,"embedding":[0.1,0.2]}]
    });
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .and(wiremock::matchers::body_json(payload.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let response = build_app(AppState {
        cards: vec![embedding_card("embed-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[HEADER_SERVED_LOCALITY],
        HeaderValue::from_static("loopback")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        upstream_body
    );
    server.verify().await;
}

#[tokio::test]
async fn embeddings_fails_closed_before_upstream_for_wrong_capability_and_untrusted_locality() {
    let server = MockServer::start().await;
    let payload = json!({"model":"embed-model","input":"secret"});

    let chat_only = super::tests::resident_component_card("chat-only", &server.uri());
    let response = build_app(AppState {
        cards: vec![chat_only],
        ..AppState::default()
    })
    .oneshot(request(payload.clone(), &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let mut untrusted = embedding_card("untrusted", &server.uri());
    untrusted.allowed_egress = vec![Egress::Internet];
    let response = build_app(AppState {
        cards: vec![untrusted],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn tenant_embeddings_requires_tenant_before_upstream() {
    let server = MockServer::start().await;
    let (app, key) = tenant_test_app(
        AppState {
            deploy_mode: idoris_contracts::DeployMode::Tenant,
            cards: vec![embedding_card("embed-local", &server.uri())],
            ..AppState::default()
        },
        "acme",
    );
    let response = app
        .oneshot(tenant_test_request(
            request(json!({"model":"embed-model","input":"tenant data"}), &[]),
            &key,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "tenant_missing");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn paid_embeddings_fail_closed_before_upstream() {
    let server = MockServer::start().await;
    let mut card = embedding_card("embed-paid", &server.uri());
    card.provider.cost.input_per_m = 1.0;
    let response = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"embed-model","input":"paid data"}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "paid_proxy_unavailable");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn free_embeddings_respect_exhausted_spend_gate_all_before_upstream() {
    let server = MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 0, "UTC", SpendGate::All)
        .unwrap();
    let response = build_app(AppState {
        cards: vec![embedding_card("embed-local", &server.uri())],
        budget_ledger: Some(Arc::new(ledger)),
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"embed-model","input":"free but gated"}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "budget_exceeded");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cached_embeddings_still_respect_exhausted_spend_gate_all() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = Arc::new(BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap());
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 1, "UTC", SpendGate::All)
        .unwrap();
    let app = build_app(AppState {
        cards: vec![embedding_card("embed-local", &server.uri())],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let payload = json!({"model":"embed-model","input":"cache then gate"});
    let headers = [(HEADER_REQUEST_ID, "embed-gated-cache")];

    let first = app
        .clone()
        .oneshot(request(payload.clone(), &headers))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 0, "UTC", SpendGate::All)
        .unwrap();

    let second = app.oneshot(request(payload, &headers)).await.unwrap();
    assert_eq!(second.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = second.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "budget_exceeded");
    server.verify().await;
}

#[tokio::test]
async fn embeddings_audit_usage_and_cache_origin_follow_record_id_contract() {
    let server = MockServer::start().await;
    let store = Arc::new(std::sync::Mutex::new(
        TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"object":"embedding","index":0,"embedding":[0.1]}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let app = build_app(AppState {
        cards: vec![embedding_card("embed-local", &server.uri())],
        record_store: Some(store.clone()),
        ..AppState::default()
    });
    let payload = json!({"model":"embed-model","input":"cache me"});
    let headers = [(HEADER_REQUEST_ID, "embed-cache-1")];

    let first = app
        .clone()
        .oneshot(request(payload.clone(), &headers))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_record_id = first.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string();
    assert!(first.headers().get(HEADER_CACHED).is_none());

    let second = app.oneshot(request(payload, &headers)).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_record_id = second.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(second_record_id, first_record_id);
    assert_eq!(second.headers()[HEADER_CACHED], "true");
    assert_eq!(
        second.headers()[HEADER_ORIGIN_RECORD_ID].to_str().unwrap(),
        first_record_id
    );

    {
        let guard = store.lock().unwrap();
        let audit = guard
            .list(Some(budget::PERSONAL_TENANT_ID), Some(RecordKind::Audit))
            .unwrap();
        assert_eq!(audit.len(), 2);
        assert_eq!(
            audit
                .iter()
                .find(|row| row.record_id == second_record_id)
                .unwrap()
                .origin_record_id
                .as_deref(),
            Some(first_record_id.as_str())
        );
        let usage = guard
            .list(Some(budget::PERSONAL_TENANT_ID), Some(RecordKind::Usage))
            .unwrap();
        assert_eq!(usage.len(), 1, "cache replay must not duplicate usage");
        assert_eq!(usage[0].record_id, first_record_id);
        assert_eq!(usage[0].payload["cost_minor"], json!(0));
    }
    server.verify().await;
}

/// 伴生卡的形状（`setup omlx --bind embed=…` 的产物）：resident、只有 embedding、绑定 embed 角色。
fn companion_card(endpoint: &str, model: &str) -> idoris_contracts::ComponentCard {
    let mut card = embedding_card("omlx-embed", endpoint);
    card.version_pin = "omlx@0.6.4".to_string();
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"embed": model}
        }))
        .unwrap(),
    );
    card
}

/// 主卡：默认的 on_demand oMLX 卡（能力有 embedding，但不是 resident，所以 `/v1/embeddings` 选不到它）。
fn on_demand_main(endpoint: &str) -> idoris_contracts::ComponentCard {
    let mut card = super::tests::sample_component_card("omlx");
    card.provider.capabilities = vec![Capability::Chat, Capability::Embedding];
    card.version_pin = "omlx@0.6.4".to_string();
    card.endpoint = endpoint.to_string();
    card.load_policy = Some(idoris_contracts::load_policy::LoadPolicy {
        mode: idoris_contracts::load_policy::LoadMode::OnDemand,
        keepalive: idoris_contracts::load_policy::Keepalive::IdleTtl { idle_ttl_s: 300 },
        admission: idoris_contracts::load_policy::Admission::RequiresEviction,
    });
    card
}

async fn embeddings_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"object":"embedding","index":0,"embedding":[0.1]}]
        })))
        .mount(server)
        .await;
}

fn auth_free(body: serde_json::Value) -> Request<Body> {
    request(body, &[])
}

/// `idoris/embed` 走伴生卡：主卡是 on_demand，不是候选；上游收到的是绑定的具体模型。
#[tokio::test]
async fn idoris_embed_routes_to_the_companion_card_and_rewrites_the_bound_model() {
    let server = MockServer::start().await;
    embeddings_ok(&server).await;
    let app = build_app(AppState {
        cards: vec![
            on_demand_main(&server.uri()),
            companion_card(&server.uri(), "bge-m3-mlx-fp16"),
        ],
        ..AppState::default()
    });
    let response = app
        .oneshot(auth_free(json!({"model":"idoris/embed","input":["a","b"]})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[HEADER_SERVED_LOCALITY],
        HeaderValue::from_static("loopback")
    );
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(sent["model"], "bge-m3-mlx-fp16");
    assert_eq!(sent["input"], json!(["a", "b"]));
}

/// 没有伴生卡时，默认安装（只有 on_demand 主卡）的 embeddings 仍是 503，不碰上游。
#[tokio::test]
async fn without_the_companion_card_idoris_embed_has_no_candidate() {
    let server = MockServer::start().await;
    embeddings_ok(&server).await;
    let app = build_app(AppState {
        cards: vec![on_demand_main(&server.uri())],
        ..AppState::default()
    });
    let response = app
        .oneshot(auth_free(json!({"model":"idoris/embed","input":"x"})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// 绑定只在伴生卡上：别的角色（`idoris/daily`）发 embeddings 请求，伴生卡没有这个角色，选不到。
#[tokio::test]
async fn a_role_the_companion_does_not_bind_does_not_reach_it() {
    let server = MockServer::start().await;
    embeddings_ok(&server).await;
    let app = build_app(AppState {
        cards: vec![
            on_demand_main(&server.uri()),
            companion_card(&server.uri(), "bge-m3-mlx-fp16"),
        ],
        ..AppState::default()
    });
    let response = app
        .oneshot(auth_free(json!({"model":"idoris/daily","input":"x"})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// embeddings 有自己的响应头超时：上游慢过聊天路径的头超时、没慢过 embeddings 自己的，仍然成功。
#[tokio::test]
async fn embeddings_use_their_own_header_timeout_not_the_chat_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(250))
                .set_body_json(json!({"object":"list","data":[]})),
        )
        .mount(&server)
        .await;
    let build = |chat: u64, embed: u64| {
        let mut proxy = proxy::ChatProxy::new(reqwest::Client::new());
        proxy.header_timeout = std::time::Duration::from_millis(chat);
        proxy.embeddings_header_timeout = std::time::Duration::from_millis(embed);
        build_app(AppState {
            cards: vec![companion_card(&server.uri(), "bge-m3-mlx-fp16")],
            proxy: Arc::new(proxy),
            ..AppState::default()
        })
    };
    // 聊天的头超时只有 50 毫秒，embeddings 的有 5 秒：成功。
    let ok = build(50, 5_000)
        .oneshot(auth_free(json!({"model":"idoris/embed","input":"x"})))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    // embeddings 自己的头超时短于上游的延迟：504（超时确实作用在这条路径上）。
    let slow = build(5_000, 50)
        .oneshot(auth_free(json!({"model":"idoris/embed","input":"x"})))
        .await
        .unwrap();
    assert_eq!(slow.status(), StatusCode::GATEWAY_TIMEOUT);
}

/// 缺省值：embeddings 30 秒，聊天仍是 10 秒；别的路径（rerank、messages）不沿用 embeddings 的宽超时。
#[tokio::test]
async fn the_default_embeddings_header_timeout_is_30s_and_other_paths_keep_the_chat_one() {
    let p = proxy::ChatProxy::new(reqwest::Client::new());
    assert_eq!(
        p.embeddings_header_timeout,
        std::time::Duration::from_secs(30)
    );
    assert_eq!(p.header_timeout, std::time::Duration::from_secs(10));
    for path in [
        proxy::BufferedUpstreamPath::CHAT_COMPLETIONS,
        proxy::BufferedUpstreamPath::RERANK,
        proxy::BufferedUpstreamPath::MESSAGES,
    ] {
        assert_eq!(p.header_timeout_for(path), p.header_timeout);
    }
    assert_eq!(
        p.header_timeout_for(proxy::BufferedUpstreamPath::EMBEDDINGS),
        p.embeddings_header_timeout
    );
}

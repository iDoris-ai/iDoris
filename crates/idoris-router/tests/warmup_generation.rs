//! IDORIS-CORE S6 × S8：resident 预热的最小请求在**自己的任务**里跑，调用方放弃之后它照常跑完（冷加载最长 180 秒），所以它可能活
//! 到引擎被停止、重启、换了进程之后。它绑定准入时的引擎代际：代理在每一次发送（连接失败的重试也算）时核对，端点被封上、或者代际
//! 已经不是准入时的，一个字节都不发——不带新一代的 key，请求体（预热用的那一句话）也不会发给新进程。
//!
//! 全部进程内、假上游用 `wiremock`，不启动任何真实模型进程。登记表是进程全局的，每个用例用自己的端口作端点。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_router::readiness::{HealthState, UpstreamState, UpstreamStatus};
use idoris_router::{AppState, build_app};
use idoris_upstream::omlx::key::{EndpointKey, block_endpoint, govern_endpoint, release_endpoint};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 登记表是进程全局的，端口会被很快复用：每个用例结束时撤销自己的登记。
struct Governed(String);

impl Drop for Governed {
    fn drop(&mut self) {
        release_endpoint(&self.0);
    }
}

/// 一张 resident 的托管 oMLX 卡（loopback，会按登记表带 key）。
fn resident_managed_card(endpoint: &str) -> ComponentCard {
    serde_yaml::from_str(&format!(
        r#"provider:
  id: omlx
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
extensions:
  _degradation: "Removing these bindings disables aliases."
  idoris_role_models: {{ daily: "qwen" }}
  idoris_managed: {{ engine: omlx, mode: auto }}
"#
    ))
    .unwrap()
}

/// 数据面此刻认为这个托管上游在服务（所以预热能走到发送那一步）。
fn serving_health() -> HealthState {
    HealthState::with_upstreams(
        0,
        vec![UpstreamStatus {
            id: "omlx".to_owned(),
            engine_kind: "omlx".to_owned(),
            state: UpstreamState::Ready,
            reason: None,
            managed: true,
            owned: true,
        }],
    )
}

fn app(endpoint: &str) -> Router {
    build_app(AppState {
        cards: vec![resident_managed_card(endpoint)],
        health_state: serving_health(),
        ..AppState::default()
    })
}

struct Reply {
    status: StatusCode,
    body: Value,
}

async fn warm(app: &Router) -> Reply {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/models/warmup")
                .header("content-type", "application/json")
                .body(Body::from(json!({"model": "idoris/daily"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn engine_on(listener: std::net::TcpListener) -> MockServer {
    let server = MockServer::builder().listener(listener).start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .mount(&server)
        .await;
    server
}

fn authorization(request: &wiremock::Request) -> Option<String> {
    request
        .headers
        .get("authorization")
        .map(|value| value.to_str().unwrap().to_owned())
}

#[tokio::test]
async fn a_resident_warmup_whose_engine_is_blocked_sends_nothing() {
    // 状态检查读到「在服务」之后、发送之前，端点被封上了（引擎正在被停止、替换）：一个字节都不发。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = engine_on(listener).await;
    let _governed = Governed(uri.clone());
    block_endpoint(&uri);
    let reply = warm(&app(&uri)).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.body
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

/// 预热的最小请求第一次发送连接被拒（端口上还没有引擎），等重试的时候引擎被换了、端口上起来了新引擎：重试**不能**把请求（和新引擎
/// 的 key）发给它。对照：引擎没换，同一个重试正常到达、带的是准入时那一代的 key。
#[tokio::test]
async fn a_resident_warmup_task_is_never_retried_onto_a_replacement_engine_but_is_without_one() {
    for replaced in [false, true] {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let uri = format!("http://127.0.0.1:{port}");
        let _governed = Governed(uri.clone());
        govern_endpoint(&uri, EndpointKey::Private("key-of-the-old-engine".into()));

        let app = app(&uri);
        let warmup = tokio::spawn(async move { warm(&app).await });
        // 第一次发送已经失败、在等代理的重试（250 毫秒后）：这时端口上起来了一个引擎。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let server = engine_on(std::net::TcpListener::bind(("127.0.0.1", port)).unwrap()).await;
        if replaced {
            block_endpoint(&uri);
            govern_endpoint(&uri, EndpointKey::Private("key-of-the-new-engine".into()));
        }
        let reply = warmup.await.unwrap();
        let requests = server.received_requests().await.unwrap();
        if replaced {
            assert_eq!(
                reply.status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{}",
                reply.body
            );
            assert_eq!(
                requests.len(),
                0,
                "the minimal request was not sent to the engine that replaced the one it was admitted for"
            );
        } else {
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
            assert_eq!(requests.len(), 1);
            assert_eq!(
                authorization(&requests[0]).as_deref(),
                Some("Bearer key-of-the-old-engine")
            );
        }
    }
}

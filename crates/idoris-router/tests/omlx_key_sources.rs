//! 接口规范 §2.9「oMLX 密钥」在 iDoris 连接 oMLX 的路径上的行为：
//! 环境变量 → `~/.omlx/settings.json` 的 `auth.api_key` → 无鉴权；两处都没有而 oMLX 返回 401 时，
//! 原样映射为带 remediation 的 502、不重试、key 不出现在响应和进程输出里。
//!
//! key 来自进程环境，所以每个用例在子进程里跑（同 `resident_omlx_key.rs`），HOME 指向临时假目录。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/state_isolation.rs"]
mod state_isolation;
use state_isolation::IsolateState;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_contracts::{ComponentCard, load_policy::LoadMode};
use idoris_router::{AppState, build_app};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "IDORIS_OMLX_KEY_SOURCES_CASE";
const HOME_ENV: &str = "IDORIS_OMLX_KEY_SOURCES_HOME";
const KEY_ENV: &str = "IDORIS_OMLX_API_KEY";
const FILE_KEY: &str = "canary-file-key-a1b2";
const ENV_KEY: &str = "canary-env-key-c3d4";
const FILE_JUNK: &str = "canary-settings-junk-e5f6";
const UPSTREAM_TEXT: &str = "upstream-auth-text-g7h8";

fn card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = endpoint.to_string();
    card.provider.id = "omlx".to_string();
    card.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"daily": "Qwen3-0.6B-4bit", "fast": "Qwen3-0.6B-4bit"}
        }))
        .unwrap(),
    );
    card
}

fn chat_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("X-iDoris-Privacy", "local_only")
        .body(Body::from(
            json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "hi"}]})
                .to_string(),
        ))
        .unwrap()
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// 只接受 `Bearer <accepted>` 的 oMLX；accepted 为 `None` 时一律 401。
async fn backend(accepted: Option<&str>) -> MockServer {
    let server = MockServer::start().await;
    if let Some(key) = accepted {
        let bearer = format!("Bearer {key}");
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", bearer.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "Qwen3-0.6B-4bit",
                "choices": [{"message": {"content": "answer"}}]
            })))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", bearer.as_str()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"id": "Qwen3-0.6B-4bit"}]})),
            )
            .with_priority(1)
            .mount(&server)
            .await;
    }
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": UPSTREAM_TEXT, "type": "authentication_error"}
        })))
        .with_priority(5)
        .mount(&server)
        .await;
    server
}

fn assert_remediation_without_secrets(body: &str) {
    let json: Value = serde_json::from_str(body).unwrap();
    let remediation = json["error"]["remediation"].as_str().unwrap_or_default();
    assert!(remediation.contains("IDORIS_OMLX_API_KEY"), "{body}");
    assert!(remediation.contains("settings.json"), "{body}");
    assert!(remediation.contains("auth.api_key"), "{body}");
    for secret in [FILE_KEY, ENV_KEY, FILE_JUNK, UPSTREAM_TEXT] {
        assert!(!body.contains(secret), "{secret} leaked: {body}");
    }
}

async fn run_case(case: &str) {
    let accepted = match case {
        "file_key" => Some(FILE_KEY),
        "env_wins" => Some(ENV_KEY),
        _ => None,
    };
    let upstream = backend(accepted).await;
    let app = build_app(AppState {
        cards: vec![card(&upstream.uri())],
        ..AppState::default()
    });
    match case {
        "file_key" | "env_wins" => {
            let (status, body) = send(&app, chat_request()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (status, body) = send(
                &app,
                Request::get("/v1/models").body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            // 另一个来源的 key 绝不会被发出去。
            let other = if case == "file_key" {
                ENV_KEY
            } else {
                FILE_KEY
            };
            for request in upstream.received_requests().await.unwrap() {
                let auth = request
                    .headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string())
                    .unwrap_or_default();
                assert!(!auth.contains(other), "{auth}");
            }
        }
        // 文件持续读不出完整内容（两次读都不是合法 JSON）：key 来源无法核验，拒绝发送（#646）：本地 503，没有任何请求到达上游，
        // 也不会退成匿名连接。
        "bad_file" => {
            let (status, body) = send(&app, chat_request()).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
            let json: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(json["error"]["type"], "upstream_unavailable");
            assert!(!body.contains(FILE_JUNK), "{body}");
            let (status, body) = send(
                &app,
                Request::get("/v1/models").body(Body::empty()).unwrap(),
            )
            .await;
            // 模型列表在上游不可用时是降级的空列表（既有行为），关键是它同样没有向上游发出任何请求。
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["data"],
                json!([]),
                "{body}"
            );
            assert!(
                upstream.received_requests().await.unwrap().is_empty(),
                "没有核验过的凭证来源，不向上游发任何请求"
            );
        }
        // 两处都没有 / 文件里的 key 被拒：401 → 502 + remediation，只请求一次，不重试。
        "none" | "rejected_file_key" => {
            let (status, body) = send(&app, chat_request()).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            let json: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(json["error"]["type"], "upstream_auth_failed");
            assert_remediation_without_secrets(&body);
            assert_eq!(
                upstream.received_requests().await.unwrap().len(),
                1,
                "401 不重试"
            );
            let (status, body) = send(
                &app,
                Request::get("/v1/models").body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            let json: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                json["error"]["reason_code"],
                "upstream_authentication_failed"
            );
            assert_remediation_without_secrets(&body);
            assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
            let sent: Vec<_> = upstream
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|r| r.headers.contains_key("authorization"))
                .collect();
            match case {
                "rejected_file_key" => assert_eq!(sent, [true, true]),
                _ => assert_eq!(sent, [false, false], "没有 key 时按无鉴权连接"),
            }
        }
        other => panic!("unknown case {other}"),
    }
}

#[tokio::test]
async fn omlx_key_sources_on_the_connection_paths() {
    if let Ok(case) = std::env::var(CHILD) {
        run_case(&case).await;
        return;
    }
    // (用例, 环境变量里的 key, settings.json 内容)
    let cases: [(&str, Option<&str>, Option<String>); 6] = [
        (
            "file_key",
            None,
            Some(format!(r#"{{"auth":{{"api_key":"{FILE_KEY}"}}}}"#)),
        ),
        (
            "env_wins",
            Some(ENV_KEY),
            Some(format!(r#"{{"auth":{{"api_key":"{FILE_KEY}"}}}}"#)),
        ),
        // 环境变量是空字符串：按没设，往下读文件（用例名沿用 file_key 的期望）。
        (
            "file_key",
            Some(""),
            Some(format!(r#"{{"auth":{{"api_key":"{FILE_KEY}"}}}}"#)),
        ),
        ("none", None, None),
        ("bad_file", None, Some(format!("not json {FILE_JUNK}"))),
        (
            "rejected_file_key",
            None,
            Some(format!(r#"{{"auth":{{"api_key":"{FILE_KEY}"}}}}"#)),
        ),
    ];
    for (case, env_key, settings) in cases {
        let home = std::env::temp_dir().join(format!(
            "idoris-omlx-key-sources-{}-{case}-{}",
            std::process::id(),
            env_key.map_or("noenv", |k| if k.is_empty() { "emptyenv" } else { "env" })
        ));
        std::fs::create_dir_all(home.join(".omlx")).unwrap();
        if let Some(settings) = &settings {
            std::fs::write(home.join(".omlx/settings.json"), settings).unwrap();
        }
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "omlx_key_sources_on_the_connection_paths",
            "--nocapture",
        ])
        .env(CHILD, case)
        .env(HOME_ENV, &home)
        .env("HOME", &home)
        .env_remove(KEY_ENV);
        if let Some(key) = env_key {
            cmd.env(KEY_ENV, key);
        }
        let output = cmd.isolate_state().output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_dir_all(&home);
        assert!(
            output.status.success(),
            "case {case}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        // key 与 settings.json 里的杂质从不进日志。
        for secret in [FILE_KEY, ENV_KEY, FILE_JUNK] {
            assert!(
                !stdout.contains(secret) && !stderr.contains(secret),
                "case {case}: {secret} leaked"
            );
        }
    }
}

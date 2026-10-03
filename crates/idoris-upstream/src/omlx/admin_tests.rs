#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_backend::Pressure;
use reqwest::Method;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{OmlxAdapter, OmlxAdapterConfig};

const KEY: &str = "main-key-must-not-leak";
const COOKIE: &str = "omlx_admin_session=session-must-not-leak";
const ACTIVITY: &str = "/admin/api/activity";
const SETTINGS: &str = "/admin/api/models/qwen3-8b/settings";

fn adapter(server: &MockServer) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        api_key: Some(KEY.into()),
        call_timeout: std::time::Duration::from_millis(100),
    })
    .unwrap()
}

fn login_response(cookie: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("set-cookie", format!("{cookie}; Path=/admin; HttpOnly"))
        .set_body_json(json!({"success": true}))
}

async fn login(server: &MockServer, response: ResponseTemplate, count: u64) {
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .and(body_json(json!({"api_key": KEY})))
        .respond_with(response)
        .up_to_n_times(count)
        .expect(count)
        .mount(server)
        .await;
}

async fn legacy_status(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "loaded_models": ["qwen3-8b"], "model_memory_max": 0,
            "model_memory_used": 0, "pressure": "soft"
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn login_is_lazy_and_cached_across_concurrent_admin_requests() {
    let server = MockServer::start().await;
    login(&server, login_response(COOKIE), 1).await;
    Mock::given(method("GET"))
        .and(path(ACTIVITY))
        .and(header("cookie", COOKIE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(2)
        .mount(&server)
        .await;
    let adapter = adapter(&server);
    assert!(server.received_requests().await.unwrap().is_empty());
    let (first, second) = tokio::join!(
        adapter.admin_request(Method::GET, ACTIVITY, None),
        adapter.admin_request(Method::GET, ACTIVITY, None)
    );
    first.unwrap();
    second.unwrap();
    server.verify().await;
}

#[tokio::test]
async fn login_rejection_has_no_retry_and_never_exposes_key_or_cookie() {
    let server = MockServer::start().await;
    login(
        &server,
        ResponseTemplate::new(401).set_body_string(format!("{KEY} {COOKIE}")),
        1,
    )
    .await;
    let err = adapter(&server)
        .admin_request(Method::PUT, SETTINGS, Some(&json!({"is_pinned": true})))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("401"));
    assert!(!format!("{err:?} {err}").contains(KEY));
    assert!(!format!("{err:?} {err}").contains(COOKIE));
    server.verify().await;
}

#[tokio::test]
async fn expired_session_relogs_once_and_retries_with_the_new_cookie() {
    let server = MockServer::start().await;
    login(&server, login_response(COOKIE), 1).await;
    Mock::given(method("GET"))
        .and(path(ACTIVITY))
        .and(header("cookie", COOKIE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    let adapter = adapter(&server);
    adapter
        .admin_request(Method::GET, ACTIVITY, None)
        .await
        .unwrap();
    // Expire the already cached session, then return a different cookie.
    Mock::given(method("GET"))
        .and(path(ACTIVITY))
        .and(header("cookie", COOKIE))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    login(&server, login_response("omlx_admin_session=fresh"), 1).await;
    Mock::given(method("GET"))
        .and(path(ACTIVITY))
        .and(header("cookie", "omlx_admin_session=fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"fresh": true})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        adapter
            .admin_request(Method::GET, ACTIVITY, None)
            .await
            .unwrap()["fresh"],
        true
    );
    server.verify().await;
}

#[tokio::test]
async fn repeated_admin_401_stops_after_one_relogin_and_one_retry() {
    let server = MockServer::start().await;
    login(&server, login_response(COOKIE), 2).await;
    Mock::given(method("PUT"))
        .and(path(SETTINGS))
        .and(header("cookie", COOKIE))
        .and(body_json(json!({"is_pinned": true})))
        .respond_with(ResponseTemplate::new(401).set_body_string(format!("{KEY} {COOKIE}")))
        .expect(2)
        .mount(&server)
        .await;
    let adapter = adapter(&server);
    let err = adapter.pin("qwen3-8b").await.unwrap_err();
    assert!(!err.to_string().contains(KEY));
    assert!(!err.to_string().contains(COOKIE));
    assert!(adapter.admin_session.lock().await.is_none());
    server.verify().await;
}

#[tokio::test]
async fn pin_uses_cookie_and_requires_readonly_verification() {
    for (index, verify) in [
        ResponseTemplate::new(200)
            .set_body_json(json!({"models": [{"id": "qwen3-8b", "loaded": true, "pinned": true}]})),
        ResponseTemplate::new(200).set_body_json(
            json!({"models": [{"id": "qwen3-8b", "loaded": true, "pinned": false}]}),
        ),
        ResponseTemplate::new(503).set_body_string(format!("{KEY} {COOKIE}")),
    ]
    .into_iter()
    .enumerate()
    {
        let expected_success = index == 0;
        let server = MockServer::start().await;
        login(&server, login_response(COOKIE), 1).await;
        Mock::given(method("PUT"))
            .and(path(SETTINGS))
            .and(header("cookie", COOKIE))
            .and(body_json(json!({"is_pinned": true})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"settings": {"is_pinned": true}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models/status"))
            .and(header("authorization", format!("Bearer {KEY}")))
            .respond_with(verify)
            .expect(1)
            .mount(&server)
            .await;
        let result = adapter(&server).pin("qwen3-8b").await;
        assert_eq!(result.is_ok(), expected_success);
        if !expected_success {
            let err = result.unwrap_err();
            assert!(!err.to_string().contains(KEY));
            assert!(!err.to_string().contains(COOKIE));
        }
        server.verify().await;
    }
}

#[tokio::test]
async fn activity_pressure_overrides_legacy_for_all_tiers_and_fails_closed() {
    for (enabled, level, expected) in [
        (json!(true), json!("ok"), Pressure::Ok),
        (json!(true), json!("soft"), Pressure::Soft),
        (json!(true), json!("hard"), Pressure::Hard),
        (json!(true), json!("ceiling"), Pressure::Ceiling),
        (json!(true), json!("future-tier"), Pressure::Unknown),
        (json!(true), Value::Null, Pressure::Unknown),
        (Value::Null, json!("ok"), Pressure::Unknown),
        (json!(false), json!("ceiling"), Pressure::Ok),
        (json!(false), Value::Null, Pressure::Ok),
    ] {
        let server = MockServer::start().await;
        legacy_status(&server).await;
        login(&server, login_response(COOKIE), 1).await;
        Mock::given(method("GET"))
            .and(path(ACTIVITY))
            .and(header("cookie", COOKIE))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "active_models": {"memory_pressure": {"enabled": enabled, "pressure_level": level}}
            })))
            .mount(&server)
            .await;
        let result = adapter(&server).status().await.unwrap();
        assert_eq!(result.pressure, expected);
        assert_eq!(result.loaded, ["qwen3-8b"]);
    }
}

#[tokio::test]
async fn status_preserves_legacy_pressure_when_admin_access_fails() {
    for (login_response, activity_response) in [
        (ResponseTemplate::new(401), ResponseTemplate::new(200)),
        (login_response(COOKIE), ResponseTemplate::new(503)),
        (
            login_response(COOKIE),
            ResponseTemplate::new(200).set_body_string("invalid-json"),
        ),
        (
            login_response(COOKIE),
            ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(250)),
        ),
    ] {
        let server = MockServer::start().await;
        legacy_status(&server).await;
        login(&server, login_response, 1).await;
        Mock::given(method("GET"))
            .and(path(ACTIVITY))
            .respond_with(activity_response)
            .mount(&server)
            .await;
        assert_eq!(
            adapter(&server).status().await.unwrap().pressure,
            Pressure::Soft
        );
    }
    let server = MockServer::start().await;
    legacy_status(&server).await;
    let mut adapter = adapter(&server);
    adapter.api_key = None;
    assert_eq!(adapter.status().await.unwrap().pressure, Pressure::Soft);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn login_requires_success_and_a_nonempty_session_cookie() {
    for response in [
        ResponseTemplate::new(200).set_body_json(json!({"success": true})),
        login_response("omlx_admin_session="),
        login_response(COOKIE).set_body_json(json!({"success": false, "error": KEY})),
        login_response(COOKIE).set_body_string(format!("{KEY} {COOKIE}")),
        login_response(COOKIE).set_delay(std::time::Duration::from_millis(250)),
    ] {
        let server = MockServer::start().await;
        login(&server, response, 1).await;
        let err = adapter(&server)
            .admin_request(Method::GET, ACTIVITY, None)
            .await
            .unwrap_err();
        assert!(!err.to_string().contains(KEY));
        assert!(!err.to_string().contains(COOKIE));
    }
}

#[test]
fn config_debug_redacts_the_api_key() {
    let config = OmlxAdapterConfig {
        api_key: Some(KEY.into()),
        ..Default::default()
    };
    assert!(!format!("{config:?}").contains(KEY));
}

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

// ---------------------------------------------------------------------------------------------
// IDORIS-CORE S6，Codex 终审 H4：管理员 cookie 属于签发它的那一代引擎
// ---------------------------------------------------------------------------------------------

use idoris_backend::generation::scoped;

use super::key::{
    EndpointKey, block_endpoint, endpoint_generation, forget_endpoint, govern_endpoint,
};

const FIRST_KEY: &str = "first-engine-key-must-not-leak";
const SECOND_KEY: &str = "second-engine-key-must-not-leak";
const FIRST_COOKIE: &str = "omlx_admin_session=first-engine-session-must-not-leak";
const SECOND_COOKIE: &str = "omlx_admin_session=second-engine-session-must-not-leak";

/// 登记表是进程全局的：结束时把这个端点从登记表里整条删掉（代际也回到 0）。
struct Governed(String);

impl Drop for Governed {
    fn drop(&mut self) {
        forget_endpoint(&self.0);
    }
}

/// 一个按请求取 key 的适配器，对着引擎管理器登记过的端点（登记的是私有 key `key`）。
fn per_request_adapter(server: &MockServer) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        api_key: None,
        call_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap()
    .with_per_request_key()
}

async fn mock_engine_login(server: &MockServer, key: &str, cookie: &str) {
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .and(body_json(json!({"api_key": key})))
        .respond_with(login_response(cookie))
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(path(SETTINGS))
        .and(header("cookie", cookie))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"settings": {"is_pinned": true}})),
        )
        .mount(server)
        .await;
}

/// 发给这个服务器、带着 `cookie` 的请求数。
async fn requests_carrying(server: &MockServer, cookie: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| {
            request
                .headers
                .get("cookie")
                .is_some_and(|value| value.to_str().unwrap() == cookie)
        })
        .count()
}

/// 已有 cookie、引擎被换掉（端点封上）之后，旧引擎上还在做的操作（resident 的加载接着去 pin）在发送**之前**就被拒绝：cookie 一个字节也没有送出去。
/// 之前缓存命中的请求直接带着旧 cookie 发出去，旧 load 可以跨过 block、对着此刻监听的进程发一个改 pin 状态的 `PUT`。
#[tokio::test]
async fn an_old_operation_with_a_cached_cookie_is_refused_after_the_engine_was_replaced_and_sends_nothing()
 {
    let server = MockServer::start().await;
    let _governed = Governed(server.uri());
    govern_endpoint(&server.uri(), EndpointKey::Private(FIRST_KEY.into()));
    mock_engine_login(&server, FIRST_KEY, FIRST_COOKIE).await;
    let adapter = per_request_adapter(&server);
    let admitted = endpoint_generation(&server.uri());

    // The old operation had logged in under the first engine (the cookie is cached with that generation).
    scoped(
        Some(admitted),
        adapter.admin_request(Method::PUT, SETTINGS, Some(&json!({"is_pinned": true}))),
    )
    .await
    .unwrap();
    assert_eq!(requests_carrying(&server, FIRST_COOKIE).await, 1);
    let before = server.received_requests().await.unwrap().len();

    // The engine is replaced: the endpoint is blocked. The old operation goes on to pin (a second admin send, cached cookie).
    block_endpoint(&server.uri());
    let refused = scoped(
        Some(admitted),
        adapter.admin_request(Method::PUT, SETTINGS, Some(&json!({"is_pinned": true}))),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        before,
        "the cached cookie was not sent: nothing at all went out"
    );

    // Even after a new engine is registered, work admitted for the old one stays refused (it is a different generation).
    govern_endpoint(&server.uri(), EndpointKey::Private(SECOND_KEY.into()));
    let refused = scoped(
        Some(admitted),
        adapter.admin_request(Method::PUT, SETTINGS, Some(&json!({"is_pinned": true}))),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

/// 换代之后的**第一次** Admin 调用：缓存里是旧引擎签发的 cookie，不能送给新进程；丢掉它、对着新引擎用新 key 重新登录。
#[tokio::test]
async fn the_first_admin_call_after_a_replacement_logs_in_again_instead_of_sending_the_old_cookie()
{
    let server = MockServer::start().await;
    let _governed = Governed(server.uri());
    govern_endpoint(&server.uri(), EndpointKey::Private(FIRST_KEY.into()));
    mock_engine_login(&server, FIRST_KEY, FIRST_COOKIE).await;
    mock_engine_login(&server, SECOND_KEY, SECOND_COOKIE).await;
    let adapter = per_request_adapter(&server);
    adapter
        .admin_request(Method::PUT, SETTINGS, Some(&json!({"is_pinned": true})))
        .await
        .unwrap();
    assert_eq!(requests_carrying(&server, FIRST_COOKIE).await, 1);

    // The server is now the second engine: blocked, then registered under a new key and a new generation.
    block_endpoint(&server.uri());
    govern_endpoint(&server.uri(), EndpointKey::Private(SECOND_KEY.into()));
    let before = requests_carrying(&server, FIRST_COOKIE).await;
    adapter
        .admin_request(Method::PUT, SETTINGS, Some(&json!({"is_pinned": true})))
        .await
        .unwrap();
    assert_eq!(
        requests_carrying(&server, FIRST_COOKIE).await,
        before,
        "the first engine's cookie went to the second engine"
    );
    assert_eq!(requests_carrying(&server, SECOND_COOKIE).await, 1);
    // The first engine's key was never offered to the second one either: every login after the replacement used the new key.
    let logins: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/admin/api/login")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(logins.len(), 2);
    assert_eq!(logins[0]["api_key"], FIRST_KEY);
    assert_eq!(logins[1]["api_key"], SECOND_KEY);
}

/// 登录那一次往返期间引擎被换掉：登录得到的会话是上一代引擎签发的（它记着登录前授权的那一代）；紧接着的那个 Admin 请求在发送之前再授权一次，
/// 代际已经不一样，**拒绝**，不把刚得到的会话发给换上来的进程。假的引擎在「回答登录的那一刻」完成换代（登记表里换成新引擎的 key），
/// 所以换代恰好落在登录完成与随后的请求之间。之前登录完直接发。
#[tokio::test]
async fn an_engine_replaced_while_the_admin_session_was_being_established_gets_nothing_from_that_session()
 {
    let server = MockServer::start().await;
    let url = server.uri();
    let _governed = Governed(url.clone());
    govern_endpoint(&url, EndpointKey::Private(FIRST_KEY.into()));
    let replaced_when_answering = {
        let url = url.clone();
        move |_: &wiremock::Request| {
            govern_endpoint(&url, EndpointKey::Private(SECOND_KEY.into()));
            login_response(FIRST_COOKIE)
        }
    };
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .respond_with(replaced_when_answering)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(SETTINGS))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"settings": {"is_pinned": true}})),
        )
        .mount(&server)
        .await;
    let adapter = per_request_adapter(&server);

    let refused = adapter
        .admin_request(Method::PUT, SETTINGS, Some(&json!({"is_pinned": true})))
        .await;
    let error = refused.unwrap_err().to_string();
    assert!(error.contains("was replaced"), "{error}");
    assert_eq!(
        requests_carrying(&server, FIRST_COOKIE).await,
        0,
        "the session of the engine that was replaced was sent to the one that took over"
    );
    let paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect();
    assert_eq!(
        paths,
        vec!["/admin/api/login".to_owned()],
        "only the login went out"
    );
}

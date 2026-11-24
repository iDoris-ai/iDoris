//! Codex re-review of layer 2, M1': every send of the oMLX adapter takes its last look at the endpoint **after the request is built, right before
//! it is sent**. The authorization is obtained at an `await`; building the request (headers, the JSON body, a session cookie) comes after it,
//! and in between a restart can have replaced the engine. A barrier parks each request exactly there (between the build and that last look),
//! replaces the engine, and lets it go: the request must not be sent, and a refusal at that point is a **definite local refusal** — a load refused
//! there is not an unconfirmed load.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use idoris_backend::{BackendError, ChatMessage, ChatRequest};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::omlx::key::test_hooks::between_build_and_send_for;
use idoris_upstream::omlx::key::{EndpointKey, forget_endpoint, govern_endpoint};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Leaves the registry as the test found it, panics included.
struct Forget(String);

impl Drop for Forget {
    fn drop(&mut self) {
        forget_endpoint(&self.0);
    }
}

fn adapter(endpoint: &str) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: endpoint.to_owned(),
        api_key: Some("constructed".to_owned()),
        call_timeout: Duration::from_secs(5),
    })
    .unwrap()
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())
    .with_per_request_key()
}

/// An engine that answers everything an adapter operation sends, including a session login.
async fn engine() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", "omlx_admin_session=session-1; Path=/")
                .set_body_json(serde_json::json!({"success": true})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hi"}}]
        })))
        .mount(&server)
        .await;
    server
}

/// Governs the endpoint, then arranges for the engine to be replaced the moment a request whose URL starts with `url_prefix` has been built.
fn replace_the_engine_between_build_and_send(
    endpoint: &str,
    url_prefix: String,
) -> idoris_upstream::omlx::key::test_hooks::HookGuard {
    govern_endpoint(endpoint, EndpointKey::Private("first-engine".into()));
    let endpoint = endpoint.to_owned();
    between_build_and_send_for(&url_prefix, move || {
        govern_endpoint(&endpoint, EndpointKey::Private("replacement".into()));
    })
}

async fn paths_received(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect()
}

fn policy(mode: LoadMode) -> LoadPolicy {
    LoadPolicy {
        mode,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

fn chat() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "a prompt nobody else may read".into(),
        }],
        native: None,
    }
}

fn refused(error: &BackendError) {
    assert!(matches!(error, BackendError::Upstream { .. }), "{error:?}");
    assert!(error.to_string().contains("was replaced"), "{error}");
}

/// A read (`GET`): built for the first engine, refused when the engine was replaced before the send. Nothing reaches the engine.
#[tokio::test]
async fn a_read_built_for_a_replaced_engine_is_not_sent() {
    let server = engine().await;
    let endpoint = server.uri();
    let _forget = Forget(endpoint.clone());
    let adapter = adapter(&endpoint);
    let _hook = replace_the_engine_between_build_and_send(&endpoint, format!("{endpoint}/v1/"));
    refused(&adapter.list().await.unwrap_err());
    assert_eq!(paths_received(&server).await, Vec::<String>::new());
}

/// A chat (`POST` with a body, a prompt): refused before the prompt goes out.
#[tokio::test]
async fn a_chat_built_for_a_replaced_engine_is_not_sent() {
    let server = engine().await;
    let endpoint = server.uri();
    let _forget = Forget(endpoint.clone());
    let adapter = adapter(&endpoint);
    let _hook = replace_the_engine_between_build_and_send(&endpoint, format!("{endpoint}/v1/chat"));
    let error = adapter
        .chat(chat(), CancellationToken::new())
        .await
        .map(|_| ())
        .unwrap_err();
    refused(&error);
    assert_eq!(paths_received(&server).await, Vec::<String>::new());
}

/// An unload (a mutating `POST` with no body).
#[tokio::test]
async fn an_unload_built_for_a_replaced_engine_is_not_sent() {
    let server = engine().await;
    let endpoint = server.uri();
    let _forget = Forget(endpoint.clone());
    let adapter = adapter(&endpoint);
    let _hook = replace_the_engine_between_build_and_send(&endpoint, format!("{endpoint}/v1/"));
    refused(&adapter.unload("m").await.unwrap_err());
    assert_eq!(paths_received(&server).await, Vec::<String>::new());
}

/// A load mutates engine state, and a missing answer would make it an unconfirmed load. A load refused at the last look **was never sent**:
/// it is a definite local refusal, not `LoadUnconfirmed` (which would make the caller keep the load fence for a load nobody started).
#[tokio::test]
async fn a_load_refused_at_the_last_look_is_a_definite_refusal_not_an_unconfirmed_load() {
    let server = engine().await;
    let endpoint = server.uri();
    let _forget = Forget(endpoint.clone());
    let adapter = adapter(&endpoint);
    let _hook = replace_the_engine_between_build_and_send(&endpoint, format!("{endpoint}/v1/"));
    let error = adapter
        .load("m", Some(&policy(LoadMode::OnDemand)))
        .await
        .unwrap_err();
    refused(&error);
    assert!(
        !matches!(error, BackendError::LoadUnconfirmed { .. }),
        "{error:?}"
    );
    assert_eq!(paths_received(&server).await, Vec::<String>::new());
}

/// The Admin login: its body carries the **main key**. Refused at the last look, it is not sent.
#[tokio::test]
async fn an_admin_login_built_for_a_replaced_engine_is_not_sent() {
    let server = engine().await;
    let endpoint = server.uri();
    let _forget = Forget(endpoint.clone());
    let adapter = adapter(&endpoint);
    let _hook =
        replace_the_engine_between_build_and_send(&endpoint, format!("{endpoint}/admin/api/login"));
    // A resident load: the `POST` load goes out (it was built and checked before the engine was replaced...), then the pin needs a session.
    // The pin step is refused at the login (a definite refusal, see `admin_tests`), and the follow-up read of the loaded state is refused too (the operation
    // is pinned to the generation it began under): the load's own `POST` was answered, so the model may be resident: an **unconfirmed** load, never a success.
    let error = adapter
        .load("m", Some(&policy(LoadMode::Resident)))
        .await
        .unwrap_err();
    assert!(
        matches!(error, BackendError::LoadUnconfirmed { .. }),
        "{error:?}"
    );
    let received = paths_received(&server).await;
    assert!(
        !received
            .iter()
            .any(|line| line.contains("/admin/api/login")),
        "the main key went out in a login that was refused: {received:?}"
    );
}

/// The Admin request with the session cookie: refused at the last look (the engine was replaced after the session was established), it is not
/// sent — and it is a definite refusal, so a settings change refused here is not reported as an unknown outcome.
#[tokio::test]
async fn an_admin_request_built_for_a_replaced_engine_is_not_sent() {
    let server = engine().await;
    let endpoint = server.uri();
    let _forget = Forget(endpoint.clone());
    let adapter = adapter(&endpoint);
    let _hook = replace_the_engine_between_build_and_send(
        &endpoint,
        format!("{endpoint}/admin/api/models/"),
    );
    let error = adapter
        .load("m", Some(&policy(LoadMode::Resident)))
        .await
        .unwrap_err();
    assert!(
        matches!(error, BackendError::LoadUnconfirmed { .. }),
        "{error:?}"
    );
    let received = paths_received(&server).await;
    assert!(
        !received
            .iter()
            .any(|line| line.starts_with("PUT /admin/api/models/")),
        "the settings change went out although the engine had been replaced: {received:?}"
    );
}

/// The control: the same operations with nothing replacing the engine all go out (so the empty request lists above mean "refused", not "never
/// attempted").
#[tokio::test]
async fn without_a_replacement_the_same_requests_go_out() {
    let server = engine().await;
    let endpoint = server.uri();
    let _forget = Forget(endpoint.clone());
    govern_endpoint(&endpoint, EndpointKey::Private("first-engine".into()));
    let adapter = adapter(&endpoint);
    adapter.list().await.unwrap();
    adapter.unload("m").await.unwrap();
    adapter
        .chat(chat(), CancellationToken::new())
        .await
        .map(|_| ())
        .unwrap();
    assert_eq!(paths_received(&server).await.len(), 3);
}

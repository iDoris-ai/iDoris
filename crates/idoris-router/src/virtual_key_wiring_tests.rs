#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;
use serde_json::Value;
use tower::ServiceExt;
use wiremock::matchers::{method, path};

use super::*;

fn scope(
    privacy: Vec<PrivacyClass>,
    roles: &[&str],
    expires_at_ms: Option<i64>,
) -> VirtualKeyScope {
    VirtualKeyScope {
        owner: "agent24-instance".into(),
        allowed_privacy: privacy,
        allowed_roles: roles.iter().map(|role| (*role).to_string()).collect(),
        budget_ref: None,
        expires_at_ms,
        admin_scopes: Vec::new(),
    }
}

fn memory_auth(
    key: &MintedVirtualKey,
    key_scope: &VirtualKeyScope,
) -> (auth::VirtualKeyAuthenticator, Arc<Mutex<VirtualKeyStore>>) {
    let store = Arc::new(Mutex::new(
        VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    store
        .lock()
        .unwrap()
        .insert_active(&key.key_id, key.hash, key_scope)
        .unwrap();
    (auth::VirtualKeyAuthenticator::new(store.clone()), store)
}

fn resident_card(endpoint: &str) -> ComponentCard {
    ComponentCard {
        provider: ProviderDescriptor {
            id: "auth-boundary".into(),
            family: Family::Local,
            tier: Tier::Local,
            capabilities: vec![Capability::Chat],
            privacy_class: PrivacyClass::LocalOnly,
            cost: Cost {
                input_per_m: 0.0,
                output_per_m: 0.0,
            },
            locality: Locality::Loopback,
            extensions: None,
        },
        form: Form::HttpService,
        endpoint: endpoint.to_string(),
        version_pin: "test".into(),
        privacy_class: PrivacyClass::LocalOnly,
        allowed_egress: vec![Egress::Loopback],
        fallback_policy: FallbackPolicy::FailClosed,
        fail_closed: true,
        load_policy: Some(LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        }),
        extensions: None,
    }
}

fn chat_request(headers: &[(&str, String)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    builder
        .body(Body::from(
            r#"{"model":"idoris/fast","messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .unwrap()
}

fn bearer(key: &MintedVirtualKey) -> String {
    format!("Bearer {}", key.secret.expose_secret())
}

fn state(endpoint: &str, authenticator: Option<auth::VirtualKeyAuthenticator>) -> AppState {
    AppState {
        cards: vec![resident_card(endpoint)],
        virtual_key_authenticator: authenticator,
        ..AppState::default()
    }
}

async fn json_body(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn valid_key_reaches_execution_boundary_and_none_authenticator_preserves_legacy_behavior() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})),
        )
        .expect(2)
        .mount(&server)
        .await;

    let key = MintedVirtualKey::mint();
    let (authenticator, _store) =
        memory_auth(&key, &scope(vec![PrivacyClass::LocalOnly], &["fast"], None));
    let authenticated = build_app(state(&server.uri(), Some(authenticator)))
        .oneshot(chat_request(&[("authorization", bearer(&key))]))
        .await
        .unwrap();
    assert_eq!(authenticated.status(), StatusCode::OK);

    let legacy = build_app(state(&server.uri(), None))
        .oneshot(chat_request(&[]))
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::OK);
    server.verify().await;
}

#[tokio::test]
async fn all_auth_failures_are_the_same_401_and_never_reach_upstream() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let good_scope = scope(vec![PrivacyClass::LocalOnly], &["fast"], None);

    let active = MintedVirtualKey::mint();
    let (active_auth, _active_store) = memory_auth(&active, &good_scope);
    let wrong = MintedVirtualKey::mint();

    let expired = MintedVirtualKey::mint();
    let (expired_auth, _expired_store) = memory_auth(
        &expired,
        &scope(vec![PrivacyClass::LocalOnly], &["fast"], Some(1)),
    );

    let revoked = MintedVirtualKey::mint();
    let (revoked_auth, revoked_store) = memory_auth(&revoked, &good_scope);
    revoked_store
        .lock()
        .unwrap()
        .revoke(&revoked.key_id, 1)
        .unwrap();

    let poisoned = MintedVirtualKey::mint();
    let (poisoned_auth, poisoned_store) = memory_auth(&poisoned, &good_scope);
    let poison_target = poisoned_store.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poison_target.lock().unwrap();
        panic!("poison virtual-key mutex");
    })
    .join();

    let busy_dir = tempfile::TempDir::new().unwrap();
    let busy_db = busy_dir.path().join("busy.sqlite3");
    let busy_key = MintedVirtualKey::mint();
    let busy_store = Arc::new(Mutex::new(VirtualKeyStore::open(&busy_db).unwrap()));
    busy_store
        .lock()
        .unwrap()
        .insert_active(&busy_key.key_id, busy_key.hash, &good_scope)
        .unwrap();
    let busy_auth = auth::VirtualKeyAuthenticator::new(busy_store);
    let blocker = Connection::open(&busy_db).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let cases = [
        (active_auth.clone(), Vec::new()),
        (
            active_auth,
            vec![("authorization", "Bearer malformed".into())],
        ),
        (
            auth::VirtualKeyAuthenticator::new(_active_store.clone()),
            vec![("authorization", bearer(&wrong))],
        ),
        (expired_auth, vec![("authorization", bearer(&expired))]),
        (revoked_auth, vec![("authorization", bearer(&revoked))]),
        (poisoned_auth, vec![("authorization", bearer(&poisoned))]),
        (busy_auth, vec![("authorization", bearer(&busy_key))]),
    ];

    let mut expected = None;
    for (authenticator, headers) in cases {
        let response = build_app(state(&server.uri(), Some(authenticator)))
            .oneshot(chat_request(&headers))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        assert_eq!(body["error"]["reason_code"], "VIRTUAL_KEY_UNAUTHORIZED");
        assert_eq!(
            body["error"]["remediation"],
            "virtual key authentication failed"
        );
        assert_eq!(expected.get_or_insert_with(|| body.clone()), &body);
        let rendered = body.to_string();
        assert!(!rendered.contains("idk_"));
        assert!(!rendered.contains("sqlite"));
        assert!(!rendered.contains("poison"));
    }
    blocker.execute_batch("ROLLBACK").unwrap();
    server.verify().await;
}

#[tokio::test]
async fn scope_and_fallback_fail_403_before_any_upstream_call() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let key = MintedVirtualKey::mint();
    let (authenticator, _store) =
        memory_auth(&key, &scope(vec![PrivacyClass::LocalOnly], &["fast"], None));
    let token = bearer(&key);

    let cases = [
        (
            vec![
                ("authorization", token.clone()),
                ("x-idoris-privacy", "any".into()),
            ],
            "VIRTUAL_KEY_PRIVACY_FORBIDDEN",
        ),
        (
            vec![("authorization", token.clone())],
            "VIRTUAL_KEY_ROLE_FORBIDDEN",
        ),
        (
            vec![
                ("authorization", token),
                ("x-idoris-fallback", "next_in_chain".into()),
            ],
            "VIRTUAL_KEY_FALLBACK_FORBIDDEN",
        ),
    ];

    for (headers, reason) in cases {
        let mut request = chat_request(&headers);
        if reason == "VIRTUAL_KEY_ROLE_FORBIDDEN" {
            *request.body_mut() = Body::from(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello"}]}"#,
            );
        }
        let response = build_app(state(&server.uri(), Some(authenticator.clone())))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "policy_violation");
        assert_eq!(body["error"]["reason_code"], reason);
    }
    server.verify().await;
}

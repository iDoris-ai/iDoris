#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        tenant_id: None,
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

fn dev_state(endpoint: &str, authenticator: auth::VirtualKeyAuthenticator) -> AppState {
    let mut state = state(endpoint, Some(authenticator));
    state.dev_no_key_enabled = true;
    state
}

async fn json_body(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn wall_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

#[tokio::test]
async fn explicitly_personal_key_accepts_its_scope_and_rejects_foreign_or_duplicate_headers() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
        .expect(2)
        .mount(&server)
        .await;
    let key = MintedVirtualKey::mint();
    let mut key_scope = scope(vec![PrivacyClass::LocalOnly], &["fast"], None);
    key_scope.tenant_id = Some(budget::PERSONAL_TENANT_ID.into());
    let (authenticator, _) = memory_auth(&key, &key_scope);
    let app = build_app(state(&server.uri(), Some(authenticator)));
    for tenant in [None, Some(budget::PERSONAL_TENANT_ID)] {
        let mut headers = vec![("authorization", bearer(&key))];
        if let Some(tenant) = tenant {
            headers.push(("x-idoris-tenant", tenant.into()));
        }
        let response = app.clone().oneshot(chat_request(&headers)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    for tenants in [vec!["tenant-b"], vec![""], vec!["personal", "personal"]] {
        let mut headers = vec![("authorization", bearer(&key))];
        headers.extend(
            tenants
                .into_iter()
                .map(|tenant| ("x-idoris-tenant", tenant.into())),
        );
        let response = app.clone().oneshot(chat_request(&headers)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            json_body(response).await["error"]["reason_code"],
            "VIRTUAL_KEY_TENANT_FORBIDDEN"
        );
    }
    server.verify().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn personal_deployment_does_not_promote_a_tenant_bound_key() {
    let key = MintedVirtualKey::mint();
    let mut key_scope = scope(vec![PrivacyClass::LocalOnly], &["fast"], None);
    key_scope.tenant_id = Some("tenant-a".into());
    let (authenticator, _) = memory_auth(&key, &key_scope);
    let log = Arc::new(
        idoris_tenancy::event_log::EventLogStore::new(Connection::open_in_memory().unwrap())
            .unwrap(),
    );
    log.append(
        Some("tenant-b"),
        &idoris_tenancy::event_log::NewEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            tenant_id: "tenant-b".into(),
            record_id: "private-record".into(),
            event_type: idoris_tenancy::event_log::EventType::RequestReceived,
            ts_utc_ms: 10,
            request_id: None,
            session_id: None,
            trace_id: None,
            parent_id: None,
            origin_record_id: None,
            metadata: Default::default(),
        },
    )
    .unwrap();
    let app = build_app(AppState {
        deploy_mode: idoris_contracts::DeployMode::Personal,
        virtual_key_authenticator: Some(authenticator),
        event_log: Some(log),
        ..AppState::default()
    });
    let response = app
        .oneshot(
            Request::builder()
                .uri("/idoris/tenants/tenant-b/requests/private-record")
                .header("authorization", bearer(&key))
                .header("x-idoris-tenant", "tenant-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = json_body(response).await;
    assert_eq!(body["error"]["reason_code"], "VIRTUAL_KEY_TENANT_FORBIDDEN");
    assert!(!body.to_string().contains("private-record"));
}

#[tokio::test]
async fn revoked_tenant_key_cannot_replay_warmed_inference_caches() {
    for (endpoint, capability, body) in [
        (
            "/v1/chat/completions",
            Capability::Chat,
            json!({"model":"idoris/fast","messages":[{"role":"user","content":"hello"}]}),
        ),
        (
            "/v1/embeddings",
            Capability::Embedding,
            json!({"model":"m","input":["hello"]}),
        ),
        (
            "/v1/rerank",
            Capability::Rerank,
            json!({"model":"m","query":"q","documents":["a"]}),
        ),
        (
            "/v1/messages",
            Capability::Chat,
            json!({"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}),
        ),
    ] {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
            .expect(1)
            .mount(&server)
            .await;
        let key = MintedVirtualKey::mint();
        let roles = idoris_policy::ROLES
            .iter()
            .map(|role| role.as_str())
            .collect::<Vec<_>>();
        let mut key_scope = scope(vec![PrivacyClass::LocalOnly], &roles, None);
        key_scope.tenant_id = Some("tenant-a".into());
        let (authenticator, keys) = memory_auth(&key, &key_scope);
        let log = Arc::new(
            idoris_tenancy::event_log::EventLogStore::new(Connection::open_in_memory().unwrap())
                .unwrap(),
        );
        let records = Arc::new(Mutex::new(
            idoris_tenancy::store::TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
        ));
        let mut protected = state(&server.uri(), Some(authenticator));
        protected.cards[0].provider.capabilities = vec![capability];
        protected.deploy_mode = idoris_contracts::DeployMode::Tenant;
        protected.event_log = Some(log.clone());
        protected.record_store = Some(records.clone());
        let app = build_app(protected);
        let request = || {
            Request::builder()
                .method("POST")
                .uri(endpoint)
                .header("content-type", "application/json")
                .header("authorization", bearer(&key))
                .header("x-idoris-tenant", "tenant-a")
                .header(HEADER_REQUEST_ID, "revoked-cache")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let first = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK, "{endpoint}");
        assert!(!first.headers().contains_key(HEADER_CACHED));
        assert_eq!(json_body(first).await, json!({"ok":true}));
        let warmed = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(warmed.status(), StatusCode::OK, "{endpoint}");
        assert!(warmed.headers().contains_key(HEADER_CACHED), "{endpoint}");
        assert_eq!(json_body(warmed).await, json!({"ok":true}));
        let query = idoris_tenancy::event_log::EventLogQuery {
            limit: 1000,
            ..Default::default()
        };
        let events_before = log
            .events_for_tenant(Some("tenant-a"), &query)
            .unwrap()
            .len();
        assert!(events_before > 0 && events_before < query.limit);
        let records_before = records
            .lock()
            .unwrap()
            .list(Some("tenant-a"), None)
            .unwrap()
            .len();
        assert!(records_before > 0);
        assert!(
            keys.lock()
                .unwrap()
                .revoke(&key.key_id, wall_clock_ms())
                .unwrap()
        );
        let denied = app.oneshot(request()).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "{endpoint}");
        assert!(!denied.headers().contains_key(HEADER_CACHED));
        assert_eq!(
            json_body(denied).await["error"]["reason_code"],
            "VIRTUAL_KEY_UNAUTHORIZED"
        );
        assert_eq!(
            log.events_for_tenant(Some("tenant-a"), &query)
                .unwrap()
                .len(),
            events_before,
            "{endpoint}: no forged events"
        );
        assert_eq!(
            records
                .lock()
                .unwrap()
                .list(Some("tenant-a"), None)
                .unwrap()
                .len(),
            records_before,
            "{endpoint}: no new tenant records"
        );
        assert!(
            log.events_for_tenant(Some("tenant-b"), &query)
                .unwrap()
                .is_empty()
        );
        server.verify().await;
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "{endpoint}"
        );
    }
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
async fn key_expiring_while_waiting_for_store_lock_is_401_and_never_reaches_upstream() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let key = MintedVirtualKey::mint();
    let expires_at_ms = wall_clock_ms() + 500;
    let (authenticator, store) = memory_auth(
        &key,
        &scope(
            vec![PrivacyClass::LocalOnly],
            &["fast"],
            Some(expires_at_ms),
        ),
    );
    let before_lock = Arc::new(tokio::sync::Notify::new());
    let authenticator = authenticator.notify_before_store_lock(before_lock.clone());
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let held_store = store.clone();
    let holder = std::thread::spawn(move || {
        let _guard = held_store.lock().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let app = build_app(state(&server.uri(), Some(authenticator)));
    let request = chat_request(&[("authorization", bearer(&key))]);
    let task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), before_lock.notified())
        .await
        .unwrap();
    assert!(wall_clock_ms() < expires_at_ms);
    tokio::time::timeout(Duration::from_secs(2), async {
        while wall_clock_ms() <= expires_at_ms {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    let response = task.await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(response).await;
    assert_eq!(body["error"]["reason_code"], "VIRTUAL_KEY_UNAUTHORIZED");
    assert_eq!(
        body["error"]["remediation"],
        "virtual key authentication failed"
    );
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

#[tokio::test]
async fn dev_no_key_allows_only_missing_auth_to_free_loopback_local_only() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let key = MintedVirtualKey::mint();
    let (authenticator, _store) =
        memory_auth(&key, &scope(vec![PrivacyClass::LocalOnly], &["fast"], None));
    let app = build_app(dev_state(&server.uri(), authenticator));

    let response = app.oneshot(chat_request(&[])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    server.verify().await;
}

#[tokio::test]
async fn dev_no_key_rejects_any_paid_remote_and_invalid_bearer_before_egress() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let key = MintedVirtualKey::mint();
    let (authenticator, _store) =
        memory_auth(&key, &scope(vec![PrivacyClass::LocalOnly], &["fast"], None));

    let any = build_app(dev_state(&server.uri(), authenticator.clone()))
        .oneshot(chat_request(&[("x-idoris-privacy", "any".into())]))
        .await
        .unwrap();
    assert_eq!(any.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(any).await["error"]["reason_code"],
        "DEV_NO_KEY_SCOPE_FORBIDDEN"
    );

    let mut paid_state = dev_state(&server.uri(), authenticator.clone());
    paid_state.cards[0].provider.cost.input_per_m = 1.0;
    let paid = build_app(paid_state)
        .oneshot(chat_request(&[]))
        .await
        .unwrap();
    assert_eq!(paid.status(), StatusCode::FORBIDDEN);

    let mut remote_state = dev_state(&server.uri(), authenticator.clone());
    remote_state.cards[0].provider.locality = Locality::Remote;
    remote_state.cards[0].provider.privacy_class = PrivacyClass::Any;
    remote_state.cards[0].privacy_class = PrivacyClass::Any;
    remote_state.cards[0].allowed_egress = vec![Egress::Internet];
    let remote = build_app(remote_state)
        .oneshot(chat_request(&[]))
        .await
        .unwrap();
    assert!(!remote.status().is_success());

    let invalid = build_app(dev_state(&server.uri(), authenticator))
        .oneshot(chat_request(&[(
            "authorization",
            "Bearer malformed".into(),
        )]))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_body(invalid).await["error"]["reason_code"],
        "VIRTUAL_KEY_UNAUTHORIZED"
    );
    server.verify().await;
}

#[tokio::test]
async fn health_reports_dev_no_key_enabled_without_auth_material() {
    let key = MintedVirtualKey::mint();
    let (authenticator, _store) =
        memory_auth(&key, &scope(vec![PrivacyClass::LocalOnly], &["fast"], None));
    let response = build_app(dev_state("http://127.0.0.1:1", authenticator))
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["dev_no_key_enabled"], true);
    assert!(!body.to_string().contains("idk_"));
}

#[tokio::test]
async fn data_plane_requires_bearer_by_default_on_loopback_and_remote() {
    let key = MintedVirtualKey::mint();
    let roles = idoris_policy::ROLES
        .iter()
        .map(|role| role.as_str())
        .collect::<Vec<_>>();
    let (authenticator, _store) = memory_auth(
        &key,
        &scope(
            vec![PrivacyClass::LocalOnly, PrivacyClass::Any],
            &roles,
            None,
        ),
    );

    let routes = [
        ("GET", "/v1/models"),
        ("GET", "/capabilities"),
        ("GET", "/idoris/tenants/acme/usage"),
        ("GET", "/idoris/tenants/acme/audit"),
        ("GET", "/idoris/tenants/acme/requests/record-1"),
        ("GET", "/idoris/tenants/acme/budget"),
        ("POST", "/v1/chat/completions"),
        ("POST", "/v1/feedback"),
        ("POST", "/v1/embeddings"),
        ("POST", "/v1/rerank"),
        ("POST", "/v1/messages"),
    ];
    for remote_bind_requires_auth in [false, true] {
        let mut protected = state("http://127.0.0.1:1", Some(authenticator.clone()));
        protected.remote_bind_requires_auth = remote_bind_requires_auth;
        let app = build_app(protected);
        for (method, uri) in routes {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "remote={remote_bind_requires_auth} {method} {uri}"
            );
            assert_eq!(
                json_body(response).await["error"]["reason_code"],
                "VIRTUAL_KEY_UNAUTHORIZED",
                "remote={remote_bind_requires_auth} {method} {uri}"
            );
        }

        let health = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn explicit_loopback_dev_no_key_allows_missing_bearer_but_rejects_invalid_bearer() {
    let key = MintedVirtualKey::mint();
    let (authenticator, _store) =
        memory_auth(&key, &scope(vec![PrivacyClass::LocalOnly], &["fast"], None));
    let app = build_app(dev_state("http://127.0.0.1:1", authenticator));

    let anonymous = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let invalid = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer malformed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protocol_routes_reject_privacy_outside_authenticated_key_scope() {
    let key = MintedVirtualKey::mint();
    let roles = idoris_policy::ROLES
        .iter()
        .map(|role| role.as_str())
        .collect::<Vec<_>>();
    let (authenticator, _store) =
        memory_auth(&key, &scope(vec![PrivacyClass::LocalOnly], &roles, None));
    let mut remote = state("http://127.0.0.1:1", Some(authenticator));
    remote.remote_bind_requires_auth = true;
    let app = build_app(remote);
    for endpoint in ["/v1/embeddings", "/v1/rerank", "/v1/messages"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(endpoint)
                    .header("authorization", bearer(&key))
                    .header("x-idoris-privacy", "any")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"idoris/fast","messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{endpoint}");
        assert_eq!(
            json_body(response).await["error"]["reason_code"],
            "VIRTUAL_KEY_PRIVACY_FORBIDDEN",
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn tenant_keys_reject_cross_tenant_unbound_and_anonymous_requests_without_side_effects() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let roles = idoris_policy::ROLES
        .iter()
        .map(|role| role.as_str())
        .collect::<Vec<_>>();
    let bound = MintedVirtualKey::mint();
    let mut bound_scope = scope(vec![PrivacyClass::LocalOnly], &roles, None);
    bound_scope.tenant_id = Some("tenant-a".into());
    let (authenticator, store) = memory_auth(&bound, &bound_scope);
    let unbound = MintedVirtualKey::mint();
    store
        .lock()
        .unwrap()
        .insert_active(
            &unbound.key_id,
            unbound.hash,
            &scope(vec![PrivacyClass::LocalOnly], &roles, None),
        )
        .unwrap();
    let event_log = Arc::new(
        idoris_tenancy::event_log::EventLogStore::new(Connection::open_in_memory().unwrap())
            .unwrap(),
    );
    let records = Arc::new(Mutex::new(
        idoris_tenancy::store::TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    let mut protected = state(&server.uri(), Some(authenticator));
    protected.deploy_mode = idoris_contracts::DeployMode::Tenant;
    protected.event_log = Some(event_log.clone());
    protected.record_store = Some(records.clone());
    protected.dev_no_key_enabled = true;
    let app = build_app(protected);
    for (method, uri) in [
        ("POST", "/v1/chat/completions"),
        ("POST", "/v1/embeddings"),
        ("POST", "/v1/rerank"),
        ("POST", "/v1/messages"),
        ("POST", "/v1/inspect"),
        ("POST", "/v1/feedback"),
        ("GET", "/idoris/tenants/tenant-b/requests/record-1"),
        ("GET", "/idoris/tenants/tenant-b/audit"),
        ("GET", "/idoris/tenants/tenant-b/usage"),
        ("GET", "/idoris/tenants/tenant-b/budget"),
    ] {
        for (token, tenant, status) in [
            (
                Some(bearer(&bound)),
                Some("tenant-b"),
                StatusCode::FORBIDDEN,
            ),
            (
                Some(bearer(&unbound)),
                Some("tenant-a"),
                StatusCode::FORBIDDEN,
            ),
            (None, Some("tenant-b"), StatusCode::UNAUTHORIZED),
            (
                Some("Bearer malformed".into()),
                Some("tenant-b"),
                StatusCode::UNAUTHORIZED,
            ),
        ] {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", token);
            }
            if let Some(tenant) = tenant {
                request = request.header("x-idoris-tenant", tenant);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{method} {uri}");
            let record_id = response.headers()[HEADER_RECORD_ID]
                .to_str()
                .unwrap()
                .to_owned();
            for tenant in ["tenant-a", "tenant-b"] {
                assert!(
                    event_log
                        .events_for_record(Some(tenant), &record_id)
                        .unwrap()
                        .is_empty()
                );
            }
            let body = json_body(response).await;
            assert_eq!(
                body["error"]["reason_code"],
                if status == StatusCode::FORBIDDEN {
                    "VIRTUAL_KEY_TENANT_FORBIDDEN"
                } else {
                    "VIRTUAL_KEY_UNAUTHORIZED"
                }
            );
            assert!(!body.to_string().contains("tenant-a"));
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", bearer(&bound))
                    .header("x-idoris-tenant", "tenant-a")
                    .header("x-idoris-tenant", "tenant-b")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    for tenant in ["tenant-a", "tenant-b"] {
        assert!(
            records
                .lock()
                .unwrap()
                .list(Some(tenant), None)
                .unwrap()
                .is_empty()
        );
    }
    server.verify().await;
}

#[tokio::test]
async fn bound_tenant_key_serves_only_its_tenant_and_records_the_authenticated_chain() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
        .expect(1)
        .mount(&server)
        .await;
    let key = MintedVirtualKey::mint();
    let mut key_scope = scope(vec![PrivacyClass::LocalOnly], &["fast"], None);
    key_scope.tenant_id = Some("tenant-a".into());
    let (authenticator, _) = memory_auth(&key, &key_scope);
    let event_log = Arc::new(
        idoris_tenancy::event_log::EventLogStore::new(Connection::open_in_memory().unwrap())
            .unwrap(),
    );
    let mut protected = state(&server.uri(), Some(authenticator));
    protected.deploy_mode = idoris_contracts::DeployMode::Tenant;
    protected.event_log = Some(event_log.clone());
    protected.record_store = Some(Arc::new(Mutex::new(
        idoris_tenancy::store::TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    )));
    let response = build_app(protected)
        .oneshot(chat_request(&[
            ("authorization", bearer(&key)),
            ("x-idoris-tenant", "tenant-a".into()),
        ]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let record_id = response.headers()[HEADER_RECORD_ID].to_str().unwrap();
    let events = event_log
        .events_for_record(Some("tenant-a"), record_id)
        .unwrap();
    assert_eq!(
        events[0].event.event_type,
        idoris_tenancy::event_log::EventType::RequestReceived
    );
    assert!(events.iter().all(|row| row.event.tenant_id == "tenant-a"));
    assert!(events.iter().any(|row| row.event.event_type
        == idoris_tenancy::event_log::EventType::AuditFinalized));
    assert!(
        event_log
            .events_for_record(Some("tenant-b"), record_id)
            .unwrap()
            .is_empty()
    );
    server.verify().await;
}

#[tokio::test]
async fn tenant_library_without_authenticator_fails_closed_without_egress_or_audit() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    for dev_no_key_enabled in [false, true] {
        let log = Arc::new(
            idoris_tenancy::event_log::EventLogStore::new(Connection::open_in_memory().unwrap())
                .unwrap(),
        );
        let records = Arc::new(Mutex::new(
            idoris_tenancy::store::TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
        ));
        let mut unconfigured = state(&server.uri(), None);
        unconfigured.deploy_mode = idoris_contracts::DeployMode::Tenant;
        unconfigured.dev_no_key_enabled = dev_no_key_enabled;
        unconfigured.event_log = Some(log.clone());
        unconfigured.record_store = Some(records.clone());
        let app = build_app(unconfigured);
        for token in [None, Some(bearer(&MintedVirtualKey::mint()))] {
            let mut headers = vec![("x-idoris-tenant", "tenant-a".into())];
            if let Some(token) = token {
                headers.push(("authorization", token));
            }
            let response = app.clone().oneshot(chat_request(&headers)).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let record_id = response.headers()[HEADER_RECORD_ID].to_str().unwrap();
            assert!(
                log.events_for_record(Some("tenant-a"), record_id)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                records
                    .lock()
                    .unwrap()
                    .list(Some("tenant-a"), None)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                json_body(response).await["error"]["reason_code"],
                "VIRTUAL_KEY_UNAUTHORIZED"
            );
        }
        let health = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }
    server.verify().await;
}

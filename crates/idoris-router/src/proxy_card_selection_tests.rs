//! Request-local F1 snapshots. Catalogue fixtures use globally unique IDs.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::dispatch;
use crate::tests::{post_chat, resident_component_card, sample_component_card};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn proxy() -> ChatProxy {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.test_clock_ms.store(0, Ordering::SeqCst);
    proxy.retry_delays.clear();
    proxy
}
fn open(proxy: &ChatProxy, key: BreakerKey, class: CredentialClass) {
    let mut book = proxy.breakers.lock().unwrap();
    let ticket = book.admit(&key, class, 0).ticket.unwrap();
    book.on_retry_after(&key, class, ticket, 0, 10_000);
}
fn profile() -> crate::profile::ParsedProfile {
    crate::profile::parse_profile(
        &Default::default(),
        None,
        idoris_contracts::DeployMode::Personal,
    )
    .unwrap()
}
fn select(
    proxy: &ChatProxy,
    cards: &[ComponentCard],
) -> Result<dispatch::Selected, dispatch::DispatchError> {
    dispatch::select_with_gate_fallback(
        cards,
        &profile(),
        "hi",
        None,
        &Default::default(),
        &proxy.blocked_cards(cards),
    )
}
async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"marker":"healthy","model":"actual","choices":[]}),
            ),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn current_origin_replacement_and_shared_origin_alternatives_dispatch_in_both_orders() {
    for reverse in [false, true] {
        let old = server().await;
        let healthy = server().await;
        let p = Arc::new(proxy());
        open(
            &p,
            connection_key(&old.uri()).unwrap(),
            CredentialClass::Local,
        );
        // Unique provider now resolves at B: an old A connection is not provider-wide.
        let current = resident_component_card("moving-provider", &healthy.uri());
        assert_eq!(p.blocked_cards(std::slice::from_ref(&current)), [false]);
        let mut cards = vec![
            resident_component_card("a", &old.uri()),
            resident_component_card("b", &old.uri()),
            current,
        ];
        if reverse {
            cards.reverse();
        }
        let selected = select(&p, &cards).unwrap();
        assert_eq!(selected.card.provider.id, "moving-provider");
        assert_eq!(selected.card.endpoint, healthy.uri());
        let app = crate::build_app(crate::AppState {
            cards,
            proxy: p,
            ..Default::default()
        });
        let response = app
            .oneshot(post_chat(r#"{"model":"concrete","messages":[]}"#, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(old.received_requests().await.unwrap().len(), 0);
        let requests = healthy.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "concrete");
    }
}

#[test]
fn canonical_origins_and_provider_class_isolation() {
    for (old, current, class) in [
        (
            "http://LOCALHOST/a",
            "http://localhost:80/b",
            CredentialClass::Local,
        ),
        (
            "https://EXAMPLE.com/a",
            "https://example.com:443/b",
            CredentialClass::ApiKey,
        ),
        (
            "http://[0:0:0:0:0:0:0:1]/a",
            "http://[::1]:80/b",
            CredentialClass::Local,
        ),
    ] {
        let p = proxy();
        open(&p, connection_key(old).unwrap(), class);
        let card = resident_component_card("unique", current);
        assert_eq!(p.blocked_cards(std::slice::from_ref(&card)), [true]);
        let mut native = card.clone();
        native.load_policy = None;
        assert_eq!(
            p.blocked_cards(&[native]),
            [false],
            "native endpoint is decorative"
        );
        let other = proxy();
        open(&other, provider_key("unique"), class);
        let mut moved = card;
        moved.endpoint = "https://different.example:9443".into();
        assert_eq!(
            other.blocked_cards(std::slice::from_ref(&moved)),
            [class == CredentialClass::ApiKey]
        );
        moved.endpoint = "http://127.0.0.1:9456".into();
        assert_eq!(
            other.blocked_cards(&[moved]),
            [class == CredentialClass::Local]
        );
    }
}

#[test]
fn preview_is_nonmutating_at_missing_expiry_and_inflight_probe() {
    let p = proxy();
    let cards = [resident_component_card("p", "http://localhost")];
    let empty = format!("{:?}", p.breakers.lock().unwrap());
    for _ in 0..20 {
        assert_eq!(p.blocked_cards(&cards), [false]);
    }
    assert_eq!(format!("{:?}", p.breakers.lock().unwrap()), empty);
    for key in [
        provider_key("p"),
        connection_key("http://localhost").unwrap(),
    ] {
        open(&p, key, CredentialClass::Local);
    }
    p.test_clock_ms.store(10_000, Ordering::SeqCst);
    let expired = format!("{:?}", p.breakers.lock().unwrap());
    for _ in 0..20 {
        assert_eq!(p.blocked_cards(&cards), [false]);
    }
    assert_eq!(
        format!("{:?}", p.breakers.lock().unwrap()),
        expired,
        "issuer/sequence/probe/state unchanged"
    );
    let claim = p.claim_send("p", "http://localhost").unwrap();
    assert_eq!(p.blocked_cards(&cards), [true]);
    claim.finish(200, None);
    assert_eq!(p.blocked_cards(&cards), [false]);
}

#[tokio::test]
async fn previews_race_but_actual_http_probe_has_single_observed_winner() {
    let server = server().await;
    let p = Arc::new(proxy());
    let cards = [resident_component_card("p", &server.uri())];
    open(&p, provider_key("p"), CredentialClass::Local);
    open(
        &p,
        connection_key(&server.uri()).unwrap(),
        CredentialClass::Local,
    );
    p.test_clock_ms.store(10_000, Ordering::SeqCst);
    assert_eq!(p.blocked_cards(&cards), [false]);
    assert_eq!(p.blocked_cards(&cards), [false]);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let first = tokio::spawn({
        let p = p.clone();
        let url = server.uri();
        let entered = entered.clone();
        let release = release.clone();
        async move {
            p.forward_stream_observed(
                &url,
                &serde_json::json!({"messages":[]}),
                "p",
                || async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok::<_, ()>(())
                },
            )
            .await
            .unwrap()
        }
    });
    entered.notified().await;
    assert_eq!(p.blocked_cards(&cards), [true]);
    let calls = Arc::new(AtomicUsize::new(0));
    let second = p
        .forward_stream_observed(
            &server.uri(),
            &serde_json::json!({"messages":[]}),
            "p",
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(())
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        second,
        StreamOutcome::Buffered { status: 503, .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(server.received_requests().await.unwrap().is_empty());
    release.notify_one();
    let _first = first.await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn disabled_and_poisoned_snapshots_fail_as_specified() {
    let p = proxy();
    let cards = [resident_component_card("p", "http://localhost")];
    open(&p, provider_key("p"), CredentialClass::Local);
    assert_eq!(p.blocked_cards(&cards), [true]);
    p.set_breakers_enabled(false);
    assert_eq!(p.blocked_cards(&cards), [false]);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _lock = p.breakers.lock().unwrap();
        panic!("poison");
    }));
    p.set_breakers_enabled(true);
    assert_eq!(p.blocked_cards(&cards), [true]);
    p.set_breakers_enabled(false);
    assert_eq!(p.blocked_cards(&cards), [false]);
}

#[test]
fn subscription_uses_validated_provider_only_oauth_and_keeps_invalid_boundary() {
    use idoris_contracts::{common::Tier, component_card::Form};
    let p = proxy();
    let mut card = sample_component_card("subscription");
    card.form = Form::SpawnCli;
    card.endpoint = "spawn://subscription".into();
    card.provider.tier = Tier::Remote;
    card.provider.privacy_class = PrivacyClass::Any;
    card.privacy_class = PrivacyClass::Any;
    card.extensions = None;
    assert!(crate::subscription::card::classify_subscription_card(&card).is_ok());
    open(&p, provider_key("subscription"), CredentialClass::ApiKey);
    open(&p, provider_key("subscription"), CredentialClass::Local);
    assert_eq!(p.blocked_cards(std::slice::from_ref(&card)), [false]);
    open(&p, provider_key("subscription"), CredentialClass::OAuth);
    assert_eq!(p.blocked_cards(std::slice::from_ref(&card)), [true]);
    card.endpoint = "https://wrong.example".into();
    assert!(crate::subscription::card::classify_subscription_card(&card).is_err());
    assert_eq!(
        p.blocked_cards(&[card]),
        [false],
        "preview does not invent a valid transport for malformed subscription"
    );
}

#[tokio::test]
async fn all_open_mixed_in_both_id_orders_and_affinities_never_executes_lifecycle() {
    for lifecycle_first in [false, true] {
        for session in [None, Some("one"), Some("two")] {
            let server = server().await;
            let (native_id, http_id) = if lifecycle_first {
                ("a-native", "z-http")
            } else {
                ("z-native", "a-http")
            };
            let native = sample_component_card(native_id);
            let http = resident_component_card(http_id, &server.uri());
            let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
                idoris_backend::ModelInfo {
                    id: native_id.into(),
                    memory_gb: 1.0,
                },
            ]));
            let handle =
                idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
            let p = Arc::new(proxy());
            for id in [native_id, http_id] {
                open(&p, provider_key(id), CredentialClass::Local);
            }
            let state = crate::AppState {
                cards: vec![native.clone(), http],
                runtimes: dispatch::BoundSupervisor::new(&native, handle).into(),
                proxy: p,
                ..Default::default()
            };
            assert_eq!(state.cards.len(), 2);
            let app = crate::build_app(state);
            let headers: Vec<_> = session
                .map(|s| vec![("x-idoris-session", s)])
                .unwrap_or_default();
            let response = app
                .oneshot(post_chat(r#"{"messages":[]}"#, &headers))
                .await
                .unwrap();
            assert_eq!(response.status(), 503);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("provider_circuit_open"));
            assert!(server.received_requests().await.unwrap().is_empty());
            assert!(
                adapter.event_log().is_empty(),
                "no list/load/chat/unload: {:?}",
                adapter.event_log()
            );
        }
    }
}

#[tokio::test]
async fn lifecycle_only_open_returns_policy_rejection_without_ungated_recompute() {
    let native = sample_component_card("native");
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "native".into(),
            memory_gb: 1.0,
        },
    ]));
    let handle = idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
    let p = Arc::new(proxy());
    open(&p, provider_key("native"), CredentialClass::Local);
    let state = crate::AppState {
        cards: vec![native.clone()],
        runtimes: dispatch::BoundSupervisor::new(&native, handle).into(),
        proxy: p,
        ..Default::default()
    };
    assert_eq!(state.cards.len(), 1);
    let response = crate::build_app(state)
        .oneshot(post_chat(r#"{"messages":[]}"#, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&body).contains("provider_circuit_open"));
    assert!(adapter.event_log().is_empty());
}

#[tokio::test]
async fn either_available_execution_path_is_selected_and_preserves_exact_model() {
    for native_available in [false, true] {
        let server = server().await;
        let mut native = sample_component_card("native");
        native.extensions.as_mut().unwrap().insert(
            "idoris_role_models".into(),
            serde_json::json!({"daily":"exact-backend"}),
        );
        let http = resident_component_card("http", &server.uri());
        let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
            idoris_backend::ModelInfo {
                id: "wrong-first".into(),
                memory_gb: 1.0,
            },
            idoris_backend::ModelInfo {
                id: "exact-backend".into(),
                memory_gb: 1.0,
            },
        ]));
        let handle =
            idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
        let p = Arc::new(proxy());
        open(
            &p,
            provider_key(if native_available { "http" } else { "native" }),
            CredentialClass::Local,
        );
        let state = crate::AppState {
            cards: vec![native.clone(), http],
            runtimes: dispatch::BoundSupervisor::new(&native, handle).into(),
            proxy: p,
            ..Default::default()
        };
        let response = crate::build_app(state)
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[]}"#,
                &[("x-idoris-session", "stable")],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            usize::from(!native_available)
        );
        assert_eq!(
            adapter.load_call_count("exact-backend"),
            u32::from(native_available)
        );
        assert_eq!(adapter.load_call_count("wrong-first"), 0);
        if !native_available {
            assert!(adapter.event_log().is_empty());
        }
    }
}

#[test]
#[should_panic(expected = "gate snapshot must match candidate slice")]
fn mask_length_is_checked_before_filtering() {
    dispatch::select_with_gate_snapshot(
        &[sample_component_card("p")],
        &profile(),
        "",
        None,
        &Default::default(),
        &[],
    )
    .unwrap();
}

#[tokio::test]
async fn all_protocols_and_stream_selects_use_the_same_origin_snapshot() {
    use idoris_contracts::common::Capability;
    for reverse in [false, true] {
        for (path, stream, body) in [
            (
                "/v1/chat/completions",
                false,
                serde_json::json!({"model":"exact","messages":[]}),
            ),
            (
                "/v1/chat/completions",
                true,
                serde_json::json!({"model":"exact","messages":[],"stream":true}),
            ),
            (
                "/v1/messages",
                false,
                serde_json::json!({"model":"exact","messages":[],"max_tokens":1}),
            ),
            (
                "/v1/messages",
                true,
                serde_json::json!({"model":"exact","messages":[],"max_tokens":1,"stream":true}),
            ),
            (
                "/v1/embeddings",
                false,
                serde_json::json!({"model":"exact","input":"hi"}),
            ),
            (
                "/v1/rerank",
                false,
                serde_json::json!({"model":"exact","query":"hi","documents":["hi"]}),
            ),
        ] {
            let old = server().await;
            let healthy = MockServer::start().await;
            let response = if stream {
                ResponseTemplate::new(200).set_body_raw(
                    if path.ends_with("messages") {
                        "event: message_stop\ndata: {}\n\n"
                    } else {
                        "data: [DONE]\n\n"
                    },
                    "text/event-stream",
                )
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"marker":"healthy"}))
            };
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&healthy)
                .await;
            let p = Arc::new(proxy());
            open(
                &p,
                connection_key(&old.uri()).unwrap(),
                CredentialClass::Local,
            );
            let mut cards = vec![
                resident_component_card("a-cooled", &old.uri()),
                resident_component_card("z-healthy", &healthy.uri()),
            ];
            for card in &mut cards {
                card.provider.capabilities =
                    vec![Capability::Chat, Capability::Embedding, Capability::Rerank];
            }
            if reverse {
                cards.reverse();
            }
            let app = crate::build_app(crate::AppState {
                cards,
                proxy: p,
                ..Default::default()
            });
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), 200, "{path} stream={stream}");
            response.into_body().collect().await.unwrap();
            assert!(old.received_requests().await.unwrap().is_empty());
            let requests = healthy.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url.path(), path);
            let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(sent["model"], "exact");
        }
    }
}

#[test]
fn fallback_preserves_role_capability_privacy_cost_and_ready_admission_order() {
    let mut http = resident_component_card("a-http", "http://localhost");
    let native = sample_component_card("z-native");
    let mut parsed = profile();
    let ready = ["a-http".to_owned()].into_iter().collect();
    let choose = |cards: &[ComponentCard],
                  parsed: &crate::profile::ParsedProfile,
                  blocked: &[bool]| {
        dispatch::select_with_gate_fallback(cards, parsed, "hi", Some("session"), &ready, blocked)
    };
    assert_eq!(
        choose(&[http.clone(), native.clone()], &parsed, &[true, false])
            .unwrap()
            .card
            .provider
            .id,
        "z-native",
        "ready affinity cannot revive a blocked HTTP card"
    );
    parsed.role = Some(idoris_policy::Role::Daily);
    http.extensions = None;
    assert!(
        choose(&[http.clone(), native.clone()], &parsed, &[true, true]).is_err(),
        "role-ineligible resident cannot relax the blocked lifecycle card"
    );
    parsed.role = None;
    http.provider.capabilities.clear();
    assert!(choose(&[http.clone(), native.clone()], &parsed, &[true, true]).is_err());
    http.provider.capabilities = native.provider.capabilities.clone();
    http.provider.locality = Locality::Remote;
    http.provider.tier = idoris_contracts::common::Tier::Remote;
    http.provider.privacy_class = PrivacyClass::Any;
    http.privacy_class = PrivacyClass::Any;
    assert!(
        choose(&[http.clone(), native.clone()], &parsed, &[true, true]).is_err(),
        "resident relaxation cannot bypass local-only privacy"
    );
    http.provider.locality = Locality::Loopback;
    http.provider.tier = idoris_contracts::common::Tier::Local;
    http.provider.privacy_class = PrivacyClass::LocalOnly;
    http.privacy_class = PrivacyClass::LocalOnly;
    http.provider.cost.input_per_m = f64::NAN;
    assert!(
        choose(&[http, native], &parsed, &[true, true]).is_err(),
        "unknown price cannot manufacture eligible fallback"
    );
}

#[tokio::test]
async fn cached_request_id_replays_on_the_cooling_card_and_does_not_execute_the_alternate() {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"marker": "a"})))
        .expect(1)
        .mount(&origin)
        .await;
    let alternate = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"marker": "b"})))
        .expect(0)
        .mount(&alternate)
        .await;
    let state = crate::AppState {
        cards: vec![
            resident_component_card("a", &origin.uri()),
            resident_component_card("b", &alternate.uri()),
        ],
        ..crate::AppState::default()
    };
    state.proxy.set_breakers_enabled(true);
    state.proxy.test_clock_ms.store(5_000, Ordering::SeqCst);
    let proxy = std::sync::Arc::clone(&state.proxy);
    let app = crate::build_app(state);
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    let first = app
        .clone()
        .oneshot(post_chat(body, &[("x-idoris-request-id", "retry-1")]))
        .await
        .unwrap();
    assert_eq!(first.status(), axum::http::StatusCode::OK);
    let first_body = first.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&first_body[..], br#"{"marker":"a"}"#);
    {
        let mut book = proxy.breakers.lock().unwrap();
        let key = provider_key("a");
        let class = credential_class(&origin.uri());
        let ticket = book.admit(&key, class, 5_000).ticket.unwrap();
        book.on_retry_after(&key, class, ticket, 5_000, 60_000);
    }
    let second = app
        .oneshot(post_chat(body, &[("x-idoris-request-id", "retry-1")]))
        .await
        .unwrap();
    assert_eq!(second.status(), axum::http::StatusCode::OK);
    assert_eq!(
        second
            .headers()
            .get("x-idoris-cached")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    second.into_body().collect().await.unwrap();
    origin.verify().await;
    alternate.verify().await;
}

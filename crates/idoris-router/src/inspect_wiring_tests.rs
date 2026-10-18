#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::InspectResponse;
use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;

const ID: &str = "11010519491231002X";

fn inspect_request(body: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/inspect")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The response must satisfy the frozen contract, not just look like JSON.
fn contract(body: &str) -> InspectResponse {
    let value: Value = serde_json::from_str(body).unwrap();
    idoris_contracts::parse::<InspectResponse>(&value).unwrap()
}

#[tokio::test]
async fn findings_are_located_but_never_echoed() {
    let app = build_app(AppState::default());
    let body = json!({ "input": format!("证件 {ID}，电话 13812345678") }).to_string();
    let (status, text) = send(&app, inspect_request(&body, &[("x-idoris-privacy", "any")])).await;
    assert_eq!(status, StatusCode::OK);
    let response = contract(&text);
    assert_eq!(
        response.sensitivity,
        idoris_contracts::inspect::Sensitivity::S3
    );
    assert_eq!(
        response.action,
        idoris_contracts::inspect::InspectAction::Block
    );
    assert_eq!(response.findings.len(), 2);
    assert!(
        !text.contains(ID) && !text.contains("13812345678"),
        "{text}"
    );
}

#[tokio::test]
async fn declared_privacy_decides_allow_for_clean_input() {
    let app = build_app(AppState::default());
    let body = json!({ "input": "今天天气不错" }).to_string();
    let cases = [
        (vec![], "BLOCK"),
        (vec![("x-idoris-privacy", "local_only")], "BLOCK"),
        (vec![("x-idoris-privacy", "any")], "ALLOW"),
    ];
    for (headers, action) in cases {
        let (status, text) = send(&app, inspect_request(&body, &headers)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["action"],
            action
        );
    }
    // No consent wire format exists yet, so personal data is never REDACT.
    let personal = json!({ "input": "电话 13812345678" }).to_string();
    let (_, text) = send(
        &app,
        inspect_request(&personal, &[("x-idoris-privacy", "any")]),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["action"],
        "BLOCK"
    );
}

#[tokio::test]
async fn invalid_bodies_are_rejected_without_echoing_input() {
    let app = build_app(AppState::default());
    for body in [
        format!(r#"{{"input":"{ID}","privacy":"any"}}"#),
        format!(r#"{{"input":["{ID}"]}}"#),
        format!(r#"{{"messages":[{{"role":"user","content":"{ID}"}}]}}"#),
        format!(r#"{{"input":"{ID}""#),
        format!(r#""{ID}""#),
    ] {
        let (status, text) = send(&app, inspect_request(&body, &[])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!text.contains(ID), "{text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["error"]["type"],
            "invalid_body"
        );
    }
}

#[tokio::test]
async fn invalid_privacy_and_missing_tenant_are_profile_errors() {
    let app = build_app(AppState::default());
    let body = json!({ "input": "x" }).to_string();
    let (status, _) = send(
        &app,
        inspect_request(&body, &[("x-idoris-privacy", "bogus")]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let tenant_app = build_app(AppState {
        deploy_mode: idoris_contracts::DeployMode::Tenant,
        ..AppState::default()
    });
    let (status, text) = send(&tenant_app, inspect_request(&body, &[])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text.contains("tenant_missing"), "{text}");
    let (status, _) = send(
        &tenant_app,
        inspect_request(&body, &[("x-idoris-tenant", "acme")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

fn authenticated_app(allowed_privacy: Vec<PrivacyClass>, roles: &[&str]) -> (Router, String) {
    let key = MintedVirtualKey::mint();
    let store = Arc::new(Mutex::new(
        VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    let scope = VirtualKeyScope {
        owner: "agent24-instance".into(),
        allowed_privacy,
        allowed_roles: roles.iter().map(|role| (*role).to_string()).collect(),
        budget_ref: None,
        expires_at_ms: None,
        admin_scopes: Vec::new(),
    };
    store
        .lock()
        .unwrap()
        .insert_active(&key.key_id, key.hash, &scope)
        .unwrap();
    let app = build_app(AppState {
        virtual_key_authenticator: Some(auth::VirtualKeyAuthenticator::new(store)),
        ..AppState::default()
    });
    (app, format!("Bearer {}", key.secret.expose_secret()))
}

#[tokio::test]
async fn key_privacy_ceiling_applies_but_roles_do_not() {
    let body = json!({ "input": "hello" }).to_string();
    // A role-limited key (typical for Agent24) may still inspect.
    let (app, bearer) =
        authenticated_app(vec![PrivacyClass::LocalOnly, PrivacyClass::Any], &["fast"]);
    let (status, _) = send(
        &app,
        inspect_request(
            &body,
            &[("authorization", &bearer), ("x-idoris-privacy", "any")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, inspect_request(&body, &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (local_app, local_bearer) = authenticated_app(vec![PrivacyClass::LocalOnly], &["fast"]);
    let (status, _) = send(
        &local_app,
        inspect_request(
            &body,
            &[
                ("authorization", &local_bearer),
                ("x-idoris-privacy", "any"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &local_app,
        inspect_request(&body, &[("authorization", &local_bearer)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn dev_no_key_only_answers_local_only() {
    let app = build_app(AppState {
        dev_no_key_enabled: true,
        ..AppState::default()
    });
    let body = json!({ "input": "hello" }).to_string();
    let (status, _) = send(&app, inspect_request(&body, &[])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, inspect_request(&body, &[("x-idoris-privacy", "any")])).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn only_post_is_routed() {
    let app = build_app(AppState::default());
    let request = Request::builder()
        .method("GET")
        .uri("/v1/inspect")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Reviewer's A2: an ordinary year/quantity table must stay public. Before the
/// value-boundary fix, the router joined distinct JSON values with newlines and
/// the scanner's newline separator merged the rows into one Luhn-valid 16-digit
/// run, pinning a declared-`any` request to local_only (and 503 when only remote
/// cards serve it).
#[tokio::test]
async fn ordinary_numeric_table_is_not_pinned_to_local() {
    for body in [
        json!({"messages":[{"role":"user","content":"year qty\n2023 1203\n2024 1305\n2025 1411\n2026 1520"}]}),
        json!({"messages":[{"role":"user","content":"2023 1204 2024 1306"}]}),
        json!({"model":"embed","input":["y1 1203","y2 1305"]}),
    ] {
        let inspection = inspect::inspect_body(&body, PrivacyClass::Any, false);
        assert_eq!(
            inspection.effective,
            PrivacyClass::Any,
            "false positive on {body}"
        );
        assert!(
            inspection.response.findings.is_empty(),
            "unexpected findings on {body}: {:?}",
            inspection.response.findings
        );
    }
}

#[test]
fn checksum_free_numeric_lists_do_not_pin_chat_to_local_only() {
    for text in ["1350\n2210\n980", "138\n1234 5678", "1101011990\n0307737X"] {
        let body = json!({"messages": [{"role":"user", "content":text}]});
        let result = inspect::inspect_body(&body, PrivacyClass::Any, false);
        assert_eq!(
            result.effective,
            PrivacyClass::Any,
            "{text}: {:?}",
            result.response
        );
    }
    let body = json!({"messages":[{"role":"user","content":"1101051949\n1231002X"}]});
    assert_eq!(
        inspect::inspect_body(&body, PrivacyClass::Any, false).effective,
        PrivacyClass::LocalOnly
    );
}

/// E (reviewer): the token-array rule must follow the top-level `input` subtree,
/// not "any array of >= 8 integers anywhere". A tool schema's `enum` or a
/// `metadata.scores` list used to pin a clean chat to local_only, which 503s
/// remote-only callers.
#[tokio::test]
async fn integer_arrays_outside_the_embeddings_input_do_not_pin_a_clean_chat() {
    for body in [
        // tool schema enum
        json!({"messages":[{"role":"user","content":"hi"}],
               "tools":[{"type":"function","function":{"name":"pick","parameters":{
                   "type":"object","properties":{"level":{"type":"integer","enum":[0,1,2,3,4,5,6,7]}}}}}]}),
        // ordinary metadata list of eight integers
        json!({"messages":[{"role":"user","content":"hi"}],
               "metadata":{"scores":[0,1,2,3,4,5,6,7]}}),
    ] {
        let inspection = inspect::inspect_body(&body, PrivacyClass::Any, false);
        assert_eq!(
            inspection.effective,
            PrivacyClass::Any,
            "a clean chat must not be pinned by an unrelated integer array: {body}"
        );
    }

    // The real token-array payload is still opaque, with no length floor:
    for input in [
        json!([1, 2, 3, 4]),
        json!([12345, 67890, 11111]),
        json!(["hello", [0, 1, 2, 3, 4, 5, 6, 7]]),
        json!([[0, 1, 2, 3, 4, 5, 6, 7], []]),
        json!([[0, 1, 2, 3, 4, 5, 6, 7], null]),
        json!({"ids": [0,1,2,3,4,5,6,7]}),
        json!({"wrapper": ["hello", {"ids": [7]}]}),
    ] {
        let body = json!({"model":"embed","input":input});
        let inspection = inspect::inspect_body(&body, PrivacyClass::Any, false);
        assert_eq!(
            inspection.effective,
            PrivacyClass::LocalOnly,
            "an input token array must stay opaque: {body}"
        );
        assert!(inspection.uninspectable, "{body}");
    }
}

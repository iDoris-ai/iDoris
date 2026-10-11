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

#[tokio::test]
async fn embedded_base64_payloads_answer_block() {
    let app = build_app(AppState::default());
    let body =
        json!({ "input": "看图 ![x](data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==)" }).to_string();
    let (status, text) = send(&app, inspect_request(&body, &[("x-idoris-privacy", "any")])).await;
    assert_eq!(status, StatusCode::OK);
    let response = contract(&text);
    assert_eq!(
        response.action,
        idoris_contracts::inspect::InspectAction::Block
    );
    assert_eq!(
        response.sensitivity,
        idoris_contracts::inspect::Sensitivity::S3
    );
}

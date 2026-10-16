#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::InspectResponse;
use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{
    FindingKind, InspectAction, InspectFinding, InspectSpan, Sensitivity,
};
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
async fn known_l0_misses_are_never_allowed_by_authenticated_http() {
    let (app, bearer) =
        authenticated_app(vec![PrivacyClass::LocalOnly, PrivacyClass::Any], &["fast"]);
    let inputs = [
        "6222\t0212\t3456\t7894",
        "6222.0212.3456.7894",
        "6222/0212/3456/7894",
        "138.1234.5678",
        r"C:\Users\alice\.aws\credentials",
        concat!(
            "DJANGO_SECRET_KEY_FOR_PRODUCTION_SETTINGS=",
            "zMCTnu5i8RRUKO7CCgpDdFZwTMKHPBZU1HmjS9jA"
        ),
    ];
    let mut outcomes = Vec::new();
    for input in inputs {
        let body = json!({ "input": input }).to_string();
        let (status, text) = send(
            &app,
            inspect_request(
                &body,
                &[("authorization", &bearer), ("x-idoris-privacy", "any")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{input}: {text}");
        outcomes.push((input, contract(&text).action));
    }
    for (input, action) in &outcomes {
        assert_ne!(*action, InspectAction::Allow, "{input}: {outcomes:?}");
    }
    for (input, privacy, expected) in [
        ("6222 0212 3456 7894", "any", InspectAction::Block),
        ("hello", "any", InspectAction::Allow),
        ("hello", "local_only", InspectAction::Block),
    ] {
        let body = json!({ "input": input }).to_string();
        let (status, text) = send(
            &app,
            inspect_request(
                &body,
                &[("authorization", &bearer), ("x-idoris-privacy", privacy)],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(contract(&text).action, expected);
    }
    for input in inputs {
        let body = json!({ "input": input }).to_string();
        let (status, text) = send(
            &app,
            inspect_request(
                &body,
                &[
                    ("authorization", &bearer),
                    ("x-idoris-privacy", "local_only"),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(contract(&text).action, InspectAction::Block);
    }
}

#[tokio::test]
async fn grouped_resident_ids_are_blocked_with_original_spans_over_http() {
    let (app, bearer) =
        authenticated_app(vec![PrivacyClass::LocalOnly, PrivacyClass::Any], &["fast"]);
    for entity in [
        "110105.19491231.0003",
        "110105\t19491231\t0003",
        "110105/19491231/0003",
        "110105.19491231.002X",
        "110105\t19491231\t002X",
        "110105/19491231/002X",
        "440308.19900101.1239",
        "440308.1990.0101.1239",
        "830000199001011234",
    ] {
        let input = format!("😀 证件：{entity}。");
        let response = authenticated_inspection(&app, &bearer, &input).await;
        assert_http_entity(
            &input,
            entity,
            &response,
            FindingKind::CnResidentId,
            Sensitivity::S3,
        );
    }
}

#[tokio::test]
async fn neighboring_numbers_cannot_hide_entities_over_http() {
    let (app, bearer) =
        authenticated_app(vec![PrivacyClass::LocalOnly, PrivacyClass::Any], &["fast"]);
    for (input, entity, kind, sensitivity) in [
        (
            "1203 138.1234.5678",
            "138.1234.5678",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        (
            "6222.0212.3456.7894 2028",
            "6222.0212.3456.7894",
            FindingKind::BankCard,
            Sensitivity::S3,
        ),
        (
            "2026.10.15 6222.0212.3456.7894",
            "6222.0212.3456.7894",
            FindingKind::BankCard,
            Sensitivity::S3,
        ),
        (
            "6222\t0212\t3456\t7894\t1000",
            "6222\t0212\t3456\t7894",
            FindingKind::BankCard,
            Sensitivity::S3,
        ),
        (
            "138.1234.5678 7 139.1234.5678",
            "138.1234.5678",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        (
            "138.1234.5678 7 139.1234.5678",
            "139.1234.5678",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        (
            "1 2 3 138.1234.5678",
            "138.1234.5678",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        (
            "1111.6222.0212.3456.7894.2222",
            "6222.0212.3456.7894",
            FindingKind::BankCard,
            Sensitivity::S3,
        ),
        (
            "1203 138 1234 5678",
            "138 1234 5678",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        (
            "1203 138-1234-5678",
            "138-1234-5678",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
    ] {
        let input = format!("🚗😀 {input}。");
        let response = authenticated_inspection(&app, &bearer, &input).await;
        assert_http_entity(&input, entity, &response, kind, sensitivity);
    }
}

#[tokio::test]
async fn gate_one_short_neighbors_preserve_action_sensitivity_and_span() {
    let (app, bearer) =
        authenticated_app(vec![PrivacyClass::LocalOnly, PrivacyClass::Any], &["fast"]);
    for (input, kind, sensitivity, start, length) in [
        (
            "138-1234-5678 1",
            FindingKind::CnMobile,
            Sensitivity::S2,
            0,
            13,
        ),
        (
            "1 138-1234-5678",
            FindingKind::CnMobile,
            Sensitivity::S2,
            2,
            13,
        ),
        (
            "6222 0212 3456 7894 1",
            FindingKind::BankCard,
            Sensitivity::S3,
            0,
            19,
        ),
        (
            "12 6222 0212 3456 7894",
            FindingKind::BankCard,
            Sensitivity::S3,
            3,
            19,
        ),
    ] {
        let response = authenticated_inspection(&app, &bearer, input).await;
        assert_eq!(response.action, InspectAction::Block, "{input}");
        assert_eq!(response.sensitivity, sensitivity, "{input}");
        assert_eq!(
            response.findings,
            [InspectFinding {
                kind,
                span: Some(InspectSpan { start, length }),
            }],
            "{input}"
        );
    }
}

#[tokio::test]
async fn unknown_capability_is_rejected_without_echoing_header_content() {
    let (app, bearer) =
        authenticated_app(vec![PrivacyClass::LocalOnly, PrivacyClass::Any], &["fast"]);
    let (status, text) = send(
        &app,
        inspect_request(
            r#"{"input":"hello"}"#,
            &[
                ("authorization", &bearer),
                ("x-idoris-privacy", "any"),
                ("x-idoris-capabilities", "SENTINEL_SECRET_42"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!text.contains("SENTINEL_SECRET_42"), "{text}");
}

async fn authenticated_inspection(app: &Router, bearer: &str, input: &str) -> InspectResponse {
    let body = json!({ "input": input }).to_string();
    let (status, text) = send(
        app,
        inspect_request(
            &body,
            &[("authorization", bearer), ("x-idoris-privacy", "any")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{input}: {text}");
    contract(&text)
}

fn assert_http_entity(
    input: &str,
    entity: &str,
    response: &InspectResponse,
    kind: FindingKind,
    sensitivity: Sensitivity,
) {
    assert_eq!(
        response.action,
        InspectAction::Block,
        "{input}: {response:?}"
    );
    assert_eq!(response.sensitivity, sensitivity, "{input}: {response:?}");
    let start = input[..input.find(entity).unwrap()].encode_utf16().count() as u64;
    let expected = InspectFinding {
        kind,
        span: Some(InspectSpan {
            start,
            length: entity.encode_utf16().count() as u64,
        }),
    };
    assert!(
        response.findings.contains(&expected),
        "{input}: {response:?}"
    );
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

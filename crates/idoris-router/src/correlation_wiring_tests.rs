#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
use serde_json::Value;
use tower::ServiceExt;
use wiremock::matchers::{method, path};

use super::*;

fn chat_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn error_json(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn resident_card(endpoint: &str) -> ComponentCard {
    ComponentCard {
        provider: ProviderDescriptor {
            id: "correlation-upstream".into(),
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

fn duplicate_session(request: &mut Request<Body>) {
    request
        .headers_mut()
        .append("x-idoris-session", HeaderValue::from_static("first"));
    request
        .headers_mut()
        .append("x-idoris-session", HeaderValue::from_static("second"));
}

#[tokio::test]
async fn correlation_validation_preserves_body_and_profile_error_precedence() {
    let app = build_app(AppState::default());

    let mut invalid_json = chat_request("{not-json");
    duplicate_session(&mut invalid_json);
    let response = app.clone().oneshot(invalid_json).await.unwrap();
    assert_eq!(error_json(response).await["error"]["type"], "invalid_json");

    let mut invalid_body = chat_request("[]");
    duplicate_session(&mut invalid_body);
    let response = app.clone().oneshot(invalid_body).await.unwrap();
    assert_eq!(error_json(response).await["error"]["type"], "invalid_body");

    let mut invalid_profile = chat_request("{}");
    duplicate_session(&mut invalid_profile);
    invalid_profile
        .headers_mut()
        .insert("x-idoris-privacy", HeaderValue::from_static("bogus"));
    let response = app.oneshot(invalid_profile).await.unwrap();
    assert_eq!(
        error_json(response).await["error"]["type"],
        "invalid_privacy"
    );
}

#[tokio::test]
async fn malformed_correlation_headers_are_generic_400_without_value_echo() {
    let cases: Vec<(&str, HeaderValue, &str, bool)> = vec![
        (
            "x-idoris-trace-id",
            HeaderValue::from_bytes(b"\xff").unwrap(),
            "INVALID_CORRELATION_TRACE_ID",
            false,
        ),
        (
            "x-idoris-parent-id",
            HeaderValue::from_static("   "),
            "INVALID_CORRELATION_PARENT_ID",
            false,
        ),
        (
            "x-idoris-parent-id",
            HeaderValue::from_bytes(b"bad\tparent").unwrap(),
            "INVALID_CORRELATION_PARENT_ID",
            false,
        ),
        (
            "x-idoris-trace-id",
            HeaderValue::from_bytes(format!("{}a", "😀".repeat(64)).as_bytes()).unwrap(),
            "INVALID_CORRELATION_TRACE_ID",
            false,
        ),
        (
            "x-idoris-session",
            HeaderValue::from_static("first"),
            "INVALID_CORRELATION_SESSION",
            true,
        ),
    ];
    let app = build_app(AppState::default());
    for (name, value, reason, duplicate) in cases {
        let mut request = chat_request("{}");
        request.headers_mut().append(name, value);
        if duplicate {
            request
                .headers_mut()
                .append(name, HeaderValue::from_static("second"));
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = error_json(response).await;
        assert_eq!(body["error"]["type"], "invalid_correlation_header");
        assert_eq!(body["error"]["reason_code"], reason);
        assert_eq!(body["error"]["remediation"], "invalid correlation header");
        let rendered = body.to_string();
        assert!(!rendered.contains("first"));
        assert!(!rendered.contains("second"));
        assert!(!rendered.contains("bad\\tparent"));
    }
}

#[tokio::test]
async fn correlation_is_chat_only_and_normal_chat_keeps_server_record_id_authority() {
    let health = build_app(AppState::default());
    let mut health_request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    duplicate_session(&mut health_request);
    assert_eq!(
        health.oneshot(health_request).await.unwrap().status(),
        StatusCode::OK
    );

    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "upstream",
                "object": "chat.completion",
                "choices": []
            })),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let app = build_app(AppState {
        cards: vec![resident_card(&upstream.uri())],
        ..AppState::default()
    });
    let mut request =
        chat_request(r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello"}]}"#);
    request
        .headers_mut()
        .insert("x-idoris-session", HeaderValue::from_static(" session-1 "));
    request.headers_mut().insert(
        "x-idoris-trace-id",
        HeaderValue::from_bytes("😀".repeat(64).as_bytes()).unwrap(),
    );
    request
        .headers_mut()
        .insert("x-idoris-parent-id", HeaderValue::from_static("parent-1"));
    request.headers_mut().insert(
        HEADER_RECORD_ID,
        HeaderValue::from_static("caller-controlled-record-id"),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    assert_ne!(record_id, "caller-controlled-record-id");
    assert!(Uuid::parse_str(record_id).is_ok());
    upstream.verify().await;
}

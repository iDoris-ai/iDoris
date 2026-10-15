#![allow(clippy::unwrap_used)]

use super::*;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn a_declared_completion_cap_that_exceeds_budget_is_denied_before_post() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"result"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":4096,"total_tokens":4097}
        })))
        .expect(0)
        .mount(&upstream)
        .await;
    let (_dir, ledger) = super::tests::configured_budget_ledger(3000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"max_tokens":4096,"messages":[{"role":"user","content":"cap test"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert!(
        upstream.received_requests().await.unwrap().is_empty(),
        "unaffordable declared completion reached the paid upstream"
    );
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "budget_exceeded");
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(after.spent_minor, before.spent_minor);
    assert_eq!(after.reserved_minor, before.reserved_minor);
    upstream.verify().await;
}

#[tokio::test]
async fn small_completion_cap_preserves_payload_and_legacy_admission() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"answer"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":4,"total_tokens":5}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let (_dir, ledger) = super::tests::configured_budget_ledger(3000);
    let ledger = Arc::new(ledger);
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"max_tokens":8,"messages":[{"role":"user","content":"small cap"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests[0].body_json::<serde_json::Value>().unwrap()["max_tokens"],
        8
    );
    let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(view.spent_minor, 9);
    assert_eq!(view.reserved_minor, 0);
    upstream.verify().await;
}

async fn cap_admission_case(caps: &str, budget_minor: i64, expected: StatusCode) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"answer"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":4,"total_tokens":5}
        })))
        .mount(&upstream)
        .await;
    let (_dir, ledger) = super::tests::configured_budget_ledger(budget_minor);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    // One minor unit per completion token makes precision boundaries observable.
    card.provider.cost = idoris_contracts::provider::Cost {
        input_per_m: 0.0,
        output_per_m: 1_000_000.0,
    };
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let mut payload: serde_json::Value = serde_json::from_str(caps).unwrap();
    payload["messages"] = json!([{"role":"user","content":"cap test"}]);
    let response = app
        .oneshot(super::tests::post_chat(&payload.to_string(), &[]))
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let requests = upstream.received_requests().await.unwrap();
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(status, expected, "caps={caps}; response={body}");
    assert_eq!(after.reserved_minor, before.reserved_minor, "caps={caps}");
    if expected == StatusCode::OK {
        assert_eq!(requests.len(), 1, "caps={caps}");
        let forwarded = requests[0].body_json::<serde_json::Value>().unwrap();
        for field in ["max_tokens", "max_completion_tokens", "n"] {
            assert_eq!(forwarded.get(field), payload.get(field), "caps={caps}");
        }
        assert_eq!(after.spent_minor - before.spent_minor, 4);
    } else {
        assert!(requests.is_empty(), "caps={caps}");
        assert_eq!(after.spent_minor, before.spent_minor, "caps={caps}");
        assert_eq!(
            body["error"]["type"],
            if expected == StatusCode::BAD_REQUEST && payload.get("n").is_some() {
                "unsupported_completion_count"
            } else if expected == StatusCode::BAD_REQUEST {
                "invalid_completion_cap"
            } else {
                "budget_exceeded"
            }
        );
    }
}

#[tokio::test]
async fn invalid_completion_caps_are_400_without_post_or_charge() {
    for field in ["max_tokens", "max_completion_tokens"] {
        for invalid in [
            "4096.0",
            "4096.5",
            "4.096e3",
            "\"4096\"",
            "-1",
            "true",
            "[]",
            "{}",
            "9007199254740993",
            "4611686018427388415",
            "18446744073709551615",
            "18446744073709551616",
        ] {
            cap_admission_case(
                &format!("{{\"{field}\":{invalid}}}"),
                3000,
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
        // Neither a valid nor a null sibling may mask an invalid field.
        let other = if field == "max_tokens" {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        for sibling in ["8", "null"] {
            cap_admission_case(
                &format!("{{\"{field}\":\"4096\",\"{other}\":{sibling}}}"),
                3000,
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn both_completion_fields_use_the_larger_quote() {
    for caps in [
        r#"{"max_completion_tokens":4096}"#,
        r#"{"max_tokens":null,"max_completion_tokens":4096}"#,
        r#"{"max_tokens":4096,"max_completion_tokens":null}"#,
        r#"{"max_tokens":8,"max_completion_tokens":4096}"#,
        r#"{"max_tokens":4096,"max_completion_tokens":8}"#,
    ] {
        cap_admission_case(caps, 3000, StatusCode::PAYMENT_REQUIRED).await;
    }
}

#[tokio::test]
async fn completion_precision_boundary_is_quoted_without_truncation() {
    for field in ["max_tokens", "max_completion_tokens"] {
        for cap in [9_007_199_254_740_991_u64, 9_007_199_254_740_992] {
            let caps = format!("{{\"{field}\":{cap}}}");
            let exact = i64::try_from(cap).unwrap();
            cap_admission_case(&caps, exact - 1, StatusCode::PAYMENT_REQUIRED).await;
            // Existing floating-point pricing may round upward by one minor unit.
            cap_admission_case(&caps, exact + 1, StatusCode::OK).await;
        }
    }
}

#[tokio::test]
async fn valid_caps_and_defaults_preserve_payload_and_floor() {
    for caps in [
        "{}",
        r#"{"max_tokens":null}"#,
        r#"{"max_completion_tokens":null}"#,
        r#"{"max_tokens":null,"max_completion_tokens":null}"#,
        r#"{"max_tokens":null,"max_completion_tokens":8}"#,
        r#"{"max_tokens":8,"max_completion_tokens":null}"#,
        r#"{"max_tokens":null,"max_completion_tokens":2048}"#,
        r#"{"max_tokens":2048,"max_completion_tokens":null}"#,
        r#"{"max_tokens":0}"#,
        r#"{"max_tokens":8}"#,
        r#"{"max_completion_tokens":8}"#,
        r#"{"max_tokens":8,"max_completion_tokens":16}"#,
        r#"{"max_tokens":2048,"max_completion_tokens":16}"#,
    ] {
        cap_admission_case(caps, 3000, StatusCode::OK).await;
        cap_admission_case(caps, 1023, StatusCode::PAYMENT_REQUIRED).await;
    }
}

#[tokio::test]
async fn unsupported_completion_counts_never_post_or_charge() {
    for count in [
        "2",
        "3",
        "0",
        "-1",
        "1.0",
        "2.0",
        "\"2\"",
        "true",
        "[]",
        "{}",
        "18446744073709551616",
    ] {
        for stream in [false, true] {
            cap_admission_case(
                &format!("{{\"n\":{count},\"stream\":{stream},\"max_tokens\":2048}}"),
                3000,
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn single_completion_defaults_keep_nullable_caps_and_budget_floor() {
    for caps in [
        r#"{"n":1,"max_tokens":null}"#,
        r#"{"n":null,"max_completion_tokens":null}"#,
    ] {
        cap_admission_case(caps, 3000, StatusCode::OK).await;
        cap_admission_case(caps, 1023, StatusCode::PAYMENT_REQUIRED).await;
    }
}

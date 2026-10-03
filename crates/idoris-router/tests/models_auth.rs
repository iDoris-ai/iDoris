#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app};
use std::ffi::OsString;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "IDORIS_MODELS_AUTH_CASE";
const KEY_ENV: &str = "IDORIS_OMLX_API_KEY";
const SECRET: &str = "credential-fragment";
const SECOND: &str = "second-fragment";

// Each case runs in a child; the parallel test runner's environment is untouched.
#[tokio::test]
async fn model_listing_auth_configuration_is_fail_closed() {
    if let Ok(case) = std::env::var(CHILD) {
        let upstream = MockServer::start().await;
        let invalid = case == "invalid";
        let status = case.parse::<u16>().unwrap_or(200);
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(status).set_body_json(serde_json::json!({
                    "data": [{"id": "Qwen3-0.6B-4bit"}]
                })),
            )
            .expect(if invalid { 0 } else { 1 })
            .mount(&upstream)
            .await;
        let mut card: ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        card.provider.id = "Qwen3-0.6B-4bit".into();
        card.endpoint = upstream.uri();
        use idoris_contracts::provider::{Family, Locality};
        match case.as_str() {
            "family" => card.provider.family = Family::Other,
            "locality" => card.provider.locality = Locality::Remote,
            "runtime" => card.version_pin = "other@1".into(),
            "url" => card.endpoint = format!("http://upstream.test:{}", upstream.address().port()),
            _ => {}
        }
        // A successful card must not turn a later 401/403 into a partial 200.
        let mut good = card.clone();
        good.endpoint.push_str("/good");
        Mock::given(path("/good/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":[{"id":"good"}]})),
            )
            .mount(&upstream)
            .await;
        let http_client = reqwest::Client::builder()
            .no_proxy()
            .resolve("upstream.test", *upstream.address())
            .build()
            .unwrap();
        let app = build_app(AppState {
            cards: vec![good, card],
            http_client,
            ..AppState::default()
        });
        let response = app
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let failed = invalid || status != 200;
        assert_eq!(response.status(), if failed { 502 } else { 200 });
        if failed {
            assert_eq!(response.headers()["x-idoris-served-locality"], "loopback");
            assert!(!response.headers()["x-idoris-record-id"].is_empty());
        }
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if failed {
            assert_eq!(json["error"]["type"], "upstream_error");
            assert_eq!(
                json["error"]["reason_code"],
                "upstream_authentication_failed"
            );
        } else {
            assert_eq!(json["data"][0]["id"], "good");
            assert_eq!(json["data"][1]["id"], "Qwen3-0.6B-4bit");
        }
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2 * usize::from(!invalid));
        for request in &requests {
            let auth = request.headers.get("authorization");
            assert_eq!(
                auth.map(|h| h.to_str().unwrap()),
                (case == "bearer").then_some("Bearer test-secret")
            );
        }
        for fragment in [SECRET, SECOND, "test-secret"] {
            assert!(!String::from_utf8_lossy(&body).contains(fragment));
        }
        return;
    }
    let cases: [(&str, Option<OsString>); 12] = [
        ("bearer", Some("test-secret".into())),
        ("unset", None),
        ("empty", Some("".into())),
        ("invalid", Some(format!("{SECRET}\n{SECOND}").into())),
        ("invalid", Some(format!("{SECRET}\r{SECOND}").into())),
        ("invalid", Some(format!("{SECRET}\r\n{SECOND}").into())),
        ("family", Some("test-secret".into())),
        ("locality", Some("test-secret".into())),
        ("runtime", Some("test-secret".into())),
        ("url", Some("test-secret".into())),
        ("401", None),
        ("403", None),
    ];
    #[cfg(unix)]
    let cases = {
        use std::os::unix::ffi::OsStringExt;
        let key = OsString::from_vec([SECRET.as_bytes(), &[0xff], SECOND.as_bytes()].concat());
        cases.into_iter().chain([("invalid", Some(key))])
    };
    for (case, key) in cases {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "model_listing_auth_configuration_is_fail_closed",
            "--nocapture",
        ])
        .env(CHILD, case)
        .env_remove(KEY_ENV);
        if let Some(key) = key {
            cmd.env(KEY_ENV, key);
        }
        let output = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        for fragment in [SECRET, SECOND, "test-secret"] {
            assert!(!stdout.contains(fragment));
            assert!(!stderr.contains(fragment));
        }
        if case == "invalid" || case.parse::<u16>().is_ok() {
            let event = stderr
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|value| value["event"] == "upstream_model_listing_authentication_failed")
                .unwrap();
            assert_eq!(event["provider_id"], "Qwen3-0.6B-4bit");
            assert_eq!(event["locality"], "loopback");
            if case == "invalid" {
                assert_eq!(event["reason"], "invalid_authorization_header");
            } else {
                assert_eq!(event["status"], case.parse::<u16>().unwrap());
            }
        }
    }
}

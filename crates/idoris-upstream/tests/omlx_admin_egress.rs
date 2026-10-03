//! Admin login and settings requests must stay on the configured origin and
//! must ignore process proxy settings.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use serde_json::json;
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path},
};

const KEY: &str = "admin-egress-secret-key";
const COOKIE: &str = "omlx_admin_session=admin-egress-secret-cookie";
const ID: &str = "qwen3-8b";

fn policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::Resident,
        keepalive: Keepalive::Pinned { pinned: true },
        admission: Admission::Coexist,
    }
}

fn adapter(endpoint: &str) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: endpoint.into(),
        api_key: Some(KEY.into()),
        call_timeout: Duration::from_secs(2),
    })
    .unwrap()
}

async fn mount_load(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/models/{ID}/load")))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_login(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .and(body_json(json!({"api_key": KEY})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", format!("{COOKIE}; Path=/admin; HttpOnly"))
                .set_body_json(json!({"success": true})),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn admin_login_and_settings_redirects_never_reach_another_origin() {
    for status in [307, 308] {
        for redirect_login in [true, false] {
            let origin = MockServer::start().await;
            let capture = MockServer::start().await;
            mount_load(&origin).await;
            if redirect_login {
                Mock::given(method("POST"))
                    .and(path("/admin/api/login"))
                    .and(body_json(json!({"api_key": KEY})))
                    .respond_with(
                        ResponseTemplate::new(status)
                            .insert_header("Location", format!("{}/capture", capture.uri())),
                    )
                    .expect(1)
                    .mount(&origin)
                    .await;
            } else {
                mount_login(&origin).await;
                Mock::given(method("PUT"))
                    .and(path(format!("/admin/api/models/{ID}/settings")))
                    .and(wiremock::matchers::header("cookie", COOKIE))
                    .and(body_json(json!({"is_pinned": true})))
                    .respond_with(
                        ResponseTemplate::new(status)
                            .insert_header("Location", format!("{}/capture", capture.uri())),
                    )
                    .expect(1)
                    .mount(&origin)
                    .await;
            }
            Mock::given(wiremock::matchers::any())
                .respond_with(ResponseTemplate::new(200))
                .mount(&capture)
                .await;

            let error = adapter(&origin.uri())
                .load(ID, Some(&policy()))
                .await
                .expect_err("admin redirect must leave the resident load unconfirmed");
            assert_eq!(error.reason_code(), "load_unconfirmed");
            let diagnostic = format!("{error:?} {error}");
            assert!(!diagnostic.contains(KEY));
            assert!(!diagnostic.contains(COOKIE));
            assert!(capture.received_requests().await.unwrap().is_empty());
            origin.verify().await;
        }
    }
}

#[tokio::test]
async fn proxy_environment_child() {
    let Ok(endpoint) = std::env::var("IDORIS_OMLX_ADMIN_EGRESS_ENDPOINT") else {
        return;
    };
    if std::env::var_os("IDORIS_OMLX_ADMIN_EGRESS_CONTROL").is_some() {
        let _ = reqwest::Client::new()
            .get(endpoint)
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        return;
    }
    adapter(&endpoint)
        .load(ID, Some(&policy()))
        .await
        .expect("production oMLX admin flow should reach the configured origin");
}

#[tokio::test]
async fn admin_requests_ignore_http_proxy_environment() {
    let origin = MockServer::start().await;
    mount_load(&origin).await;
    mount_login(&origin).await;
    Mock::given(method("PUT"))
        .and(path(format!("/admin/api/models/{ID}/settings")))
        .and(wiremock::matchers::header("cookie", COOKIE))
        .and(body_json(json!({"is_pinned": true})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"id": ID, "loaded": true, "pinned": true}]
        })))
        .expect(1)
        .mount(&origin)
        .await;

    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let proxy_hits = Arc::new(AtomicUsize::new(0));
    let received = proxy_hits.clone();
    let listener = tokio::task::spawn(async move {
        while let Ok((_socket, _)) = proxy.accept().await {
            received.fetch_add(1, Ordering::SeqCst);
        }
    });

    for control in [true, false] {
        let before = proxy_hits.load(Ordering::SeqCst);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "proxy_environment_child", "--nocapture"])
            .env("IDORIS_OMLX_ADMIN_EGRESS_ENDPOINT", origin.uri());
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env(key, &proxy_url);
        }
        for key in ["NO_PROXY", "no_proxy"] {
            command.env(key, "");
        }
        if control {
            command.env("IDORIS_OMLX_ADMIN_EGRESS_CONTROL", "1");
        } else {
            command.env_remove("IDORIS_OMLX_ADMIN_EGRESS_CONTROL");
        }
        let output = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if control {
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(
                proxy_hits.load(Ordering::SeqCst) > before,
                "proxy positive control failed"
            );
        } else {
            assert_eq!(
                proxy_hits.load(Ordering::SeqCst),
                before,
                "oMLX admin requests used the proxy"
            );
        }
    }
    origin.verify().await;
    listener.abort();
}

//! H6: exercise production constructors, with proxy environment isolated in a child process.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use idoris_contracts::{ComponentCard, common::PrivacyClass, provider::Locality};
use idoris_router::{
    AppState, models,
    proxy::{ForwardOpts, StreamOutcome},
};
use idoris_upstream::remote::{
    CredentialSource, RemoteClient, RemoteClientConfig, RemoteProviderKind,
};
use idoris_upstream::{ChatRequest, OmlxAdapter, OmlxAdapterConfig, RemoteChat};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn adapter(endpoint: &str) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: endpoint.into(),
        api_key: Some("test-key".into()),
        call_timeout: Duration::from_millis(300),
    })
    .unwrap()
}
struct FixedKey;
#[async_trait::async_trait]
impl CredentialSource for FixedKey {
    async fn api_key(&self, _: &str) -> Result<String, idoris_upstream::UpstreamError> {
        Ok("test-key".into())
    }
}
fn remote(endpoint: &str) -> RemoteClient {
    RemoteClient::new(
        RemoteClientConfig {
            kind: RemoteProviderKind::OpenAiCompatible,
            base_url: format!("{endpoint}/v1/"),
            provider_label: "openai".into(),
        },
        Arc::new(FixedKey),
    )
}
async fn stream_failed(client: &RemoteClient) -> bool {
    match client
        .chat_stream(request(), Instant::now() + Duration::from_millis(300))
        .await
    {
        Err(_) => true,
        Ok(mut stream) => matches!(
            std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await,
            Some(Err(_))
        ),
    }
}
fn request() -> ChatRequest {
    ChatRequest {
        model: "test".into(),
        messages: vec![],
    }
}
fn card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = endpoint.into();
    card
}
fn opts() -> ForwardOpts<'static> {
    ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "test",
        provider_id: "test",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::LocalOnly,
    }
}

#[tokio::test]
async fn upstream_redirects_never_reach_the_other_origin() {
    let origin = MockServer::start().await;
    let outside = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"data": [], "choices": [{"message": {"content": "leaked"}}]}),
            ),
        )
        .mount(&outside)
        .await;
    for status in [307, 308] {
        origin.reset().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", format!("{}/capture", outside.uri())),
            )
            .mount(&origin)
            .await;
        let state = AppState::default();
        let listed = models::list_models(&state.http_client, &[card(&origin.uri())])
            .await
            .unwrap();
        let buffered = state
            .proxy
            .forward_buffered(&origin.uri(), &json!({"messages": []}), &opts())
            .await;
        let streamed = state
            .proxy
            .forward_stream(&origin.uri(), &json!({"messages": []}))
            .await;
        let omlx = adapter(&origin.uri());
        let listed_omlx = omlx.list().await;
        let unloaded = omlx.unload("test").await;
        // Fixed credentials avoid mixing H7's environment fallback into H6.
        let genai = remote(&origin.uri());
        let chat = genai
            .chat(request(), Instant::now() + Duration::from_secs(2))
            .await;
        let stream = stream_failed(&genai).await;
        assert_eq!(
            outside.received_requests().await.unwrap().len(),
            0,
            "{status} escaped the declared origin"
        );
        assert!(listed.data.is_empty());
        assert_eq!(buffered.status, status);
        assert!(matches!(streamed, StreamOutcome::Buffered { status: s, .. } if s == status));
        assert!(listed_omlx.is_err() && unloaded.is_err() && chat.is_err() && stream);
        assert_eq!(origin.received_requests().await.unwrap().len(), 7);
    }
}

#[tokio::test]
async fn proxy_environment_child() {
    let Ok(endpoint) = std::env::var("IDORIS_H6_CHILD_ENDPOINT") else {
        return;
    };
    if std::env::var_os("IDORIS_H6_CONTROL").is_some() {
        let _ = reqwest::Client::new()
            .get(&endpoint)
            .timeout(Duration::from_millis(300))
            .send()
            .await;
        return;
    }
    let state = AppState::default();
    assert!(
        state
            .http_client
            .get(&endpoint)
            .timeout(Duration::from_millis(300))
            .send()
            .await
            .is_err()
    );
    assert_eq!(
        state
            .proxy
            .forward_buffered(&endpoint, &json!({}), &opts())
            .await
            .status,
        502
    );
    assert!(matches!(
        state.proxy.forward_stream(&endpoint, &json!({})).await,
        StreamOutcome::Buffered { status: 502, .. }
    ));
    assert!(adapter(&endpoint).list().await.is_err());
    assert!(adapter(&endpoint).unload("test").await.is_err());
    let genai = remote(&endpoint);
    assert!(
        genai
            .chat(request(), Instant::now() + Duration::from_millis(300))
            .await
            .is_err()
    );
    assert!(stream_failed(&genai).await);
}

#[tokio::test]
async fn upstream_clients_ignore_system_proxy() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let probe_url = format!("http://{}", probe.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let received = hits.clone();
    let listener = tokio::spawn(async move {
        loop {
            let (socket, _) = probe.accept().await.unwrap();
            received.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    // A closed loopback port makes direct connections fail immediately, without public egress.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    for scheme in ["http", "https"] {
        for control in [true, false] {
            let before = hits.load(Ordering::SeqCst);
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "proxy_environment_child", "--nocapture"])
                .env("IDORIS_H6_CHILD_ENDPOINT", format!("{scheme}://{addr}"))
                .env("OPENAI_API_KEY", "test-key")
                .env_remove("IDORIS_H6_CONTROL");
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                command.env(key, &probe_url);
            }
            for key in ["NO_PROXY", "no_proxy"] {
                command.env(key, "");
            }
            if control {
                command.env("IDORIS_H6_CONTROL", "1");
            }
            let output = tokio::task::spawn_blocking(move || command.output().unwrap())
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
            let count = hits.load(Ordering::SeqCst) - before;
            if control {
                assert!(count > 0, "proxy positive control failed for {scheme}");
            } else {
                assert_eq!(count, 0, "{scheme} requests used the system proxy");
            }
        }
    }
    listener.abort();
}

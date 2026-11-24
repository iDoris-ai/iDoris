//! Codex re-review of layer 2, M2': the engine manager's credential-carrying health probes of an **external** engine (adopting it, supervising
//! it) used to check the listener in one place and then send the key with a bare probe to the endpoint *as the card spells it* — two separate
//! steps, and the probe did not dial the address that was verified. [`idoris_upstream::engine::probe_authorized`] sends them down the same
//! authorization path as every data-plane request: the guard re-verifies the listener on the sending task, the probe dials only the verified
//! address over a connection opened after the authorization, a refusal sends nothing, and the probe is an in-flight operation on the endpoint.
//!
//! The registry is process-global and every test uses its own wiremock port as the endpoint, so they do not touch each other.
#![allow(clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use idoris_upstream::engine::{EngineProbe, probe_authorized};
use idoris_upstream::omlx::engine::OmlxRecipe;
use idoris_upstream::omlx::key::{
    EndpointGuard, EndpointKey, KeyRefusal, Verification, block_endpoint, endpoint_flights,
    forget_endpoint, govern_endpoint_guarded,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A guard with a scripted verdict that confirms a fixed dial address (what the engine manager's guards do).
#[derive(Debug)]
struct Scripted {
    verdict: Verification,
    dial: SocketAddr,
    asked: AtomicUsize,
}

impl Scripted {
    fn new(verdict: Verification, dial: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            verdict,
            dial,
            asked: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl EndpointGuard for Scripted {
    async fn verify(&self) -> Verification {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.verdict
    }
    fn dial(&self) -> Option<SocketAddr> {
        Some(self.dial)
    }
}

/// Leaves the registry as the test found it, panics included.
struct Forget(String);

impl Drop for Forget {
    fn drop(&mut self) {
        forget_endpoint(&self.0);
    }
}

fn external() -> EndpointKey {
    EndpointKey::External {
        settings_key_verified: true,
    }
}

fn models() -> serde_json::Value {
    serde_json::json!({"data": []})
}

async fn engine(bearer: Option<&str>) -> MockServer {
    let server = MockServer::start().await;
    let mut mock = Mock::given(method("GET")).and(path("/v1/models"));
    if let Some(bearer) = bearer {
        mock = mock.and(header("authorization", format!("Bearer {bearer}").as_str()));
    }
    mock.respond_with(ResponseTemplate::new(200).set_body_json(models()))
        .mount(&server)
        .await;
    server
}

fn dial_of(server: &MockServer) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), server.address().port())
}

async fn probe(url: &str, bearer: Option<&str>) -> Result<EngineProbe, KeyRefusal> {
    let client = reqwest::Client::new();
    probe_authorized(
        &OmlxRecipe::new(),
        url,
        &client,
        bearer,
        Duration::from_secs(5),
    )
    .await
}

/// The probe dials the **verified** address, not the card's spelling, and carries the key the caller resolved first. The endpoint here is
/// written `127.0.0.2:<port>`, a spelling the registry folds into the same endpoint (every loopback spelling is one) but where nothing
/// listens: only a probe that dials the guard's verified `127.0.0.1:<port>` can reach the engine.
#[tokio::test]
async fn the_probe_dials_the_verified_address_and_carries_the_key_resolved_first() {
    let server = engine(Some("external-key")).await;
    let written = format!("http://127.0.0.2:{}", server.address().port());
    let _forget = Forget(written.clone());
    let guard = Scripted::new(Verification::Verified, dial_of(&server));
    govern_endpoint_guarded(&written, external(), Some(guard.clone()));

    assert_eq!(
        probe(&written, Some("external-key")).await,
        Ok(EngineProbe::Ready)
    );
    assert_eq!(guard.asked.load(Ordering::SeqCst), 1, "the guard was asked");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "it reached the engine at the verified address"
    );
    assert_eq!(endpoint_flights(&written), 0, "and left nothing in flight");
    // Without a key the same engine does not recognise the request.
    assert_eq!(
        probe(&written, None).await,
        Ok(EngineProbe::NotThisEngine),
        "a probe carries exactly the key it was given"
    );
}

/// The guard no longer vouches for the listener (it was replaced between the owner check and the send, or cannot be read): the probe is
/// refused **before a byte goes out** — the key included.
#[tokio::test]
async fn a_guard_that_no_longer_vouches_for_the_listener_stops_the_probe_before_a_byte_goes_out() {
    for (verdict, expected) in [
        (Verification::Mismatch, KeyRefusal::ListenerChanged),
        (Verification::Unverifiable, KeyRefusal::Unverified),
    ] {
        let server = engine(None).await;
        let url = server.uri();
        let _forget = Forget(url.clone());
        govern_endpoint_guarded(
            &url,
            external(),
            Some(Scripted::new(verdict, dial_of(&server))),
        );
        assert_eq!(
            probe(&url, Some("external-key")).await,
            Err(expected),
            "{verdict:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "{verdict:?}: nothing was sent"
        );
        assert_eq!(endpoint_flights(&url), 0, "{verdict:?}");
    }
}

/// A blocked endpoint (nothing verified is behind it) is never probed.
#[tokio::test]
async fn a_blocked_endpoint_is_never_probed() {
    let server = engine(None).await;
    let url = server.uri();
    let _forget = Forget(url.clone());
    govern_endpoint_guarded(
        &url,
        external(),
        Some(Scripted::new(Verification::Verified, dial_of(&server))),
    );
    assert!(block_endpoint(&url));
    assert_eq!(
        probe(&url, Some("external-key")).await,
        Err(KeyRefusal::Blocked)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

/// The probe is an in-flight operation on the endpoint for as long as it is outstanding: the engine manager's drain waits for it.
#[tokio::test]
async fn the_probe_is_an_in_flight_operation_until_it_has_been_answered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(models())
                .set_delay(Duration::from_millis(400)),
        )
        .mount(&server)
        .await;
    let url = server.uri();
    let _forget = Forget(url.clone());
    govern_endpoint_guarded(
        &url,
        external(),
        Some(Scripted::new(Verification::Verified, dial_of(&server))),
    );
    let outstanding = {
        let url = url.clone();
        tokio::spawn(async move { probe(&url, None).await })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while endpoint_flights(&url) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the probe never registered"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(outstanding.await.unwrap(), Ok(EngineProbe::Ready));
    assert_eq!(endpoint_flights(&url), 0, "answered: no longer in flight");
}

/// **没有核验过的拨号地址就不发**（Codex 复审 L2 的终检）：端点没登记过、或登记了却没带守卫，授权什么都没核验过。帮手自己守住这条线：不管有没有 bearer，
/// 一个请求都不发（之前它会照卡里写的 URL、用调用方的客户端把 bearer 发出去）。
#[tokio::test]
async fn an_endpoint_without_a_verified_dial_address_is_never_probed_with_or_without_a_key() {
    // 没登记过的端点。
    let unregistered = engine(None).await;
    let url = unregistered.uri();
    for bearer in [Some("external-key"), None] {
        assert_eq!(
            probe(&url, bearer).await,
            Err(KeyRefusal::Unverified),
            "an endpoint nobody governs: {bearer:?}"
        );
    }
    assert_eq!(unregistered.received_requests().await.unwrap().len(), 0);

    // 登记了，但没带守卫（没有核验过的拨号地址）。
    let unguarded = engine(None).await;
    let url = unguarded.uri();
    let _forget = Forget(url.clone());
    govern_endpoint_guarded(&url, external(), None);
    for bearer in [Some("external-key"), None] {
        assert_eq!(
            probe(&url, bearer).await,
            Err(KeyRefusal::Unverified),
            "a registration without a guard: {bearer:?}"
        );
    }
    assert_eq!(unguarded.received_requests().await.unwrap().len(), 0);
    assert_eq!(
        endpoint_flights(&url),
        0,
        "a refused probe leaves nothing in flight"
    );
}

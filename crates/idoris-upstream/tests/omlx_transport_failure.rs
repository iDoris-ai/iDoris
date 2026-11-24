//! Codex pre-review of layer 2, M4: the dedicated HTTP client for managed endpoints (it keeps no idle connection, so a credential only
//! travels over a connection opened after its authorization) could not be built, and the failure was cached for the life of the process while
//! the sender quietly fell back to the caller's shared client — pooled connections, possibly another proxy and redirect policy. Now a client
//! that cannot be built is a **local refusal** ([`KeyRefusal::TransportUnavailable`]): nothing is registered in flight, nothing is sent, and
//! the failure is not cached (the next authorization tries again).
//!
//! The switch that makes the client unbuildable is process-global, so this file holds exactly one test and nothing else runs next to it.
#![allow(clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use idoris_upstream::omlx::key::{
    EndpointGuard, EndpointKey, KeyRefusal, Verification, authorize_omlx_request, endpoint_flights,
    forget_endpoint, govern_endpoint_guarded, make_the_managed_client_unbuildable,
};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// A guard that confirms a fixed dial address (what the engine manager's guards do).
#[derive(Debug)]
struct Dialing {
    dial: SocketAddr,
}

#[async_trait]
impl EndpointGuard for Dialing {
    async fn verify(&self) -> Verification {
        Verification::Verified
    }
    fn dial(&self) -> Option<SocketAddr> {
        Some(self.dial)
    }
}

/// Leaves the process-global state as the test found it, panics included.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        make_the_managed_client_unbuildable(false);
        forget_endpoint(&self.0);
    }
}

#[tokio::test]
async fn a_managed_client_that_cannot_be_built_is_a_local_refusal_that_sends_nothing_and_is_not_cached()
 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    {
        let accepted = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"models\":[]}  ",
                    )
                    .await;
            }
        });
    }
    let url = format!("http://127.0.0.1:{port}");
    let _cleanup = Cleanup(url.clone());
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("engine-key".into()),
        Some(Arc::new(Dialing {
            dial: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        })),
    );
    let adapter = OmlxAdapter::new(OmlxAdapterConfig {
        base_url: url.clone(),
        api_key: None,
        call_timeout: Duration::from_secs(5),
    })
    .unwrap()
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())
    .with_per_request_key();

    // Healthy: the managed client is built (and cached), the request reaches the engine.
    let authorization = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(authorization.key(), Some("engine-key"));
    drop(authorization);
    let _ = adapter.list().await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the healthy request reached the engine"
    );

    // The managed client cannot be built: a local refusal, nothing in flight, and not one connection more.
    make_the_managed_client_unbuildable(true);
    assert_eq!(
        authorize_omlx_request(&url, None).await.unwrap_err(),
        KeyRefusal::TransportUnavailable
    );
    assert_eq!(
        endpoint_flights(&url),
        0,
        "a refused authorization registers no flight"
    );
    assert!(adapter.list().await.is_err());
    assert!(
        idoris_upstream::omlx::key::authorize_omlx_request_without_key(&url, None)
            .await
            .is_err(),
        "a card that may not carry the key is refused the same way"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "a request went out over some other client while the managed one could not be built"
    );

    // The failure is not cached: once the client can be built again, the very next authorization works.
    make_the_managed_client_unbuildable(false);
    let again = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(again.key(), Some("engine-key"));
    drop(again);
    let _ = adapter.list().await;
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
}

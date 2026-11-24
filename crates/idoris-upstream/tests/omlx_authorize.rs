//! IDORIS-CORE S6，Codex 终审 H1、H5、M1：发往托管端点的每个请求，发出之前经 `authorize_omlx_request`——在同一个任务里核对代际与封上、
//! 让登记时交来的守卫**重新**确认监听者、登记一个在途的操作；loopback 的各种写法落在同一条登记上。登记表是进程全局的，每个用例用自己的
//! 端口，结束时把自己的登记整条删掉（`forget_endpoint`，`test-support` 特性：代际也回到 0）。
#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use idoris_upstream::omlx::key::{
    EndpointGuard, EndpointKey, KeyRefusal, Verification, authorize_omlx_request, block_endpoint,
    check_endpoint_admission, endpoint_flights, endpoint_generation, forget_endpoint,
    govern_endpoint, govern_endpoint_guarded, governed_endpoint,
};

/// 守卫的回答是编排的（按顺序，最后一个重复）；每次被问都计数，并可以挂一个钩子（在回答之前执行，模拟「校验期间发生了别的事」）。
struct Scripted {
    answers: Mutex<VecDeque<Verification>>,
    asked: AtomicUsize,
    during: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl Scripted {
    fn answering(answers: &[Verification]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.iter().copied().collect()),
            asked: AtomicUsize::new(0),
            during: Mutex::new(None),
        })
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for Scripted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scripted")
    }
}

#[async_trait]
impl EndpointGuard for Scripted {
    async fn verify(&self) -> Verification {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = self.during.lock().unwrap().as_mut() {
            hook();
        }
        let mut answers = self.answers.lock().unwrap();
        if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers
                .front()
                .copied()
                .unwrap_or(Verification::Unverifiable)
        }
    }
}

/// 结束时把这个端点从登记表里整条删掉：端口会被操作系统很快复用给同一个进程里别的用例。
struct Governed(String);

impl Drop for Governed {
    fn drop(&mut self) {
        forget_endpoint(&self.0);
    }
}

fn endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn a_verified_listener_gets_the_key_and_the_request_is_an_in_flight_operation_until_it_is_dropped()
 {
    let url = endpoint(19801);
    let _governed = Governed(url.clone());
    let guard = Scripted::answering(&[Verification::Verified]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("engine-key".into()),
        Some(guard.clone()),
    );
    assert_eq!(endpoint_flights(&url), 0);

    let authorization = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(authorization.key(), Some("engine-key"));
    assert_eq!(authorization.generation(), endpoint_generation(&url));
    assert_eq!(guard.asked(), 1, "the guard was asked on this very call");
    assert_eq!(
        endpoint_flights(&url),
        1,
        "the request is in flight while the authorization is held"
    );

    // Every request asks again: nothing is cached from the last one.
    let second = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(guard.asked(), 2);
    assert_eq!(endpoint_flights(&url), 2);
    drop(authorization);
    assert_eq!(endpoint_flights(&url), 1);
    drop(second);
    assert_eq!(endpoint_flights(&url), 0);
}

/// Codex final review H1 (owner decision): a listener that is no longer the one registered, or that cannot be confirmed, gets
/// nothing, and the refusal leaves no in-flight operation behind.
#[tokio::test]
async fn a_listener_that_changed_or_cannot_be_confirmed_is_refused_and_leaves_no_flight() {
    let url = endpoint(19802);
    let _governed = Governed(url.clone());
    let guard = Scripted::answering(&[
        Verification::Mismatch,
        Verification::Unverifiable,
        Verification::Verified,
    ]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("engine-key".into()),
        Some(guard.clone()),
    );

    assert_eq!(
        authorize_omlx_request(&url, None).await.unwrap_err(),
        KeyRefusal::ListenerChanged
    );
    assert_eq!(endpoint_flights(&url), 0);
    assert_eq!(
        authorize_omlx_request(&url, None).await.unwrap_err(),
        KeyRefusal::Unverified
    );
    assert_eq!(endpoint_flights(&url), 0);
    // The guard says yes again: the same registration serves.
    assert!(authorize_omlx_request(&url, None).await.is_ok());
    assert_eq!(guard.asked(), 3);
}

/// External engines are guarded the same way; a request with no key is still a request to whatever listens, so it is checked too.
#[tokio::test]
async fn an_external_registration_is_guarded_even_when_there_is_no_key_to_send() {
    let url = endpoint(19803);
    let _governed = Governed(url.clone());
    let guard = Scripted::answering(&[Verification::Mismatch]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::External {
            settings_key_verified: true,
        },
        Some(guard.clone()),
    );
    assert_eq!(
        authorize_omlx_request(&url, None).await.unwrap_err(),
        KeyRefusal::ListenerChanged
    );
    assert_eq!(guard.asked(), 1);
}

#[tokio::test]
async fn a_blocked_endpoint_or_a_stale_generation_is_refused_before_the_guard_is_even_asked() {
    let url = endpoint(19804);
    let _governed = Governed(url.clone());
    let guard = Scripted::answering(&[Verification::Verified]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("engine-key".into()),
        Some(guard.clone()),
    );
    let admitted = endpoint_generation(&url);

    // The engine is replaced: blocked, then the new engine registered under a new generation.
    block_endpoint(&url);
    assert_eq!(
        authorize_omlx_request(&url, None).await.unwrap_err(),
        KeyRefusal::Blocked
    );
    assert_eq!(
        check_endpoint_admission(&url, None).unwrap_err(),
        KeyRefusal::Blocked
    );
    let fresh = Scripted::answering(&[Verification::Verified]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("new-key".into()),
        Some(fresh.clone()),
    );
    // Work admitted for the old engine does not get the new engine's key, and the new guard is not even consulted for it.
    let refused = authorize_omlx_request(&url, Some(admitted))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, KeyRefusal::StaleGeneration { .. }),
        "{refused:?}"
    );
    assert_eq!(guard.asked(), 0, "the old guard was cleared by the block");
    assert_eq!(fresh.asked(), 0);
    // The same work, admitted for the current generation, is served.
    let authorization = authorize_omlx_request(&url, Some(endpoint_generation(&url)))
        .await
        .unwrap();
    assert_eq!(authorization.key(), Some("new-key"));
}

/// The registration is read again after the guard answered: a revoke (or a replacement) that lands while the check runs wins.
#[tokio::test]
async fn a_registration_revoked_while_the_check_is_running_is_refused() {
    let url = endpoint(19805);
    let _governed = Governed(url.clone());
    let guard = Scripted::answering(&[Verification::Verified]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("engine-key".into()),
        Some(guard.clone()),
    );
    let revoke_url = url.clone();
    *guard.during.lock().unwrap() = Some(Box::new(move || {
        block_endpoint(&revoke_url);
    }));
    let refused = authorize_omlx_request(&url, None).await.unwrap_err();
    assert!(
        matches!(refused, KeyRefusal::StaleGeneration { .. }),
        "{refused:?}"
    );
    assert_eq!(endpoint_flights(&url), 0);
    assert_eq!(governed_endpoint(&url), Some(EndpointKey::Blocked));
}

/// Registering the same policy with the same guard again changes nothing (the generation stays: requests in flight stay valid); a
/// different guard object is a different registration.
#[test]
fn the_same_guard_registered_again_keeps_the_generation_and_another_guard_moves_it() {
    let url = endpoint(19806);
    let _governed = Governed(url.clone());
    let guard = Scripted::answering(&[Verification::Verified]);
    let policy = || EndpointKey::Private("engine-key".into());
    govern_endpoint_guarded(&url, policy(), Some(guard.clone()));
    let first = endpoint_generation(&url);
    govern_endpoint_guarded(&url, policy(), Some(guard.clone()));
    assert_eq!(endpoint_generation(&url), first);
    govern_endpoint_guarded(
        &url,
        policy(),
        Some(Scripted::answering(&[Verification::Verified])),
    );
    assert!(endpoint_generation(&url) > first);
    // Dropping the guard (the same policy, no guard) is a change too.
    let second = endpoint_generation(&url);
    govern_endpoint(&url, policy());
    assert!(endpoint_generation(&url) > second);
}

/// A registration without a guard (the plain policy) behaves as before: the key is the policy's, nothing is asked.
#[tokio::test]
async fn an_endpoint_registered_without_a_guard_is_served_as_before_and_still_counted() {
    let url = endpoint(19807);
    let _governed = Governed(url.clone());
    govern_endpoint(&url, EndpointKey::Private("plain-key".into()));
    let authorization = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(authorization.key(), Some("plain-key"));
    assert_eq!(endpoint_flights(&url), 1);
}

/// Codex final review H5: the engine manager registers `127.0.0.1:<port>`; every other loopback spelling of the port lands on the
/// same registration. A blocked endpoint is blocked for `localhost` and `[::1]` too (no §2.9 fallback to a settings or
/// environment key), and a guarded one asks the same guard whatever the spelling.
#[tokio::test]
async fn every_loopback_spelling_of_a_governed_port_lands_on_the_same_registration() {
    let main = endpoint(19808);
    let _governed = Governed(main.clone());
    let aliases = [
        "http://localhost:19808",
        "http://localhost.:19808",
        "https://localhost:19808/v1",
        "http://[::1]:19808",
        "http://127.0.0.2:19808",
    ];
    block_endpoint(&main);
    for alias in aliases {
        assert_eq!(
            governed_endpoint(alias),
            Some(EndpointKey::Blocked),
            "{alias}"
        );
        assert_eq!(
            authorize_omlx_request(alias, None).await.unwrap_err(),
            KeyRefusal::Blocked,
            "{alias}"
        );
    }
    let guard = Scripted::answering(&[Verification::Verified]);
    govern_endpoint_guarded(
        &main,
        EndpointKey::Private("engine-key".into()),
        Some(guard.clone()),
    );
    for (index, alias) in aliases.iter().enumerate() {
        let authorization = authorize_omlx_request(alias, None).await.unwrap();
        assert_eq!(authorization.key(), Some("engine-key"), "{alias}");
        assert_eq!(guard.asked(), index + 1, "{alias}");
    }
    // Another port is another endpoint: not governed, nothing asked.
    assert_eq!(governed_endpoint("http://localhost:19809"), None);
}

/// An endpoint nobody governs has no registration to guard, no generation to check and nothing to count.
#[tokio::test]
async fn an_endpoint_nobody_governs_is_unchanged() {
    let url = endpoint(19810);
    assert_eq!(governed_endpoint(&url), None);
    let authorization = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(authorization.generation(), 0);
    assert_eq!(endpoint_flights(&url), 0);
    // Work "admitted for generation 0" is fine; work admitted for a generation that never existed is refused.
    assert!(authorize_omlx_request(&url, Some(0)).await.is_ok());
    assert!(matches!(
        authorize_omlx_request(&url, Some(3)).await.unwrap_err(),
        KeyRefusal::StaleGeneration { .. }
    ));
}

/// **The limit that remains** (Codex final review H1, owner decision 2026-11-22; spec errata ten). The re-check narrows the window
/// between "the listener was verified" and "the request is sent" from a supervision tick to the milliseconds between one
/// verification and one send on the same task; it does not close it, and there is no per-connection peer check. A process of
/// **another user** that grabs the port in exactly that interval still receives that one request. (A same-user process gains
/// nothing: it can read the key files and the process environment anyway. libproc and `lsof` cannot see another user's sockets,
/// so the check is blind to them as well.)
///
/// This test runs the scenario and **documents** it: it asserts nothing about what the impostor saw, on purpose. If the window is
/// ever closed (a peer check on the connection), this is where the assertion goes.
#[tokio::test]
async fn known_limit_a_listener_switched_between_the_recheck_and_the_send_is_not_caught() {
    // A plain listener stands for the engine: dropping it frees the port at once (a wiremock server would hand its listener back
    // to a pool instead).
    let engine = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = engine.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");
    let _governed = Governed(url.clone());
    let guard = Scripted::answering(&[Verification::Verified]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("canary-engine-key".into()),
        Some(guard),
    );

    // The re-check passes: whatever listens on the port is the engine.
    let authorization = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(authorization.key(), Some("canary-engine-key"));

    // The port changes hands inside the window, before the send.
    drop(engine);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let impostor = loop {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                break wiremock::MockServer::builder()
                    .listener(listener)
                    .start()
                    .await;
            }
            Err(error) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the port never came free: {error}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    };
    let _ = reqwest::Client::new()
        .get(format!("{url}/v1/models"))
        .bearer_auth(authorization.key().unwrap())
        .send()
        .await;

    // Documented, not asserted.
    let seen = impostor.received_requests().await.unwrap_or_default().len();
    eprintln!(
        "known limit: {seen} request(s) reached a listener that took the port between the re-check and the send"
    );
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Codex focused review of the per-request check (d900486d), H1 and H6: the address that was verified is the address that is dialed, and
// a credential only travels over a connection opened after the authorization.
// ---------------------------------------------------------------------------------------------------------------------------------

mod dial {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use idoris_backend::{ChatMessage, ChatRequest};
    use idoris_contracts::LoadPolicy;
    use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
    use idoris_upstream::omlx::key::{
        EndpointGuard, EndpointKey, Verification, authorize_omlx_request, forget_endpoint,
        govern_endpoint, govern_endpoint_guarded,
    };
    use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::sync::CancellationToken;

    const ID: &str = "qwen3-8b";

    /// A guard that confirms a fixed dial address: what the engine manager's guards do (`dial()` is the address they verified).
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

    struct Governed(String);

    impl Drop for Governed {
        fn drop(&mut self) {
            forget_endpoint(&self.0);
        }
    }

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    /// One request a [`RawServer`] saw.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        method: String,
        path: String,
        authorization: Option<String>,
        cookie: Option<String>,
    }

    /// A minimal HTTP/1.1 server with keep-alive, on a socket the test owns: it answers by path (the oMLX endpoints the adapter uses) and
    /// records every request. [`Self::stop_accepting`] closes the **listening** socket only: connections already accepted stay open and
    /// are still served, which is what an engine that gave its port away but stayed alive looks like.
    struct RawServer {
        label: &'static str,
        seen: Arc<Mutex<Vec<Seen>>>,
        connections: Arc<AtomicUsize>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        accept: Option<tokio::task::JoinHandle<()>>,
    }

    impl RawServer {
        fn start(listener: TcpListener, label: &'static str) -> Self {
            let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
            let connections = Arc::new(AtomicUsize::new(0));
            let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
            let accept = {
                let (seen, connections) = (seen.clone(), connections.clone());
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = &mut stopped => break,
                            accepted = listener.accept() => {
                                let Ok((stream, _)) = accepted else { break };
                                connections.fetch_add(1, Ordering::SeqCst);
                                tokio::spawn(serve(stream, label, seen.clone()));
                            }
                        }
                    }
                    // `listener` is dropped here: the port is free again. The connection tasks live on.
                })
            };
            Self {
                label,
                seen,
                connections,
                stop: Some(stop),
                accept: Some(accept),
            }
        }

        async fn on_port(port: u16, label: &'static str) -> Self {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match TcpListener::bind(("127.0.0.1", port)).await {
                    Ok(listener) => return Self::start(listener, label),
                    Err(error) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the port never came free: {error}"
                        );
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        fn count(&self) -> usize {
            self.seen().len()
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }

        /// Close the listening socket; keep serving the connections already accepted.
        async fn stop_accepting(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(accept) = self.accept.take() {
                let _ = accept.await;
            }
        }
    }

    impl std::fmt::Debug for RawServer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "RawServer({})", self.label)
        }
    }

    /// Serve one connection until the peer closes it: HTTP/1.1, keep-alive.
    async fn serve(stream: TcpStream, label: &'static str, seen: Arc<Mutex<Vec<Seen>>>) {
        let mut reader = BufReader::new(stream);
        loop {
            let mut head = String::new();
            let mut request_line = String::new();
            match reader.read_line(&mut request_line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let mut length = 0_usize;
            let (mut authorization, mut cookie) = (None, None);
            loop {
                head.clear();
                match reader.read_line(&mut head).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let line = head.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_owned());
                    match name.as_str() {
                        "content-length" => length = value.parse().unwrap_or(0),
                        "authorization" => authorization = Some(value),
                        "cookie" => cookie = Some(value),
                        _ => {}
                    }
                }
            }
            if length > 0 {
                let mut body = vec![0_u8; length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
            }
            let mut parts = request_line.split_whitespace();
            let (method, path) = (
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
            );
            seen.lock().unwrap().push(Seen {
                method: method.clone(),
                path: path.clone(),
                authorization,
                cookie,
            });
            let (extra, body) = match (method.as_str(), path.as_str()) {
                ("GET", "/v1/models/status") => (
                    String::new(),
                    format!(
                        r#"{{"models":[{{"id":"{ID}","estimated_size":1000000000,"loaded":true,"pinned":true}}]}}"#
                    ),
                ),
                ("GET", "/api/status") => (
                    String::new(),
                    r#"{"loaded_models":[],"model_memory_used":0,"model_memory_max":17179869184,"pressure":"ok"}"#
                        .to_owned(),
                ),
                ("POST", "/admin/api/login") => (
                    format!(
                        "Set-Cookie: omlx_admin_session={label}-session; Path=/; HttpOnly\r\n"
                    ),
                    r#"{"success":true}"#.to_owned(),
                ),
                ("POST", "/v1/chat/completions") => (
                    String::new(),
                    format!(r#"{{"model":"{ID}","choices":[{{"message":{{"content":"{label}"}}}}]}}"#),
                ),
                ("POST", _) | ("PUT", _) => (String::new(), "{}".to_owned()),
                _ => (String::new(), r#"{"error":"not found"}"#.to_owned()),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            if reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    }

    fn adapter(url: &str) -> OmlxAdapter {
        OmlxAdapter::new(OmlxAdapterConfig {
            base_url: url.to_owned(),
            api_key: None,
            call_timeout: Duration::from_secs(5),
        })
        .unwrap()
        .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())
        .with_per_request_key()
    }

    fn policy() -> LoadPolicy {
        LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        }
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            native: None,
            model: ID.to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: "hi".to_owned(),
            }],
        }
    }

    /// What the authorization of a governed endpoint tells the sender, for every loopback spelling of it: the address the guard verified,
    /// and the URL with the host and port swapped for it (path and query kept).
    #[tokio::test]
    async fn the_authorization_carries_the_verified_address_and_rewrites_every_spelling_to_it() {
        let main = "http://127.0.0.1:19821".to_owned();
        let _governed = Governed(main.clone());
        govern_endpoint_guarded(
            &main,
            EndpointKey::Private("engine-key".into()),
            Some(Arc::new(Dialing { dial: v4(19821) })),
        );
        for alias in [
            "http://127.0.0.1:19821",
            "http://localhost:19821",
            "http://[::1]:19821",
            "http://127.0.0.2:19821",
        ] {
            let authorization = authorize_omlx_request(alias, None).await.unwrap();
            assert_eq!(authorization.dial(), Some(v4(19821)), "{alias}");
            assert_eq!(
                authorization.dial_url(&format!("{alias}/v1/models?limit=3")),
                "http://127.0.0.1:19821/v1/models?limit=3",
                "{alias}"
            );
        }
        // An endpoint nobody governs, or one registered without a guard, is sent to as written.
        let ungoverned = authorize_omlx_request("http://localhost:19822", None)
            .await
            .unwrap();
        assert_eq!(ungoverned.dial(), None);
        assert_eq!(
            ungoverned.dial_url("http://localhost:19822/v1/models"),
            "http://localhost:19822/v1/models"
        );
        let plain = "http://127.0.0.1:19823".to_owned();
        let _plain_governed = Governed(plain.clone());
        govern_endpoint(&plain, EndpointKey::Private("plain-key".into()));
        let unguarded = authorize_omlx_request("http://localhost:19823", None)
            .await
            .unwrap();
        assert_eq!(unguarded.dial(), None);
        assert_eq!(
            unguarded.dial_url("http://localhost:19823/x"),
            "http://localhost:19823/x"
        );
    }

    /// A URL that cannot be pointed at the verified address is **not sent at all** (the empty string is not a URL any client can send): a
    /// string that is not a URL, a relative one, and one that has no host to replace.
    #[tokio::test]
    async fn a_url_that_cannot_be_rewritten_to_the_verified_address_is_not_sent() {
        let main = "http://127.0.0.1:19824".to_owned();
        let _governed = Governed(main.clone());
        govern_endpoint_guarded(
            &main,
            EndpointKey::Private("engine-key".into()),
            Some(Arc::new(Dialing { dial: v4(19824) })),
        );
        let authorization = authorize_omlx_request(&main, None).await.unwrap();
        for unsendable in [
            "not a url",
            "/v1/models",
            "mailto:someone@example.com",
            "data:text/plain,hello",
        ] {
            assert_eq!(authorization.dial_url(unsendable), "", "{unsendable:?}");
        }
        // and a good one still is
        assert_eq!(
            authorization.dial_url("http://localhost:19824/v1/models"),
            "http://127.0.0.1:19824/v1/models"
        );
    }

    /// A listener on `[::1]:<port>` while the engine is on `127.0.0.1:<port>`: an adapter whose card is spelled `[::1]` (a companion card
    /// of the managed engine) must dial the address the guard verified, not the spelling. Every adapter call (GET, POST, the load with its
    /// admin login and settings) reaches the engine; the impostor on IPv6 loopback sees nothing.
    #[tokio::test]
    async fn an_adapter_whose_card_is_spelled_as_ipv6_loopback_dials_the_verified_ipv4_address() {
        // The engine on 127.0.0.1:<port>, the impostor on [::1]:<same port> (retry for a port free on both).
        let mut bound = None;
        for _ in 0..20 {
            let engine = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = engine.local_addr().unwrap().port();
            match TcpListener::bind((IpAddr::V6(Ipv6Addr::LOCALHOST), port)).await {
                Ok(impostor) => {
                    bound = Some((engine, impostor, port));
                    break;
                }
                Err(_) => continue,
            }
        }
        let Some((engine, impostor, port)) = bound else {
            eprintln!("no IPv6 loopback on this machine: nothing to show");
            return;
        };
        let engine = RawServer::start(engine, "engine");
        let impostor = RawServer::start(impostor, "impostor");
        let main = format!("http://127.0.0.1:{port}");
        let _governed = Governed(main.clone());
        govern_endpoint_guarded(
            &main,
            EndpointKey::Private("engine-key".into()),
            Some(Arc::new(Dialing { dial: v4(port) })),
        );
        let companion = adapter(&format!("http://[::1]:{port}"));

        companion.list().await.unwrap();
        companion.status().await.unwrap();
        companion.unload(ID).await.unwrap();
        companion
            .chat(chat_request(), CancellationToken::new())
            .await
            .unwrap();
        companion.load(ID, Some(&policy())).await.unwrap();

        let engine_saw = engine.seen();
        assert!(engine_saw.len() >= 8, "{engine_saw:?}");
        assert!(
            engine_saw
                .iter()
                .filter(|seen| !seen.path.starts_with("/admin/api/"))
                .all(|seen| seen.authorization.as_deref() == Some("Bearer engine-key")),
            "{engine_saw:?}"
        );
        assert!(
            engine_saw
                .iter()
                .any(|seen| seen.path.starts_with("/admin/api/models/")
                    && seen.cookie.as_deref() == Some("omlx_admin_session=engine-session")),
            "the admin settings request carried the engine's own session: {engine_saw:?}"
        );
        assert_eq!(
            impostor.count(),
            0,
            "the IPv6 listener was sent something: {:?}",
            impostor.seen()
        );
    }

    /// One call of the adapter, made twice with the engine behind the endpoint replaced in between: the first goes to A over a connection
    /// that is kept alive, then A gives the port away **but stays alive with that connection open**, B takes the port and the engine is
    /// registered again (a new generation). The same adapter's next call must reach B — over a new connection. A client that pools
    /// connections would send it down the old one, to A, with the credential of the new engine.
    async fn after_a_changeover_the_same_adapter_reaches_only_the_new_engine<F, Fut>(call: F)
    where
        F: Fn(Arc<OmlxAdapter>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut a = RawServer::start(listener, "A");
        let url = format!("http://127.0.0.1:{port}");
        let _governed = Governed(url.clone());
        let guard = || -> Arc<dyn EndpointGuard> { Arc::new(Dialing { dial: v4(port) }) };
        govern_endpoint_guarded(&url, EndpointKey::Private("key-of-a".into()), Some(guard()));
        // The adapter outlives both engines.
        let adapter = Arc::new(adapter(&url));

        call(adapter.clone()).await;
        let before = a.count();
        assert!(before >= 1, "A served the first call");

        // A lets go of the port and stays alive; its connection stays open and keeps being served. B takes the port.
        a.stop_accepting().await;
        let b = RawServer::on_port(port, "B").await;
        govern_endpoint_guarded(&url, EndpointKey::Private("key-of-b".into()), Some(guard()));

        call(adapter.clone()).await;
        assert_eq!(
            a.count(),
            before,
            "the second call went down the connection to the old engine: {:?}",
            a.seen()
        );
        assert!(b.count() >= 1, "B never saw the call");
        assert!(
            b.seen()
                .iter()
                .all(|seen| seen.authorization.as_deref() != Some("Bearer key-of-a")),
            "{:?}",
            b.seen()
        );
        assert!(b.connections() >= 1);
    }

    #[tokio::test]
    async fn a_get_after_a_changeover_never_reuses_the_connection_to_the_old_engine() {
        after_a_changeover_the_same_adapter_reaches_only_the_new_engine(|adapter| async move {
            adapter.list().await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_status_read_after_a_changeover_never_reuses_the_connection_to_the_old_engine() {
        after_a_changeover_the_same_adapter_reaches_only_the_new_engine(|adapter| async move {
            adapter.status().await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn an_unload_after_a_changeover_never_reuses_the_connection_to_the_old_engine() {
        after_a_changeover_the_same_adapter_reaches_only_the_new_engine(|adapter| async move {
            adapter.unload(ID).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_chat_after_a_changeover_never_reuses_the_connection_to_the_old_engine() {
        after_a_changeover_the_same_adapter_reaches_only_the_new_engine(|adapter| async move {
            adapter
                .chat(chat_request(), CancellationToken::new())
                .await
                .unwrap();
        })
        .await;
    }

    /// The resident load: the load POST, the admin login (a cookie of the engine that issued it), the settings PUT and the verification
    /// read — none of them over a connection to the old engine.
    #[tokio::test]
    async fn a_load_and_its_admin_session_after_a_changeover_never_reuse_the_connection_to_the_old_engine()
     {
        after_a_changeover_the_same_adapter_reaches_only_the_new_engine(|adapter| async move {
            adapter.load(ID, Some(&policy())).await.unwrap();
        })
        .await;
    }

    /// A guard that says the listener behind the endpoint is not the engine any more (the managed listener left, another process took the port).
    #[derive(Debug)]
    struct Refusing {
        dial: SocketAddr,
    }

    #[async_trait]
    impl EndpointGuard for Refusing {
        async fn verify(&self) -> Verification {
            Verification::Mismatch
        }
        fn dial(&self) -> Option<SocketAddr> {
            Some(self.dial)
        }
    }

    /// The adapter built for a card that may not carry the oMLX key (not in per-request mode, no key of its own).
    fn adapter_without_the_key(url: &str) -> OmlxAdapter {
        OmlxAdapter::new(OmlxAdapterConfig {
            base_url: url.to_owned(),
            api_key: None,
            call_timeout: Duration::from_secs(5),
        })
        .unwrap()
        .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())
    }

    /// Codex pre-review of layer 2, H1: governed is decided by the endpoint, not by the adapter's mode. The adapter of a card that may not carry
    /// the key (a hand-written card that claims to be oMLX, a card on an IPv4-mapped spelling of the port...) aims at a port the engine
    /// manager governs: it goes to the verified address, carries no key, and is refused — nothing sent — when the endpoint is blocked or when
    /// the listener is no longer the engine. Before, it skipped the whole check and sent its prompts to whatever held the port.
    #[tokio::test]
    async fn an_adapter_that_may_not_carry_the_key_is_still_gated_by_its_governed_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = RawServer::start(listener, "engine");
        let url = format!("http://127.0.0.1:{port}");
        let _governed = Governed(url.clone());
        govern_endpoint_guarded(
            &url,
            EndpointKey::Private("engine-key".into()),
            Some(Arc::new(Dialing { dial: v4(port) })),
        );
        let adapter = adapter_without_the_key(&url);
        assert!(
            idoris_backend::RuntimeAdapter::engine_generation(&adapter).is_some(),
            "bound to the endpoint's generation like any adapter of a governed endpoint"
        );

        // Healthy: the request goes out, with no credential (the registered key is not this adapter's).
        adapter.list().await.unwrap();
        assert_eq!(server.count(), 1);
        assert_eq!(
            server.seen()[0].authorization,
            None,
            "the engine's private key went to an adapter that may not carry it"
        );

        // The endpoint is blocked (stopped, being replaced): nothing is sent.
        idoris_upstream::omlx::key::block_endpoint(&url);
        assert!(adapter.list().await.is_err());
        assert_eq!(server.count(), 1, "a request went to a blocked endpoint");

        // A new engine is registered, but the guard says the listener behind the port is not it: nothing is sent either.
        govern_endpoint_guarded(
            &url,
            EndpointKey::Private("other-key".into()),
            Some(Arc::new(Refusing { dial: v4(port) })),
        );
        assert!(adapter.list().await.is_err());
        assert_eq!(
            server.count(),
            1,
            "a request went to a listener the guard no longer vouches for"
        );
    }

    /// An endpoint nobody governs keeps the shared, pooling client: this change is about connections that carry an engine credential.
    #[tokio::test]
    async fn an_ungoverned_endpoint_keeps_its_pooled_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = RawServer::start(listener, "plain");
        let adapter = adapter(&format!("http://127.0.0.1:{port}"));
        for _ in 0..4 {
            adapter.list().await.unwrap();
        }
        assert_eq!(server.count(), 4);
        assert_eq!(
            server.connections(),
            1,
            "four requests over one connection: an endpoint nobody governs is not forced onto fresh connections"
        );
    }

    /// A governed endpoint opens a connection per request (a client that keeps no idle connection): the credential only ever travels over
    /// a connection opened after the authorization.
    #[tokio::test]
    async fn a_governed_endpoint_opens_a_fresh_connection_for_every_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = RawServer::start(listener, "engine");
        let url = format!("http://127.0.0.1:{port}");
        let _governed = Governed(url.clone());
        govern_endpoint_guarded(
            &url,
            EndpointKey::Private("engine-key".into()),
            Some(Arc::new(Dialing { dial: v4(port) })),
        );
        let adapter = adapter(&url);
        for _ in 0..4 {
            adapter.list().await.unwrap();
        }
        assert_eq!(server.count(), 4);
        assert_eq!(server.connections(), 4);
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Codex pre-review of layer 2: L1 (no bearer in `Debug`), H3 (the last-moment check), H1 (governed by the endpoint, one classification of
// loopback)
// ---------------------------------------------------------------------------------------------------------------------------------

/// L1: the derived `Debug` printed the bearer into diagnostics and failing assertions. It is redacted now, wherever it is formatted (alone,
/// pretty-printed, inside a `Result`).
#[tokio::test]
async fn an_authorization_never_prints_its_bearer() {
    let url = endpoint(19850);
    let _governed = Governed(url.clone());
    govern_endpoint(&url, EndpointKey::Private("canary-bearer-value".into()));
    let authorization = authorize_omlx_request(&url, None).await.unwrap();
    assert_eq!(authorization.key(), Some("canary-bearer-value"));
    for printed in [format!("{authorization:?}"), format!("{authorization:#?}")] {
        assert!(!printed.contains("canary-bearer-value"), "{printed}");
        assert!(printed.contains("REDACTED"), "{printed}");
        assert!(
            !printed.contains("19850"),
            "the endpoint is not printed either: {printed}"
        );
    }
    let result: Result<_, KeyRefusal> = Ok(authorization);
    assert!(!format!("{result:?}").contains("canary-bearer-value"));
    // An authorization without a key says so, and still prints no key.
    let keyless = idoris_upstream::omlx::key::authorize_omlx_request_without_key(&url, None)
        .await
        .unwrap();
    assert_eq!(keyless.key(), None);
    assert!(!format!("{keyless:?}").contains("canary-bearer-value"));
}

/// H3: an authorization is the old engine's the moment a restart overtakes it. `ensure_current` is the synchronous look the senders take right
/// before they send (after the authorization, the request is built, the body serialized: time in which the engine can be replaced): the
/// endpoint was blocked, or a new engine registered (another generation) — refuse; a fresh authorization is current; an ungoverned endpoint
/// has nothing to overtake it.
#[tokio::test]
async fn an_authorization_is_not_current_once_a_restart_has_overtaken_it() {
    let url = endpoint(19851);
    let _governed = Governed(url.clone());
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("old-key".into()),
        Some(Scripted::answering(&[Verification::Verified])),
    );
    let authorization = authorize_omlx_request(&url, None).await.unwrap();
    assert!(authorization.ensure_current().is_ok());

    // The restart: the old engine's registration is blocked ...
    block_endpoint(&url);
    assert!(matches!(
        authorization.ensure_current().unwrap_err(),
        KeyRefusal::StaleGeneration { .. }
    ));
    // ... and the replacement registers its own key: the authorization held here is still the old one's.
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("new-key".into()),
        Some(Scripted::answering(&[Verification::Verified])),
    );
    assert!(matches!(
        authorization.ensure_current().unwrap_err(),
        KeyRefusal::StaleGeneration { .. }
    ));
    let fresh = authorize_omlx_request(&url, None).await.unwrap();
    assert!(fresh.ensure_current().is_ok());
    assert_eq!(fresh.key(), Some("new-key"));

    let ungoverned = authorize_omlx_request("http://127.0.0.1:19852", None)
        .await
        .unwrap();
    assert!(ungoverned.ensure_current().is_ok());
}

/// H1: "governed" is a property of the endpoint (the canonical identity), not of the card that asks. Every spelling of the port is
/// governed once the manager registered it; an authorization for a card that may not carry the key gets the same gate (blocked, stale
/// generation, listener re-verified, verified dial) and **no key**, not even the registered private one.
#[tokio::test]
async fn a_governed_endpoint_is_governed_whatever_card_asks_and_a_card_without_the_key_gets_the_gate_but_no_key()
 {
    use idoris_upstream::omlx::key::{authorize_omlx_request_without_key, is_governed_endpoint};

    let url = endpoint(19853);
    let _governed = Governed(url.clone());
    let spellings = [
        "http://127.0.0.1:19853",
        "http://localhost:19853",
        "http://[::1]:19853",
        "http://[::ffff:127.0.0.1]:19853",
        "http://127.0.0.2:19853",
    ];
    for spelling in spellings {
        assert!(
            !is_governed_endpoint(spelling),
            "{spelling}: nobody governs it yet"
        );
    }
    let guard = Scripted::answering(&[Verification::Verified]);
    govern_endpoint_guarded(
        &url,
        EndpointKey::Private("engine-key".into()),
        Some(guard.clone()),
    );
    for spelling in spellings {
        assert!(is_governed_endpoint(spelling), "{spelling}");
        let gated = authorize_omlx_request_without_key(spelling, None)
            .await
            .unwrap();
        assert_eq!(
            gated.key(),
            None,
            "{spelling}: the registered key is not for this card"
        );
    }
    assert_eq!(
        guard.asked(),
        spellings.len(),
        "the guard was asked for every one"
    );
    assert_eq!(endpoint_flights(&url), 0, "none is held any more");

    // The gate is real: a stale generation, a blocked endpoint and a listener that is no longer the engine all refuse it.
    let admitted = endpoint_generation(&url);
    guard_says_mismatch(&url, &guard);
    assert_eq!(
        authorize_omlx_request_without_key("http://[::ffff:127.0.0.1]:19853", Some(admitted))
            .await
            .unwrap_err(),
        KeyRefusal::ListenerChanged
    );
    assert_eq!(
        endpoint_flights(&url),
        0,
        "a refusal leaves nothing in flight"
    );
    block_endpoint(&url);
    for spelling in spellings {
        assert_eq!(
            authorize_omlx_request_without_key(spelling, None)
                .await
                .unwrap_err(),
            KeyRefusal::Blocked,
            "{spelling}"
        );
    }
    assert!(matches!(
        authorize_omlx_request_without_key(&url, Some(admitted))
            .await
            .unwrap_err(),
        KeyRefusal::StaleGeneration { .. }
    ));
}

/// The scripted guard answers `Mismatch` from now on (the managed listener left, another process took the port).
fn guard_says_mismatch(_url: &str, guard: &Arc<Scripted>) {
    *guard.answers.lock().unwrap() = [Verification::Mismatch].into_iter().collect();
}

fn omlx_card(
    endpoint: &str,
    extensions: Option<serde_json::Value>,
) -> idoris_contracts::ComponentCard {
    let mut card: idoris_contracts::ComponentCard = serde_json::from_value(serde_json::json!({
        "provider": {"id": "omlx-x", "family": "local", "tier": "local",
            "capabilities": ["chat"], "privacy_class": "local_only",
            "cost": {"input_per_m": 0, "output_per_m": 0}, "locality": "loopback"},
        "form": "http_service", "endpoint": endpoint, "version_pin": "omlx@0.6.4",
        "privacy_class": "local_only", "allowed_egress": ["loopback"],
        "fallback_policy": "fail_closed", "fail_closed": true
    }))
    .unwrap();
    card.extensions = extensions.map(|value| value.as_object().unwrap().clone());
    card
}

/// H1: one classification of "loopback" for the registry (the endpoint identity) and for key eligibility (`card_may_carry_omlx_key`,
/// `is_loopback_base_url`, which also decides whether `settings.json` may be read for the target). The registry folds the IPv4-mapped
/// spelling into the same port; eligibility used `IpAddr::is_loopback`, which does not: a card spelled that way sat on a governed port
/// as "not loopback, no key, not the registry's business".
#[test]
fn the_registry_and_key_eligibility_classify_every_spelling_of_loopback_the_same_way() {
    use idoris_upstream::omlx::key::{
        card_may_carry_omlx_key, endpoint_identity, is_loopback_base_url, is_loopback_host,
    };

    let plain = "http://127.0.0.1:19840";
    for spelling in [
        "http://127.0.0.1:19840",
        "http://localhost:19840",
        "http://localhost.:19840",
        "http://[::1]:19840",
        "http://[::ffff:127.0.0.1]:19840",
        "http://127.0.0.2:19840",
        "https://localhost:19840",
    ] {
        assert_eq!(
            endpoint_identity(spelling),
            endpoint_identity(plain),
            "{spelling}"
        );
        assert!(endpoint_identity(spelling).is_some(), "{spelling}");
        assert!(is_loopback_base_url(spelling), "{spelling}");
        assert!(
            card_may_carry_omlx_key(&omlx_card(spelling, None)),
            "{spelling}: the registry treats it as the managed port, so eligibility must too"
        );
        assert!(
            !card_may_carry_omlx_key(&omlx_card(
                spelling,
                Some(serde_json::json!({"idoris_upstream": {"kind": "openai_compat"}}))
            )),
            "{spelling}: a card with idoris_upstream is never eligible"
        );
    }
    // The same function everywhere: what the registry folds into 127.0.0.1 is loopback, what it does not is not.
    assert!(is_loopback_host("localhost."));
    assert!(is_loopback_host("::ffff:127.0.0.1"));
    assert!(is_loopback_host("[::ffff:127.0.0.1]"));
    assert!(!is_loopback_host("::ffff:8.8.8.8"));
    assert!(!is_loopback_base_url("http://[::ffff:8.8.8.8]:19840"));
    assert!(!card_may_carry_omlx_key(&omlx_card(
        "http://[::ffff:8.8.8.8]:19840",
        None
    )));
    assert!(!is_loopback_base_url("http://example.com:19840"));
}

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

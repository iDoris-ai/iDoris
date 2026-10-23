//! Captured external execution, exercised through the real router and Supervisor.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::dispatch::{self, ProviderAdmission, ProviderGuard, ProviderOutcome};
use idoris_backend::{BackendError, ChatRequest, ChatResponse, ModelInfo, RuntimeAdapter};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn proxy() -> ChatProxy {
    let mut p = ChatProxy::new(reqwest::Client::new());
    p.test_clock_ms.store(0, Ordering::SeqCst);
    p.retry_delays.clear();
    p
}
fn clock(p: &ChatProxy, ms: u64) {
    p.test_clock_ms.store(ms, Ordering::SeqCst);
}
fn ready(p: &ChatProxy, id: &str, class: CredentialClass, now: u64) -> Admission {
    p.breakers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .readiness(&provider_key(id), class, now)
}
fn open(p: &ChatProxy, id: &str, class: CredentialClass, until: u64) {
    let key = provider_key(id);
    let mut book = p.breakers.lock().unwrap();
    let ticket = book.admit(&key, class, 0).ticket.unwrap();
    book.on_retry_after(&key, class, ticket, 0, until);
}
fn snapshot(p: &ChatProxy) -> String {
    format!(
        "{:?}",
        p.breakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    )
}
async fn http_faults(p: &ChatProxy, id: &str, count: usize) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    for _ in 0..count {
        let response = p
            .forward_buffered(
                &server.uri(),
                &serde_json::json!({"messages":[]}),
                &ForwardOpts {
                    request_id: None,
                    tenant_id: None,
                    record_id: "external-test",
                    provider_id: id,
                    served_locality: Locality::Loopback,
                    privacy: PrivacyClass::Any,
                    require_openai_usage: false,
                },
            )
            .await;
        assert_eq!(response.status, 500);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), count);
}

#[tokio::test]
async fn external_capture_precedes_construction_and_stale_success_cannot_reset_real_http_fault() {
    let p = proxy();
    let captured = p.drive_external(
        "p",
        CredentialClass::Local,
        || {
            assert!(
                snapshot(&p).contains("Provider(\"p\")"),
                "capture must precede future construction"
            );
            async {}
        },
        |_| ProviderOutcome::Success,
    );
    captured.await;
    let entered = Notify::new();
    let release = Notify::new();
    let mut late = Box::pin(p.drive_external(
        "p",
        CredentialClass::Local,
        || async {
            entered.notify_one();
            release.notified().await;
        },
        |_| ProviderOutcome::Success,
    ));
    tokio::select! { _ = entered.notified() => {}, _ = &mut late => panic!("early result") }
    http_faults(&p, "p", 2).await;
    assert_eq!(
        ready(&p, "p", CredentialClass::Local, 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    release.notify_one();
    late.await;
    assert_eq!(
        ready(&p, "p", CredentialClass::Local, 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    clock(&p, 15_000);
    p.drive_external(
        "p",
        CredentialClass::Local,
        || async { true },
        |_| ProviderOutcome::Success,
    )
    .await;
    assert_eq!(
        ready(&p, "p", CredentialClass::Local, 15_000),
        Admission::Allowed
    );
    http_faults(&p, "p", 1).await;
    p.drive_external(
        "p",
        CredentialClass::Local,
        || async {},
        |_| ProviderOutcome::Success,
    )
    .await;
    http_faults(&p, "p", 1).await;
    assert_eq!(
        ready(&p, "p", CredentialClass::Local, 15_000),
        Admission::Allowed,
        "same generation success resets one fault"
    );
}

#[tokio::test]
async fn disabled_refused_and_poisoned_captures_execute_without_book_authority() {
    for outcome in [ProviderOutcome::Success, ProviderOutcome::Fault] {
        let p = proxy();
        p.set_breakers_enabled(false);
        let before = snapshot(&p);
        let result = p
            .drive_external(
                "absent",
                CredentialClass::OAuth,
                || async {
                    p.set_breakers_enabled(true);
                    17
                },
                |_| outcome,
            )
            .await;
        assert_eq!(result, 17);
        assert_eq!(
            snapshot(&p),
            before,
            "later enable must not mint an entry/ticket"
        );
        open(&p, "p", CredentialClass::OAuth, 50_000);
        let before = snapshot(&p);
        let calls = AtomicUsize::new(0);
        p.drive_external(
            "p",
            CredentialClass::OAuth,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
            },
            |_| outcome,
        )
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "capture refusal is not an execution gate"
        );
        assert_eq!(
            snapshot(&p),
            before,
            "untracked fault/success cannot reset or count"
        );
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lock = p.breakers.lock().unwrap();
            panic!("poison");
        }));
        let before = snapshot(&p);
        p.drive_external(
            "new-poisoned",
            CredentialClass::OAuth,
            || async { 1 },
            |_| outcome,
        )
        .await;
        assert_eq!(snapshot(&p), before);
        assert!(p.breakers.is_poisoned());
    }
}

#[tokio::test]
async fn captured_class_and_original_toggle_control_owned_settlement() {
    for class in [CredentialClass::Local, CredentialClass::OAuth] {
        let p = proxy();
        open(&p, "p", class, 1000);
        clock(&p, 1000);
        let capture = p.begin_provider("p", class);
        assert!(capture.probe);
        p.set_breakers_enabled(false);
        p.finish_provider(capture, ProviderOutcome::Success);
        assert_eq!(ready(&p, "p", class, 1000), Admission::Allowed);
        let other = if class == CredentialClass::Local {
            CredentialClass::OAuth
        } else {
            CredentialClass::Local
        };
        assert_eq!(ready(&p, "p", other, 1000), Admission::Allowed);
        p.set_breakers_enabled(true);
        open(&p, "p", class, 1000);
        let capture = p.begin_provider("p", class);
        clock(&p, 2000);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lock = p.breakers.lock().unwrap();
            panic!("poison after issuance");
        }));
        p.finish_provider(capture, ProviderOutcome::Fault);
        let floor = if class == CredentialClass::Local {
            15_000
        } else {
            60_000
        };
        assert_eq!(
            ready(&p, "p", class, 2000),
            Admission::Rejected {
                retry_after_ms: floor
            }
        );
        assert_eq!(
            ready(&p, "p", other, 2000),
            Admission::Allowed,
            "class must be captured, never URL-derived"
        );
        assert!(p.breakers.is_poisoned());
    }
}

#[tokio::test]
async fn ignored_and_drop_discard_normal_tickets_and_release_probe_at_drop_time() {
    for class in [CredentialClass::Local, CredentialClass::OAuth] {
        let p = proxy();
        for _ in 0..10 {
            p.drive_external("p", class, || async {}, |_| ProviderOutcome::Ignored)
                .await;
            drop(ProviderGuard::begin(Some(&p), "p", class));
        }
        assert_eq!(ready(&p, "p", class, 0), Admission::Allowed);
        open(&p, "p", class, 1000);
        clock(&p, 1000);
        let entered = Notify::new();
        let mut pending = Box::pin(p.drive_external(
            "p",
            class,
            || async {
                entered.notify_one();
                std::future::pending::<()>().await;
            },
            |_| ProviderOutcome::Success,
        ));
        tokio::select! { _ = entered.notified() => {}, _ = &mut pending => panic!("early result") }
        assert_eq!(
            ready(&p, "p", class, 1000),
            Admission::Rejected { retry_after_ms: 0 }
        );
        clock(&p, 9000);
        drop(pending);
        let floor = if class == CredentialClass::Local {
            15_000
        } else {
            60_000
        };
        assert_eq!(
            ready(&p, "p", class, 9000),
            Admission::Rejected {
                retry_after_ms: floor
            }
        );
        assert_eq!(
            ready(&p, "p", class, 9000 + floor - 1),
            Admission::Rejected { retry_after_ms: 1 }
        );
        assert_eq!(ready(&p, "p", class, 9000 + floor), Admission::Probe);
    }
}

#[tokio::test]
async fn classifier_panic_keeps_capture_until_drop_and_no_callback_holds_book_lock() {
    let p = Arc::new(proxy());
    open(&p, "p", CredentialClass::OAuth, 1000);
    clock(&p, 1000);
    let worker = tokio::spawn({
        let p = p.clone();
        async move {
            p.drive_external(
                "p",
                CredentialClass::OAuth,
                || {
                    assert!(p.breakers.try_lock().is_ok());
                    async {}
                },
                |_| {
                    assert!(p.breakers.try_lock().is_ok());
                    clock(&p, 7000);
                    panic!("classifier panic")
                },
            )
            .await;
        }
    });
    assert!(worker.await.unwrap_err().is_panic());
    assert_eq!(
        ready(&p, "p", CredentialClass::OAuth, 7000),
        Admission::Rejected {
            retry_after_ms: 60_000
        }
    );
    clock(&p, 67_000);
    p.drive_external(
        "p",
        CredentialClass::OAuth,
        || async { 4 },
        |_| ProviderOutcome::Success,
    )
    .await;
    assert_eq!(
        ready(&p, "p", CredentialClass::OAuth, 67_000),
        Admission::Allowed
    );
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    None,
    List,
    Status,
    Load,
    Chat,
}
#[derive(Clone, Debug)]
enum FaultMode {
    LoadPanic,
    VictimPanic,
    ChatPanic,
    LoadError(BackendError),
    ChatError(BackendError),
    StartupUpstream,
    VictimError,
}
struct BarrierAdapter {
    fault: Option<FaultMode>,
    inner: idoris_backend::MockAdapter,
    phase: Phase,
    entered: Notify,
    release: Notify,
    executing: std::sync::atomic::AtomicBool,
    first_load: std::sync::atomic::AtomicBool,
}
impl BarrierAdapter {
    fn new(phase: Phase) -> Self {
        Self {
            fault: None,
            inner: idoris_backend::MockAdapter::new(vec![
                ModelInfo {
                    id: "native".into(),
                    memory_gb: 1.0,
                },
                ModelInfo {
                    id: "victim".into(),
                    memory_gb: 1.0,
                },
            ]),
            phase,
            entered: Notify::new(),
            release: Notify::new(),
            executing: false.into(),
            first_load: true.into(),
        }
    }
    async fn barrier(&self, phase: Phase) {
        if self.phase == phase && self.executing.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}
#[async_trait::async_trait]
impl RuntimeAdapter for BarrierAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.inner.load_fence_path()
    }
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.executing.store(true, Ordering::SeqCst);
        self.barrier(Phase::List).await;
        self.inner.list().await
    }
    async fn status(&self) -> Result<idoris_backend::BackendStatus, BackendError> {
        self.barrier(Phase::Status).await;
        if matches!(self.fault, Some(FaultMode::StartupUpstream)) {
            return Err(BackendError::Upstream {
                message: "startup aggregate unavailable".into(),
            });
        }
        self.inner.status().await
    }
    async fn load(
        &self,
        id: &str,
        policy: Option<&idoris_contracts::load_policy::LoadPolicy>,
    ) -> Result<(), BackendError> {
        self.barrier(Phase::Load).await;
        if id == "native" {
            match &self.fault {
                Some(FaultMode::LoadPanic) => panic!("target load panic"),
                Some(FaultMode::LoadError(error)) => return Err(error.clone()),
                _ => {}
            }
        }
        if self.phase == Phase::Status && self.first_load.swap(false, Ordering::SeqCst) {
            return Err(BackendError::oom(id));
        }
        self.inner.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        if id == "victim" {
            match self.fault {
                Some(FaultMode::VictimPanic) => panic!("victim unload panic"),
                Some(FaultMode::VictimError) => {
                    return Err(BackendError::Upstream {
                        message: "victim unload failed".into(),
                    });
                }
                _ => {}
            }
        }
        self.inner.unload(id).await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.inner.probe_ready(id).await
    }
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.barrier(Phase::Chat).await;
        match &self.fault {
            Some(FaultMode::ChatPanic) => panic!("target chat panic"),
            Some(FaultMode::ChatError(error)) => return Err(error.clone()),
            _ => {}
        }
        self.inner.chat(request, cancel).await
    }
}
fn native_app(p: Arc<ChatProxy>, adapter: Arc<BarrierAdapter>) -> axum::Router {
    let mut card = crate::tests::sample_component_card("native");
    // Native execution class must remain Local despite a nonloopback decorative URL.
    card.endpoint = "https://decorative.invalid/native".into();
    let handle = idoris_backend::Supervisor::spawn(
        adapter,
        idoris_backend::SupervisorConfig {
            oom_retry_backoff: Duration::ZERO,
            ..Default::default()
        },
    )
    .unwrap();
    crate::build_app(crate::AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, handle).into(),
        proxy: p,
        ..Default::default()
    })
}
fn native_request() -> axum::http::Request<Body> {
    crate::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
        &[],
    )
}

#[tokio::test]
async fn real_native_lib_list_status_load_chat_hold_original_capture_before_new_http_fault() {
    for phase in [Phase::List, Phase::Status, Phase::Load, Phase::Chat] {
        let p = Arc::new(proxy());
        let adapter = Arc::new(BarrierAdapter::new(phase));
        let app = native_app(p.clone(), adapter.clone());
        let worker = tokio::spawn(app.oneshot(native_request()));
        adapter.entered.notified().await;
        assert!(
            snapshot(&p).contains("native"),
            "missing native capture before {phase:?}"
        );
        http_faults(&p, "native", 2).await;
        adapter.release.notify_one();
        let response = worker.await.unwrap().unwrap();
        assert_eq!(response.status(), 200, "{phase:?}");
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 0),
            Admission::Rejected {
                retry_after_ms: 15_000
            },
            "late {phase:?} success cannot reset HTTP fault generation"
        );
        assert_eq!(
            ready(&p, "native", CredentialClass::ApiKey, 0),
            Admission::Allowed
        );
    }
}

#[tokio::test]
async fn real_native_abort_releases_probe_from_current_clock() {
    let p = Arc::new(proxy());
    open(&p, "native", CredentialClass::Local, 1000);
    clock(&p, 1000);
    let adapter = Arc::new(BarrierAdapter::new(Phase::Chat));
    let app = native_app(p.clone(), adapter.clone());
    let worker = tokio::spawn(app.oneshot(native_request()));
    adapter.entered.notified().await;
    assert_eq!(
        ready(&p, "native", CredentialClass::Local, 1000),
        Admission::Rejected { retry_after_ms: 0 }
    );
    clock(&p, 12_000);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert_eq!(
        ready(&p, "native", CredentialClass::Local, 12_000),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    adapter.release.notify_one();
}

#[cfg(unix)]
#[tokio::test]
async fn actual_subscription_lib_relay_is_captured_with_oauth_before_suspended_completion() {
    use crate::subscription::{config::*, runtime::*};
    use idoris_upstream::subscription::{
        profile::{SandboxProfile, SubscriptionCli},
        relay::SubscriptionRelayConfig,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let root = tempfile::tempdir().unwrap();
    let entered_path = root.path().join("entered");
    let release_path = root.path().join("release");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&entered_path)
            .arg(&release_path)
            .status()
            .unwrap()
            .success()
    );
    let fifo = |path: &std::path::Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)
            .unwrap()
    };
    let entered = tokio::io::unix::AsyncFd::new(fifo(&entered_path)).unwrap();
    let mut release = fifo(&release_path);
    let program = root.path().join("claude");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf x > '{}'\nread go < '{}'\nprintf relay-success\n",
            entered_path.display(),
            release_path.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let card: idoris_contracts::ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap();
    let cfg = SubscriptionConfig::snapshot(
        Some("personal"),
        Some("1"),
        None,
        Some(SANDBOX_PROFILE_ID),
        Some("claude"),
    );
    let authorized = authorize_subscription(&cfg, &card).unwrap().unwrap();
    let relay = SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from(format!("{}:/usr/bin:/bin", root.path().display())),
        )]),
    );
    let handle =
        SubscriptionRuntimeHandle::build_with_relay_config(authorized, &card, relay).unwrap();
    let mut subscriptions = SubscriptionRuntimeRegistry::default();
    subscriptions.insert(handle).unwrap();
    let p = Arc::new(proxy());
    let app = crate::build_app(crate::AppState {
        cards: vec![card], subscriptions, proxy: p.clone(),
        routing_policy: serde_yaml::from_str("routing_policy:\n  version: 1\n  rules: []\n  default: { tiers: [remote], fail_closed: true }\n").unwrap(),
        ..Default::default()
    });
    let mut request = crate::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello"}]}"#,
        &[("x-idoris-privacy", "any")],
    );
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        crate::connection::ConnectionInfo::from_parts(
            "127.0.0.1:9000".parse().unwrap(),
            CancellationToken::new(),
        ),
    ));
    let mut worker = tokio::spawn(app.oneshot(request));
    // Readiness notification comes from the real relay's fixture, never a sleep.
    let notified = async {
        loop {
            let mut ready = entered.readable().await.unwrap();
            if let Ok(result) = ready.try_io(|fd| {
                let mut file = fd.get_ref();
                let mut byte = [0];
                file.read_exact(&mut byte)
            }) {
                result.unwrap();
                break;
            }
        }
    };
    tokio::select! {
        _ = notified => {},
        response = &mut worker => panic!("relay completed before fixture barrier: {:?}", response.unwrap().unwrap().status()),
    }
    let captured = snapshot(&p);
    assert!(
        captured.contains("subscription"),
        "actual lib caller must capture before relay construction/execution"
    );
    assert_eq!(
        ready(&p, "subscription", CredentialClass::Local, 0),
        Admission::Allowed
    );
    // The retained OAuth generation must not be reset by this older relay result.
    for _ in 0..3 {
        p.drive_external(
            "subscription",
            CredentialClass::OAuth,
            || async {},
            |_| ProviderOutcome::Fault,
        )
        .await;
    }
    assert_eq!(
        ready(&p, "subscription", CredentialClass::OAuth, 0),
        Admission::Rejected {
            retry_after_ms: 60_000
        }
    );
    release.write_all(b"go\n").unwrap();
    let response = worker.await.unwrap().unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        ready(&p, "subscription", CredentialClass::OAuth, 0),
        Admission::Rejected {
            retry_after_ms: 60_000
        }
    );
    assert_eq!(
        ready(&p, "subscription", CredentialClass::Local, 0),
        Admission::Allowed
    );
}

#[tokio::test]
async fn three_real_subscription_failures_block_the_fourth_before_spawn() {
    use crate::subscription::{config::*, runtime::*};
    use idoris_upstream::subscription::{
        profile::{SandboxProfile, SubscriptionCli},
        relay::SubscriptionRelayConfig,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let counter = root.path().join("spawn-count");
    let program = root.path().join("claude");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ncat >/dev/null\nn=$(cat '{}' 2>/dev/null || printf 0)\nn=$((n + 1))\nprintf '%s' \"$n\" > '{}'\nprintf relay-failure\nexit 1\n",
            counter.display(),
            counter.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

    let card: idoris_contracts::ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap();
    let cfg = SubscriptionConfig::snapshot(
        Some("personal"),
        Some("1"),
        None,
        Some(SANDBOX_PROFILE_ID),
        Some("claude"),
    );
    let authorized = authorize_subscription(&cfg, &card).unwrap().unwrap();
    let relay = SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from(format!("{}:/usr/bin:/bin", root.path().display())),
        )]),
    );
    let handle =
        SubscriptionRuntimeHandle::build_with_relay_config(authorized, &card, relay).unwrap();
    let mut subscriptions = SubscriptionRuntimeRegistry::default();
    subscriptions.insert(handle).unwrap();
    let p = Arc::new(proxy());
    let app = crate::build_app(crate::AppState {
        cards: vec![card.clone()],
        subscriptions,
        proxy: p.clone(),
        routing_policy: serde_yaml::from_str(
            "routing_policy:\n  version: 1\n  rules: []\n  default: { tiers: [remote], fail_closed: true }\n",
        )
        .unwrap(),
        ..Default::default()
    });

    for expected_spawns in 1..=3 {
        let mut request = crate::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello"}]}"#,
            &[("x-idoris-privacy", "any")],
        );
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            crate::connection::ConnectionInfo::from_parts(
                "127.0.0.1:9000".parse().unwrap(),
                CancellationToken::new(),
            ),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 502, "attempt {expected_spawns}");
        let actual_spawns: usize = std::fs::read_to_string(&counter).unwrap().parse().unwrap();
        assert_eq!(actual_spawns, expected_spawns);
    }

    assert_eq!(p.blocked_cards(std::slice::from_ref(&card)), [true]);
    let mut fourth = crate::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello"}]}"#,
        &[("x-idoris-privacy", "any")],
    );
    fourth.extensions_mut().insert(axum::extract::ConnectInfo(
        crate::connection::ConnectionInfo::from_parts(
            "127.0.0.1:9000".parse().unwrap(),
            CancellationToken::new(),
        ),
    ));
    let response = app.oneshot(fourth).await.unwrap();
    assert_eq!(
        response.status(),
        503,
        "the fourth request is breaker-rejected"
    );
    assert_eq!(std::fs::read_to_string(&counter).unwrap(), "3");
    assert_eq!(p.blocked_cards(std::slice::from_ref(&card)), [true]);
    assert_eq!(
        ready(&p, "subscription", CredentialClass::OAuth, 0),
        Admission::Rejected {
            retry_after_ms: 60_000
        }
    );
}

async fn fault_app(p: Arc<ChatProxy>, fault: FaultMode) -> axum::Router {
    let victim = matches!(fault, FaultMode::VictimPanic | FaultMode::VictimError);
    let startup = matches!(fault, FaultMode::StartupUpstream);
    let mut adapter = BarrierAdapter::new(Phase::None);
    adapter.fault = Some(fault);
    let card = crate::tests::sample_component_card("native");
    let handle = idoris_backend::Supervisor::spawn(
        Arc::new(adapter),
        idoris_backend::SupervisorConfig {
            budget_gb: if victim { 1.0 } else { 32.0 },
            oom_retry_backoff: Duration::ZERO,
            ..Default::default()
        },
    )
    .unwrap();
    let mut bound = dispatch::BoundSupervisor::new(&card, handle.clone());
    let profile = crate::profile::parse_profile(
        &Default::default(),
        None,
        idoris_contracts::DeployMode::Personal,
    )
    .unwrap();
    let selected = dispatch::select(std::slice::from_ref(&card), &profile, "hi").unwrap();
    if victim {
        handle
            .load("victim", 1.0, selected.load_policy)
            .await
            .unwrap();
    }
    if startup {
        bound = bound.with_admission_model("native", 1.0);
    }
    crate::build_app(crate::AppState {
        cards: vec![card],
        runtimes: bound.into(),
        proxy: p,
        ..Default::default()
    })
}

#[tokio::test]
async fn real_ambiguous_load_and_victim_panics_startup_upstream_and_victim_errors_are_ignored() {
    for fault in [
        FaultMode::LoadPanic,
        FaultMode::VictimPanic,
        FaultMode::VictimError,
        FaultMode::StartupUpstream,
        FaultMode::LoadError(BackendError::Upstream {
            message: "unqualified load upstream".into(),
        }),
        FaultMode::ChatError(BackendError::Upstream {
            message: "HTTP 400".into(),
        }),
    ] {
        let p = Arc::new(proxy());
        for _ in 0..3 {
            let app = fault_app(p.clone(), fault.clone()).await;
            let response = app.oneshot(native_request()).await.unwrap();
            assert_ne!(response.status(), 200, "fault fixture must fail: {fault:?}");
            assert_eq!(
                ready(&p, "native", CredentialClass::Local, 0),
                Admission::Allowed,
                "normal ignored must not count: {fault:?}"
            );
        }
    }
}

#[tokio::test]
async fn real_chat_panic_upstream_and_matching_typed_faults_count_wrong_ids_do_not() {
    let typed = |id: &str| {
        vec![
            BackendError::ModelUnavailable {
                model_id: id.into(),
            },
            BackendError::Oom {
                model_id: id.into(),
            },
            BackendError::ProbeTimedOut {
                model_id: id.into(),
            },
            BackendError::AdapterTimedOut {
                model_id: id.into(),
            },
            BackendError::LoadUnconfirmed {
                model_id: id.into(),
                message: "test".into(),
            },
            BackendError::LoadPostconditionFailed {
                model_id: id.into(),
                message: "test".into(),
            },
        ]
    };
    let mut faults = vec![FaultMode::ChatPanic];
    for error in typed("native") {
        faults.extend([
            FaultMode::LoadError(error.clone()),
            FaultMode::ChatError(error),
        ]);
    }
    for fault in faults {
        let p = Arc::new(proxy());
        for _ in 0..2 {
            let app = fault_app(p.clone(), fault.clone()).await;
            assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
        }
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 0),
            Admission::Rejected {
                retry_after_ms: 15_000
            },
            "matching fault: {fault:?}"
        );
    }
    let mut wrong = typed("another-provider");
    wrong.push(BackendError::AdapterPanicked {
        model_id: "another-provider".into(),
        message: "wrong identity".into(),
    });
    for error in wrong {
        for fault in [
            FaultMode::LoadError(error.clone()),
            FaultMode::ChatError(error),
        ] {
            let p = Arc::new(proxy());
            for _ in 0..3 {
                let app = fault_app(p.clone(), fault.clone()).await;
                assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
            }
            assert_eq!(
                ready(&p, "native", CredentialClass::Local, 0),
                Admission::Allowed,
                "wrong-id must not count: {fault:?}"
            );
        }
    }
}

#[tokio::test]
async fn real_local_model_faults_lock_out_while_ignored_results_preserve_prior_faults() {
    let matching = FaultMode::ChatError(BackendError::ModelUnavailable {
        model_id: "native".into(),
    });
    let p = Arc::new(proxy());
    for _ in 0..2 {
        let app = fault_app(p.clone(), matching.clone()).await;
        assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
    }
    assert_eq!(
        ready(&p, "native", CredentialClass::Local, 0),
        Admission::Rejected {
            retry_after_ms: 15_000,
        },
        "two matching Supervisor model failures open the local breaker"
    );

    clock(&p, 15_000);
    let app = fault_app(p.clone(), matching).await;
    assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
    assert_eq!(
        ready(&p, "native", CredentialClass::Local, 15_000),
        Admission::Rejected {
            retry_after_ms: 15_000,
        },
        "a failed real Supervisor probe starts a fresh full local recovery window"
    );
    assert_eq!(
        ready(&p, "native", CredentialClass::Local, 29_999),
        Admission::Rejected { retry_after_ms: 1 }
    );
    assert_eq!(
        ready(&p, "native", CredentialClass::Local, 30_000),
        Admission::Probe
    );

    for ignored in [
        FaultMode::ChatError(BackendError::ModelUnavailable {
            model_id: "another-model".into(),
        }),
        FaultMode::ChatError(BackendError::EvictionFailed {
            model_id: "native".into(),
        }),
    ] {
        let p = Arc::new(proxy());
        let app = fault_app(
            p.clone(),
            FaultMode::ChatError(BackendError::ModelUnavailable {
                model_id: "native".into(),
            }),
        )
        .await;
        assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 0),
            Admission::Allowed,
            "one matching failure is below the local threshold"
        );

        let app = fault_app(p.clone(), ignored.clone()).await;
        assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 0),
            Admission::Allowed,
            "ignored outcome must not count: {ignored:?}"
        );
        let app = fault_app(
            p.clone(),
            FaultMode::ChatError(BackendError::ModelUnavailable {
                model_id: "native".into(),
            }),
        )
        .await;
        assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 0),
            Admission::Rejected {
                retry_after_ms: 15_000,
            },
            "ignored outcome must preserve the previous matching failure: {ignored:?}"
        );

        clock(&p, 15_000);
        let app = fault_app(p.clone(), ignored.clone()).await;
        assert_ne!(app.oneshot(native_request()).await.unwrap().status(), 200);
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 15_000),
            Admission::Rejected {
                retry_after_ms: 15_000,
            },
            "ignored probe must release ownership as failure, not clear lockout: {ignored:?}"
        );
    }
}

#[tokio::test]
async fn native_observer_failure_releases_probe_and_completed_observer_cannot_undo_success() {
    for completed in [false, true] {
        let p = proxy();
        open(&p, "native", CredentialClass::Local, 1000);
        clock(&p, 1000);
        let card = crate::tests::sample_component_card("native");
        let handle = idoris_backend::Supervisor::spawn(
            Arc::new(idoris_backend::MockAdapter::new(vec![ModelInfo {
                id: "native".into(),
                memory_gb: 1.0,
            }])),
            Default::default(),
        )
        .unwrap();
        let bound = dispatch::BoundSupervisor::new(&card, handle);
        let profile = crate::profile::parse_profile(
            &Default::default(),
            None,
            idoris_contracts::DeployMode::Personal,
        )
        .unwrap();
        let selected = dispatch::select(std::slice::from_ref(&card), &profile, "hi").unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let event_log = Arc::new(idoris_tenancy::event_log::EventLogStore::new(conn).unwrap());
        // Empty tenant makes the actual append fail at its validation boundary.
        let dispatched = crate::DispatchedEventContext {
            event_log: event_log.clone(),
            tenant_id: String::new(),
            record_id: "r".into(),
            correlation: Default::default(),
        };
        let completed_context = crate::CompletedEventContext {
            event_log,
            tenant_id: String::new(),
            record_id: "r".into(),
            correlation: Default::default(),
        };
        let result = dispatch::dispatch_local_preselected_observed(
            &selected,
            dispatch::LocalExecutionContext::new(
                Some(&bound),
                None,
                None,
                if completed { None } else { Some(&dispatched) },
                if completed {
                    Some(&completed_context)
                } else {
                    None
                },
                None,
            )
            .with_provider_admission(&p),
            &profile,
            dispatch::DispatchInput::with_model(Some("native"), "hi"),
            vec![],
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(dispatch::ObservedDispatchError::EventLogUnavailable)
        ));
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 1000),
            if completed {
                Admission::Allowed
            } else {
                Admission::Rejected {
                    retry_after_ms: 15_000,
                }
            }
        );
    }
}

#[tokio::test]
async fn native_missing_supervisor_identity_and_budget_refusals_never_touch_book() {
    let p = proxy();
    let profile = crate::profile::parse_profile(
        &Default::default(),
        None,
        idoris_contracts::DeployMode::Personal,
    )
    .unwrap();
    let card = crate::tests::sample_component_card("native");
    let selected = dispatch::select(std::slice::from_ref(&card), &profile, "hi").unwrap();
    let other_card = crate::tests::sample_component_card("wrong-provider");
    let handle = idoris_backend::Supervisor::spawn(
        Arc::new(idoris_backend::MockAdapter::new(vec![])),
        Default::default(),
    )
    .unwrap();
    let wrong = dispatch::BoundSupervisor::new(&other_card, handle);
    let before = snapshot(&p);
    for bound in [None, Some(&wrong)] {
        let outcome = dispatch::dispatch_local_preselected_observed(
            &selected,
            dispatch::LocalExecutionContext::new(bound, None, None, None, None, None)
                .with_provider_admission(&p),
            &profile,
            dispatch::DispatchInput::with_model(Some("native"), "hi"),
            vec![],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(outcome.result.is_err());
        assert_eq!(snapshot(&p), before);
    }
    let card = crate::tests::paid_component_card("native");
    let selected = dispatch::select(std::slice::from_ref(&card), &profile, "hi").unwrap();
    let (_root, ledger) = crate::tests::configured_budget_ledger(0);
    let handle = idoris_backend::Supervisor::spawn(
        Arc::new(idoris_backend::MockAdapter::new(vec![])),
        Default::default(),
    )
    .unwrap();
    let bound = dispatch::BoundSupervisor::new(&card, handle);
    let outcome = dispatch::dispatch_local_preselected_observed(
        &selected,
        dispatch::LocalExecutionContext::new(Some(&bound), Some(&ledger), None, None, None, None)
            .with_provider_admission(&p),
        &profile,
        dispatch::DispatchInput::with_model(Some("native"), "hi"),
        vec![],
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome.result,
        Err(dispatch::DispatchFailure::Budget(_))
    ));
    assert_eq!(snapshot(&p), before);
}

#[tokio::test]
async fn actual_supervisor_cancellation_is_ignored_and_releases_owned_probe() {
    for probe in [false, true] {
        let p = proxy();
        if probe {
            open(&p, "native", CredentialClass::Local, 1000);
        }
        clock(&p, 1000);
        let card = crate::tests::sample_component_card("native");
        let profile = crate::profile::parse_profile(
            &Default::default(),
            None,
            idoris_contracts::DeployMode::Personal,
        )
        .unwrap();
        let selected = dispatch::select(std::slice::from_ref(&card), &profile, "hi").unwrap();
        let adapter = Arc::new(BarrierAdapter::new(Phase::Chat));
        let handle =
            idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
        let bound = dispatch::BoundSupervisor::new(&card, handle);
        let cancel = CancellationToken::new();
        let mut dispatch = Box::pin(dispatch::dispatch_local_preselected_observed(
            &selected,
            dispatch::LocalExecutionContext::new(Some(&bound), None, None, None, None, None)
                .with_provider_admission(&p),
            &profile,
            dispatch::DispatchInput::with_model(Some("native"), "hi"),
            vec![],
            cancel.clone(),
        ));
        tokio::select! { _ = adapter.entered.notified() => {}, _ = &mut dispatch => panic!("no chat barrier") }
        clock(&p, 9000);
        cancel.cancel();
        adapter.release.notify_one();
        let outcome = dispatch.await.unwrap();
        assert!(matches!(
            outcome.result,
            Err(dispatch::DispatchFailure::Backend(BackendError::Cancelled))
        ));
        assert_eq!(
            ready(&p, "native", CredentialClass::Local, 9000),
            if probe {
                Admission::Rejected {
                    retry_after_ms: 15_000,
                }
            } else {
                Admission::Allowed
            }
        );
    }
}

#[tokio::test]
async fn poisoned_issued_success_and_ignored_drop_preserve_exact_probe_floors() {
    for probe in [false, true] {
        for outcome in [
            Some(ProviderOutcome::Success),
            Some(ProviderOutcome::Ignored),
            None,
        ] {
            let p = proxy();
            if probe {
                open(&p, "p", CredentialClass::OAuth, 1000);
            }
            clock(&p, 1000);
            let guard = ProviderGuard::begin(Some(&p), "p", CredentialClass::OAuth);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _lock = p.breakers.lock().unwrap();
                panic!("poison with issued external ownership");
            }));
            clock(&p, 9000);
            match outcome {
                Some(outcome) => guard.finish(outcome),
                None => drop(guard),
            }
            let reopened = probe && !matches!(outcome, Some(ProviderOutcome::Success));
            assert_eq!(
                ready(&p, "p", CredentialClass::OAuth, 9000),
                if reopened {
                    Admission::Rejected {
                        retry_after_ms: 60_000,
                    }
                } else {
                    Admission::Allowed
                }
            );
            if reopened {
                assert_eq!(
                    ready(&p, "p", CredentialClass::OAuth, 68_999),
                    Admission::Rejected { retry_after_ms: 1 }
                );
                assert_eq!(
                    ready(&p, "p", CredentialClass::OAuth, 69_000),
                    Admission::Probe
                );
            }
            assert!(p.breakers.is_poisoned());
            let before = snapshot(&p);
            p.drive_external(
                "poisoned-new",
                CredentialClass::OAuth,
                || async {},
                |_| ProviderOutcome::Fault,
            )
            .await;
            assert_eq!(
                snapshot(&p),
                before,
                "poisoned admission remains fail closed"
            );
        }
    }
}

#[tokio::test]
async fn paid_missing_supervisor_preserves_budget_event_precedence_refund_and_no_capture() {
    use idoris_tenancy::event_log::{EventLogStore, EventType};
    for fail_event in [false, true] {
        let p = proxy();
        let before_book = snapshot(&p);
        let card = crate::tests::paid_component_card("native");
        let profile = crate::profile::parse_profile(
            &Default::default(),
            None,
            idoris_contracts::DeployMode::Personal,
        )
        .unwrap();
        let selected = dispatch::select(std::slice::from_ref(&card), &profile, "hi").unwrap();
        assert!(selected.estimated_cost_minor > 0);
        let (_budget_root, ledger) = crate::tests::configured_budget_ledger(1_000_000);
        let before = ledger
            .tenant_readview(crate::budget::PERSONAL_TENANT_ID)
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("events.sqlite3");
        let event_log = Arc::new(EventLogStore::open(&db).unwrap());
        if fail_event {
            rusqlite::Connection::open(&db).unwrap().execute_batch("CREATE TRIGGER fail_missing_supervisor_budget BEFORE INSERT ON event_log_events WHEN NEW.event_type='budget.reserved' BEGIN SELECT RAISE(ABORT,'test budget observer'); END;").unwrap();
        }
        let context = crate::BudgetReservedEventContext {
            event_log: event_log.clone(),
            tenant_id: crate::budget::PERSONAL_TENANT_ID.into(),
            record_id: "missing-supervisor".into(),
            correlation: Default::default(),
        };
        let result = dispatch::dispatch_local_preselected_observed(
            &selected,
            dispatch::LocalExecutionContext::new(
                None,
                Some(&ledger),
                Some(&context),
                None,
                None,
                None,
            )
            .with_provider_admission(&p),
            &profile,
            dispatch::DispatchInput::with_model(Some("native"), "hi"),
            vec![],
            CancellationToken::new(),
        )
        .await;
        if fail_event {
            assert!(
                matches!(
                    result,
                    Err(dispatch::ObservedDispatchError::EventLogUnavailable)
                ),
                "parent budget-observer error must precede missing Supervisor"
            );
        } else {
            let outcome = result.unwrap();
            assert!(matches!(
                outcome.result,
                Err(dispatch::DispatchFailure::Backend(
                    BackendError::SupervisorUnavailable
                ))
            ));
        }
        let events = event_log
            .events_for_record(
                Some(crate::budget::PERSONAL_TENANT_ID),
                "missing-supervisor",
            )
            .unwrap();
        assert_eq!(events.len(), usize::from(!fail_event));
        if let Some(event) = events.first() {
            assert_eq!(event.event.event_type, EventType::BudgetReserved);
            assert_eq!(
                event.event.metadata.get("reserved_minor"),
                Some(&serde_json::json!(selected.estimated_cost_minor))
            );
        }
        let after = ledger
            .tenant_readview(crate::budget::PERSONAL_TENANT_ID)
            .unwrap();
        assert_eq!(after.spent_minor, before.spent_minor);
        assert_eq!(after.reserved_minor, before.reserved_minor);
        assert_eq!(after.available_minor, before.available_minor);
        assert_eq!(
            snapshot(&p),
            before_book,
            "missing Supervisor cannot capture or insert a provider"
        );
    }
}

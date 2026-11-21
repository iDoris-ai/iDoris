//! IDORIS-CORE S6：引擎进程被换掉（重启）之后，Supervisor 的账本怎么回到真话（`SupervisorHandle::engine_replaced`）。
//!
//! 全部用 `MockAdapter`（它的 `chat` 只认引擎里真的加载了的模型，所以「账本说 Ready、引擎里没有」会真的失败）：
//! 账本过期的样子、复位之后下一次请求重新加载、共享容量账本里的占用跟着新进程走（宽松与精确两种口径）、新进程自带的驻留
//! 按启动采样的规矩记账、必须等静止（进行中的加载与聊天）、围栏与采样失败时 fail closed 并且之后可以恢复、`sample=false`
//! 不碰引擎、中毒的 actor 不被复位。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use idoris_backend::load_fence::LoadFence;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, GlobalCapacityLedger, MockAdapter,
    ModelInfo, RuntimeAdapter, Supervisor, SupervisorConfig, SupervisorHandle,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio_util::sync::CancellationToken;

const GB: u64 = 1_000_000_000;

fn catalog() -> Vec<ModelInfo> {
    [("a", 2.0), ("b", 2.0), ("big", 20.0)]
        .into_iter()
        .map(|(id, memory_gb)| ModelInfo {
            id: id.into(),
            memory_gb,
        })
        .collect()
}

fn policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
        admission: Admission::Coexist,
    }
}

fn config() -> SupervisorConfig {
    SupervisorConfig {
        budget_gb: 24.0,
        adapter_call_timeout: Duration::from_secs(1),
        ..SupervisorConfig::default()
    }
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![idoris_backend::ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }],
        native: None,
    }
}

/// `MockAdapter` plus a switch that makes `status()` fail (an engine that is up but not answering yet).
struct Engine {
    mock: MockAdapter,
    status_fails: AtomicBool,
}

impl Engine {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mock: MockAdapter::new(catalog()),
            status_fails: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl RuntimeAdapter for Engine {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }
    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.mock.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await
    }
    async fn status(&self) -> Result<BackendStatus, BackendError> {
        if self.status_fails.load(Ordering::SeqCst) {
            return Err(BackendError::Upstream {
                message: "the engine is not answering".into(),
            });
        }
        self.mock.status().await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.mock.probe_ready(id).await
    }
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.mock.chat(req, cancel).await
    }
}

/// 一个被共享容量账本跟踪的 Supervisor。`exact` 为真时账本按字节记账（精确口径），否则按 GB（宽松口径）；两种口径各跑一遍。
struct Rig {
    engine: Arc<Engine>,
    ledger: Arc<GlobalCapacityLedger>,
    handle: SupervisorHandle,
}

impl Rig {
    fn new(exact: bool) -> Self {
        let engine = Engine::new();
        let ledger = Arc::new(if exact {
            GlobalCapacityLedger::new_bytes(24 * GB)
        } else {
            GlobalCapacityLedger::new(24.0).unwrap()
        });
        let handle = Supervisor::spawn_with_startup_capacity_tracking(
            engine.clone(),
            config(),
            ledger.clone(),
            "r",
        )
        .unwrap();
        Self {
            engine,
            ledger,
            handle,
        }
    }

    fn claimed(&self) -> f64 {
        self.ledger.snapshot().unwrap().allocations["runtime:r"]
    }
}

#[tokio::test]
async fn a_ledger_that_outlived_its_engine_is_rebuilt_and_the_next_request_loads_again() {
    for exact in [false, true] {
        let rig = Rig::new(exact);
        rig.handle.load("a", 2.0, policy()).await.unwrap();
        rig.handle
            .chat(request("a"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(rig.claimed(), 2.0, "exact={exact}");

        // 引擎被换成一个新进程：什么都没加载。账本还以为 `a` 在里面：聊天白白发给一个没有它的引擎。
        rig.engine.mock.replace_engine();
        assert!(rig.handle.is_ready("a").await.unwrap(), "the stale belief");
        let stale = rig
            .handle
            .chat(request("a"), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(stale.reason_code(), "model_unavailable", "exact={exact}");

        rig.handle.engine_replaced(true).await.unwrap();

        // 账本是新进程的真话：没有任何东西加载，也没有占用；共享账本里的占用跟着放掉。
        assert!(!rig.handle.is_ready("a").await.unwrap(), "exact={exact}");
        let status = rig.handle.status().await.unwrap();
        assert!(status.loaded.is_empty(), "exact={exact}: {status:?}");
        assert_eq!(status.used_gb, 0.0, "exact={exact}");
        assert_eq!(rig.claimed(), 0.0, "exact={exact}");
        assert_eq!(rig.ledger.snapshot().unwrap().reserved_gb, 0.0);

        // 下一次请求重新加载：`load` 真的发给了新引擎，聊天恢复，占用又记上。
        rig.handle.load("a", 2.0, policy()).await.unwrap();
        rig.handle
            .chat(request("a"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(rig.engine.mock.load_call_count("a"), 2, "exact={exact}");
        assert_eq!(rig.claimed(), 2.0, "exact={exact}");
    }
}

#[tokio::test]
async fn memory_freed_by_the_replacement_is_available_to_the_other_runtimes_sharing_the_ledger() {
    for exact in [false, true] {
        let rig = Rig::new(exact);
        // 另一个运行时共用同一个全局账本：`big`（20 GB）加上 `a`（2 GB）已经把 24 GB 占满，它再加载 4 GB 的会被拒。
        let other_engine = Engine::new();
        let other = Supervisor::spawn_with_startup_capacity_tracking(
            other_engine.clone(),
            config(),
            rig.ledger.clone(),
            "s",
        )
        .unwrap();
        rig.handle.load("big", 20.0, policy()).await.unwrap();
        rig.handle.load("a", 2.0, policy()).await.unwrap();
        assert!(
            other.load("b", 4.0, policy()).await.is_err(),
            "exact={exact}: 22 + 4 does not fit in 24"
        );

        rig.engine.mock.replace_engine();
        rig.handle.engine_replaced(true).await.unwrap();

        other.load("b", 4.0, policy()).await.unwrap();
        let snapshot = rig.ledger.snapshot().unwrap();
        assert_eq!(snapshot.allocations["runtime:r"], 0.0, "exact={exact}");
        assert_eq!(snapshot.allocations["runtime:s"], 4.0, "exact={exact}");
    }
}

#[tokio::test]
async fn models_the_new_process_came_up_with_are_inherited_and_accounted_once() {
    for exact in [false, true] {
        let rig = Rig::new(exact);
        rig.handle.load("a", 2.0, policy()).await.unwrap();
        rig.engine.mock.replace_engine();
        rig.engine.mock.preload("b");

        rig.handle.engine_replaced(true).await.unwrap();

        // `b` 是新进程自带的：按启动采样的规矩，驻留算一整份（引擎测得的内存），`b` 本身是 inherited、还没验证过。
        let status = rig.handle.status().await.unwrap();
        assert_eq!(status.loaded, vec!["b".to_string()], "exact={exact}");
        assert_eq!(status.used_gb, 2.0, "exact={exact}");
        assert_eq!(rig.claimed(), 2.0, "exact={exact}");
        assert!(!rig.handle.is_ready("b").await.unwrap(), "exact={exact}");
        assert!(!rig.handle.is_ready("a").await.unwrap(), "exact={exact}");

        // 第一次 `load` 验证它：不重复记账。
        rig.handle.load("b", 2.0, policy()).await.unwrap();
        assert!(rig.handle.is_ready("b").await.unwrap(), "exact={exact}");
        assert_eq!(rig.claimed(), 2.0, "exact={exact}: no double count");
    }
}

#[tokio::test]
async fn a_reset_waits_for_quiet_and_changes_nothing_while_work_of_the_old_engine_is_in_flight() {
    let rig = Rig::new(false);
    rig.handle.load("a", 2.0, policy()).await.unwrap();

    // 一个进行中的聊天。
    rig.engine.mock.set_chat_delay("a", Duration::from_millis(400));
    let chat = {
        let handle = rig.handle.clone();
        tokio::spawn(async move { handle.chat(request("a"), CancellationToken::new()).await })
    };
    wait_until(|| {
        let engine = rig.engine.clone();
        async move { engine.mock.event_log().len() >= 2 }
    })
    .await;
    // 聊天要 400 毫秒；这期间复位被拒，账本一个字不动。
    tokio::time::sleep(Duration::from_millis(50)).await;
    let busy = rig.handle.engine_replaced(true).await.unwrap_err();
    assert_eq!(busy.reason_code(), "supervisor_busy");
    assert!(rig.handle.is_ready("a").await.unwrap());
    assert_eq!(rig.claimed(), 2.0);
    chat.await.unwrap().unwrap();
    rig.engine.mock.replace_engine();
    rig.handle.engine_replaced(true).await.unwrap();
    assert!(!rig.handle.is_ready("a").await.unwrap());

    // 一个进行中的加载。
    rig.engine.mock.set_load_delay("b", Duration::from_millis(400));
    let load = {
        let handle = rig.handle.clone();
        tokio::spawn(async move { handle.load("b", 2.0, policy()).await })
    };
    wait_until(|| {
        let engine = rig.engine.clone();
        async move { engine.mock.load_call_count("b") == 1 }
    })
    .await;
    let busy = rig.handle.engine_replaced(true).await.unwrap_err();
    assert_eq!(busy.reason_code(), "supervisor_busy");
    load.await.unwrap().unwrap();
    // 加载做完了（落在复位之前）：之后的复位把它连同账本一起忘掉，引擎里有 `b`，所以按驻留记成 inherited。
    rig.handle.engine_replaced(true).await.unwrap();
    let status = rig.handle.status().await.unwrap();
    assert_eq!(status.loaded, vec!["b".to_string()]);
    assert_eq!(rig.claimed(), 2.0);
}

/// 等一个条件成立（最多十秒）。
async fn wait_until<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !condition().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a condition"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_load_left_unconfirmed_by_the_old_engine_no_longer_blocks_its_model_after_the_replacement() {
    let rig = Rig::new(false);
    // 加载超过一次适配器调用的时限：结果未知，槽位留在 Error，围栏留着：这个模型被挡住了。
    rig.engine.mock.set_load_delay("a", Duration::from_millis(1500));
    let timed_out = rig.handle.load("a", 2.0, policy()).await.unwrap_err();
    assert_eq!(timed_out.reason_code(), "adapter_timed_out");
    rig.engine.mock.set_load_delay("a", Duration::ZERO);
    let blocked = rig.handle.load("a", 2.0, policy()).await.unwrap_err();
    assert_eq!(blocked.reason_code(), "model_unavailable");

    // 旧进程确认没了，围栏也清了（引擎管理器在拉起新引擎之前清）：这时才能放下。围栏还在，复位就 fail closed。
    rig.engine.mock.replace_engine();
    let fence = LoadFence::new(rig.engine.mock.load_fence_path().unwrap());
    let latched = rig.handle.engine_replaced(true).await.unwrap_err();
    assert!(latched.to_string().contains("fence"), "{latched}");
    // 失败的复位把错误粘滞住（和启动采样失败一样）：每个命令都得到它。
    assert!(rig.handle.status().await.is_err());
    assert!(rig.handle.load("b", 2.0, policy()).await.is_err());

    fence.clear().unwrap();
    rig.handle.engine_replaced(true).await.unwrap();
    rig.handle.load("a", 2.0, policy()).await.unwrap();
    rig.handle
        .chat(request("a"), CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failed_sample_latches_the_error_keeps_the_old_claim_and_a_later_reset_clears_it() {
    for exact in [false, true] {
        let rig = Rig::new(exact);
        rig.handle.load("a", 2.0, policy()).await.unwrap();
        rig.engine.mock.replace_engine();
        rig.engine.status_fails.store(true, Ordering::SeqCst);

        let failed = rig.handle.engine_replaced(true).await.unwrap_err();
        assert_eq!(failed.reason_code(), "upstream_error", "exact={exact}");
        // fail closed：每个命令都得到这个错误；共享账本里的占用没动（新进程的驻留不知道，不能当成空）。
        for error in [
            rig.handle.status().await.unwrap_err(),
            rig.handle.load("b", 2.0, policy()).await.unwrap_err(),
            rig.handle
                .chat(request("a"), CancellationToken::new())
                .await
                .unwrap_err(),
        ] {
            assert_eq!(error.reason_code(), "upstream_error", "exact={exact}");
        }
        assert_eq!(rig.claimed(), 2.0, "exact={exact}");

        rig.engine.status_fails.store(false, Ordering::SeqCst);
        rig.handle.engine_replaced(true).await.unwrap();
        assert_eq!(rig.claimed(), 0.0, "exact={exact}");
        rig.handle.load("a", 2.0, policy()).await.unwrap();
        assert_eq!(rig.claimed(), 2.0, "exact={exact}");
    }
}

#[tokio::test]
async fn saying_the_engine_is_gone_empties_the_ledger_without_asking_the_engine() {
    for exact in [false, true] {
        let rig = Rig::new(exact);
        rig.handle.load("a", 2.0, policy()).await.unwrap();
        rig.handle.load("b", 2.0, policy()).await.unwrap();
        assert_eq!(rig.claimed(), 4.0, "exact={exact}");
        // 引擎停了：它不会回答任何东西，复位也不去问它。
        rig.engine.status_fails.store(true, Ordering::SeqCst);

        rig.handle.engine_replaced(false).await.unwrap();

        // `status()` 只看账本，不问引擎：空的。
        let status = rig.handle.status().await.unwrap();
        assert!(status.loaded.is_empty(), "exact={exact}");
        assert_eq!(status.used_gb, 0.0, "exact={exact}");
        assert_eq!(rig.claimed(), 0.0, "exact={exact}");
        assert!(!rig.handle.is_ready("a").await.unwrap());
    }
}

#[tokio::test]
async fn a_supervisor_without_capacity_tracking_resets_too() {
    let engine = Engine::new();
    let handle = Supervisor::spawn(engine.clone(), config()).unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    engine.mock.replace_engine();
    handle.engine_replaced(true).await.unwrap();
    assert!(!handle.is_ready("a").await.unwrap());
    handle.load("a", 2.0, policy()).await.unwrap();
    handle
        .chat(request("a"), CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_supervisor_that_never_probes_does_not_sample_on_reset_either() {
    // 远程熔断开关在启动时闭合的 Supervisor 一个请求也不发（也不进共享账本）：复位同样不发。
    let engine = Engine::new();
    let ledger = Arc::new(GlobalCapacityLedger::new(24.0).unwrap());
    let handle = Supervisor::spawn_with_startup_capacity_tracking_without_probe(
        engine.clone(),
        config(),
        ledger.clone(),
        "r",
    )
    .unwrap();
    engine.status_fails.store(true, Ordering::SeqCst);
    handle.engine_replaced(true).await.unwrap();
    assert!(ledger.snapshot().unwrap().allocations.is_empty());
}

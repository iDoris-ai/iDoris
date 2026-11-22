//! IDORIS-CORE S6（Codex 预审 1、2）：Supervisor 的准入与引擎代际。
//!
//! 引擎被换掉（Admin `restart`、`stop` 之后 `start`、外部引擎被用户重启）的次序是：**关门、排空、停进程、复位、开门**。
//! 这里证明 Supervisor 这一侧：
//! - 关门之后新的加载、卸载、聊天、列表一律 `supervisor_unavailable`，不碰适配器、不碰账本；已经准入的做到完，`status`
//!   与 `is_ready` 照旧从账本回答；`is_quiescent` 说得清「排空了没有」；
//! - 每个准入的操作在**准入的那一刻**绑定引擎代际，它之后向适配器发的每一个请求（加载的 OOM 退避之后的重试、聊天写请求
//!   之前才被调度到、推迟启动的卸载）都带着那一代，一个按请求取凭证的适配器因此能拒绝它：旧一代的操作永远拿不到新一代
//!   的凭证，也不会把请求体发给新进程；
//! - 没有代际的适配器（`engine_generation() == None`）完全不受影响。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use idoris_backend::mock::LoadOutcome;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
    Supervisor, SupervisorConfig, SupervisorHandle, pinned_generation,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio_util::sync::CancellationToken;

fn catalog() -> Vec<ModelInfo> {
    [("a", 2.0), ("b", 2.0)]
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
        adapter_call_timeout: Duration::from_secs(2),
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

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_quiescent(handle: &SupervisorHandle, want: bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.is_quiescent().await.unwrap() != want {
        assert!(Instant::now() < deadline, "never became quiescent={want}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// 准入
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_closed_supervisor_refuses_new_work_and_lets_admitted_work_finish() {
    let adapter = Arc::new(MockAdapter::new(catalog()));
    adapter.set_chat_delay("a", Duration::from_millis(500));
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    assert!(handle.is_quiescent().await.unwrap());

    // 一个聊天在进行中。
    let in_flight = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.chat(request("a"), CancellationToken::new()).await })
    };
    wait_quiescent(&handle, false).await;

    handle.close_admission().await.unwrap();
    handle.close_admission().await.unwrap(); // 幂等

    // 新的聊天、加载、卸载、列表：一律 `supervisor_unavailable`。
    let refusals = [
        handle
            .chat(request("a"), CancellationToken::new())
            .await
            .map(|_| ()),
        handle.load("b", 2.0, policy()).await,
        handle.unload("a").await,
        handle.list().await.map(|_| ()),
    ];
    for result in refusals {
        assert_eq!(result.unwrap_err().reason_code(), "supervisor_unavailable");
    }
    // 没有一个碰到适配器或账本。
    assert_eq!(adapter.load_call_count("b"), 0);
    assert_eq!(adapter.unload_call_count("a"), 0);
    // `status`、`is_ready` 照旧从账本回答。
    assert!(handle.is_ready("a").await.unwrap());
    assert_eq!(handle.status().await.unwrap().loaded, vec!["a".to_owned()]);

    // 进行中的做到完，而且是成功的；排空之前 `is_quiescent` 一直是 false。
    assert!(!handle.is_quiescent().await.unwrap());
    in_flight.await.unwrap().unwrap();
    wait_quiescent(&handle, true).await;

    // 开门：新的活又被受理。
    handle.reopen_admission().await.unwrap();
    handle
        .chat(request("a"), CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_load_admitted_before_the_close_runs_to_its_end() {
    let adapter = Arc::new(MockAdapter::new(catalog()));
    adapter.set_load_delay("a", Duration::from_millis(400));
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    let load = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.load("a", 2.0, policy()).await })
    };
    wait_quiescent(&handle, false).await;
    handle.close_admission().await.unwrap();
    // 加载还在进行：没有排空；它做完之后账本里它是 Ready。
    assert!(!handle.is_quiescent().await.unwrap());
    load.await.unwrap().unwrap();
    wait_quiescent(&handle, true).await;
    assert!(handle.is_ready("a").await.unwrap());
    assert_eq!(adapter.load_call_count("a"), 1);
}

// ---------------------------------------------------------------------------------------------
// 引擎代际
// ---------------------------------------------------------------------------------------------

/// 一个带「引擎代际」的适配器：模拟 `OmlxAdapter` 的契约——在**它解析凭证的那一处**（这里是每个方法的开头）核对准入时绑定
/// 的代际，不是当前代际就拒绝，什么都不发。`sent` 记下通过了核对（也就是会到达引擎）的请求，`seen` 记下每次调用看到的绑定。
struct GenAdapter {
    inner: MockAdapter,
    /// `None` = 这个适配器没有代际（`engine_generation()` 回 `None`）。
    current: Option<AtomicU64>,
    seen: Mutex<Vec<(&'static str, Option<u64>)>>,
    sent: Mutex<Vec<&'static str>>,
    /// 非 `None` 时，`chat` 在核对**之前**先等它放行（模拟「聊天任务还没来得及写请求」）。
    chat_gate: Option<tokio::sync::Semaphore>,
    /// 同上，`list`（目录读取）：模拟「目录读取任务还没来得及写请求」。`list_entered` 记下有几个 `list` 已经走到这一步。
    list_gate: Option<tokio::sync::Semaphore>,
    list_entered: std::sync::atomic::AtomicUsize,
}

impl GenAdapter {
    fn new(generation: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            inner: MockAdapter::new(catalog()),
            current: generation.map(AtomicU64::new),
            seen: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            chat_gate: None,
            list_gate: None,
            list_entered: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn with_chat_gate(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: MockAdapter::new(catalog()),
            current: Some(AtomicU64::new(generation)),
            seen: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            chat_gate: Some(tokio::sync::Semaphore::new(0)),
            list_gate: None,
            list_entered: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn with_list_gate(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: MockAdapter::new(catalog()),
            current: Some(AtomicU64::new(generation)),
            seen: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            chat_gate: None,
            list_gate: Some(tokio::sync::Semaphore::new(0)),
            list_entered: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// 引擎被换掉了：新进程登记了它的凭证，代际前进（引擎管理器登记新 key 时做的事）。
    fn advance_generation(&self) -> u64 {
        self.current
            .as_ref()
            .unwrap()
            .fetch_add(1, Ordering::SeqCst)
            + 1
    }

    fn generation(&self) -> u64 {
        self.current.as_ref().unwrap().load(Ordering::SeqCst)
    }

    fn check(&self, op: &'static str) -> Result<(), BackendError> {
        let pinned = pinned_generation();
        self.seen.lock().unwrap().push((op, pinned));
        if let (Some(pinned), Some(current)) = (pinned, &self.current)
            && pinned != current.load(Ordering::SeqCst)
        {
            return Err(BackendError::Upstream {
                message: "the engine was replaced: this request was not sent".into(),
            });
        }
        self.sent.lock().unwrap().push(op);
        Ok(())
    }

    fn seen(&self, op: &str) -> Vec<Option<u64>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| *name == op)
            .map(|(_, pinned)| *pinned)
            .collect()
    }

    fn sent(&self, op: &str) -> usize {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|name| **name == op)
            .count()
    }
}

#[async_trait]
impl RuntimeAdapter for GenAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.inner.load_fence_path()
    }
    fn engine_generation(&self) -> Option<u64> {
        self.current
            .as_ref()
            .map(|current| current.load(Ordering::SeqCst))
    }
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.list_entered.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.list_gate {
            gate.acquire().await.unwrap().forget();
        }
        self.check("list")?;
        self.inner.list().await
    }
    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.check("load")?;
        self.inner.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.check("unload")?;
        self.inner.unload(id).await
    }
    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.check("status")?;
        self.inner.status().await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.check("probe")?;
        self.inner.probe_ready(id).await
    }
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        if let Some(gate) = &self.chat_gate {
            gate.acquire().await.unwrap().forget();
        }
        self.check("chat")?;
        self.inner.chat(req, cancel).await
    }
}

#[tokio::test]
async fn a_load_in_its_oom_backoff_when_the_engine_is_replaced_never_reaches_the_new_engine() {
    // Codex 预审 1 的场景：旧的加载已经准入（围栏已写），第一次请求 OOM，正睡在重试的退避里。引擎在这时被换掉。
    let adapter = GenAdapter::new(Some(1));
    adapter
        .inner
        .set_load_script("a", vec![LoadOutcome::Oom, LoadOutcome::Ok]);
    let handle = Supervisor::spawn(
        adapter.clone(),
        SupervisorConfig {
            oom_retry_backoff: Duration::from_millis(1500),
            ..config()
        },
    )
    .unwrap();
    let load = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.load("a", 2.0, policy()).await })
    };
    // 第一次 `load` 已经发出（OOM），之后确认内存释放，再睡退避。
    wait_for("the first load request", || {
        adapter.inner.load_call_count("a") == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let old = adapter.generation();
    let new = adapter.advance_generation();
    assert_ne!(old, new);

    // 退避结束之后的重试被适配器拒绝（它带着旧一代），一个字节也没有发给新引擎。
    let result = load.await.unwrap();
    assert_eq!(result.unwrap_err().reason_code(), "upstream_error");
    assert_eq!(
        adapter.inner.load_call_count("a"),
        1,
        "the retry never reached the engine"
    );
    assert_eq!(adapter.sent("load"), 1);
    let loads = adapter.seen("load");
    assert_eq!(loads.len(), 2, "the retry was attempted: {loads:?}");
    assert!(
        loads.iter().all(|pinned| *pinned == Some(old)),
        "every call of the old load carries the generation it was admitted under: {loads:?}"
    );

    // Supervisor 自己没事：账本复位（对着新进程重新采样）之后，新的加载带着新一代，成功。
    handle.engine_replaced(true).await.unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    assert_eq!(adapter.inner.load_call_count("a"), 2);
    assert_eq!(adapter.seen("load").last(), Some(&Some(new)));
}

#[tokio::test]
async fn a_chat_admitted_before_the_engine_was_replaced_is_refused_when_it_finally_runs() {
    // 聊天已经准入（`inflight` 已记），但它的任务还没来得及写请求，引擎就换了。
    let adapter = GenAdapter::with_chat_gate(1);
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    let chat = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.chat(request("a"), CancellationToken::new()).await })
    };
    wait_quiescent(&handle, false).await;
    let old = adapter.generation();
    adapter.advance_generation();
    adapter.chat_gate.as_ref().unwrap().add_permits(1);

    let error = chat.await.unwrap().unwrap_err();
    assert_eq!(error.reason_code(), "upstream_error");
    assert_eq!(
        adapter.sent("chat"),
        0,
        "nothing was sent to the new engine"
    );
    assert_eq!(adapter.seen("chat"), vec![Some(old)]);
    // 它完结之后账本里没有在途的聊天：可以排空、可以复位。
    wait_quiescent(&handle, true).await;
    handle.engine_replaced(true).await.unwrap();
}

#[tokio::test]
async fn a_catalog_read_admitted_before_the_engine_was_replaced_is_refused_when_it_finally_runs() {
    // 目录读取（聊天与预热在加载之前先读它）已经准入、它的任务在等，这时引擎被换掉：任务放开之后它的请求被拒绝，没有发给新引擎。
    let adapter = GenAdapter::with_list_gate(1);
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    let listing = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.list().await })
    };
    wait_for("the catalog read to be admitted and running", || {
        adapter.list_entered.load(Ordering::SeqCst) == 1
    })
    .await;
    let old = adapter.generation();
    adapter.advance_generation();
    adapter.list_gate.as_ref().unwrap().add_permits(1);

    let error = listing.await.unwrap().unwrap_err();
    assert_eq!(error.reason_code(), "upstream_error");
    assert_eq!(
        adapter.sent("list"),
        0,
        "nothing was sent to the new engine"
    );
    assert_eq!(adapter.seen("list"), vec![Some(old)]);
}

#[tokio::test]
async fn a_deferred_unload_stays_bound_to_the_generation_it_was_admitted_under() {
    // 卸载要等进行中的聊天做完才真的开始；它开始的时候当前代际可能已经变了，但它属于准入时的那一代。
    let adapter = GenAdapter::new(Some(1));
    adapter
        .inner
        .set_chat_delay("a", Duration::from_millis(600));
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    let chat = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.chat(request("a"), CancellationToken::new()).await })
    };
    wait_for("the chat to reach the engine", || adapter.sent("chat") == 1).await;
    let old = adapter.generation();
    let unload = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.unload("a").await })
    };
    // 卸载已经准入（状态 Stopping），在等聊天。
    tokio::time::sleep(Duration::from_millis(100)).await;
    adapter.advance_generation();

    chat.await.unwrap().unwrap(); // 已经在引擎里的聊天做完了：请求早就发出，不受代际变化影响
    let result = unload.await.unwrap();
    assert_eq!(result.unwrap_err().reason_code(), "upstream_error");
    assert_eq!(adapter.seen("unload"), vec![Some(old)]);
    assert_eq!(adapter.inner.unload_call_count("a"), 0);
}

#[tokio::test]
async fn work_admitted_after_the_replacement_carries_the_new_generation() {
    let adapter = GenAdapter::new(Some(4));
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    assert!(adapter.seen("load").iter().all(|pinned| *pinned == Some(4)));

    // 引擎换了：关门、（排空）、代际前进、复位、开门——这个次序里准入的东西才是新一代的。
    handle.close_admission().await.unwrap();
    adapter.inner.replace_engine();
    adapter.advance_generation();
    handle.engine_replaced(true).await.unwrap();
    handle.reopen_admission().await.unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    handle
        .chat(request("a"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(adapter.seen("load").last(), Some(&Some(5)));
    assert_eq!(adapter.seen("chat"), vec![Some(5)]);
    assert_eq!(adapter.sent("chat"), 1);
}

#[tokio::test]
async fn an_adapter_without_generations_is_never_bound() {
    let adapter = GenAdapter::new(None);
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("a", 2.0, policy()).await.unwrap();
    handle
        .chat(request("a"), CancellationToken::new())
        .await
        .unwrap();
    handle.list().await.unwrap();
    handle.unload("a").await.unwrap();
    for op in ["load", "probe", "chat", "list", "unload"] {
        assert!(
            adapter.seen(op).iter().all(Option::is_none),
            "{op}: {:?}",
            adapter.seen(op)
        );
    }
}

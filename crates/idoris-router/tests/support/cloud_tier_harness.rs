//! 云端档（计算服务 S7）测试的共用夹具。
//!
//! - **云**：S5 的云形态应用（`ingress_harness::World`：真实的账户存储 / 账本 / 尝试记录 / 云入口 / 控制面，假上游 `127.0.0.1`），
//!   这里把它挂在一个真实的 `127.0.0.1` TCP 监听上（本机 iDoris 的云端档用真实的 HTTP 去调它），最外层加一个计数中间件：
//!   云端**收到了多少个请求**（不管认证过没过）是「`local_only` 不出网」最硬的证据。可以 `stop()`（云端不可达）再 `restart()`（同一个端口恢复）。
//! - **本机**：`AppState` + 假的 oMLX 运行时（`MockAdapter`，进程内，**不启动任何推理进程**）+ 云端档，`build_app` 之后用
//!   `tower::ServiceExt::oneshot` 发请求。
//!
//! 全部对本地假服务进行，**不连接任何真实的外部服务，包括真实的 idoris.ai**。
#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]

#[path = "ingress_harness.rs"]
pub mod ingress;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::BodyExt;
use idoris_backend::mock::{LoadOutcome, UnloadOutcome};
use idoris_backend::{MockAdapter, ModelInfo};
use idoris_contracts::ComponentCard;
use idoris_contracts::RoutingPolicy;
use idoris_router::cloud::{Clock, SystemClock};
use idoris_router::cloud_tier::credential::{
    LoadedCredential, NodeFileProvider, PairedDeviceProvider,
};
use idoris_router::cloud_tier::{
    CloudCredential, CloudTier, CloudTierConfig, CredentialError, CredentialProvider,
};
use idoris_router::connection::{ConnectionInfo, ConnectionListener};
use idoris_router::runtime::RuntimeRegistry;
use idoris_router::{AppState, build_app};
use idoris_tenancy::budget::{BudgetLedger, SpendGate};
use idoris_tenancy::cloud_tier::CloudTierStore;
use idoris_tenancy::event_log::EventLogStore;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

pub use ingress::{Cred, Reply, Req, World, WorldOptions};

// ================================================================================================
// 云：S5 的云形态应用，挂在真实的 TCP 监听上，外面包一层请求计数
// ================================================================================================

/// 云端收到的一个 HTTP 请求（认证之前就记下，所以连 401 的也在）。
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub has_authorization: bool,
    pub has_member_header: bool,
    pub header_names: Vec<String>,
    pub body: Option<Value>,
}

#[derive(Clone, Default)]
pub struct RequestLog(Arc<Mutex<Vec<Seen>>>);

impl RequestLog {
    pub fn all(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// 发往聊天端点的请求。
    pub fn chats(&self) -> Vec<Seen> {
        self.all()
            .into_iter()
            .filter(|seen| seen.path == "/api/v1/chat/completions")
            .collect()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

async fn record(State(log): State<RequestLog>, request: Request<Body>, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = body
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    log.0.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        has_authorization: parts.headers.contains_key("authorization"),
        has_member_header: parts.headers.contains_key("x-idoris-member"),
        header_names: parts
            .headers
            .keys()
            .map(|name| name.as_str().to_ascii_lowercase())
            .collect(),
        body: serde_json::from_slice(&bytes).ok(),
    });
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// 挂在真实 TCP 监听上的云。
pub struct TcpCloud {
    pub world: World,
    pub log: RequestLog,
    pub addr: SocketAddr,
    server: Option<(CancellationToken, JoinHandle<()>)>,
}

impl TcpCloud {
    pub async fn start() -> Self {
        Self::start_with(WorldOptions::plenty()).await
    }

    pub async fn start_with(options: WorldOptions) -> Self {
        let world = World::new(options).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut cloud = Self {
            world,
            log: RequestLog::default(),
            addr,
            server: None,
        };
        cloud.serve(listener);
        cloud
    }

    fn serve(&mut self, listener: TcpListener) {
        let shutdown = CancellationToken::new();
        let app = self
            .world
            .app
            .clone()
            .layer(axum::middleware::from_fn_with_state(
                self.log.clone(),
                record,
            ));
        let conn = ConnectionListener::new(listener, shutdown.clone()).unwrap();
        let signal = shutdown.clone();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(
                conn,
                app.into_make_service_with_connect_info::<ConnectionInfo>(),
            )
            .with_graceful_shutdown(async move { signal.cancelled().await })
            .await;
        });
        self.server = Some((shutdown, handle));
    }

    /// 云端不再监听：之后的连接被拒绝（本机看到的是「云端不可达」）。
    pub async fn stop(&mut self) {
        if let Some((shutdown, handle)) = self.server.take() {
            shutdown.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
    }

    /// 在同一个端口上恢复监听。
    pub async fn restart(&mut self) {
        assert!(self.server.is_none(), "stop() first");
        let listener = TcpListener::bind(self.addr).await.unwrap();
        self.serve(listener);
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 数据面基址（`{base}/v1/chat/completions` 是聊天端点）。
    pub fn base_url(&self) -> String {
        format!("{}/api", self.origin())
    }

    /// 给 agent24 组织签发一张管理凭证（能给它名下的成员关系发配对码）。走运维 bootstrap，和 `idoris control bootstrap` 同一个函数。
    pub fn issue_agent24_admin(&self) -> Cred {
        struct Clock(Arc<idoris_router::cloud::ManualClock>);
        impl idoris_router::control::ControlClock for Clock {
            fn now(&self) -> idoris_compute_contracts::ids::TimestampMs {
                idoris_router::cloud::Clock::now(self.0.as_ref())
            }
        }
        let ops = [
            "create_account",
            "set_account_status",
            "manage_membership",
            "issue_credential",
            "revoke_credential",
            "freeze_credential",
            "grant_allocation",
            "create_principal",
            "revoke_principal",
            "read",
        ];
        let scopes: Vec<Value> = ops
            .iter()
            .map(|op| {
                if *op == "grant_allocation" {
                    json!({"op": op, "subtree": "agent24",
                           "max_amount": {"unit": "THB", "micros": 5_000_000}})
                } else {
                    json!({"op": op, "subtree": "agent24"})
                }
            })
            .collect();
        let spec = json!({"credentials": [{
            "binding": {"kind": "service", "account": "agent24"},
            "admin_scopes": scopes,
            "owner": "s7-agent24-admin",
            "tenant_id": "agent24",
            "allowed_privacy": ["local_only", "any"],
            "allowed_roles": ["fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto"],
        }]});
        let mut out = Vec::new();
        idoris_router::control::bootstrap::run(
            &self.world.accounts,
            &mut spec.to_string().as_bytes(),
            &mut out,
            &Clock(self.world.clock.clone()),
        )
        .unwrap();
        let line = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|line| line["kind"] == "credential")
            .unwrap();
        Cred {
            id: line["credential_id"].as_str().unwrap().to_string(),
            secret: line["secret"].as_str().unwrap().to_string(),
        }
    }

    /// 用管理凭证给一个成员关系创建一次性配对码（走控制面，不经过 TCP 计数层）。
    pub async fn mint_pairing_code(
        &self,
        admin: &Cred,
        membership: &str,
        ttl_seconds: u32,
    ) -> String {
        let reply = self
            .world
            .send(
                &Req::chat(admin)
                    .uri("/control/v1/pairing/codes")
                    .body(json!({
                        "membership": membership,
                        "ttl_seconds": ttl_seconds,
                        "allowed_privacy": ["local_only", "any"],
                        "allowed_roles": [],
                    })),
            )
            .await;
        assert_eq!(reply.status, 201, "{}", reply.text());
        reply.json()["data"]["code"].as_str().unwrap().to_string()
    }

    /// 运维紧急操作：撤销一张凭证（云端之后对它的任何请求都是 401）。
    pub fn operator_revoke_credential(&self, credential_id: &str) {
        struct Clock(Arc<idoris_router::cloud::ManualClock>);
        impl idoris_router::control::ControlClock for Clock {
            fn now(&self) -> idoris_compute_contracts::ids::TimestampMs {
                idoris_router::cloud::Clock::now(self.0.as_ref())
            }
        }
        let spec =
            json!({"credential_statuses": [{"credential_id": credential_id, "status": "revoked"}]});
        let mut out = Vec::new();
        idoris_router::control::bootstrap::run(
            &self.world.accounts,
            &mut spec.to_string().as_bytes(),
            &mut out,
            &Clock(self.world.clock.clone()),
        )
        .unwrap();
    }
}

// ================================================================================================
// 云端档配置
// ================================================================================================

/// 云端档配置 YAML：个人形态（设备凭证）或组织形态（Node 凭证文件）。
pub fn tier_config_yaml(cloud: &TcpCloud, state_dir: Option<&Path>, credential: &str) -> String {
    tier_config_for(
        &cloud.base_url(),
        state_dir,
        credential,
        "limits: {connect_timeout_ms: 1000, total_timeout_ms: 20000}",
    )
}

/// 同上，云端地址和 `limits:` 段由调用方给（对不是 `TcpCloud` 的假云，比如 wiremock）。
pub fn tier_config_for(
    base_url: &str,
    state_dir: Option<&Path>,
    credential: &str,
    limits: &str,
) -> String {
    let state_dir = state_dir
        .map(|dir| format!("state_dir: {}\n", dir.display()))
        .unwrap_or_default();
    format!(
        "schema_version: 1\nbase_url: {base_url}\nmodel: idoris/agent\n\
         role_models: {{fast: idoris/agent, daily: idoris/agent, deep: idoris/agent}}\n\
         {state_dir}{credential}\n{limits}\n"
    )
}

pub fn parse_tier_config(yaml: &str) -> CloudTierConfig {
    CloudTierConfig::parse(yaml, Path::new(".")).unwrap_or_else(|e| panic!("{e}\n{yaml}"))
}

/// 装一个云端档：真实的 HTTP 客户端、真实的调用记录库（`db` 文件）、系统时钟。
pub fn make_tier(
    config: CloudTierConfig,
    provider: Arc<dyn CredentialProvider>,
    db: &Path,
) -> Arc<CloudTier> {
    make_tier_with_clock(config, provider, Arc::new(SystemClock), db)
}

/// 同上，时钟由调用方给（测试用它在「出网之前的某一步」做手脚）。
pub fn make_tier_with_clock(
    config: CloudTierConfig,
    provider: Arc<dyn CredentialProvider>,
    clock: Arc<dyn Clock>,
    db: &Path,
) -> Arc<CloudTier> {
    let http = idoris_upstream::cloud::build_http_client(config.connect_timeout).unwrap();
    Arc::new(
        CloudTier::new(idoris_router::cloud_tier::CloudTierParts {
            config,
            store: Arc::new(CloudTierStore::open(db).unwrap()),
            provider,
            http,
            clock,
        })
        .unwrap(),
    )
}

/// 包住一个凭证来源：数 `load` 被调了几次（`load` 是闸门通过之后、出网之前的第一步，调了就说明请求走到了这一步）；
/// 可以在 `load` 里顺手把一个开关打开，模拟「通过闸门之后、出网之前」远程熔断被打开。
#[derive(Debug)]
pub struct ProbeProvider {
    inner: Arc<dyn CredentialProvider>,
    loads: std::sync::atomic::AtomicUsize,
    trip_on_load: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl ProbeProvider {
    pub fn counting(inner: Arc<dyn CredentialProvider>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            loads: std::sync::atomic::AtomicUsize::new(0),
            trip_on_load: None,
        })
    }

    pub fn tripping(
        inner: Arc<dyn CredentialProvider>,
        switch: Arc<std::sync::atomic::AtomicBool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            loads: std::sync::atomic::AtomicUsize::new(0),
            trip_on_load: Some(switch),
        })
    }

    pub fn loads(&self) -> usize {
        self.loads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl CredentialProvider for ProbeProvider {
    fn load(&self) -> Result<LoadedCredential, CredentialError> {
        self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(switch) = &self.trip_on_load {
            switch.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner.load()
    }
}

/// 第一次被问「现在几点」时把开关打开的时钟。云端档在出网路径上第一次读时钟是落 `pending` 行（`record_begin`），
/// 那是发请求之前最后几步之一：用它模拟「落行的时候熔断被打开」。
pub struct TrippingClock {
    switch: Arc<std::sync::atomic::AtomicBool>,
    calls: std::sync::atomic::AtomicUsize,
}

impl TrippingClock {
    pub fn new(switch: Arc<std::sync::atomic::AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            switch,
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Clock for TrippingClock {
    fn now(&self) -> idoris_compute_contracts::ids::TimestampMs {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.switch.store(true, std::sync::atomic::Ordering::SeqCst);
        SystemClock.now()
    }
}

/// 个人形态：设备凭证从 `state_dir` 读。
pub fn paired_provider(cloud: &TcpCloud, state_dir: &Path) -> Arc<dyn CredentialProvider> {
    Arc::new(PairedDeviceProvider::new(
        idoris_router::cloud_tier::DeviceStore::new(state_dir),
        cloud.origin(),
        None,
    ))
}

/// 组织形态：Node 凭证写进一个 0600 文件。
pub fn node_file(dir: &Path, secret: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("node-credential");
    std::fs::write(&path, format!("{secret}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

// ================================================================================================
// 本机
// ================================================================================================

/// 允许 `any` 请求用 `remote` 层的路由策略（`local_only` 仍然只有 local / lora，且 `fail_closed`）。
pub const POLICY_ALLOWING_REMOTE: &str = "routing_policy:
  version: 1
  rules:
    - if: { privacy: local_only }
      then: { tiers: [local, lora], fail_closed: true }
    - if: { privacy: any }
      then: { tiers: [local, remote] }
  default: { tiers: [local], fail_closed: true }
";

/// 仓库自带的策略（`config/routing-policy.yaml`）：只有 `complexity: complex` 的请求允许 `remote`。
pub const POLICY_SHIPPED: &str = include_str!("../../../../config/routing-policy.yaml");

/// 允许 `remote`，但命中的动作写了 `fail_closed: true`。
pub const POLICY_REMOTE_BUT_FAIL_CLOSED: &str = "routing_policy:
  version: 1
  rules:
    - if: { privacy: any }
      then: { tiers: [local, remote], fail_closed: true }
  default: { tiers: [local], fail_closed: true }
";

pub fn policy(yaml: &str) -> RoutingPolicy {
    serde_yaml::from_str(yaml).unwrap()
}

/// 本机的一种「服务不了」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// 本机运行时不在线（没有 supervisor）。
    RuntimeOffline,
    /// 运行时在线但加载失败（`Upstream`）。
    RuntimeError,
    /// 内存额度放不下（6 GB 的模型，5 GB 额度）。
    MemoryTooSmall,
    /// 运行时报 OOM（重试一次仍然 OOM）。
    Oom,
    /// 只有动用保留余量才放得下，没带确认：要问用户。
    HeadroomNeeded,
    /// 带了 `X-iDoris-Headroom-Breach: confirmed`，用满全部额度仍然放不下。
    HeadroomConfirmedStillTooBig,
    /// 需要驱逐，驱逐失败（瞬时）。
    EvictionFailed,
    /// 本机过载：另一个模型正在加载（`supervisor_busy`）。
    Overloaded,
    /// 本机预算耗尽（402）。
    BudgetExhausted,
    /// 请求声明了角色，本机没有绑定这个角色的模型。
    RoleUnbound,
    /// 对照：本机正常。
    Healthy,
}

impl Scenario {
    pub const FAILING: [Scenario; 10] = [
        Scenario::RuntimeOffline,
        Scenario::RuntimeError,
        Scenario::MemoryTooSmall,
        Scenario::Oom,
        Scenario::HeadroomNeeded,
        Scenario::HeadroomConfirmedStillTooBig,
        Scenario::EvictionFailed,
        Scenario::Overloaded,
        Scenario::BudgetExhausted,
        Scenario::RoleUnbound,
    ];

    /// 测试里手写的预期（**不是**从 `gate.rs` 的分类表抄的）：在有同意、隐私 `any`、策略允许的前提下，
    /// 这种本机失败是不是应该去云端。
    pub fn goes_to_the_cloud(self) -> bool {
        matches!(
            self,
            Scenario::RuntimeOffline
                | Scenario::RuntimeError
                | Scenario::MemoryTooSmall
                | Scenario::Oom
                | Scenario::HeadroomConfirmedStillTooBig
                | Scenario::RoleUnbound
        )
    }

    fn budget(self) -> Option<u64> {
        match self {
            Scenario::MemoryTooSmall
            | Scenario::HeadroomNeeded
            | Scenario::HeadroomConfirmedStillTooBig => Some(5_000_000_000),
            Scenario::EvictionFailed => Some(8_000_000_000),
            _ => None,
        }
    }

    fn model(self) -> &'static str {
        match self {
            Scenario::MemoryTooSmall | Scenario::HeadroomConfirmedStillTooBig => "huge",
            Scenario::HeadroomNeeded => "big",
            Scenario::EvictionFailed => "b",
            Scenario::BudgetExhausted => "paid-1",
            Scenario::RoleUnbound => "idoris/daily",
            _ => "small",
        }
    }

    fn extra_headers(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Scenario::HeadroomConfirmedStillTooBig => &[("x-idoris-headroom-breach", "confirmed")],
            _ => &[],
        }
    }
}

pub struct LocalNode {
    pub app: Router,
    pub adapter: Option<Arc<MockAdapter>>,
    pub tier: Option<Arc<CloudTier>>,
    pub event_log: Option<Arc<EventLogStore>>,
    pub ledger: Option<Arc<BudgetLedger>>,
    /// 这个节点的远程熔断开关（和 `AppState::remote_kill_switch` 是同一个共享原子变量）。
    pub kill_switch: Arc<std::sync::atomic::AtomicBool>,
    pub scenario: Scenario,
    pub dir: tempfile::TempDir,
}

pub struct NodeOptions {
    pub scenario: Scenario,
    pub tier: Option<Arc<CloudTier>>,
    pub policy: &'static str,
    pub kill_switch: bool,
    pub with_event_log: bool,
    /// 用调用方给的共享开关当 `remote_kill_switch`（好让别的部件——凭证来源、时钟——持有同一个开关）。
    pub shared_kill_switch: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 本机运行时加载 `small` 要花多久（让测试能在「本机正在处理」的窗口里动手）。
    pub local_load_delay: Option<Duration>,
}

impl NodeOptions {
    pub fn new(scenario: Scenario, tier: Option<Arc<CloudTier>>) -> Self {
        Self {
            scenario,
            tier,
            policy: POLICY_ALLOWING_REMOTE,
            kill_switch: false,
            with_event_log: false,
            shared_kill_switch: None,
            local_load_delay: None,
        }
    }
}

fn omlx_card() -> ComponentCard {
    serde_yaml::from_str(include_str!("../../../../config/components/omlx.yaml")).unwrap()
}

fn catalog() -> Vec<ModelInfo> {
    [
        ("big", 4.5),
        ("small", 1.0),
        ("huge", 6.0),
        ("a", 4.0),
        ("b", 5.0),
        ("paid-1", 1.0),
    ]
    .map(|(id, memory_gb)| ModelInfo {
        id: id.into(),
        memory_gb,
    })
    .to_vec()
}

impl LocalNode {
    pub fn build(options: NodeOptions) -> LocalNode {
        let scenario = options.scenario;
        let dir = tempfile::tempdir().unwrap();
        let mut card = omlx_card();
        let mut state = AppState {
            routing_policy: policy(options.policy),
            cloud_tier: options.tier.clone(),
            ..AppState::default()
        };
        if let Some(shared) = options.shared_kill_switch.clone() {
            state.remote_kill_switch = shared;
        }
        if options.kill_switch {
            state
                .remote_kill_switch
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let kill_switch = state.remote_kill_switch.clone();
        let mut adapter = None;
        let mut ledger = None;
        match scenario {
            Scenario::RuntimeOffline => {
                state.runtimes = RuntimeRegistry::default();
            }
            _ => {
                let mock = Arc::new(MockAdapter::new(catalog()));
                match scenario {
                    Scenario::RuntimeError => {
                        mock.set_load_script("small", vec![LoadOutcome::Fail])
                    }
                    Scenario::Oom => {
                        mock.set_load_script("small", vec![LoadOutcome::Oom, LoadOutcome::Oom])
                    }
                    Scenario::EvictionFailed => {
                        mock.set_unload_script("a", vec![UnloadOutcome::Fail]);
                    }
                    Scenario::Overloaded => {
                        mock.set_load_delay("big", Duration::from_millis(600));
                    }
                    _ => {}
                }
                if let Some(delay) = options.local_load_delay {
                    mock.set_load_delay("small", delay);
                }
                if scenario == Scenario::BudgetExhausted {
                    // 付费的本机候选 + 额度只有 1：预留一定失败（402）
                    card.provider.cost = idoris_contracts::provider::Cost {
                        input_per_m: 1_000_000.0,
                        output_per_m: 2_000_000.0,
                    };
                    let tenant_ledger =
                        BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
                    tenant_ledger
                        .configure_tenant(
                            idoris_router::budget::PERSONAL_TENANT_ID,
                            1,
                            "UTC",
                            SpendGate::PaidOnly,
                        )
                        .unwrap();
                    let tenant_ledger = Arc::new(tenant_ledger);
                    state.budget_ledger = Some(tenant_ledger.clone());
                    ledger = Some(tenant_ledger);
                }
                state.runtimes = RuntimeRegistry::spawn_with_factory_and_budget(
                    std::slice::from_ref(&card),
                    scenario.budget(),
                    {
                        let mock = mock.clone();
                        move |_| Ok(mock.clone())
                    },
                )
                .unwrap();
                adapter = Some(mock);
            }
        }
        state.cards = vec![card];
        let mut event_log = None;
        if options.with_event_log {
            let store = Arc::new(EventLogStore::open(dir.path().join("events.sqlite3")).unwrap());
            state.event_log = Some(store.clone());
            event_log = Some(store);
        }
        LocalNode {
            app: build_app(state),
            adapter,
            tier: options.tier,
            event_log,
            ledger,
            kill_switch,
            scenario,
            dir,
        }
    }

    /// 发一个请求；**本机运行时开始加载 `small` 之后**（它已经在处理这个请求了）把远程熔断打开，然后等响应。
    /// 要用带 `local_load_delay` 的节点（否则加载一瞬间就完了，来不及动手）。
    pub async fn drive_and_trip_the_kill_switch_while_the_local_load_runs(
        &self,
        headers: &[(&str, &str)],
    ) -> Captured {
        let adapter = self.adapter.clone().expect("this needs a local runtime");
        let app = self.app.clone();
        let owned: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        let request = tokio::spawn(async move {
            let borrowed: Vec<(&str, &str)> = owned
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            send_chat(&app, chat_body("small"), &borrowed).await
        });
        for _ in 0..300 {
            if adapter.load_call_count("small") >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            adapter.load_call_count("small") >= 1,
            "the local load never started"
        );
        self.kill_switch
            .store(true, std::sync::atomic::Ordering::SeqCst);
        request.await.unwrap()
    }

    /// 在这个场景里发一个请求（`Overloaded` 会先在后台占住加载互斥锁，再发探针）。
    pub async fn drive(&self, headers: &[(&str, &str)]) -> Captured {
        self.drive_with(headers, false).await
    }

    /// 同 [`Self::drive`]；`stream` 为真时请求体带 `"stream": true`。
    pub async fn drive_with(&self, headers: &[(&str, &str)], stream: bool) -> Captured {
        let scenario = self.scenario;
        let mut all: Vec<(&str, &str)> = scenario.extra_headers().to_vec();
        all.extend_from_slice(headers);
        let mut body = chat_body(scenario.model());
        if stream {
            body["stream"] = Value::Bool(true);
        }
        match scenario {
            Scenario::Overloaded => {
                let adapter = self.adapter.clone().unwrap();
                let app = self.app.clone();
                let blocker =
                    tokio::spawn(async move { send_chat(&app, chat_body("big"), &[]).await });
                // 等到 big 真的开始加载（互斥锁被占住）
                for _ in 0..200 {
                    if adapter.load_call_count("big") >= 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    adapter.load_call_count("big") >= 1,
                    "the slow load never started"
                );
                let probe = send_chat(&self.app, body, &all).await;
                let _ = blocker.await;
                probe
            }
            Scenario::EvictionFailed => {
                // 先把 a（4 GB）加载进来；b 要 5 GB，a + b 超过整个 8 GB 额度，必须驱逐 a，而 a 的卸载会失败
                let first = send_chat(&self.app, chat_body("a"), &[]).await;
                assert_eq!(
                    first.status,
                    200,
                    "preloading a must succeed: {}",
                    first.text()
                );
                send_chat(&self.app, body, &all).await
            }
            _ => send_chat(&self.app, body, &all).await,
        }
    }
}

pub fn chat_body(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hello cloud tier"}]})
}

/// 一个被完整读出来的响应。
#[derive(Debug)]
pub struct Captured {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Captured {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// 状态码、全部响应头（除了每次都不一样的本机 record id）、响应体：「逐字节相同」比的就是它。
    pub fn fingerprint(&self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut headers: Vec<(String, String)> = self
            .headers
            .iter()
            .filter(|(name, _)| name.as_str() != "x-idoris-record-id")
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).to_string(),
                )
            })
            .collect();
        headers.sort();
        (self.status, headers, self.body.clone())
    }
}

pub async fn send_chat(app: &Router, body: Value, headers: &[(&str, &str)]) -> Captured {
    send(app, "POST", "/v1/chat/completions", Some(body), headers).await
}

pub async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Captured {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Captured {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

/// 「本机没有任何本地密钥」的探针：配对不发任何本地密钥，配对测试用它填 `CliContext::probe`。
pub struct NoLocalKeys;

impl idoris_router::cloud_tier::LocalSecretProbe for NoLocalKeys {
    fn is_local_secret(&self, _secret: &str) -> bool {
        false
    }
}

/// 兑换配对码并写下设备凭证（走真实的 `idoris cloud pair` 函数，对 TCP 上的云）。返回 stdout 的 JSON。
pub async fn pair_device(cloud: &TcpCloud, state_dir: &Path, code: &str) -> Result<Value, String> {
    let config = parse_tier_config(&tier_config_yaml(cloud, Some(state_dir), ""));
    let http = idoris_upstream::http_client().unwrap();
    let mut stdin = std::io::Cursor::new(format!("{code}\n").into_bytes());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let db_path = state_dir.join("idoris.sqlite3");
    let result = idoris_router::cloud_tier::cli::run_pair(
        idoris_router::cloud_tier::cli::CliContext {
            config: &config,
            db_path: &db_path,
            http,
            now_ms: 1_700_000_000_000,
            probe: &NoLocalKeys,
        },
        None,
        &mut stdin,
        &mut stdout,
        &mut stderr,
    )
    .await;
    let out = String::from_utf8(stdout).unwrap();
    let err = String::from_utf8(stderr).unwrap();
    result
        .map(|_| serde_json::from_str(out.trim()).unwrap())
        .inspect(|_| {
            assert!(
                !out.contains("idk_") && !err.contains("idk_"),
                "pair printed a secret"
            );
        })
}

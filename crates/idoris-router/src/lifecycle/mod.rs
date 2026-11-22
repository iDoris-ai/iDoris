//! 引擎管理器（接口规范 §2.7.6、§2.7.7；设计稿 §2.7.2–§2.7.10；IDORIS-CORE S5）。
//!
//! 一个**托管卡**（`extensions.idoris_managed`）对应一个引擎实例：种类（`engine_kind`）加 endpoint。管理器按实例建模，
//! 每个实例一个任务，各自一台状态机；种类相关的一切（怎么探测、找可执行文件、拼命令、私有 key 怎么登记、围栏在哪）
//! 都在 [`EngineBackend`] 后面，v0.6.0 只有 oMLX（[`OmlxBackend`]）。加一种引擎是写一个新的 `EngineBackend`，再在 [`plan_managed`]
//! 的「引擎种类到 backend」的对应里加一行；状态机不改。
//! 引擎进程本身（拉起、所有权记录、认回孤儿、停止）是 S3 的原语（`idoris_upstream::engine`）。
//!
//! # 启动决策（adopt 优先，缺失才拉起）
//!
//! 1. 读所有权记录（`OwnedEngine::recover`），**先于探测**：`kill -9` 之后遗留的自有引擎仍在监听，先探测会把它当成外部
//!    引擎，或者因为它的私有 key 而得到 401。四项身份吻合就接管（沿用记录里的 key）；吻合但有 `load.pending` 围栏时
//!    引擎可能正处于加载中，对自己拥有的引擎：杀整组、确认消失、清围栏、重拉；没封印的记录（启动途中被杀）：停掉重拉。
//! 2. 探测 endpoint：就绪 = 外部引擎，`owned=false`，**iDoris 永远不给它发信号**；要鉴权而没有可用 key = `unavailable` +
//!    `auth_required`（或 `auth_failed`），**不再拉起第二个**；形状不符 = `failed` + `port_in_use`；连接被拒 = 没有引擎。
//! 3. 发现并校验可执行文件：找不到 = `absent` + `engine_not_found`，iDoris 本身继续服务。
//! 4. 拉起并等就绪：每次拉起都准备 iDoris 自己的 base（`<状态目录>/omlx-base`，拿不到 = `base_unavailable`），**每次**重新取
//!    S4 的 `managed_models_dir()`，并传内存护栏（有额度 `--memory-guard-gb`，没有额度 `--memory-guard balanced`）与 `--model-dir`
//!    （oMLX 把它们落盘，不传就沿用旧值）。
//!
//! # 端口属主校验（#619）
//!
//! 向某个端口发 key 之前，先确认现在监听它的就是该引擎：自己拉起的，要求这个 pid 在该端口上监听（`lsof`），之前不发私有
//! key；外部引擎要求监听者是同一个用户、代码签名团队对（`ListenerVerifier`，按 pid 加启动时间缓存），不符就**不发 key、
//! 不接管**，报 `port_in_use`。带 key 的请求一律用不保留空闲连接的客户端，所以它只走校验之后新开的连接。
//!
//! # 状态机
//!
//! `starting` → `ready`；失败（引擎提早退出、等就绪超时、私有 key 被自己的引擎拒绝、端口被别的东西占着）→ `restarting`
//! （退避 1、2、4 秒起翻倍，封顶 30 秒，±20% 抖动）；5 分钟内失败超过 5 次 → `failed` + `crash_loop`，之后每 60 秒半开重试
//! 一次，不永久放弃（门控降级放行之后例外，见下）；`stop` → `stopped`；找不到引擎 → `absent`。配置性的失败（`budget_zero`、
//! `base_unavailable`、围栏清不掉）不会自己好，不重试，等 `start`（门控降级放行之后冻结，连 `start` 也不接受，见「门控」）。
//!
//! 运行期（门控完成之后）：自有引擎的**进程退出**无法在进程内恢复（Supervisor 的账本已过期，热恢复是 v0.6.1），
//! 所以 iDoris 优雅停止并以退出码 5 退出（[`EngineFailure`]），由 agent24d 重启走全新的启动门控。进程活着而探测失败只报
//! `unavailable`，不杀它也不退出（oMLX 加载大模型时 `/v1/models` 可能很慢，不能把它当成失效）。外部引擎探测失败只报
//! `unavailable`，iDoris 不重启它。
//!
//! # 门控
//!
//! 有托管卡时，完整的 Router（它的 Supervisor 在创建时对引擎采样一次）要等**所有**托管引擎「落定」才创建并替换上去
//! （[`EngineManager::open_when_settled`]，里面是 [`EngineManager::settle`] 加 [`EngineManager::open_gate`]）：落定是 `ready`，或不会自己好的终局（配置性失败、
//! `engine_not_found`、熔断……）；`starting`、`restarting` 还在往前走，不算。之前数据面除 `/health` 外一律 503，`/health` 报
//! `phase:"starting"`。没有托管卡时整个模块不参与。
//!
//! **门控不被一个起不来的托管引擎无限期拖住**：别的卡（健康的 resident 卡、自定义上游）要照常服务。有引擎终局失败时**降级放行**
//! （[`GateOutcome`]）：路由到它的请求直接 503 带原因，别的卡正常，`/health.phase` 是 `degraded`。等待有上限：到了
//! [`EngineManager::gate_deadline`]（管理器启动之后一个启动超时）还没落定的引擎**算作落定**——放弃它（`failed` +
//! `startup_timeout`），不等熔断（熔断最坏要近 10 分钟）。降级放行之后，没就绪的托管引擎**冻结**：不再半开重试，不再接受
//! `start`（[`CommandError::RestartRequired`]），要重启 iDoris：它们的 Supervisor 是放行时创建的，之后引擎好了也读不到
//! （运行期引擎换代是 v0.6.1 / S10 的事）。退出码 5 的语义不变：只有放行时已经就绪的自有引擎，之后进程退出才触发。

mod backend;
mod backoff;
pub mod gate;
mod instance;
mod manager;

pub use backend::{EngineBackend, OmlxBackend, PrepareFailure};
pub use backoff::{CrashWindow, MIN_CONFIRM, MIN_TERM_GRACE, Timings, restart_delay, stop_windows};
pub use manager::{
    CommandError, EngineFailure, EngineManager, GateOutcome, ManagedPlan, ManagerConfig, PlanError,
    plan_managed, plan_managed_with,
};

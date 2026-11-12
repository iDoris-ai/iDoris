//! idoris.ai/api 计算服务 **S4：云供应商契约**（路由层）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §6（请求状态机与失败切换）、§8（端点级许可表）、
//! §12 的 S4 一行。这一片决定「钱算没算对、会不会重复扣费、儿童数据会不会被送到不合规的上游」，
//! 所以**每一条规则都由 S0 契约裁决**（`idoris-compute-contracts`），这里只负责用契约的类型和纯函数驱动真实的上游调用：
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`executor`] | 执行器：候选选择（`select_compliant`）→ 准入 → 尝试（先落库再写字节）→ 首字节前切换 → 发布边界 → 结算 |
//! | [`answer`] | 两种结果：等待发布的完整回答（非流式）、一条流；发布边界与客户端断开时的收尾 |
//! | [`admission`] | 候选准入的接口（S3 的接缝）与 S4 的简单实现（只算预留额，一律放行，TODO(S3)） |
//! | [`config`] | 云上游配置（YAML）：地址、密钥来源、价目、路由、静态调用方 |
//! | [`clock`] | 时钟（许可表的核验过期按「现在」判，测试要能拨） |
//! | [`safety`] | **S6 儿童安全**：租户属性绑定的安全管线、规则 / LLM 检查器、成本接口、进程级「不落盘」加固 |
//!
//! 协议与传输（OpenAI 兼容的请求、SSE、usage 解析、超时、密钥不外泄）在 `idoris-upstream::cloud`；
//! 尝试记录与结算分录（SQLite，只追加，触发器兜底）在 `idoris-tenancy::attempts`。
//!
//! # 默认关闭
//!
//! 只有设了环境变量 [`config::CONFIG_ENV`]（`IDORIS_CLOUD_UPSTREAM_CONFIG`）、并且 `deploy_mode=tenant`
//! （云形态映射到既有的 `tenant`）才会启用；不设就是 `AppState::cloud == None`，`build_app` 挂的还是原来的
//! 处理函数，**现有的本地 oMLX 路径、订阅中转和全部既有 HTTP 行为一字不变**。个人形态永远不会把请求送到云上游
//! （「个人 iDoris 不改成 SaaS」，设计稿 §1 决定 4；`local_only` 失败即关闭）。
//!
//! # 交接给 S1 / S3 / S5 / S6
//!
//! - **S3（已完成）**：[`admission::LedgerAdmission`] 包住账户存储里的准入账本（`idoris_tenancy::accounts::ledger`：
//!   五项准入、有担保的负值、宽限、免费额度），生产装配（[`CloudUpstream::from_file`]）已经换掉了
//!   [`admission::SimpleAdmission`]（它现在只是 S4 的测试替身）。尝试记录的收尾和账本结算在同一个事务里
//!   （`AttemptStore::install_ledger_hook`）；候选没被采用时执行器会 [`admission::CandidateAdmission::release`]；
//!   体验分级的响应头见 `handler` 模块文档（`X-iDoris-Reason` / `X-iDoris-Tier` / `X-iDoris-Max-Tokens`，
//!   以及 [`EXPOSED_HEADERS`]，S5 的 CORS 层要用它）；
//! - **S1 / S2 / S5**：[`config::CallerBook`] 是静态替身，换成按凭证解析的付款上下文（`resolve_payment_context`）
//!   和租户数据策略；云入口（`/api/v1`、从凭证推导租户）接在 `handler` 前面；
//! - **S6**（已完成）：非流式回答在发布前停在 [`answer::PendingAnswer`]，完整输出检查放在 `publish()` 之前，不通过就
//!   `withhold(..)`（用户付 0）；edu 一律非流式的拒绝在 `handler` 里（先于任何上游调用），执行器里再查一道。
//!   管线在 [`CloudExecutor::execute`]：处理儿童数据的租户（`TenantDataPolicy::handles_child_data()`）输入检查 -> 上游调用 ->
//!   完整输出检查，见 [`safety`] 的流程图。S6 留给 S3 的接缝是 [`safety::ExtraCostMeter`]（安全检查成本的预留与结算，
//!   现在是 [`safety::SimpleExtraCost`]，`TODO(S3)`）。

//!
//! # 对设计稿 / S0 含糊处的理解（没有悄悄拍板，等 owner 确认）
//!
//! 1. **连接失败不回到 `not_sent`**：`reqwest` 的 `send` 把连接、写请求、等响应头合在一个 future 里，没法在「连接成功」
//!    与「开始写」之间落库。为了守住「先记 `WireWriteStarted` 再写字节」，`send` 一开始就按「字节可能已经出去」算：
//!    连接被拒 / DNS / TLS 失败的尝试保持 `maybe_executed`（成本按预留额挂账，对账查无此单后释放），失败标注是
//!    `connect_failed`。`never_left_host` 只用于 `prepare` 阶段的本地失败（取不到密钥、请求体不合法）。要做到精确，
//!    得给 `reqwest` 的连接器插一层（新增 `tower` 直接依赖并关掉连接复用）或给契约加「连接失败证据」事件——都没有做。
//! 2. **所有 4xx 都按契约当作「没执行」**（含 408、429；既有 `ChatProxy` 把 408 当作不确定）；**2xx 之后的任何失败**
//!    （响应体坏、流里报错、首个事件前断开）按「确认执行了、usage 未知」（`ExecutionObservedUsageUnknown`）。
//! 3. **哪些首字节前的失败会切换**：见 [`status_allows_failover`]。请求形状被拒的 4xx（400 / 413 / 422……）不切换：
//!    换一个同样合规的上游多半也被拒，而每多发一次，孩子的输入就多送给一个上游（数据最小化）。
//! 4. **错 usage = usage 未知**：结构不对、`total_tokens` 对不上、缓存大于输入、输出超过发给上游的 `max_tokens`、
//!    金额溢出，用户按预留价付费；上游声称的数字（只含数字字段）进 `usage_claim_json` 供对账。
//! 5. **客户端取消**：首字节前取消，尝试保持 `maybe_executed`；首字节后断开，看许可表里这个端点的「取消」能力：
//!    有 → 执行确认、usage 未知（按预留结算）；没有 → 上游可能还在计费，保持 `maybe_executed`。两种情况用户都按预留价付费
//!    （契约规则：已发布且 usage 未知）——产品上可能想按已转发量估算，没做。
//! 6. **供应商请求号**：正文里的 `id` 优先，其次是配置的响应头；格式不对只留标注；同一供应商下别的尝试用过同一个号、
//!    或流中途换了号：两条记录都保留并标注，不拒绝服务（丢掉审计痕迹比重复更糟）。
//! 7. **分录**：`user_price`（只有发布的那次）、`platform_cost`（按实际用量的成本）、`hold`（按预留额挂账待核销的成本，
//!    `maybe_executed` 与 usage 未知）、`release`（从用户额度释放的价预留）；零金额不记；
//!    一个请求至多一个付费尝试、至多一条正的 `user_price`（唯一索引）。
//! 8. **S0 `ReasonCode` 之外的稳定字符串**：请求记录的 `reason_code` 里还有 `upstream_unavailable`、`client_cancelled`、
//!    `upstream_stream_failed` 等；HTTP `error.type` 里还有 `upstream_error`、`caller_not_configured`、`cloud_unavailable`、
//!    `streaming_not_allowed`。S0 没有「所有上游都失败」这一类原因码。
//! 9. **edu 一律非流式**：S0 把它划给 S6，但漏掉它会让儿童数据绕过完整输出检查，所以 S4 在处理函数里就拒绝
//!    （先于任何上游调用，400 `streaming_not_allowed`）。
//! 10. **后一个候选被准入拒绝**：已经有尝试失败时，请求以上游失败（502）结束，不是 402。
//! 11. **付款上下文**：静态调用方簿经 S0 的 `resolve_payment_context`（平台 -> 组织账户、节点凭证）得到，不绕过契约的唯一入口；
//!     `QUOTA` 单位在云形态被拒绝。S1 / S2 / S5 落地后换成账户库里按凭证解析的真实付款上下文。
//! 12. **`Request` 的持久化**：创建时的快照 JSON（不可变）+ 规范化的查询列；`state_json` 是状态的真相，`state`、`certainty`、
//!     usage 是它的派生列（CHECK 与生成列）。
//!
//! # S6（儿童安全）对设计稿 / S0 含糊处的理解（没有悄悄拍板，等 owner 确认）
//!
//! 13. **「租户属性」就是 `TenantDataPolicy::handles_child_data()`**（`data_classes` 含 `child_edu`），来自服务端配置的调用方簿
//!     （S1/S2/S5 之后是账户存储），经虚拟 key 绑定的租户解析；模型名、路由别名、任何请求头和请求体字段都改不了它。
//!     没有另设一个布尔字段：S0 已经有这个属性，并且它同时决定了端点许可表的硬约束（ZDR + 已签 DPA）。
//! 14. **管线放在执行器里，不是 handler 里**：任何入口（S5 的云入口、测试直接调执行器）走到执行器，edu 都绕不过。
//!     `RunState` 还有一道发布守卫：儿童请求的回答没有「输出检查放行」标记就发布不出去，标记在请求创建时由数据类别决定。
//! 15. **edu 的响应体是重建的，不是上游原样**：只含 `content` / `refusal` / `tool_calls` 和校验过的 `id` / `model` /
//!     `finish_reason` / 只含数字的 `usage`；上游多出来的文本字段（`reasoning_content`、`annotations`、`provider`……）不转发，
//!     因为没有检查过。非 edu 的响应体照旧原样。
//! 16. **输出被拒不换候选重试**：被拒的回答对应的尝试已经执行了（用户付 0，平台承担成本）；P0 不为了「换个模型再答一次」
//!     继续花钱，也不给孩子（或试探者）一个反复试探安全系统的循环。
//! 17. **检查结果和拒绝码**：`safety_input_blocked` / `safety_output_blocked` 用 HTTP 422，`safety_unavailable` 用 503
//!     （4xx 不会被 OpenAI 兼容客户端自动重试，503 才应该稍后重试）。设计稿只说「拒绝放行」，没有定状态码。
//! 18. **类别不落盘**：拦截类别（自伤、成人内容……）是从孩子的话里推出来的敏感推断，只在当次响应里给 eduai；
//!     请求记录里只有粗的码（`safety_input_blocked`），日志里也没有类别（PDPA 第 26 条把健康等列为敏感数据，设计稿 §7 要求最小化）。
//! 19. **系统消息只查越狱类规则**：运营方的系统提示会复述要拒绝的话题（评测集的 `DEFAULT_SYSTEM` 就是），按话题规则查会把
//!     自己的提示拒掉。代价是：孩子的话被塞进 `system` 角色时，规则检查器只会按越狱类查它（LLM 检查器查全部）。
//!     要更严需要系统提示白名单（哈希），P0 没做，等 owner 决定。
//! 20. **只用规则检查器必须显式确认**：`safety.mode` 必填，`rules_only` 启动时留一行警告。是否强制 child_edu 必须配 LLM 检查器
//!     （建议是），等 owner 决定。
//! 21. **安全检查成本只有接口，没有账本**：[`safety::SimpleExtraCost`] 只在内存里记账，进程重启就丢，所以在 S3 的实现接上之前，
//!     LLM 检查器的成本**没有**真正计入准入和结算；谁承担（用户还是平台）也是 S3 的决定。
//! 22. **非文本内容、过长文本一律拒绝**：图片、音频、文件等无法检查的内容按 `unsupported_content` 拒绝；超过检查上限的文本按
//!     `safety_unavailable`（`too_large`）拒绝。语音转写的检查是 P1。
//! 23. **旧路由对儿童租户关闭**：云形态打开时，`/v1/messages`、`/v1/embeddings`、`/v1/rerank` 仍是旧的本地流水线，没有安全管线，
//!     对儿童租户一律 403（`child_route_unavailable`）；`ChatProxy` 对这些租户同时关闭响应缓存。

use std::path::Path;
use std::sync::Arc;

use idoris_compute_contracts::ids::TimestampMs;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::attempts::AttemptStore;

pub mod admission;
pub mod answer;
pub mod clock;
pub mod config;
pub mod executor;
mod handler;
mod request;
mod run;
pub mod safety;

pub use admission::{
    AdmissionError, AdmissionRequest, CandidateAdmission, LedgerAdmission, ReleaseRequest,
    SimpleAdmission,
};
pub use answer::{AnswerMeta, CloudOutcome, CloudStream, PendingAnswer, PublishedAnswer};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{
    CallerBook, CallerPolicy, CloudLimits, CloudParts, EndpointRegistry, EndpointRuntime, RouteBook,
};
pub use executor::{CloudCall, CloudError, CloudExecutor, FailureInfo, status_allows_failover};
pub use run::Settled;
pub use safety::{
    ExtraCostMeter, GateDenial, SafetyChecker, SafetyGate, SafetyLimits, SafetyPipeline,
    SimpleExtraCost, Verdict,
};

pub use handler::{EXPOSED_HEADERS, HEADER_MAX_TOKENS, HEADER_TIER};
pub(crate) use handler::{chat_completions, child_route_guard};

/// 云上游：执行器 + 路由簿 + 调用方簿。挂在 `AppState.cloud` 上。
pub struct CloudUpstream {
    pub(crate) executor: CloudExecutor,
    pub(crate) callers: CallerBook,
    pub(crate) routes: RouteBook,
}

impl std::fmt::Debug for CloudUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudUpstream")
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl CloudUpstream {
    /// 用加载好的部件、尝试记录库、时钟和准入实现组装。
    pub fn new(
        parts: CloudParts,
        store: Arc<AttemptStore>,
        clock: Arc<dyn Clock>,
        admission: Arc<dyn CandidateAdmission>,
    ) -> Self {
        let CloudParts {
            permits,
            endpoints,
            routes,
            callers,
            limits,
            reserve_policy,
            safety,
        } = parts;
        let mut executor = CloudExecutor::new(
            Arc::new(permits),
            Arc::new(endpoints),
            admission,
            store,
            clock,
            reserve_policy,
            limits,
        );
        if let Some(setup) = safety {
            // TODO(S3)：换成读写账本的 ExtraCostMeter（见 `with_extra_cost_meter`）
            executor =
                executor.with_safety(Arc::new(setup.into_gate(Arc::new(SimpleExtraCost::new()))));
        }
        Self {
            executor,
            callers,
            routes,
        }
    }

    /// 用给定的儿童安全 gate 换掉配置里的（测试和装配用）。
    pub fn with_safety(mut self, gate: SafetyGate) -> Self {
        self.executor = self.executor.with_safety(Arc::new(gate));
        self
    }

    /// 换掉安全检查成本的计量器（S3 的接缝，`TODO(S3)`）。没有装安全 gate 时什么都不做。
    pub fn with_extra_cost_meter(mut self, meter: Arc<dyn ExtraCostMeter>) -> Self {
        if let Some(gate) = self.executor.safety.take() {
            self.executor.safety = Some(Arc::new(gate.as_ref().clone().with_cost(meter)));
        }
        self
    }

    /// 配置里有没有处理儿童数据的调用方。
    pub fn has_child_data_callers(&self) -> bool {
        self.callers.has_child_data_callers()
    }

    /// 处理儿童数据的租户标识（`ChatProxy` 缓存键里的租户范围格式），给旧代理路径关缓存用。
    pub fn child_data_tenant_scopes(&self) -> std::collections::BTreeSet<String> {
        self.callers.child_data_tenant_scopes()
    }

    /// 是否装了儿童安全 gate。
    pub fn has_safety_gate(&self) -> bool {
        self.executor.safety.is_some()
    }

    /// 从配置文件加载，用系统时钟和 **S3 的准入账本**（[`LedgerAdmission`]）；账户存储（含账本）和尝试记录库
    /// 都打开在 `db_path`（与其它 store 共用同一个 SQLite 文件）。
    ///
    /// 装配的三件事，缺一不可：
    ///
    /// 1. 准入走账本（沿付款账户所有祖先的五项原子检查，占预留）；
    /// 2. 尝试记录装上账本钩子：创建尝试要有匹配的预留，收尾时账本在**同一个事务里**结算；
    /// 3. 预留 TTL 必须长于一次请求可能持续的最长时间（总期限 + 流的总时长）：预留在上游还在计费时就过期，
    ///    额度会提前放回去。不满足就启动失败（失败即关闭），不是悄悄放行。
    ///
    /// 账户存储里没有的付款账户 / 凭证（比如还在用静态调用方簿 `callers:`，账户库里没有对应的账户）一律被准入拒绝：
    /// 账本不认识的付款人不能花钱。
    pub fn from_file(path: &Path, db_path: &Path) -> Result<Self, String> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let parts = config::load(path, clock.now())?;
        // 启动日志：哪些调用方走儿童安全管线（只有租户标识）。运营方一眼能核对「edu 租户的属性确实是 child_edu」
        let (child, other) = parts.callers.caller_summary();
        eprintln!(
            "[idoris] cloud: child-data callers (safety pipeline ON): [{}]; other callers (no safety pipeline): [{}]; safety: {}",
            child.join(", "),
            other.join(", "),
            parts
                .safety
                .as_ref()
                .map_or("not configured", |safety| match safety.mode {
                    safety::config::SafetyMode::RulesOnly => "rules_only",
                    safety::config::SafetyMode::RulesAndLlm => "rules_and_llm",
                })
        );
        if let Some(safety) = &parts.safety {
            for warning in &safety.warnings {
                eprintln!("[idoris] cloud: {warning}");
            }
        }
        let accounts = Arc::new(
            AccountStore::open(db_path)
                .map_err(|e| format!("无法打开账户数据库 \"{}\"：{e}", db_path.display()))?,
        );
        let store = AttemptStore::open(db_path)
            .map_err(|e| format!("无法打开尝试记录数据库 \"{}\"：{e}", db_path.display()))?;
        store
            .install_ledger_hook(accounts.attempt_ledger_hook())
            .map_err(|e| format!("无法安装准入账本钩子：{e}"))?;
        let ttl_ms = accounts
            .ledger_config()
            .map_err(|e| format!("无法读取准入账本配置：{e}"))?
            .reservation_ttl_ms;
        let longest = parts.limits.total_deadline + parts.limits.stream_total;
        if u128::try_from(ttl_ms).unwrap_or(0) <= longest.as_millis() {
            return Err(format!(
                "准入账本的预留 TTL（{ttl_ms} ms）必须长于一次请求可能持续的最长时间（总期限 + 流的总时长 = {} ms）：\
                 否则预留会在上游还在计费时过期，额度提前放回去",
                longest.as_millis()
            ));
        }
        Ok(Self::new(
            parts,
            Arc::new(store),
            clock,
            Arc::new(LedgerAdmission::new(accounts)),
        ))
    }

    /// 读环境变量 [`config::CONFIG_ENV`]：没设（或为空）就是关闭（返回 `None`，什么都不改变）。
    /// 设了就必须是云形态（`deploy_mode=tenant`），否则启动失败：个人形态不会把请求送到云上游。
    pub fn from_env(
        deploy_mode: idoris_contracts::DeployMode,
    ) -> Result<Option<Arc<Self>>, String> {
        let path = crate::config::optional_env_path(config::CONFIG_ENV)?;
        Self::from_env_path(path, deploy_mode, crate::storage::process_db_path)
    }

    /// [`Self::from_env`] 的可测版本：配置路径与数据库路径由调用方给。
    /// 先查形态、后碰文件和数据库：个人形态下设了开关也不会去读任何东西。
    pub fn from_env_path(
        config_path: Option<std::path::PathBuf>,
        deploy_mode: idoris_contracts::DeployMode,
        db_path: impl FnOnce() -> Result<std::path::PathBuf, String>,
    ) -> Result<Option<Arc<Self>>, String> {
        let Some(path) = config_path else {
            return Ok(None);
        };
        if deploy_mode != idoris_contracts::DeployMode::Tenant {
            return Err(format!(
                "{} 只能在 IDORIS_DEPLOY_MODE=tenant（云形态）下启用：个人形态不会把请求送到云上游",
                config::CONFIG_ENV
            ));
        }
        Ok(Some(Arc::new(Self::from_file(&path, &db_path()?)?)))
    }

    /// 测试和装配用：直接取执行器。
    pub fn executor(&self) -> &CloudExecutor {
        &self.executor
    }

    /// 解析时用的「现在」：给静态付款上下文用（配置里的凭证永不过期，这里只是满足函数签名）。
    pub fn config_time() -> TimestampMs {
        SystemClock.now()
    }
}

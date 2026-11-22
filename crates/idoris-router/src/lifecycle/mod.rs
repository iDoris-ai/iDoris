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
//! 向某个端口发 key 之前，先确认现在监听它的就是该引擎：自己拉起的，要求这个 pid 是该端口上**唯一**的监听者（`lsof`，任何地址，
//! 且至少一个套接字落在目标地址上），之前不发私有 key；外部引擎要求监听者是同一个用户、代码签名团队对（`ListenerVerifier`，按
//! pid 加启动时间缓存，端口出现监听空档时作废），不符就**不发 key、不接管**，报 `port_in_use`。带 key 的请求一律用不保留空闲
//! 连接的客户端，所以它只走校验之后新开的连接。
//!
//! **每个请求发出之前都再确认一次**（Codex 终审 H1，owner 2026-11-22 拍板；之后的要求：p99 < 5 毫秒、请求路径上不起子进程）：管理器
//! 登记口径时把一个守卫（`idoris_upstream::engine::guard`）一起交给登记表，发往这个端点的每个请求在**发送它的那个任务里、紧挨着
//! 发送之前**经 `authorize_omlx_request`，守卫重新确认，确认不了就拒绝。确认分两半：**每个请求**只问登记时认的那个进程（pid 加启动
//! 时间）此刻还握着这个端口上的一个 LISTEN 套接字（原生：macOS libproc、Linux `/proc`，两三次系统调用，不起子进程）；贵的那些——
//! 「这个端口上只有它一个监听者」（`lsof`）、外部引擎的 uid 与签名团队——留给管理器自己的监督：**每个节拍和每次状态变化**（启动、认回、
//! 重启、外部换代）确认一次，先收回凭证再发布状态。所以：引擎放掉端口或被别的进程接管，下一个请求就拒绝；一个并存的监听者（引擎的
//! 套接字还在、另一个进程也在这个端口上监听），自上一个节拍起才被看见。**已知限制**：重新确认与发送之间的毫秒里，本机另一个用户的
//! 进程抢到端口，仍能收到那一个请求（同一个用户的进程本来就读得到 key 文件；libproc 与 `lsof` 看不到别的用户的套接字）；不做逐连接的
//! 对端进程核对。
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
//!
//! # 门控放行之后的命令与 Supervisor 的账本（S6，规范 §2.11.4、勘误十）
//!
//! Admin 的 `start`/`stop`/`restart` 经 [`EngineManager::start_upstream`] 等进来。门控放行之后完整的 Router 里的 Supervisor 在放行时
//! 对引擎采样了一次：账本里是当时已加载的模型。**引擎换了一个进程**（`restart`，`stop` 之后的 `start`，外部引擎换了监听者），
//! 这份账本就过期了——它以为 Ready 的模型已经不在引擎里，共享容量账本里还占着旧进程的内存。光复位账本不够，**次序**才是正确性
//! 所在（Codex 预审 1–4）：旧引擎上还有没做完的加载或聊天时，不能清围栏、不能拉起新进程；旧一代的任务不能拿到新一代的凭证。
//!
//! ## 退役与换代的次序
//!
//! `stop`、`restart`、`start` 一个还活着却不答的引擎、外部引擎换了进程，都是这一套：
//!
//! 1. **关门**（`SupervisorHandle::close_admission`）：Supervisor 不再准入新的加载、卸载、聊天、目录读取（`supervisor_unavailable`）。
//!    聊天与预热（`ensure_loaded`）走 Supervisor 的同一个入口，所以同时关上，没有第二个入口。没有 Supervisor（门控还没放行、
//!    resident 的托管卡）什么都不用做。
//! 2. **发布不可服务的状态**（`stop` 是 `stopped`，`restart` 是 `restarting`），再回答 202：数据面立刻 503，不靠请求入口那一次性的
//!    状态检查。引擎这时还握在手里，所以元素的 `owned`、`pid` 还在。
//! 3. **排空**：等已经准入的加载、聊天、目录读取做完，**和**这个端点上登记的在途操作（`endpoint_flights`：resident 的转发、后台
//!    预热、模型列表的读取、仍在输出的流式响应体；没有 Supervisor 的托管卡也一样；Codex 终审 M1），至多
//!    [`Timings::drain_timeout`]；它们拿着准入时的凭证在旧引擎上照常跑完。到点还有在途的就**切断旧一代的凭证**（封上端点，代际
//!    前进），它们之后发的每个请求都被适配器拒绝。每一次读都在同一个绝对的截止时间里（Codex 终审 M5）。
//! 4. **停进程**（SIGTERM、宽限、SIGKILL），**确认**它消失；没能确认就不清围栏、不拉起新的。
//! 5. **封上端点**（[`EngineBackend::block`]，什么凭证都不发）、**清围栏**（进程确认消失之后才清）、拉起新进程，新引擎通过校验后登记
//!    它的私有 key（代际前进）。
//! 6. **复位账本**（`SupervisorHandle::engine_replaced`）：只在静止时（有进行中的加载或聊天回 `Busy`，管理器等；所以旧一代的完成消息
//!    不可能落在重建的账本上），对着新进程重新采样（和启动采样同一套规矩）。
//! 7. **开门**（`reopen_admission`），发布 `ready`。
//!
//! **代际绑定**：每个被 Supervisor 准入的操作绑定准入时的引擎代际（[`idoris_backend::pinned_generation`]，任务局部量；后台任务与
//! 延迟的卸载也一样），适配器取 key 的唯一入口核对它：代际不是当前的、或端点被封上，**在发送任何东西之前**拒绝。代理对 resident
//! 的托管卡（没有 Supervisor）在每一次发送前同样核对，预热的最小请求在自己的任务里跑，绑定准入时的代际。
//!
//! ## 端点的口径：私有、外部、封上
//!
//! [`idoris_upstream::omlx::key::EndpointKey`] 只有三种：私有 key、外部（`settings.json` 的 key 要先过属主校验）、**封上**（什么凭证都不发，
//! 回退链不适用）。托管的端点在管理器启动时就先封上；停止、替换、外部监听者没了、端口上换了别的东西都回到封上，直到下一个监听者
//! 通过校验。**没有「撤销」**：撤销会让环境变量与 `settings.json` 的回退链重新适用于这个端点。
//!
//! ## 密钥只发给确认过的监听者（Codex 审计 H1–H3）
//!
//! - **自有引擎，每一次**：等就绪的循环里每次带私有 key 的探测之前、监管的每个节拍里，都重新确认监听这个端口的此刻还是我们的进程
//!   （`listens_on(pid, port)`，不缓存）。进程活着不等于端口还是它的：引擎可以放掉端口而不退出，别的进程接着绑上来。确认不了就先
//!   收回私有 key（端点封上）、发布 `unavailable` 加 `port_in_use`，这一轮不发探测；又确认了，把私有 key 登记回去。
//! - **终局的失败之后保持封上**：接管外部引擎时校验通过的监听者已经被登记成「外部」，它要 key 而没有、key 被拒、围栏还在……之后
//!   没有监督再去重新校验端口，那个口径不能留着。**每一条**没有落到「引擎就绪、有持续监督」上的出口都封上（Codex 终审 H2：终局的
//!   失败、重判耗尽、放弃、被命令打断、运行期失败；接管之后账本复位失败也撤销外部授权）。
//! - **端点标识是规范化的**（Codex 终审 H5）：`localhost`、`127.x`、`::1` 同一个端口是同一个标识，所以写成别名的伴生卡与托管实例
//!   落在同一条登记上，继承它的封上与失败，不会走没人管的端点的回退链。
//! - **共用引擎的卡继承它的失败**：管理器启动时把每个托管引擎的 endpoint 登记进 `HealthState`；与它同一个 endpoint 的别的卡（伴生卡
//!   `omlx-embed`）在后台探测、模型列表、预热、转发里都读到同一个状态与原因码，一个请求都不碰那个端口。不共用引擎的上游照常服务。
//! - **生命周期适配器按卡定能不能带 key**：与代理同一个判据（`idoris_upstream::omlx::key::card_may_carry_omlx_key`），不是 `provider.id`；
//!   不能带的卡得到一个没有 key、也不查登记表的适配器；能带的每个请求取此刻的口径与代际，不缓存任何 key 字符串。
//!
//! ## 外部引擎与引擎进程的身份
//!
//! 复位只在**进程身份**变了时做：自有引擎是 pid 加启动时间，外部引擎是校验通过的监听者的 pid 加启动时间（pid 被回收也分得出来）。
//! 监听者没了：封上端点、关门、`unavailable`；**同一个进程**回来：开门，账本不动；**换了进程**：走上面的次序，之后才回 `ready`；
//! 换代期间发布明确不可服务的 `restarting`（不记失败；没有原因码的 `unavailable` 会被数据面的状态门当成「只是慢」而放行），
//! 排空之后**重新取一次监听者、重新核对身份**，仍是预期的那个进程才登记它的凭证与新的守卫、复位、开放服务（Codex 终审 H6）。
//! `start` 重新认回同一个自有引擎也不复位，不会忘掉对未确认加载的记忆。
//!
//! ## 并发的命令与启动决策里的探测
//!
//! - 命令在**入队时**盖章；一次 `start`、`restart` 操作做完时记下序号，入队时序号不超过它的 `restart` 直接并入这次操作
//!   （Codex 预审 5）。
//! - 启动决策里的探测答不上来、端口随后空了，监听者在属主校验前没了，带 key 再问时外部引擎走了：回到决策的开头重来（
//!   `Attempt::Redecide`），**不是引擎失败**，不记入熔断窗口、不退避、不加 `failure_count`；整体受一个启动超时约束（Codex 预审 6）。
//!   带 key 的探测答不上来、而监听者**还是校验通过的那一个**，是它的毛病，按一次失败算（S5 的语义；Codex 终审 M3）。
//! - 命令带一个**绝对的截止时间**（`Timings::command_timeout`，缺省 60 秒），管入队、等回答与重试：实例任务忙得收不下它就到点放弃
//!   （Admin 回 503 `idoris_stopping`），**事后也不执行**；`shutdown` 与放弃的预算从入队之前算起（Codex 终审 M5）。
//!
//! - 复位失败（引擎答不上采样、围栏还在）：Supervisor 把错误粘滞住（fail closed），这个引擎不放行，停掉，按一次失败算，退避之后
//!   重来；Supervisor 没有了（actor 退出，或检测到自己的账本损坏）是终局，要重启 iDoris。
//! - **放行时没就绪的上游（降级放行）冻结**：完整的 Router 里根本没有它的 Supervisor，`start`、`restart`
//!   [`CommandError::RestartRequired`]。这是 S5 起的已知限制，不是账本过期的问题。
//!
//! 门控放行之前这些命令照旧（真的 `start`、`restart`），但 Admin 的完整路由只在放行之后才有（放行之前除 `status` 外一律
//! 503 `idoris_starting`），所以只有进程内的调用方用得到。

mod backend;
mod backoff;
pub mod gate;
mod instance;
mod manager;

pub use backend::{EngineBackend, OmlxBackend, PrepareFailure};
pub use backoff::{CrashWindow, Timings, restart_delay};
pub use manager::{
    CommandError, EngineFailure, EngineManager, GateOutcome, ManagedPlan, ManagedView,
    ManagerConfig, PlanError, managed_backend_kind, plan_managed, plan_managed_with,
    validate_managed_plan,
};

# agentgateway/agentgateway 源码深读报告：能否作为 iDoris Rust 实现的基础

> 调研时间：2026-09-27。方法：`git clone --depth 200` 到 `scratchpad/oss/agentgateway`（HEAD `7e47ceb`，2026-09-26），逐 crate 用 `grep -n` 定位后 `Read` 关键代码确认真实语义，不凭文件名/宣传材料猜测。
>
> 背景：本报告是 `research-rust-base-oss.md` 的补充调研。该报告已深读 plano（katanemo/plano）、aisix（api7/aisix）、llamastash 三个项目，结论是"没有一个候选能同时满足 iDoris 的三大刚需：①单二进制+SQLite ②隐私 fail-closed 的确定性路由链 ③本地运行时生命周期管理"。agentgateway 当时因时间原因只做了基于 README 的初步评估（思路匹配度估 3/5，工程质量估 4/5），本报告的任务是把它补齐到同等深度，并回答：**agentgateway 是否改变这个结论**。
>
> 结论先行：**没有改变**。agentgateway 在"预算/guardrail/CEL 路由/审计/Admin API"五个维度上是本次两轮调研里工程质量最高、覆盖面最广的候选，但它同样不满足三大刚需中的任意一条的完整形态（预算不是两阶段、价格未知不拒绝、完全没有本地运行时管理）。它改变的是**角色定位**：不建议 fork，而是本次调研中"作为前置可信数据面 + 外部 policy 扩展"这条路径里目前最强的候选，比 plano、aisix 都更适合承担这个具体角色。详见 §11。

---

## 0. 基本信息核实

- 仓库：https://github.com/agentgateway/agentgateway，本地 clone HEAD `7e47ceb576aa9d3300cb0d2c7c35848fb0af048f`，提交时间 `2026-09-26 17:09:50 -0700`。
- 语言构成（`find . -name "*.rs" | xargs wc -l`，不含 `.git`）：**Rust 约 264,368 行**；Go 控制器（`controller/`，仅 K8s Gateway API 控制面）约 **91,248 行**；管理 UI 前端（`ui/src/**/*.ts(x)`）约 **34,863 行**。Rust 占绝对主体，与背景材料"Rust 为主"一致。
- 治理：`CHARTER.md` 第 1-6 行确认为 **"agentgateway a Series of LF Projects, LLC"**，即正式 Linux Foundation 项目群下的技术章程治理（Technical Steering Committee 模式，CHARTER.md 全文）。README.md 与 CHARTER.md 全文搜索均**未找到 "Solo.io" 字样**——仓库自身文档不体现商业公司背书，只呈现为 LF 治理项目（背景材料提到的"原属 kgateway 生态"属于外部背景知识，本报告未在仓库内证实或证伪）。

---

## 1. 架构与部署

### 1.1 crate 结构与代码规模

`Cargo.toml` workspace 成员（13 个 crate + xtask）：

| crate | 行数（.rs） | 定位 |
|---|---:|---|
| `crates/agentgateway` | 189,461 | 主体：路由/代理/LLM策略/CEL/存储/Admin/UI后端，几乎全部业务逻辑在此 |
| `crates/llm` | 32,276 | Provider 协议转换层（`agent-llm`） |
| `crates/cel-fork/cel`+`cel-derive` | 23,034 | 完整的 CEL 语言解释器 fork |
| `crates/pool` | 6,052 | 连接池（见 §10.3 许可证说明） |
| `crates/core` | 4,476 | 基础类型/工具（`agent-core`） |
| `crates/celx` | 2,628 | agentgateway 自己的 CEL 函数扩展（cidr/字符串/数学等） |
| `crates/http` | 2,323 | HTTP 基础设施 |
| `crates/hbone` | 1,315 | mTLS 隧道协议（源自 Istio ztunnel 血缘） |
| `crates/xds` | 1,035 | xDS 客户端协议 |
| `crates/htpasswd-verify-fork` | 385 | Basic Auth 密码校验 |
| `crates/protos` | 169 | proto 生成绑定 |
| `crates/xtask` | 335 | 构建脚本任务 |
| `crates/agentgateway-app` | 747 | 二进制入口（CLI/main），依赖上面所有 crate |

`crates/agentgateway-app/Cargo.toml:1-6`：`[[bin]] name = "agentgateway" path = "src/main.rs"` —— 单一可执行文件。`crates/agentgateway-app/src/main.rs:14-15` 用 `include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist")` 把管理 UI 静态资源编译进二进制（`ui` feature 开启时）。

### 1.2 能否脱离 K8s 单机运行；配置方式

**可以，flat YAML 是一等公民，xDS 是可选项。** 证据：
- `crates/agentgateway/src/config.rs:35` 定义 `local_config_source: Option<ConfigSource>`；`config.rs:69-72` 从环境变量 `LOCAL_XDS_PATH` 或配置项 `raw.local_xds_path` 解析本地文件配置源。
- `config.rs:95-98` 中 xDS 控制面地址 `XDS_ADDRESS`/`config.xdsAddress` 是可选解析（`validate_uri(empty_to_none(...).or(raw.xds_address))`），并非强制项；相邻注释 `// if local_config.is_none() && address.is_none()` 显示代码作者本身把两者视为互斥/可选的两条路径。
- `examples/` 下 40 个示例目录（`llm-basic`、`llm-ollama-postgres`、`llm-guardrail-jev`、`llm-cost-routing` 等）**全部**使用纯 `binds:`/`llm:`/`config:` 顶层 YAML 结构，零 K8s CRD、零 xDS 地址配置，可直接 `agentgateway -f config.yaml` 单进程运行。
- K8s Gateway API 模式由独立的 `controller/`（Go，91K 行）承担，是**另一个可执行文件**，不参与数据面二进制的运行路径。

### 1.3 状态存储

`crates/agentgateway/src/database.rs:8-11`：
```rust
pub enum DatabasePool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}
```
`database.rs:17-49`（`connect_with_max_connections`）：URL 以 `postgres://`/`postgresql://` 开头才走 Postgres；**其余一律按 SQLite 处理**，`SqliteConnectOptions` 设置 `create_if_missing(true)`、`journal_mode(SqliteJournalMode::Wal)`、`synchronous(SqliteSynchronous::Normal)`、`busy_timeout(5s)`——是一套干净、生产可用的默认 SQLite 配置，不是"能跑但没调过"的糊弄实现。

全仓库 `grep -rli "redis\|clickhouse"` 在 `crates/` 下**零命中**（除本文件系统枚举出的 database.rs/相关 budget/log_store 文件外，未发现任何 Redis/ClickHouse 依赖）。`examples/llm-guardrail-jev/config.yaml:23-25` 直接演示 `database: url: 'sqlite::memory:'`；`examples/llm-ollama-postgres/config.yaml:3-5` 演示同一份 schema 换成 Postgres——**同一套代码路径，SQLite 是默认后端，Postgres 是可选后端，无 Redis/ClickHouse 强依赖**。这一点比此前评测的 plano（无内建存储）、aisix（无 SQLite，多副本要 etcd/Redis）、millwright（默认 SQLite 但项目已停滞 7 周）、TensorZero（无 SQLite 适配）都更贴合 iDoris 的硬约束。

需要注意：数据库是**可选**依赖——只有"API key 预算"和"可写 Admin API 的资源存储/热更新"这两个子系统需要它（`crates/agentgateway/src/http/budget/mod.rs` 的 `register()` 函数校验 `anyhow::ensure!(!has_budgets || self.database.get().is_some() || database_configured, "API key budgets require config.database to be configured")`，测试 `budgets_require_a_database` 验证了这一点，见 §3）。不配置数据库时，代理/路由/guardrail/日志（写 stdout）仍可正常工作。

### 1.4 能否作为 library 嵌入

`crates/agentgateway/Cargo.toml:8` 有 `[lib]` 段，`src/lib.rs:26-` 起对外暴露 31 个 `pub mod`（`a2a`、`agentcore`、`app`、`aws`、`cel`、`client`、`config`、`config_store`、`control`、`crypto`、`database`、`http`、`import`、`json`、`llm` 等）。`crates/agentgateway-app`（747 行）只是围绕它的一层 CLI 壳。

**技术上可以作为 Rust 依赖嵌入，但代价很大**：这不是 plano 那种"薄壳 crate + 独立纯 async 库（hermesllm）"的干净拆分，而是**整个 189,461 行的单体 crate 就是"库"本身**——没有单独可拆出的"只要 router"或"只要 budget"子 crate。嵌入意味着连带引入其完整依赖图：`async-openai`、AWS SDK 全家桶、`sqlx`（postgres+sqlite 两个 feature）、`rustls`/`aws-lc-rs`、`opentelemetry`、经由 `agent_xds` 间接引入的 k8s 相关依赖等。相比之下，之前报告推荐的 `genai`/`hermesllm` 作为纯出站客户端要轻量得多。

---

## 2. LLM 网关能力

### 2.1 OpenAI 兼容性：chat / embeddings / streaming

- 协议转换集中在 `crates/llm/src/conversion/`：`completions.rs`（Chat Completions）、`messages.rs`（Anthropic Messages）、`responses.rs`（OpenAI Responses API）、`bedrock.rs`、`vertex.rs`、`vertex_gemini.rs`、`openai_compat.rs`（Responses↔Completions 互转，`openai_compat.rs:1-30` 定义 `from_responses::translate`）。`crates/llm/src/types/embeddings.rs`、`crates/llm/src/types/rerank.rs` 覆盖 embeddings/rerank 类型。
- 流式：`crates/llm/src/parse/sse.rs`（通用 SSE）、`crates/llm/src/parse/aws_sse.rs`（AWS event-stream 帧，供 Bedrock 用）。
- **流式场景下的护栏是真实按 chunk 做的**，不是"流式转发但不过滤"：`crates/agentgateway/src/llm/policy/streaming_guardrails.rs:232` 定义 `GuardedSseBody`，`streaming_guardrails.rs:63` 的 `evaluate_window` 对滑动文本窗口做检测；测试 `streaming_guardrails.rs:609-728` 验证了一个 SSN 号码被拆分到两个连续 SSE delta 块中依然能被检测出来（覆盖 OpenAI delta 格式和 Gemini candidates 格式两种 chunk 形状）。

### 2.2 多 provider 支持与"任意 base_url"能力

Provider 抽象在 `crates/llm/src/custom.rs:8` 的 `pub struct Provider`（自定义 provider，含 `formats: Vec<ProviderFormatConfig>` 声明支持哪些 wire format）+ `custom.rs:53` 的 `pub enum ProviderPreset`（Cohere/Ollama/Baseten/Cerebras/Deepinfra/Deepseek/Groq/Huggingface/Mistral/Openrouter/Togetherai/XAI/Fireworks 等内置预设，各自在 `custom.rs:71` 的 `base_url()` 里硬编码默认地址，如 `custom.rs:74`：`Self::Ollama => "http://localhost:11434/v1"`）。原生 provider 模块另有 `crates/llm/src/{openai,anthropic,bedrock,azure,gemini,vertex,copilot}.rs`。

**关键结论（对接 oMLX/mlx_lm.server 至关重要）**：`examples/llm-ollama-postgres/config.yaml:8-10` 演示 `provider: ollama`（直接用内置预设指向本地 11434 端口）；`examples/llm-guardrail-jev/config.yaml` 中另一个模型用 `provider: {custom: {providerOverride: typesafe}}` + `params.baseUrl: https://api.typesafe.ai` —— **任意 OpenAI 兼容后端只需配置一个 `baseUrl` 字段，无需写新 Rust 代码**。这意味着接入 oMLX（`http://127.0.0.1:8088/v1`）或 llama.cpp/mlx_lm.server 的 OpenAI 兼容端点，在 agentgateway 里是纯配置操作，与此前报告对 `genai`（有专门 `omlx` adapter）、`aisix-provider-openai`（`resolve_base()` 模式）的结论一致——这是三个项目里第三个独立验证出"任意 OpenAI 兼容 base_url 零代码接入"这一设计共识的项目。

### 2.3 fallback / retry / 负载均衡

- 通用负载均衡：`crates/agentgateway/src/types/loadbalancer.rs` 实现 `EndpointGroup<T>`/`EndpointSet<T>`，`Sampler` 枚举含 `Weighted`（`WeightedIndex`，loadbalancer.rs:76-77,111-113）与基于 HRW 一致性哈希的 `RendezvousScore::Weighted`（loadbalancer.rs:442-453，用于会话亲和/一致性路由场景），配合驱逐 worker（`EvictionWorkerState`/`EndpointEvent`/`EvictionEvent`，loadbalancer.rs:589-607）根据健康事件动态调整活跃端点集合。
- 被动故障隔离：`crates/agentgateway/src/http/outlierdetection.rs`（101 行，连续失败驱逐，Envoy outlier detection 同款语义）。
- 主动重试：`crates/agentgateway/src/http/retry/mod.rs:15-26` 定义 `Policy` 结构体，含 `backoff: Option<Duration>` 等字段。
- LLM 虚拟模型层的加权/条件路由：`crates/agentgateway/src/llm/model_router.rs:170` `VirtualModelRouting`、`model_router.rs:186` `ConditionalTarget`、`model_router.rs:177` `WeightedTarget`；`model_router.rs:436` 用 `targets.choose_weighted(&mut rand::rng(), |target| target.weight)` 做加权随机选择。`examples/llm-cost-routing/config.yaml:24-33` 演示了一组按 `llmRequest.metadata.cost_tier`/`llmRequest.max_tokens` 的 CEL 条件表达式选择 economy/balanced/premium 三档模型，**且要求必须有一个无条件的兜底 target**（配置注释明确写"The final target is the required fallback"）——即"没有匹配候选就落到显式兜底"，与 iDoris 不变式 #10"没有合格候选时保留现状"语义不同（agentgateway 要求显式兜底目标，不是"保持原状"）。

意图/语义路由（按分类结果选 backend）**不是内建能力**：`examples/llm-semantic-routing/` 目录展示的是与外部项目 **vllm-project/semantic-router**（此前报告 §2.9 已评估并排除为 Rust 基础候选，因主体是 Go）集成的用法——`examples/llm-semantic-routing/standalone/tier-aware-single-runtime/semantic-router-config.yaml` 本身就是 semantic-router 自己的配置 schema（`providers.defaults`/`routing.strategy: priority`/`signals.keywords`/`role_bindings`），而不是 agentgateway 的配置格式。即"意图分类"在 agentgateway 生态里是一个文档化的外部集成模式，不是内建分类器。

---

## 3. "budget and spend controls" 的真实实现

代码位置：`crates/agentgateway/src/http/budget/{mod.rs(769行), database.rs(313行), status.rs(101行), sqlite_schema.sql, sqlite_upsert.sql, postgres_schema.sql, postgres_upsert.sql}`。

### 3.1 是真金额预算，不只是 token 限流

`budget/mod.rs:126-172`：`Budget { name, limit: BudgetLimit{ unit: BudgetLimitUnit, amount: BudgetAmount }, window, on_budget_exceeded }`；`BudgetLimitUnit` 有 `Usd` 和 `Tokens` 两种（mod.rs:210-224）；`BudgetAmount` 内部是 `rust_decimal::Decimal`，反序列化时拒绝负数（mod.rs:163-172）。这是**真实的美元金额记账**，比此前评测的 aisix（`budget.rs` 文档自述"纯粹问 Cloud 控制面是否放行"，本地零额度表）、Noveum（"内存态ledger，重启即丢"）都更进一步。

### 3.2 有没有 reserve/settle 两阶段

**没有。** 是"先查已用量再放行，响应回来后再记账"，不是"先扣预留额度再放行"：

- `check()`（`budget/mod.rs` 内，对应此前读到的完整实现）：只 `refresh()` 计数器窗口并读取当前累计 `amount` 与限额比较，超限且 `on_budget_exceeded=Block` 才拒绝；**这一步不对任何计数器做增量或预留**。
- `settle()`（`budget/mod.rs:478-517`）：只有在 LLM 响应返回、拿到真实 usage/cost 后才把 `charged` 加到 `counter.amount`/`counter.pending`。
- 结论：`check()` 和 `settle()` 之间没有互斥/预留机制。若干个并发请求在预算接近上限时同时到达，可以**全部**通过 `check()`（因为谁都没有预扣），然后各自结算——超支幅度等于"检查瞬间的并发请求数 × 单请求成本"，这是设计使然，不是需要修的 bug（代码注释和测试都没有试图掩饰这一点）。这与 iDoris 不变式 #3"预算是拒绝不是降级"隐含的"准入必须原子"的要求**直接冲突**。

### 3.3 并发下是否原子

上一条已回答：**准入判定（check）非原子**。但**落盘（flush）阶段是原子且经过多进程并发测试验证的**：`budget/database.rs` 的 `flush()`（对应 §3 已读源码）用 SQLite/Postgres 事务批量 `UPSERT`（`sqlite_upsert.sql` 用 `ON CONFLICT DO UPDATE SET used_amount = CASE WHEN 窗口/单位匹配 THEN 旧值+新值 ELSE 覆盖 END`，实现"同窗口累加、跨窗口覆盖"的幂等增量写入），随后重新读取全部持久化行做 `reconcile()`。`budget/mod.rs` 测试模块里的 `flushes_only_new_usage_with_atomic_increments`（完整测试代码已读）显式模拟了两个独立 `BudgetPolicy` 实例（模拟多进程共享同一数据库）分别写入 `pending=3` 和 `pending=4`，验证两次 flush 后数据库和内存最终一致地得到 `7`——这是一个真实的多写者正确性测试，工程质量很高。**结论：写入路径是原子且经过并发测试的，但准入判定路径不是**——这是两件独立的事，评测时不能因为持久化写得好就误判整体"预算是原子的"。

### 3.4 持久化方式

SQLite 或 Postgres（见 §1.3）。后台每 5 秒（`FLUSH_INTERVAL = Duration::from_secs(5)`，`budget/database.rs:16`）触发一次 `flush()`。若进程在 `settle()` 之后、下一次 `flush()` 之前崩溃，最多丢失 5 秒内累积的 `pending` 用量（尚未落盘）。

### 3.5 价格未知时的语义：不是 fail-closed，而是"未知即不计费"

`budget/mod.rs:481-492`（已读源码原文）：
```rust
let charged = match budget.limit.unit {
    BudgetLimitUnit::Usd => response.cost.as_ref().map(|cost| cost.total()),
    BudgetLimitUnit::Tokens => response.total_tokens.map(Decimal::from),
};
let Some(charged) = charged else {
    tracing::debug!(..., "API key budget could not be charged because usage was unavailable");
    continue;
};
```
当模型不在成本目录（catalog）里、`response.cost` 为 `None` 时，这条分支**直接跳过**——请求既不被拒绝，也不会计费，效果等同于"未知价格 = 免费通过"。这与 iDoris 不变式 #3 明确要求的"价格未知 ≠ 免费：要么拒绝，要么按保守上限估算"**正相反**，是本报告发现的最重要的一处不变式冲突点，若走 fork 路径必须重写这段逻辑。

### 3.6 拿不到状态时 fail-open 还是 fail-closed

分两层看：
- **配置期**：`register()` 强制要求"有预算的 API key 必须配置数据库"，否则直接返回 `Err`（对应测试 `budgets_require_a_database`）——**配置期是 fail-closed**（不允许"预算功能悄悄不生效"）。
- **运行期**：`check()` 只读内存态 `DashMap`，从不直接访问数据库；数据库短暂不可达时，准入判定仍基于最近一次成功 reconcile 的内存状态继续工作，只是下一次 `flush()` 会重试/排队。**即数据库运行期不可达不会让请求路径 fail-closed，只会延迟持久化**——内存态才是准入判定的权威来源，这是一个合理但需要如实记录的设计取舍（类似 TensorZero 的 `observability.enabled: null` 三态降级思路），不完全等同于 iDoris "拿不到预算状态就拒绝"的诉求。

---

## 4. guardrail / PII

代码位置：`crates/agentgateway/src/llm/policy/{mod.rs(2708行), webhook.rs(614行), streaming_guardrails.rs(888行), moderation.rs(56行), bedrock_guardrails.rs(317行), google_model_armor.rs(401行), azure_content_safety.rs(382行), pii/*.rs}`。

### 4.1 内置检测器

`crates/agentgateway/src/llm/policy/pii/` 下有 6 个正则识别器文件：`email_recognizer.rs`、`phone_recognizer.rs`、`us_ssn_recognizer.rs`、`ca_sin_recognizer.rs`（加拿大社保号）、`credit_card_recognizer.rs`、`url_recognizer.rs`，加一个通用 `pattern_recognizer.rs`（用户自定义正则）。**但这套 `Recognizer` trait 实现经核实是死代码/在制品**：`pii/mod.rs:1` 明确标注 `#![allow(dead_code)]`，全仓库 `grep -rn "policy::pii|pii::Recognizer"` 除 `pii/` 目录自身文件外**零调用点**，未接入 `policy/mod.rs` 主链路。
真正在生产路径上生效的内置检测器是**另一套独立的正则规则**：`policy/mod.rs:2093-2105` 定义 `pub enum Builtin { Ssn, CreditCard, PhoneNumber, Email, CaSin }`（5 种，与 `pii/` 目录的 5 个识别器同名但代码不复用），通过 `RegexRule::Builtin { builtin: ... }` 挂到 `promptGuard.request[].regex.rules[]`，单测 `mod.rs:2653-2669` 与 `examples/llm-prompt-guard/config.yaml:12-40`（`regex: {action: reject, rules: [{pattern: "SSN"}, {builtin: email}]}`）均证实其被路由主链路使用。**结论**：实际可用的内置检测器是 5 种纯正则匹配（无 Luhn/ISO7064 等校验位算法），少于 aisix 的 11 种；`pii/` 目录是一套写好但未启用的候选实现，评估时不应把它算作"已生效能力"。

### 4.2 外部集成（均为真实实现，非桩代码）

- OpenAI Moderation：`moderation.rs:33`，真实 POST 到 `https://api.openai.com/v1/moderations`。
- AWS Bedrock Guardrails：`bedrock_guardrails.rs:240`，真实拼接 `bedrock-runtime.{region}.amazonaws.com`。
- Google Model Armor：`google_model_armor.rs`，定义了 RAI 过滤、越狱/提示注入过滤、恶意 URL 过滤、CSAM 过滤、病毒扫描、去标识化等结果结构体，覆盖 Model Armor 的完整响应 schema。
- Azure AI Content Safety：`azure_content_safety.rs`（382 行）。

### 4.3 外部 webhook 机制

`crates/agentgateway/src/llm/policy/webhook.rs`：请求侧 POST 到 webhook 的 `/request` 路径，body 为 `GuardrailsPromptRequest{body: PromptMessages{messages}}`（webhook.rs:41-45）；响应侧 POST 到 `/response`，body 为 `GuardrailsResponseRequest{body: ResponseChoices{choices}}`（webhook.rs:60-65）。webhook 需返回 `RequestAction`/`ResponseAction`（`Pass`/`Mask`/`Reject`三态，webhook.rs:126-140）。**CEL 上下文注入**：`apply_header_expressions()`（webhook.rs:199-226）允许把 `request.headers[...]`、`jwt.sub`、`llmRequest.model` 等 CEL 表达式求值结果写进发给 webhook 的 HTTP 头，同时有专门测试 `claims_are_not_attached_to_the_outgoing_request`（webhook.rs 测试模块）验证**原始 JWT 不会被泄漏到发给外部 guardrail 的请求体本身**——这是一个干净的"控制面上下文传递但凭证不出站"范式。

### 4.4 失败时的语义：默认 fail-closed，可显式配置 fail-open

`llm/policy/mod.rs:2128-2137`：
```rust
/// Defines how the proxy behaves when a guardrail provider is unreachable or returns an error.
/// Defaults to `failClosed`. When failing closed, the error is propagated and the LLM request
/// is rejected. When failing open, the request is allowed through despite the provider failure.
pub enum FailureMode {
    #[default]
    FailClosed,
    FailOpen,
}
```
实际生效位置：`mod.rs:983-989`（`apply_single_request_guard`）——`Err(e) if guard.failure_mode() == FailureMode::FailOpen => (GuardrailOutcome::FailOpen, None)`，否则错误直接向上传播导致请求被拒绝；同样的分支模式在 realtime/WebSocket 路径（`mod.rs:589-602`）和响应侧护栏（`mod.rs:1885-1889`）里各自重复了一遍。**这是本次两轮调研（含 plano/aisix/llamastash/TensorZero）里最清晰、覆盖最全的"默认 fail-closed、按需 fail-open"guardrail 实现**——优于 plano（护栏是死代码）、aisix（guardrail 本身可用但 budget 的 fail-mode 只是纯 RPC 降级）、Noveum（`failClosed` 字段只覆盖成本上限,不覆盖 guardrail）。

### 4.5 过滤阶段

三种阶段都是原生支持：请求前过滤（`apply_single_request_guard`，作用于 prompt）、响应后过滤（`ResponseGuard`，作用于完整 completion）、**真·流式/chunk 级过滤**（`streaming_guardrails.rs:232` 的 `GuardedSseBody` + `evaluate_window`，见 §2.1）。

---

## 5. 授权与策略

### 5.1 API key / 虚拟 key

`crates/agentgateway/src/http/apikey.rs:441-514`：`LocalAPIKeys`/`LocalAPIKey`（支持明文 key 或预哈希 `key_hash`），每个 key 可挂 `AllowedModels`（模型白名单/通配模式，apikey.rs:174-277 一带的 `AllowedModelPattern`/`ModelAccessPolicy`）和独立的 `budgets: Vec<Budget>`（apikey.rs:466,481-484）。这本质上就是一套**虚拟 key 系统**：网关持有真正的上游 provider 凭证（配置在 `llm.models[].params.apiKey`），对外发放的是可独立限权、独立记预算的虚拟 key，与 iDoris"可信网关持有凭证，调用方只认虚拟 key"的不变式 #7 相符。

### 5.2 JWT

`crates/agentgateway/src/http/jwt.rs` + `jwt_tests.rs`；另有 `crates/agentgateway/src/http/oidc/{mod.rs, session.rs}` 支持 OIDC 会话（`ui.rs` 里 `AuthorizationContext` 与会话对象的交互，约 290-332 行区域）。

### 5.3 CEL 策略引擎能表达什么

两个 crate 支撑：`crates/cel-fork/cel`（23,034 行，完整 CEL 解释器 fork）+ `crates/celx`（2,628 行，agentgateway 自己的函数扩展：`cidr()`、字符串/flatten/math helper）。上下文对象 `Executor`（`crates/agentgateway/src/cel/types.rs:41`）暴露：
- `request.*`：method/path/headers（`cel/tests.rs:351-415` 显示 headers 支持 `.redacted()`/`.raw()`/`.split()`/`.join()` 等访问器）；
- `jwt.*`：JWT claims（`cel/mod.rs:393`）；
- `llmRequest.*`：解析后的请求体字段，如 `.model`/`.max_tokens`/`.metadata.*`（`cel/types.rs:64`，`examples/llm-cost-routing/config.yaml` 大量使用）。

**关键问题的答案——能否基于自定义请求头做路由/拒绝，能否限定只路由到特定 backend：可以，且是两条独立路径都支持，不需要写代码。**
1. CEL 路径：任何 `promptGuard`/webhook/`virtualModels.routing.conditional.when` 表达式都可以读 `request.headers["x-idoris-privacy"]`，据此 Reject 或选择一个命名的 target。
2. 结构化路由匹配路径：`crates/agentgateway/src/types/agent.rs:1158` `RouteMatch`、`agent.rs:1184` `HeaderMatch`，通过 `model_router.rs:692-699` 的 `header_matches`/`headers_match` 参与路由决策——即"带 `local_only` 头的请求只匹配到某条指向 loopback backend 的 Route，其余 Route 一律不匹配"是原生声明式配置，不需要插件。

### 5.4 虚拟 key 与租户隔离

虚拟 key 已在 §5.1 确认存在。**专门的租户隔离层未找到**——全仓库 `tenant` 关键词命中的都是命名惯例（如 webhook 测试里的 `x-tenant` 头示例）或 `mcp/auth.rs`/OAuth 相关的无关代码，没有一个"Tenant"实体拥有自己的独立存储命名空间。隔离实际是靠"每个虚拟 key 有自己的 budget 行"实现的（`budget/mod.rs` 的 `budget_id(api_key_id, budget)` 函数按 `api_key_id` 生成唯一键）——对 iDoris"个人/小团队多虚拟 key"场景够用，但不是不变式 #4 要求的那种专门的租户数据隔离层（FU-15）。

---

## 6. 路由扩展点

综合 §2.3、§5.3 的证据：**CEL 条件路由 + 结构化 header/path 匹配**两条声明式路径已经覆盖"按隐私级别只选 loopback backend"和"按 metadata/cost_tier 选择 backend"，都不需要写代码或插件。"按意图分类结果选 backend"本身可以表达（只要有个东西把分类结果写进 header 或 `llmRequest.metadata`，CEL 立刻能用），但**分类推理这一步不是内建的**——agentgateway 没有自己的意图分类器，文档化的做法是接一个外部的 `vllm-project/semantic-router` 服务。

**除 webhook 外，还有两个更底层、独立于 guardrail 机制的标准 Envoy 扩展点，经核实是完整实现（不是只有文档提到的 webhook 一种）**：
- **ext_proc（gRPC External Processor）**：`crates/agentgateway/src/http/ext_proc.rs`。`ExtProc` 结构体（约272行起）的 `mutate_request`/`mutate_response`（约239/265行）可在请求/响应阶段调用外部 gRPC 服务改写请求、注入路由用的动态元数据；`FailureMode`（`ext_proc.rs:152,179`，默认 `FailClosed`，可配 `FailOpen`）。官方示例 `examples/llm-semantic-routing/standalone/tier-aware-single-runtime/agentgateway.yaml` 里 `llm.policies.extProc: {host: semantic-router:50051, failureMode: failClosed, processingOptions: {requestBodyMode: fullDuplexStreamed, ...}}` 把 vLLM semantic-router 接成外部 ext_proc 服务做语义路由——这是 iDoris 想把自己的 System-1 判定层（隐私/意图/复杂度）做成独立进程、完全不碰 agentgateway 源码的最直接落地范例。
- **ext_authz（gRPC External Authorization）**：`crates/agentgateway/src/http/ext_authz.rs`（1509行）。`FailureMode`（`ext_authz.rs:94-96`）支持 `Allow`/`Deny`/`DenyWithStatus(status_code)`（`ext_authz.rs:321-332`）三态，比 ext_proc/guardrail 的二态 fail-open/fail-closed 更细粒度，可以精确控制"外部授权服务不可达时返回哪个 HTTP 状态码"。

扩展机制层面：agentgateway **没有 WASM/插件 ABI**（不同于 plano 的 Envoy+WASM 架构），进程内脚本能力是 CEL 表达式；CEL 之外的定制靠 **webhook（guardrail 专用）+ ext_proc/ext_authz（通用请求/鉴权拦截，Envoy 标准协议）** 两类外部服务钩子，或改源码，三选一。

---

## 7. 观测与审计

### 7.1 OTel GenAI 支持

真实实现了 OTel GenAI 语义约定属性：`crates/agentgateway/src/telemetry/log.rs:1824-1863` 包含 `gen_ai.operation.name`、`gen_ai.provider.name`、`gen_ai.request.model`、`gen_ai.response.model`、`gen_ai.usage.input_tokens`、`gen_ai.usage.output_tokens`、`gen_ai.usage.cache_creation.input_tokens`、`gen_ai.usage.cache_read.input_tokens`。

### 7.2 访问日志能否只存元数据

**可以，是内建的二态开关。** `crate::types::frontend::DatabaseLlmMode` 枚举含 `Full` 和 `Metadata` 两个变体（`telemetry/log.rs:149,152,167`），测试 `telemetry/log.rs:3345-3406` 显式验证 `Metadata` 模式下 `database_llm_payload()` 返回 `None`（即 prompt/completion 正文被丢弃），`Full` 模式才携带正文。配置项 `frontendPolicies.accessLog.database.llm: full`（见 `examples/llm-guardrail-jev/config.yaml:5-7`）意味着 `metadata` 是另一个可选值。这与 iDoris 不变式 #5"审计账本只存元数据"的诉求高度吻合，且是比 iDoris 规划中"字段白名单+黑名单"更简单的一个二态开关，可以直接借鉴思路。

### 7.3 反馈/实验能力

**未找到**。`feedback`/`experiment`/`canary`/`ab_test` 等关键词在 `crates/agentgateway`、`crates/llm` 下均无对应模块或端点。

---

## 8. 本地模型运行时管理

**确认没有，与预期一致。** 全仓库 `tokio::process::Command`/`std::process::Command` 的唯一使用点是 `crates/agentgateway/src/mcp/upstream/mod.rs:681-683`，用于**通过 stdio 拉起 MCP 工具服务器**（Model Context Protocol 的 tool backend），与 LLM 推理进程无关。唯一的"idle_ttl"/驱逐概念是 `crates/agentgateway/src/mcp/mod.rs:54` 的 `DEFAULT_SESSION_IDLE_TTL`（30 分钟），作用对象是 **MCP 会话对象**，不是模型进程。全仓库没有 `load_model`/`unload_model`/模型进程驱逐的任何代码路径。

结论：agentgateway 是纯数据面/网关，"本地运行时"对它而言就是一个已经在跑的 HTTP 端点（无论是 OpenAI、Bedrock，还是 `http://localhost:11434/v1` 的 Ollama，或手工配置的 `http://127.0.0.1:8088/v1` oMLX）——它从不管理这个端点背后的进程生命周期。这一条完全没有改变此前报告"没有一个候选做本地运行时管理，llamastash 是当前最佳参考"的结论。

---

## 9. Admin API

### 9.1 是否可写

**可写，且有独立的授权层。** `crates/agentgateway/src/ui.rs:92-116` 定义的路由包含：
- `GET/POST /api/config`（`.get(get_config).post(write_config)`，ui.rs:94）
- `GET /api/config/effective`、`GET /api/config/resources`（ui.rs:95-96）
- `GET+PUT /api/config/resources/{kind}`（`list_config_resources_by_kind`/`upsert_config_resources_by_kind`，ui.rs:97-100）
- `PUT+DELETE /api/config/resources/{kind}/{id}`（`update_config_resource`/`delete_config_resource`，ui.rs:101-104）
- `GET /api/budgets/status`（ui.rs:114）、`POST /api/costs/refresh-base`（ui.rs:113）
- 日志检索：`POST /api/logs/{search,get,tail}`、`/api/logs/analytics/summary`（ui.rs:108-111）

所有写操作经过 `AuthorizationContext.authorize_write()` 校验（ui.rs:131,565,582,672,692），支持 OIDC 会话（ui.rs 约 290-332 行）——即写权限不是"只要能连上就能写"，而是有一层可配置的授权判断。

另有一套独立的**运维/调试**面（不是配置 CRUD）：`crates/agentgateway/src/management/admin.rs`（653 行）暴露 `/debug/pprof/*`、`/memory`、`/quitquitquit`（优雅关闭）、`/debug/tasks`、`/debug/trace`、`/config_dump`、`/logging`（admin.rs:199-209），功能上对应 Envoy 的 admin 端口。

### 9.2 配置热更新机制

`crates/agentgateway/src/config_store.rs`：`ConfigResourceStore` 用 `tokio::sync::watch` channel（`change_tx`，config_store.rs:10,19）做进程内变更通知；跨进程场景（多个 agentgateway 实例共享同一个 Postgres）用 `pg_notify`（config_store.rs:1592,1708,1755,1780-1784，日志文案"postgres config change listener reconnected; reloading config"在 config_store.rs:192）。对 iDoris 的单机个人版场景（单进程 + SQLite）而言，进程内 `watch` channel 已经足够——**Admin API 写入配置后无需重启进程即可生效**。

---

## 10. 工程质量

### 10.1 测试规模与 CI

- 测试属性数：`crates/agentgateway/src` 下 `#[test]`/`#[tokio::test]` 合计约 **1,937** 个，`crates/llm/src` 下约 **323** 个；另有 `crates/agentgateway/tests/` 下 **27 个**集成测试文件（含 `tests/llm.rs`、`tests/llm_providers.rs`、`tests/config_store.rs`、`tests/substrate.rs` 等）。
- Fuzz：`fuzz/fuzz_targets/{llm_request_conversions.rs, cel_expression.rs, proxy_protocol.rs}`——三个 fuzz target 里有一个专门针对 LLM 请求转换，一个专门针对 CEL 表达式解析，是真实的 fuzz 覆盖，不是摆设。
- CI：`.github/workflows/{pull_request.yml, nightly.yml, release.yml, model-catalog.yml, cache-e2e, debug_cache.yml}`。`pull_request.yml` 内并行任务包括：Rust 测试（`make test`，启动真实校验依赖 `tools/manage-validation-deps.sh`，行 64）、`make lint`（clippy）+ `make generate-schema check-clean-repo check-default-members`（schema 漂移检查，行 98）、UI 端到端测试（Playwright，`pnpm test:e2e`，行 123）、Go controller 的 `go test -race ./...`（行 147）与真实 kind 集群 e2e（`TEST_MODE=e2e ./controller/test/setup/setup-kind-ci.sh`，行 189）。根目录 `osv-scanner.toml` 表明依赖漏洞扫描接入了 CI/发布流程。

### 10.2 近 3 个月的提交频率与贡献者构成

**数据限制说明**：本地 clone 用的是 `--depth 200`，只能看到最近 200 次提交，不是完整历史，无法直接回答"近 3 个月"这个字面问题——200 次提交的时间跨度是 `git log --format="%ad" --date=short` 显示的 **2026-09-08 至 2026-09-26，共 18 天**，换算约 **每天 11 次提交**，属于本次两轮调研里活跃度最高的项目之一（与 plano、TensorZero 全盛期相当，远高于 aisix 的 2-5 次/天）。若要精确统计"近 3 个月"提交数，需要不限深度的完整 clone。

贡献者集中度（200 次提交样本）：`John Howard` 115 次（**57.5%**），`github-actions[bot]` 14 次，`Keith Mattix II` 7 次，`Jacob Bohanon`/`Ian Davies`/`Daneyon Hansen` 各 4 次，其余约 20+ 位贡献者各 1-3 次。**这是一个单一维护者高度主导的项目**（比 aisix 的 10 人分布、plano 的更大社区都更集中），尽管有正式 LF 治理章程兜底（不依赖单一公司善意），仍应把"关键逻辑高度集中在一个人手上"记为 bus factor 风险点。

### 10.3 许可证逐字核对（含子目录）

- 根目录 `LICENSE`：标准 Apache-2.0 全文，与官方文本 diff 仅剩排版空白差异和末尾模板化的 Copyright 占位符附录，**无附加条款**。
- `find . -iname "LICENSE*" -not -path "./.git/*"` 找到两个文件：`./LICENSE`（Apache-2.0）和 `./crates/pool/LICENSE`（**MIT**，`Copyright (c) 2023-2025 Sean McArthur`）。`crates/pool/Cargo.toml` 中 `license = { workspace = true }` 表明该 crate**自身对外声明的许可证仍是 workspace 统一的 Apache-2.0**，保留的 MIT LICENSE 文件是为了保留其 fork 自的上游代码（连接池实现）的原始署名——MIT 代码并入 Apache-2.0 项目是标准且合规的常见做法，不影响整体仓库的许可证结论，也不构成 GPL/AGPL/Elastic/BSL 类风险。
- CLA/DCO：**未找到**明确的 CLA bot 配置或 `CLA.md`；`.github/pull_request_template.md` 只有一条"人类撰写文字，非 LLM 生成"的 Code of Conduct 勾选项，采样到的 workflow 文件里也没有 DCO sign-off 强制检查。鉴于本项目是 LF Projects 章程治理（`CHARTER.md` 的 TSC 模式），贡献条款很可能在 LF 层面统一处理而非仓库本地 CLA bot，但本报告**未能在仓库内找到具体文件**，需要向项目维护者/LF 渠道另行确认。
- Go controller（`go.mod` module `github.com/agentgateway/agentgateway`）与主仓库共用同一顶层 `LICENSE`，未发现独立许可证声明。
- 另有两个子 crate 通过 `Cargo.toml` 元数据（而非独立 LICENSE 文件）声明了与 workspace 不同的许可证：`crates/cel-fork/cel/Cargo.toml:9` 与 `crates/cel-fork/cel-derive/Cargo.toml:6` 均为 `license = "MIT"`（google/cel-rust 的 fork）；`crates/htpasswd-verify-fork/Cargo.toml:6` 为 `license = "Apache-2.0"`（与主许可证一致）。均为宽松许可证，与 Apache-2.0 兼容共存，不构成风险。

### 10.4 商业公司背景

README.md、CHARTER.md **全文搜索均未出现 "Solo.io"**——仓库自身文档呈现为纯 LF Projects 治理项目（TSC 模式），不体现某商业公司控制关系。但这只是"文档不提及"，不等于"没有商业公司事实主导"：本地 200 个提交样本按作者邮箱域名统计，**约 67% 来自 `@solo.io` 邮箱**（与 §10.2 统计的 `John Howard` 单人 57.5% 提交占比相符——John Howard 及多名高频贡献者的邮箱均属 `solo.io` 域名，二者是同一批数据的不同切面，不矛盾）。Solo.io 是做 Envoy/Istio/Gateway API 商业化的公司（Gloo Gateway 厂商）。**结论**：这是"Linux Foundation 治理背书 + Solo.io 事实主导日常开发"的典型模式，背景材料里"原属 kgateway 生态"的说法与此一致，本报告在仓库内未直接找到该表述但邮箱域名分布可以佐证 Solo.io 的实际主导地位。风险提示：与 katanemo/plano 背靠 DigitalOcean 类似，需关注未来商业化重心是否转向 Solo.io 自己的产品线导致开源侧维护节奏下降，截至调研当天（每天约11次提交）无任何降速迹象。

---

## 11. 结论：三条路径评估

### (a) fork agentgateway，把 iDoris 策略链加进去

- **工作量估算：6-10 周**（2 名熟悉 Rust 的工程师）。拆解：2-3 周吃透 189K 行主 crate 的请求生命周期（`proxy/httpproxy.rs`）+ 两层 CEL 实现；2-3 周把 `budget` 从"先查后记"改造成真正的 reserve/settle 两阶段并接入原子 SQLite 事务；1-2 周把 guardrail 的 `FailureMode` 语义与 iDoris"隐私只能收紧不能放宽"的规则对齐；1-2 周替换"价格未知不计费"为"价格未知即拒绝"。
- **风险**：①单一维护者主导（57.5%提交）+ 每天约 11 次提交的高速迭代，fork 后长期 rebase 成本高；②189K 行单体 crate + 31 个 `pub mod` 的"整体即库"架构，即使只想要 budget+guardrail+CEL 路由三个子系统也要拖入整个依赖图（AWS SDK、`agent_xds` 间接 k8s 依赖、OpenTelemetry 全家桶），与 iDoris"单二进制、依赖精简"的精神有张力；③K8s/xDS/Go controller 虽非强制运行依赖，但整个仓库的 CI/发布习惯围绕它设计，fork 后需要自建一套"仅数据面"的裁剪构建流程。
- **与 14 条不变式冲突点**：#1——**更正**：策略顺序其实是显式、硬编码的，不是"隐式由代码排布决定"。`crates/agentgateway/src/proxy/httpproxy.rs` 的 `apply_request_policies`（约200-362行）按固定顺序依次调用：`cors → oidc → jwt → basic_auth → api_key → budget → ext_authz → authorization → substrate_egress → substrate_ingress`，budget 排在身份鉴权之后、外部裁决之前，**全链路没有独立的"隐私"步骤**，顺序语义是"先鉴权、再查预算、最后授权/外部裁决"，与 iDoris 要求的"隐私→预算→意图→admission→降级"是两套不同的顺序契约，需要在自己的 ext_proc/ext_authz 扩展点或重排 fork 后的主链路里重建，不能假设"顺序已经对"；好消息是重试循环在这个策略链之后才发生，budget/apikey 检查不会因重试被重复触发。#3（check 非原子、价格未知不拒绝，需要重写）；#4（无专门租户存储隔离层，需要新增）；#12（`model_router.rs` 的多目标负载均衡用 `rand::rng()` 加权随机，与"路由必须确定性"冲突，见§2.3）；**#14（不 vendor 第三方源码/进程边界即授权边界）与路径 (a) 本质冲突**——fork 一个 189K 行的仓库并长期维护，很难被论证为"不是 vendor 第三方源码"，除非团队明确拍板"fork 之后就是我们自己的代码，全权维护不再同步上游"，否则不变式 #14 直接否决路径 (a)。

### (b) 把 agentgateway 当作前置数据面，iDoris 策略用外部服务/扩展实现

- **做法细化**：优先用 **ext_proc**（§6，`failureMode: failClosed` 有官方示例背书，能同时改写请求/拿到动态元数据供后续 CEL/`Backend::Dynamic` 路由消费）承载 iDoris 的隐私/预算/意图判定服务，比 guardrail 专用的 webhook 协议更通用；简单的"按 header 值路由到指定 backend"场景可以直接用原生 `HeaderMatch` 路由规则（§5.3），不必上 ext_proc。
- **工作量估算：3-5 周**。拆解：1 周跑通"单二进制 + SQLite + Ollama/自定义 baseUrl"的最小闭环；1 周实现 iDoris 自己的隐私/预算判定 ext_proc 微服务（复用上一份报告里 aisix 的 `pii.rs`/`cooldown.rs` 等自研代码作为其内部实现）；1 周把 CEL/HeaderMatch 路由规则配置到位并测试"带 `X-iDoris-Privacy: local_only` 头的请求只路由到 loopback backend"这条关键路径；1-2 周做端到端集成测试和失败模式验证（ext_proc/webhook 不可达时是否真的 fail-closed，需要在 iDoris 自己的服务里显式处理，不能白拿 agentgateway 的默认行为）。
- **风险**：①把 iDoris 核心策略做成外部服务，增加一次网络跳转（单机场景是本地回环，性能影响可控）和一个必须与 agentgateway 同生共死的额外进程——除非把这个 webhook 服务和 agentgateway 一起纳入 iDoris 自己的 Runtime Supervisor 管理范围（iDoris 本来就要管理 oMLX 等本地进程，把 agentgateway 也当作一个被管理的子进程是自然延伸，而不是额外负担）；②如果不完全信任 agentgateway 原生 budget/guardrail 实现（尤其是 §3.2/§3.5 指出的两个缺口），关键策略必须整体退化成外部 webhook，agentgateway 原生功能实际只剩路由/协议转换/可观测性三块被真正使用；③Admin API 写路径和配置热更新依赖数据库配置，`authorize_write` 的 CEL 授权机制有额外学习成本。
- **与 14 条不变式冲突点**：这是三条路径里唯一**不与 #14 冲突**的路径——完全不 touch agentgateway 源码，只使用其公开的 webhook/CEL/配置接口，agentgateway 作为独立进程天然满足"进程边界即授权边界"。#1（顺序完全由 iDoris 自己的 webhook 服务决定，反而更容易满足"顺序即语义"）；#7（可信网关侧持有凭证与 agentgateway 的虚拟 key/params.apiKey 设计基本吻合）；风险点在 #8（不静默）——降级语义需要 iDoris 在自己的 webhook 里显式实现并写入响应头，agentgateway 本身的 fail-open/closed 只是"默认值友好"，不能替代 iDoris 自己的责任。

### (c) 只借鉴设计

- **工作量**：0（记入常规设计文档编写，不单独立项）。
- **风险**：错失一个已经把"CEL 声明式路由 + 分层 guardrail + 真金额 budget + metadata-only 审计日志 + 可写 Admin API + 配置热更新"做到生产级、且是纯净 Apache-2.0 + LF 治理的现成实现的机会，"重新发明轮子"的成本是三条路径里最高的。
- **值得借鉴的具体设计点**（无版权风险，只学思路不抄代码）：
  1. `FailureMode::{FailClosed(默认)/FailOpen}`——"每个 guard/webhook 可单独配置，但默认 fail-closed"的模式（`llm/policy/mod.rs:2128-2137`）。
  2. `DatabaseLlmMode::{Full, Metadata}`——审计日志"全量/仅元数据"二态切换字段设计（`telemetry/log.rs:149-167`），比 iDoris 规划的"字段白名单+黑名单闸门"更简单，可以先落地这个再逐步细化。
  3. CEL 驱动的 conditional routing + HeaderMatch——用同一套表达式语言同时表达"guardrail 拒绝条件"和"路由选择条件"，避免自己发明两套 DSL。
  4. `BudgetPolicy` 的"内存态权威 + 定期 flush 落盘 + 启动时预加载 + 跨进程 reconcile"架构——即便准入非原子，这套"内存态优先、异步持久化、多写者收敛"的骨架本身值得借鉴；iDoris 可以在此基础上加一层真正的原子 reserve（check 阶段就用 SQLite 事务扣减，settle 阶段再修正差额）来补齐两阶段语义。
  5. `webhook.rs` 的 CEL header 注入模式——把 JWT/请求上下文作为 CEL 表达式注入外部 guardrail 请求头，同时保证原始 JWT 不出现在请求体本身（`claims_are_not_attached_to_the_outgoing_request` 测试）——直接对应不变式 #11"远程路径要去关联"。

### 推荐与排名

**推荐路径 (b) 为主、(c) 为辅，不推荐路径 (a)。**

理由：agentgateway 在"预算/guardrail/CEL 路由/审计/可写 Admin API"这五个 iDoris 相关维度上，是本次两轮调研（plano、aisix、llamastash、TensorZero + agentgateway）里工程质量最高、覆盖面最广的一个，尤其"默认 fail-closed 的 guardrail"和"metadata-only 审计日志开关"是目前看到的最佳实现。但它依然不满足三大刚需中任意一条的完整形态（预算非两阶段原子、价格未知不拒绝、零本地运行时管理），**没有改变"没有单一候选能同时满足三大刚需"的整体结论**。它改变的是角色定位——189K 行单体、单一维护者主导、每天 11 次提交的高速迭代节奏，使得 fork 路径的长期维护成本远高于把它当"外部、独立进程的可信数据面"使用；这恰好也符合它自身的架构初衷（CEL/webhook 是其设计好的策略外置扩展点，不是一个期待被 fork 改源码的框架）。

与 plano、aisix 的对比排名（延续上一份报告"没有整体冠军，各有专精"的结论，补充维度）：

| 维度 | 排名 | 依据 |
|---|---|---|
| 能否直接作为 iDoris 对外数据面使用（路径 b 视角） | **agentgateway > plano > aisix** | aisix 的 budget 是纯 Cloud RPC 桩、Admin 只读；plano 护栏是死代码；agentgateway 两者都是生产级实现，只是 budget 非两阶段。 |
| 能否直接抄局部代码到 iDoris 自己的 Rust 骨架（vendor/参考） | **三者互补，无单一冠军** | aisix 的 `pii.rs`/`cooldown.rs`/ratelimit reserve-like trait 设计 + plano 的 `hermesllm` 协议转换 + agentgateway 的 CEL/webhook/`DatabaseLlmMode` 设计，各有各的最佳实现。 |
| 工程质量/维护活跃度 | **agentgateway(LF治理+当天提交+CI全面) ≈ aisix(APISIX商业团队+测试覆盖最高) > plano(社区大但护栏是空壳)** | 见各自 §10/背景报告 §2.1-2.2 对应章节。 |

综合建议：把 agentgateway 加入 iDoris 技术选型清单，角色是"可信前置数据面 + 外部 policy webhook 扩展"，并明确记录三处必须自研/改造的缺口（预算两阶段原子化、价格未知 fail-closed、租户存储隔离层），且完全不 touch 其源码（满足不变式 #14）。

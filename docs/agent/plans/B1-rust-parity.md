# B1 `feat/rust-parity` —— Rust 版追平 TS 参考实现（R6 切换前提）

> 规划：工作站 A（主会话 Opus）· 差异分析与任务拆分：工作站 B Codex（2026-10-01，基线 `main@30d43c3`）
> 协作规则见 [`../COLLAB.md`](../COLLAB.md)。本文是 B1 的唯一任务来源；每个 task 的范围、验收、负对照以下文 §2 表格为准。

## 0. 执行约定

- 集成分支 `feat/rust-parity`（从 `main` 拉出）。每个 task 一个 worktree + 分支 `feat/rust-parity-NN-<短名>`，PR 目标 `feat/rust-parity`；
  依赖的 task 还没合并时，从依赖 task 的分支拉出、PR 目标设为依赖分支（堆叠 PR），依赖合并后 GitHub 会自动改指向。
- 每个 PR ≤ 300 行（含测试、fixture、文档）。新增 FU 只用 200–299。
- 合并条件：prdaemon 批准 + CI 全绿（B 机 steward 自动合并目标为 `feat/*` 的 PR）。

## 1. 发版切片（每个切片完成后开一次 release PR：`feat/rust-parity` → `main`）

| 切片 | task | 说明 |
|---|---|---|
| ① CI + 安全/规则 | 01–07 | rust job 跑 conformance；组件卡语义校验、`local_only` 三条件复核、routing-policy 真正生效 |
| ② 审计与账单查询 | 16–29 | 落 FU-20：record_id 可查；按租户用量/预算/审计查询（Rust 新增功能，用 Rust HTTP 验收，不进共同 conformance） |
| ③ 意图 + 后端接线 | 09–15、30 | 意图兜底、后端探测/工厂、逐卡 runtime、真实模型名下发、models 失败冷却 |
| ④ 容量接口 | 31–33 | `/capabilities`；33 依赖 B2 推荐器 |
| 杂项（随时插入） | 08、34–39 | 选择语义、CLI、配置根、启动出网验收、输入边界、驱逐语义、版本 drift（FU-21） |

## 2. 暂定拍板（⚠️ 待 jason 确认；B 先按此执行，推翻时再调整）

| # | 问题 | 暂定结论 |
|---|---|---|
| D-B1-1 | Rust 与 TS 的行为差异是否允许保留 | **保留 Rust 的安全改进**（scheme 白名单、models 超时、强类型 schema、Supervisor Busy 互斥），列入"允许差异清单"并在 conformance 里写成已知差异；**不允许例外**：YAML 规则执行（07）、`local_only` 三条件（05）必须补齐。多候选选择（08）：保留 Rust 按 admission→估价→ID 的确定性排序，作为允许差异记录，不改回"注册顺序第一张"。 |
| D-B1-2 | FU-20 查询形状 | 沿用 `docs/agent/contract-tenancy.md` 的 tenant audit 端点，增加 `record_id=` 精确过滤；15 字段审计 payload 不变，关联 ID 放存储/响应元数据。新查询只做 Rust 必跑 HTTP 验收，TS 不补 M4 新功能。 |
| D-B1-3 | 租户配置、余额口径、持久化失败 | 时区/限额来自本地可信配置；`remaining = limit − spent`，另加 `reserved`、`available` 字段；配置或存储不可用时报错，**不能返回假零账单**；审计写入失败 → 请求照常返回但记录告警并计数（不阻塞推理），在 PR 里写明。 |
| D-B1-4 | 单二进制的配置定位 | 显式配置根（环境变量 / 参数）或安装配套目录，支持绝对路径；不嵌入开发机路径；验收 `cwd=/` 能启动。34 采用 `idoris serve` 子命令，同时保留无参数启动（兼容现有脚本）。 |

---

## 附：B 机 Codex 的差异分析与任务拆分（原文，路径已改为仓库相对路径）

核对基线是本地 `main@30d43c3`。已读 AGENTS.md、远端交接分支上的 COLLAB/HANDOFF，以及总体规划 §7.0；未修改文件，也未运行会生成构建产物的测试。下述 47 passed / 7 todo 沿用交接基线，不是本次重新跑出的结果。

**结论：B1 的缺口比交接表列出的四项更多。优先补注册语义校验、隐私复核和 routing-policy 接线；随后补审计、用量查询、后端探测、意图兜底。`/capabilities` 完整交付依赖 B2，订阅门禁依赖 B3。**

**1．差异清单**

位置缩写：TS 路径均相对 `packages/`；Rust 链接指向当前工作区。表中的“未覆盖”专指 `conformance/`，不表示没有单元测试。

| 功能 | TS 位置及实际行为 | Rust 现状与证据 | conformance 覆盖 |
|---|---|---|---|
| `/capabilities` HTTP 入口 | `router/src/server.ts:202`；顶层数组，提供者失败返回 503 `capabilities_unavailable` | **没有**。注册路由只有 health、models、chat；该地址落入 404。[router/lib.rs:211](../../../crates/idoris-router/src/lib.rs:211) | 未覆盖 |
| 容量条目与动态快照 | `router/src/capabilities.ts:103`；resident/temp/blocked、内存两位小数、能力优先级、每次重算推荐并读取状态 | **没有**。推荐器仍为空模块。[recommender/lib.rs:10](../../../crates/idoris-recommender/src/lib.rs:10)；完整实现依赖 **B2** | 未覆盖 |
| 容量的 `queue_depth` | `router/src/capabilities.ts:152`；所有后端 `loaded.length` 求和；失败后端不贡献，但记脱敏警告 | **部分**。oMLX 状态解析已有，容量聚合没有。[upstream/omlx/status.rs:56](../../../crates/idoris-upstream/src/omlx/status.rs:56) | 未覆盖 |
| 组件卡**语义**校验 | `contracts/src/validate.ts:75`；local_only 必须 fail_closed、provider 隐私下限、local tier/locality、自洽 load_policy、none 独占、local_only 出网约束 | **部分，重要缺口**。`ComponentCard::validate` 主要检查结构、非空字段；注册层另有重复 ID、URL/locality、relay 矛盾检查，没有上述完整规则。[contracts/component_card.rs:51](../../../crates/idoris-contracts/src/component_card.rs:51)、[policy/registry.rs:177](../../../crates/idoris-policy/src/registry.rs:177) | 未覆盖这些负例 |
| extensions 降级声明 | `contracts/src/validate.ts:45`；卡及 provider 的非空 extensions 必须有非空 `_degradation` | **没有**。Rust 允许 extensions，当前校验不检查降级声明。[contracts/component_card.rs:47](../../../crates/idoris-contracts/src/component_card.rs:47) | 未覆盖 |
| routing-policy 首条匹配与默认规则 | `router/src/policy.ts:22,39`；privacy/intent/complexity/capabilities 条件，首条命中，默认 action，tiers 与隐私允许集合相交 | **只有加载校验**。二进制读完 policy 后丢弃；实际决定不消费 YAML rules。[router/bin/idoris.rs:91](../../../crates/idoris-router/src/bin/idoris.rs:91)、[router/dispatch.rs:126](../../../crates/idoris-router/src/dispatch.rs:126) | **部分**：默认文件存在、坏路径退出；没有证明“改变规则会改变路由” |
| 候选选择顺序 | `router/src/dispatch.ts:44`；通过 tier/privacy 筛选后选注册顺序第一张卡 | **行为不同**。Rust 按 admission、估价、ID 排序选择；不是 TS 的注册顺序。[policy/pipeline/mod.rs:146](../../../crates/idoris-policy/src/pipeline/mod.rs:146) | 未覆盖多候选选择 |
| local_only 运行时复核 | `router/src/dispatch.ts:25`；要求 effective locality=loopback、卡 privacy_class=local_only、egress 全为 none/loopback | **部分，重要缺口**。Rust 隐私阶段只筛 effective locality，没有后两项复核。[policy/pipeline/mod.rs:179](../../../crates/idoris-policy/src/pipeline/mod.rs:179) | **部分**：只有唯一 remote 候选；没有“loopback 但不可信”的卡 |
| locality 与启动 URL 断言 | `router/src/locality.ts:30`、`registry.ts:40,77`；relay 实际 locality=remote，拒绝矛盾声明、loopback host 不符 | **已有**，Rust 还扩大了网络 form/scheme 检查范围。[policy/privacy.rs:34](../../../crates/idoris-policy/src/privacy.rs:34)、[policy/registry.rs:121](../../../crates/idoris-policy/src/registry.rs:121) | **部分**：响应头覆盖普通 HTTP；注册负例、relay 情况未覆盖 |
| 启动出网断言 | `router/test/egress-probe.ts:79`、`egress.test.ts:25`；socket 探针覆盖启动、models、health，并验证探针确实看到真实连接路径 | **没有等价验收**。当前启动流程没有连接观测断言。[router/bin/idoris.rs:77](../../../crates/idoris-router/src/bin/idoris.rs:77) | 未覆盖。Node 探针不能观测 Rust 子进程 |
| 订阅 egress guard | `router/src/egress-guard.ts:61,92`；来源 loopback/Tailscale、personal 部署、enable/disable、沙箱门禁 | **没有完整门禁**；只有 relay locality/矛盾声明检查。注册模块明确排除了部署/沙箱门禁。[policy/registry.rs:1](../../../crates/idoris-policy/src/registry.rs:1) | 未覆盖；归 **B3** |
| 后端探测 | `adapters/src/detect.ts:34`；Apple Silicon→omlx，Win/Linux+NVIDIA→未实现 vllm，其余→未实现 llama_cpp；事实可注入 | **没有**。当前启动直接构造 OmlxAdapter。[router/bin/idoris.rs:54](../../../crates/idoris-router/src/bin/idoris.rs:54) | 未覆盖。TS 的探测函数也没有接进 serve；当前 NVIDIA 事实默认 false |
| 后端工厂与逐卡绑定 | `adapters/src/factory.ts:17`、`router/src/registry.ts:128`；逐卡创建后端，未知 provider 报错 | **部分**。Resident HTTP 直接转发；生命周期卡只找第一张、构造 oMLX；AppState 只有一个 Supervisor。[router/bin/idoris.rs:57](../../../crates/idoris-router/src/bin/idoris.rs:57)、[router/lib.rs:156](../../../crates/idoris-router/src/lib.rs:156) | 未覆盖；所有 fixture 都是 Resident HTTP |
| 实际模型 ID 接线 | `router/src/server.ts:378`；HTTP 转发保留请求 body/model | **部分**。直连路径已有；Supervisor 路径把 chosen provider ID 当 model ID，没有传请求中的具体模型。[router/dispatch.rs:284](../../../crates/idoris-router/src/dispatch.rs:284)、[router/dispatch.rs:365](../../../crates/idoris-router/src/dispatch.rs:365) | 未覆盖 Supervisor 路径；角色→catalog 模型绑定依赖 **B2** |
| 意图识别兜底 | `router/src/intent.ts:40,69,181,220`；示例 embedding+余弦、阈值、编码成功缓存/失败重试、显式头优先、remote detector 不处理 local_only | **没有识别器**；只有 header/default=chat 两种来源。[router/profile.rs:57](../../../crates/idoris-router/src/profile.rs:57)、[router/lib.rs:486](../../../crates/idoris-router/src/lib.rs:486) | 未覆盖自动识别。TS 默认是 **hashing embedding**，尚未接真实 embedding 服务 |
| 审计记录校验 | `router/src/audit.ts:170`；15 字段白名单、内容黑名单、标量、500 字符、四类 reason、tenant 一致 | **没有**。TenantStore 仍为空模块。[tenancy/lib.rs:9](../../../crates/idoris-tenancy/src/lib.rs:9) | 未覆盖 |
| 请求审计接线、record_id 可查 | `router/src/server.ts:156`；finish/close 各路径只写一次 stderr JSON。**AuditWriter 未接入，无持久化/HTTP 查询** | **只有 Record-Id 头**，中间件不持久化记录。[router/lib.rs:717](../../../crates/idoris-router/src/lib.rs:717)；FU-20 是两边共同缺口，B1 明确要求补齐 | **部分**：ID 生成/不可伪造/缓存 origin；不验证能查到记录 |
| tenant-scoped Store | `tenancy/src/store.ts:25`；usage/budget/audit 的 put/list/get 都必须有 scope，A 查不到 B；TS 是内存数组 | **没有**。[tenancy/lib.rs:9](../../../crates/idoris-tenancy/src/lib.rs:9)。预算账本的 tenant 隔离不能替代通用记录存储 | 未覆盖 |
| 月度聚合与 UTC 区间 | `tenancy/src/billing.ts:195,217,264`；租户 IANA 时区、`[from,to)`、tokens/cost/calls、range 回显、可选 audit 来源 | **部分**。已有按时区计算 `YYYY-MM` 的预算分桶；billing 模块为空，没有区间/聚合查询。[tenancy/budget/period.rs:31](../../../crates/idoris-tenancy/src/budget/period.rs:31)、[tenancy/lib.rs:22](../../../crates/idoris-tenancy/src/lib.rs:22) | 未覆盖 |
| 预算余额查询语义 | `tenancy/src/billing.ts:292`；最新 budget 快照或 TenantContext；remaining=limit−spent | **部分且语义不同**。已有 tenant_balance，但返回 limit−spent−active reservations；没有完整响应字段。[tenancy/budget/ledger.rs:437](../../../crates/idoris-tenancy/src/budget/ledger.rs:437) | 未覆盖 |
| usage/budget/audit 查询入口 | 文档 `docs/agent/contract-tenancy.md:133` 已列端点；TS billing/store 仅库函数，server 未注册 | **没有 HTTP 入口**。[router/lib.rs:211](../../../crates/idoris-router/src/lib.rs:211)。不能称为“已有 TS HTTP API 的移植” | 未覆盖 |
| 驱逐锁 | `router/src/evict-lock.ts:16`；per-backend 等锁、超时 OomError。**该工具类未接 server 请求路径** | **已有互斥，等待行为不同**。Supervisor 合并相同 load，其他冲突立即 Busy，没有等待队列。[backend/supervisor.rs:820](../../../crates/idoris-backend/src/supervisor.rs:820) | 未覆盖；不应再造一把锁 |
| 后端失败冷却 | `router/src/health.ts:12`、`server.ts:190`；models 连续失败3次后冷却30秒，成功清零 | **没有**。Rust models 每次继续访问全部 HTTP 卡，只跳过本次失败。[router/models.rs:71](../../../crates/idoris-router/src/models.rs:71) | 只覆盖正常 models 列表 |
| `/health` 字段、版本号 | `router/src/server.ts:176`、`version.ts:12` | **已有且当前一致**：status/service/version/contract_version/instance_id/components；服务版本均0.1.0，契约版本均1.0.1。[router/lib.rs:232](../../../crates/idoris-router/src/lib.rs:232)、[contracts/version.rs:12](../../../crates/idoris-contracts/src/version.rs:12) | 字段、契约版本、同实例ID稳定已覆盖；服务版本精确值及重启变化未覆盖 |
| CLI、配置定位、启动诊断 | `router/src/cli.ts:17`、`serve.ts:55,96`；serve 子命令、相对仓库根定位、端口占用提示、mock 跳过提示 | **部分**。端口/loopback/启动失败已有；不解析 argv，相对 cwd 定位，mock 另需 dev-mock feature。[router/bin/idoris.rs:77](../../../crates/idoris-router/src/bin/idoris.rs:77)、[router/components.rs:34](../../../crates/idoris-router/src/components.rs:34) | 默认策略与坏路径部分覆盖；任意 cwd、CLI 参数、mock 双开关未覆盖 |
| 请求边角输入 | `router/src/server.ts:251` 空 body 按 `{}`；`profile.ts:34` 空头当缺省；health 仅 GET | **行为不同**：空 body invalid_json；空控制头拒绝；axum `get(health)` 还接受 HEAD。[router/lib.rs:467](../../../crates/idoris-router/src/lib.rs:467)、[router/profile.rs:91](../../../crates/idoris-router/src/profile.rs:91) | 未覆盖这些边界 |
| proxy 重试/流式/幂等 | `router/src/proxy.ts:198` | **已有主要行为**：重试、SSE、缓存隔离/上限/TTL、历史 locality、origin ID。[router/proxy.rs:208](../../../crates/idoris-router/src/proxy.rs:208)、[router/lib.rs:584](../../../crates/idoris-router/src/lib.rs:584) | **部分**：未覆盖跨租户同ID、TTL/上限、网络异常及截断 body 等全部分支 |
| 契约版本 drift | `contracts/src/version.ts`；FU-21 已登记常量不受生成检查管理 | **当前值已有**，但仍手动同步，Rust drift 测试主要检查 schema 形状。[contracts/lib.rs:9](../../../crates/idoris-contracts/src/lib.rs:9) | 锁定当前1.0.1；不是通用 drift 门禁 |
| LoRA mount/aggregate 身份门禁 | `contracts/src/adapter-gate.ts:75,110` | **没有对应业务门禁**，只有 manifest 结构校验。[contracts/adapter_manifest.rs:51](../../../crates/idoris-contracts/src/adapter_manifest.rs:51) | 未覆盖；消费方为 growth/federation，按 §7.0 留到 M7，排除 B1 |

另外，TS `policy.ts` 返回的 `then.capability/load` 当前也未由 TS dispatch 执行；不能在移植时顺手赋予新语义。付费直连预算、鉴权、完整协议面分别归 B4、B5、B7，不因 B1 扩充用例而纳入本次。

**2．按依赖排序的任务拆分**

以下是可评审切片，**每项预算包含新增、删除、测试、fixture、文档，总计≤300行**；目标控制在240–260行，留出评审修改空间。所有 task 用独立 worktree，PR 目标为 `feat/rust-parity`；新增 FU 只使用200–299。

文件缩写：

- `R/`＝`crates/idoris-router/src/`
- `P/`＝`crates/idoris-policy/src/`
- `T/`＝`crates/idoris-tenancy/src/`
- `U/`＝`crates/idoris-upstream/src/`
- `C/`＝`conformance/`

“先扩充”表示**该任务先写断言，证明 TS 正控通过、旧 Rust 负控失败，再修实现后合并**，不先合入一个持续红灯的测试 PR。新建文件以下只列规划路径。

| 顺序／任务分支 | 改动文件与依赖 | 验收：新增测试与负对照 | 先扩充 conformance |
|---|---|---|---|
| `feat/rust-parity-01-ci` | `.github/workflows/ci.yml`、`scripts/conformance-rust.sh`；≤100行 | rust job 真正跑 release 二进制；指向错误被测命令/破坏一条行为时 job 红 | 否，先接现有套件 |
| `feat/rust-parity-02-fixtures` | `C/src/fixtures.ts`、`harness.ts`；≤200行；供03–08使用 | 支持省略 load_policy、extensions、定制 policy、cwd；保持原47例通过；坏路径仍真实提前退出 | 基础设施扩充 |
| `feat/rust-parity-03-card-policy` | `P/card_validation.rs`、`registry.rs`；≤260行；依赖02 | 移植隐私/fail_closed、tier/locality、load_policy、egress交叉规则；每类合法卡正控、仅改变一个字段即启动非零退出 | 是，新增启动负例 |
| `feat/rust-parity-04-extension-policy` | `P/card_validation.rs`、对应测试；≤200行；依赖03 | provider及卡extensions：空对象通过、有声明通过、缺/空/非字符串声明拒绝 | 是 |
| `feat/rust-parity-05-local-capable` | `P/privacy.rs`、`pipeline/mod.rs`、`C/tests/local-only-privacy.test.ts`；≤240行；依赖03 | local_only完整三条件；loopback+privacy=any拒绝且上游0次，改成可信卡则成功；纯函数负例覆盖绕过注册的egress违规卡 | 是 |
| `feat/rust-parity-06-rule-evaluator` | `R/routing_policy.rs`、独立测试；≤260行 | 纯函数移植条件、首条命中/default、tiers交集；冲突规则证明首条优先；remote规则不能放宽local_only | 否，先纯逻辑测试 |
| `feat/rust-parity-07-rule-wiring` | `R/bin/idoris.rs`、`lib.rs`、`dispatch.rs`；≤260行；依赖06、05 | policy存入状态并用于两条执行路径；只改policy时请求从200变503/改选tier；未命中走default | 是 |
| `feat/rust-parity-08-selection-contract` | `P/pipeline/mod.rs`或差异说明、`C/tests/routing-policy.test.ts`；≤240行；依赖07，须先确定选择语义 | 双候选证明注册顺序与ID顺序不同；锁定批准的选择规则；禁止“任一成功即通过”的弱断言 | 是 |
| `feat/rust-parity-09-intent-hashing` | `R/intent/embedding.rs`及测试；≤260行 | TS固定向量对照：FNV、signed hashing、L2、余弦、零向量；中文/emoji验证UTF-16行为；相异文本负控 | 否 |
| `feat/rust-parity-10-intent-detector` | `R/intent/detector.rs`及测试；≤260行；依赖09 | 每route取最大分、阈值、最后非空user、示例成功缓存；首次embed失败后第二次能恢复；低分/空文本不命中 | 否 |
| `feat/rust-parity-11-intent-wiring` | `R/intent/mod.rs`、`profile.rs`、`lib.rs`；≤260行；依赖10、07 | 显式头不调用detector；默认缺头可识别；异常/非法intent回chat；remote detector对local_only调用数0，privacy=any正控调用 | 是：自定义coding规则使识别结果可观测 |
| `feat/rust-parity-12-backend-detect` | `U/detect.rs`、`lib.rs`及测试；≤220行 | 注入OS/arch/NVIDIA矩阵，与TS相同kind/implemented；Linux无GPU不得误报omlx已实现；不加载模型 | 否 |
| `feat/rust-parity-13-backend-factory` | `U/factory.rs`、`R/runtime.rs`；≤240行；依赖12 | 显式卡决定适配器，探测结果仅作推荐；未知生命周期provider拒绝，不静默套oMLX；Linux假上游仍能运行 | 否，Rust工厂测试 |
| `feat/rust-parity-14-runtime-registry` | `R/runtime.rs`、`lib.rs`、`bin/idoris.rs`；≤260行；依赖13 | 逐卡保存并按chosen provider取handle；两MockAdapter互不串用；missing handle拒绝且另一后端正常 | 否，Rust集成测试 |
| `feat/rust-parity-15-model-dispatch` | `R/dispatch.rs`、`lib.rs`；≤240行；依赖14 | 具体模型名传入选中adapter，不拿provider ID冒充模型；provider=model不同的正控；未知模型拒绝 | 否；角色模型解析部分等B2 |
| `feat/rust-parity-16-record-schema` | `T/store.rs`、`store/migrations/`、`lib.rs`；≤220行 | SQLite usage/audit记录结构，独立record_id与request_id；重开库仍有记录；同request_id允许多个HTTP记录 | 否 |
| `feat/rust-parity-17-scoped-store` | `T/store.rs`及测试；≤260行；依赖16 | put/list/get强制tenant；A/B同record_id隔离；缺/空scope报错；去掉tenant条件后测试必失败 | 否 |
| `feat/rust-parity-18-store-bootstrap` | `R/storage.rs`、`AppState`、`bin/idoris.rs`；≤240行；依赖17 | 明确DB及租户配置来源，personal内部scope固定；重启数据保留；不可写DB/未知tenant不得默认为空账单 | 否，子进程测试 |
| `feat/rust-parity-19-audit-validation` | `R/audit.rs`、`reason.rs`及测试；≤260行；依赖17 | 15字段、黑名单、标量、reason、tenant一致、UTC时间；500/501与emoji边界；内容sentinel被拒且库中不存在 | 否 |
| `feat/rust-parity-20-audit-buffered` | `R/audit.rs`、`lib.rs`、`dispatch.rs`；≤260行；依赖18、19 | 成功、解析/策略拒绝、后端错误各写一次；record_id等于响应头；缓存重放有新record及origin关联；重复finish不重复写 | 否，Rust HTTP测试 |
| `feat/rust-parity-21-audit-streaming` | `R/audit_body.rs`、流式接线；≤260行；依赖20 | 在body完成/取消时终结，记录完整耗时；收到响应头时尚未冒充完成；中断只落一条，正文sentinel不入库 | 否，Rust真实HTTP测试 |
| `feat/rust-parity-22-usage-write` | `T/usage.rs`、`R/usage.rs`、完成路径接线；≤260行；依赖20、21 | 免费调用记calls/tokens及0成本；付费成本取已settle结果；缓存不重复记推理用量；失败/取消不捏造完整usage，未知token不能当已测0 | 否；预算包装仍归B4 |
| `feat/rust-parity-23-billing-range` | `T/billing/period.rs`、`billing.rs`；≤240行 | YYYY-MM→租户时区UTC半开区间；曼谷精确边界、跨年、DST；非法月份/时区拒绝，服务器UTC边界负控不同 | 否 |
| `feat/rust-parity-24-billing-aggregate` | `T/billing.rs`及测试；≤260行；依赖17、23 | TS billing fixture逐字段对照；from包含/to排除；usage或显式audit来源只选一个；坏时间/数值不能静默漏账 | 否 |
| `feat/rust-parity-25-budget-readview` | `T/budget/ledger.rs`及测试；≤240行 | 同一读事务返回limit/spent/reserved/timezone/scope；reserve改变available不改变spent；settle后只转移一次，跨tenant隔离 | 否 |
| `feat/rust-parity-26-usage-http` | `R/queries/usage.rs`、路由注册；≤240行；依赖18、24 | `/idoris/tenants/{id}/usage?period=`响应齐全；path/header不一致拒绝；缺scope拒绝；调用方时区覆盖拒绝 | 否＊ |
| `feat/rust-parity-27-budget-http` | `R/queries/budget.rs`、路由注册；≤220行；依赖25、18 | `/budget`完整字段；锁定remaining与available口径；未知tenant不回全量/默认预算；查询不产生扣费 | 否＊ |
| `feat/rust-parity-28-audit-http` | `R/queries/audit.rs`、路由注册；≤260行；依赖20、21 | `/audit?from=&to=&limit=`；建议增加`record_id=`精确过滤；limit有上限、稳定排序；他租户ID无法查到 | 否＊ |
| `feat/rust-parity-29-query-acceptance` | `crates/idoris-router/tests/query_acceptance.rs`、固定fixture；≤260行；依赖26–28 | 请求→拿ID→查记录→重启→仍可查；三个TZ子进程月账一致且等于固定答案；漏scope/重复计缓存的变异必失败 | 否＊，独立必跑HTTP验收 |
| `feat/rust-parity-30-models-health` | `R/health.rs`、`models.rs`、AppState；≤260行 | 三次失败冷却30秒；冷却期上游调用数不变；时钟前进后恢复；成功清零；不同provider互不影响 | 是，新增models失败观测；时间边界用注入时钟单测 |
| `feat/rust-parity-31-capacity-fixtures` | `C/src/fake-upstream.ts`、容量fixture；≤200行 | 支持`/api/status`正常/失败、loaded变化；fixture自身正控验证状态与访问计数 | 测试设施扩充 |
| `feat/rust-parity-32-capacity-surface` | `R/capabilities.rs`、AppState、路由；≤240行；依赖14 | Provider边界可注入，数组8字段；提供者失败503；全响应有record_id；测试提供者不得成为生产假容量 | 否，先Rust HTTP测试 |
| `feat/rust-parity-33-capacity-live` | `R/capabilities.rs`、启动配置、recommender依赖边；≤260行；依赖31、32、**B2已发布接口** | resident/temp/blocked映射、两位小数、能力优先级、动态queue；单backend失败仍可用且日志无秘密；同输入与TS固定快照一致 | 是，形状及动态queue；算法细节使用固定facts测试 |
| `feat/rust-parity-34-cli` | `R/cli.rs`、`bin/idoris.rs`及子进程测试；≤220行 | 支持`idoris serve`、非法命令退出非0、端口占用给操作提示；是否保留裸启动按拍板执行，script同步 | 不需要公共HTTP用例 |
| `feat/rust-parity-35-config-root` | `R/config.rs`、`components.rs`、`routing_policy.rs`；≤240行；依赖配置定位拍板 | 选定配置根规则后，从`cwd=/`启动正确；相对/绝对路径正控；错误目录明确退出，不能偷偷找另一份配置 | 是，harness传cwd |
| `feat/rust-parity-36-startup-egress` | `scripts/check-rust-egress.sh`、最小fixture、CI step；≤240行 | Linux观测真实Rust进程connect；本地配置启动+health/models没有非loopback连接；故意真实reqwest外连必须被探针捕获 | 公共HTTP套件不够，需进程连接验收 |
| `feat/rust-parity-37-input-edges` | `R/lib.rs`、`profile.rs`、`C/tests/input-edges.test.ts`；≤260行；依赖边角行为拍板 | 空body/空头/重复头/HEAD逐例锁定；正常JSON、单值头、GET为正控；不能用成功状态掩盖路由未执行 | 是 |
| `feat/rust-parity-38-eviction-contract` | backend既有测试及差异文档；≤180行；依赖等待语义拍板 | 保留Busy时明确测试互斥、singleflight、取消后释放、两backend独立；若要求等待，另拆队列及超时实现PR | 否；不把TS未接线工具当现有HTTP契约 |
| `feat/rust-parity-39-version-drift` | `scripts/check-contract-drift.mjs`、契约版本测试；≤180行，可独立FU-21 | TS/Rust版本常量及规范版本一致；只改一侧必红；health字段无需重新实现 | 否，已有health版本断言 |

＊ **26–29 是新增 Rust 功能。** TS 当前没有这些端点，不能直接加入要求两边通过的公共 conformance，否则 TS 必红。建议保留共同套件，新增 Rust HTTP验收作为 cargo test 的必跑门禁；若要求这三类查询也由共同套件验证，需要另行授权 TS 补接口。

所有实现任务采用 AGENTS.md 的完整 Rust/TS 门禁；改变路由行为再跑 `bash scripts/conformance-rust.sh`。使用 MockAdapter/wiremock，不在 B 上加载真实模型。

建议按“CI＋安全／规则”“审计＋账单查询”“意图＋后端接线”“B2完成后的容量接线”形成可交付 release 切片。08、35、37、38涉及语义选择，39可单独处理；它们不应被悄悄算成已经消除的差异。

**3．接入 CI rust job 的具体做法**

在 [ci.yml:31](../../../.github/workflows/ci.yml:31) checkout 后增加与现有 gates job 相同的环境：

```yaml
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
          cache: pnpm
      - run: pnpm install --frozen-lockfile
```

在现有 Rust 检查后增加：

```yaml
      - name: Rust HTTP conformance
        run: bash scripts/conformance-rust.sh
```

同时把现有 cargo-deny 配置补齐本地门禁：

```yaml
      - uses: EmbarkStudios/cargo-deny-action@v2
        with:
          command: check advisories bans licenses sources
```

具体注意点：

- pnpm 版本从根 `package.json` 的 `packageManager: pnpm@10.34.5` 读取，action 不再写第二个版本。
- 脚本已负责 `cargo build --release --locked -p idoris-router`；`pnpm conformance` 已负责构建 TS router 依赖，CI无需再重复 `pnpm build`。
- [harness.ts:79](../../../conformance/src/harness.ts:79) 的 `IDORIS_CONFORMANCE_ARGV` **优先于** CMD。脚本应主动设置 JSON argv，避免继承值覆盖 Rust 命令：

```bash
export IDORIS_CONFORMANCE_ARGV="$(
  node -e 'process.stdout.write(JSON.stringify([process.argv[1]]))' "$bin"
)"
```

- task34若采用必须 `serve`，这里的 argv同步追加 `"serve"`。
- 默认 release 构建不启用 dev-mock；当前 fixtures 使用真实假HTTP上游，保持这条生产构建验收。
- 不加 `continue-on-error`、路径过滤或忽略失败。仍保留 gates job 的 TS conformance，形成两边独立必过。
- 修正脚本及 Rust 模块里“骨架只有health”“所有TS路由已接齐”等过时注释。
- task36的出网验收另需Linux连接观测工具，例如 `strace`；它是额外step，不能拿Node探针替代。

CI 首个切片的验收是：现有共同套件在 TS、Rust 两个job均通过；扩充后新增断言也必须进入相应必跑门禁，不能靠增加todo维持绿灯。

**4．风险与需要 jason 拍板的事项**

主要风险：

- **47/47 有明显盲区。** fixture均为免费 Resident HTTP，绕过Supervisor、真实模型选择、预算接线和多数启动门禁；它也没有用“只改变policy”验证规则执行。
- **审计与预算要共享事实来源。** 用量记录不能再次扣费；不能把预留当已消费，也不能把缓存重放当第二次推理。目前二进制未构造BudgetLedger，账单查询必须同时明确租户配置和存储来源。
- **流式审计不能在发出响应头时完成。** SSE结束、取消、写入失败需要独立终态处理；record_id、request_id、origin_record_id必须分开。
- **不要弱化已有Rust改进来制造表面一致。** scheme白名单、models超时、schema强类型、Supervisor互斥已有价值；需要明确哪些行为差异是允许保留的。
- **B2接线才决定完整容量与角色。** 仅返回静态数组、固定Ready或固定1GB，不能算`/capabilities`追平。B3必须补订阅注册/来源/沙箱门禁后，才具备相应R6切换条件。
- 时区数据库版本、JS UTF-16 hashing/字符串长度与Rust字符处理可能造成隐蔽漂移，必须使用固定fixture及边界负控。

需要拍板的四组问题：

| 问题 | 建议 |
|---|---|
| **“追平”是否允许已批准的Rust行为差异？** 多候选排序、空头拒绝、HEAD、驱逐Busy与TS不同 | 保留安全改进；明确例外清单。YAML规则执行和可信本地三条件必须补齐，不列为例外 |
| **FU-20查询形状及新增功能验证范围** | 沿用文档tenant audit端点，增加`record_id=`过滤；15字段审计payload保持原样，关联ID放存储/响应元数据。新查询先做Rust必跑HTTP验收，TS不新增M4功能 |
| **租户配置、余额口径和持久化失败策略** | 时区/限额来自本地可信配置；`remaining=limit−spent`，另以加法字段给出reserved/available。配置或存储不可用不能返回假零账单；审计写入失败的处理必须明定 |
| **单二进制的配置定位规则** | 使用显式配置根或安装配套配置目录，支持绝对路径；避免嵌入开发机仓库绝对路径，并验收`cwd=/`启动 |

B1完成仍不等于R6完成：还需B2推荐器及模型绑定、B3订阅中转验收，以及上述例外清单与新增查询验收一起通过。
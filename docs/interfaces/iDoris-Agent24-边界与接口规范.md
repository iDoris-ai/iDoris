# iDoris × Agent24 分工边界与接口数据规范

> **状态**：✅ **定稿 v1.0（2026-09-27）**。经 R0–R4 四轮协商，双方一致同意；D-1…D-7 由 jason 拍板，全部选 (a)。下一步是把 §4 转成 `packages/contracts/schema/` 下的 JSON Schema。之后的任何修改都走「提议 → 双方同意 → 升版本」，按加法兼容处理。
> **权威**：沿用 docs/17 D0。iDoris 是能力提供方，负责主持本规范；Agent24 是需求方，负责提需求、审阅、确认。定稿后以 **JSON Schema** 为真源，放入 `iDoris/packages/contracts/schema/`，Markdown 只是说明。
> **上位**：[`../iDoris-总体规划.md`](../iDoris-总体规划.md)（§6 管理面分工、§8 服务目录、§12 协商流程）。
> **承接**：Agent24 `docs/design/INTEGRATION-AGENTEAR-IDORIS.md`（ADR-032）§9 与附录 B 中**已经和 iDoris 谈定的条款全部继承**，本规范不重新谈。

---

## 0. 一句话

**两个独立组件，一份深度定制的公开契约。**
- Agent24 **做事**：任务、会话、工具、审批、记忆，以及完整的管理页。
- iDoris **准入**：哪个调用方、在什么隐私和预算约束下、用哪个模型；同时负责记账、审计和轨迹。
- 两者之间只有版本化的 HTTP 契约，没有代码级耦合。Agent24 可以直连任何 OpenAI 兼容 provider，但那样拿不到 iDoris 的保证。

---

## 1. 总原则

| # | 原则 | 出处 |
|:---|:---|:---|
| G-1 | 按**权限域**划分，不按「智能」划分：Agent24 管动作授权，iDoris 管推理资源准入 | iDoris `ecosystem-boundaries.md` §4.1 |
| G-2 | **只有一个策略源**：推理的路由、隐私、预算、审计策略归 iDoris；Agent24 不新增此类策略，只**传递**任务画像。**澄清（R1）**：Agent24 保留的 tier 授权门、LocalOnly 硬门、按 complexity 排 tier 顺序、Served-Locality 绊线都属于**动作授权**，是与 iDoris fail-closed **同向叠加**的第二道门，**不算**第二个推理策略源；它们不涉及选模型、预算或内容过滤 | ecosystem §6 驳回条目、docs/17 D1、R1 Q-9 |
| G-3 | 进程边界即授权边界：需要排除某类 provider 时，**起不同的 iDoris 实例**，而不是让调用方带排除头 | ADR-032 §9 |
| G-4 | 隐私**单向收紧**：Agent24 声明的 privacy 只能被 iDoris 维持或收紧，iDoris 仍独立做 fail-closed 复核 | ADR-032 §9、iDoris 总体规划不变式 2 |
| G-5 | 加法优先、零回归：新能力以新端点、新 header、新字段的形式出现；破坏性变更走契约大版本 | docs/09 R1、docs/17 |
| G-6 | 不静默：不支持的字段显式拒绝；降级和实际落点在响应头里回传 | iDoris 总体规划不变式 8 |
| G-7 | 敏感开关的最终确认**留在 iDoris**，Agent24 可以发起 | 总体规划 §6.1 |

---

## 2. 历史设计复用清单（回应「有用的都用上」）

逐一核对过两边仓库的历史文档，能直接用的都列在下面，并注明在本规范哪一节使用。

| 来源 | 可复用内容 | 用在 | 状态 |
|:---|:---|:---|:---|
| iDoris `packages/contracts/schema/*.json`（9 份） | ProviderDescriptor、ComponentCard、LoadPolicy、RoutingPolicy、TaskProfile、Tenant、DeployMode、AdapterManifest、TrainingSample | §4 全部 | ✅ **已实现**，JSON Schema 真源 + zod 生成 + 漂移门禁 |
| iDoris `agent/contract-tenancy.md` v1.3 | `X-iDoris-Tenant`、TenantContext、402、整数最小货币单位、`range_utc`、用量查询接口、归属表 | §3.2、§3.6 | ✅ **已冻结**（下游已在按它实现） |
| iDoris `agent/spec.md` | 路由状态机、失败分类（8 类）、重试与幂等语义、审计字段白名单、账期时区规则 | §3.4、§3.7、§3.11 | ✅ 已实现 |
| iDoris `docs/06` §10 | 控制面 header、`extensions/_degradation`、`/capabilities` 容量字段、组件六形态 | §3.1、§3.3 | ✅ 已设计，部分实现 |
| iDoris `docs/09` R1–R6 + `agent/handoff-agent24.md` | 接为 provider、header 传画像、LocalOnly 贯穿、推荐、不绑 oMLX、危险动作走审批 | §3.1–§3.5 | ✅ 已交接 |
| iDoris `docs/17` v0.2（D0–D7 已拍板） | 角色为键的注册表、`X-iDoris-Locality`（已演进为 Served-Locality）、D3 入口路由归 Agent24、D4 embedding/rerank 归 iDoris、8088 端口 | §3.3、§3.4 | ✅ 已拍板，**角色枚举待统一**（§5 Q-3） |
| iDoris `docs/15` | 闸一（INSPECTED 阶段、L0/L1/L2、BLOCK/REDACT/ALLOW）、中文规则标准、闸二凭证代理、闸三角色映射 | §3.5 | ✅ 已设计，代码未写 |
| iDoris `docs/16` | 角色卡、场景、硬门、失败分类 F1–F8、统计纪律 | §3.8（failure_mode）、评测 | ✅ 已设计 |
| iDoris `docs/13` | catalog 候选、内存公式修正、舰队拆分（oMLX 聊天/工具，llama.cpp 长上下文） | §3.3 | ✅ 部分进 catalog |
| iDoris `ecosystem-boundaries.md` | 四个权限域、各组件自己隔离的内容、B1/B2/B7 | §1、§3.2 | ✅ 已拍板（B7 待设计） |
| iDoris `research-secret-brokering` / `research-secrets-egress` | Keychain + Secure Enclave、LaunchAgent 布局、出站 DLP 工具选型 | §3.5 | ✅ 调研完成 |
| Agent24 ADR-032 §9 / 附录 B | `idoris-local`/`idoris-any` 双逻辑 provider；**Served-Locality 三值**；容量按 URL 聚合；`/v1/models` 去重；隐私单向映射；不加 `X-iDoris-Exclude` | §3.1、§3.3、§3.5 | ✅ **双方已谈定**，本规范继承 |
| Agent24 ME4-S2（`_a24/model/complete`）+ manifest `model_access` | 模块隐私由 manifest 决定（缺省 `local_only`）；并发和限速语义 | §3.5 | ✅ Agent24 已合入 |
| Agent24 ADR-017 | 轨迹默认仅本地、加密；贡献社区只发蒸馏后的 SKILL.md | §3.8 | ✅ 与 iDoris P-1 同向 |
| Agent24 `PLAN.md` §3.3–3.4 | Memory L3「ATIF 归档」、Evolver 读轨迹 | §3.8 | ⚠️ **只有规划文字，从未实现**（Agent24 `README.md:14`）。改为「交换格式 = ATIF v1.8；存储 = Agent24 M-D 事件日志；ATIF 是按需导出的投影」，零迁移成本（R1） |
| Agent24 Desktop 的 oMLX 管理（`apps/desktop/src/main/ipc/index.ts:27-190`、`Models.tsx:3-18`）+ `omlx.sh` | 探测、启停、预热、写死的模型目录 | §3.3 | 🗑️ **待退役**（R1 Q-9）：S1 阶段检测到 iDoris 时隐藏，S2 阶段删除（`omlx.sh` 保留给没有 iDoris 的开发者）。⚠️ `omlx.sh` 中有明文 API key，建议轮换 |
| Agent24 ADR-026 | Python 只做 ML Worker；非 Rust 模块经 Node Host/MCP 接入 | §1 | ✅ 不影响本规范 |

**已过期、不再使用**：llama-swap 方案；Ornith 作为默认主力（改为 Qwen3.5-9B，docs/13 §1.4）；docs/07 旧的预算三档公式；「iDoris 只是个人网关」的旧定位；Agent24 ADR-032 中「iDoris 未合并、没有守护进程入口、progress 停在 09-07」等描述（38 个 task 已合入 `preview`；守护进程入口列入 M4）。

---

## 3. 分层边界与接口

每层都写清四件事：**谁负责 / 谁不负责 / 接口 / 数据规范**。

### 3.1 接入协议与部署

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | 常驻守护进程（M4 补 `bin` 与 LaunchAgent）、固定端口 `IDORIS_PORT`、OpenAI/Anthropic 兼容端点、SSE | 把 iDoris 接成 `agent24-models` 的 provider：`IDORIS_URL` 注册为**两个逻辑 provider** |
| 不负责 | 拉起 Agent24；Agent24 的外壳 | 启动或托管 iDoris 进程（由 brain-dist 或用户负责） |

**两个逻辑 provider（继承 ADR-032）：**

| Agent24 provider | Tier | 固定 header | 用途 |
|:---|:---|:---|:---|
| `idoris-local` | Local | `X-iDoris-Privacy: local_only` | manifest 未声明或声明 `local_only` 的模块 |
| `idoris-any` | Remote | `X-iDoris-Privacy: any` | manifest 声明 `remote_allowed` 的模块 |

- `/v1/models` 在两个 provider 下会返回相同清单，Agent24 **按 URL 去重**。
- 容量（`/capabilities`）按 URL 聚合，**不能**把两个逻辑 provider 的容量相加。
- 端点（v1）：`POST /v1/chat/completions`、`POST /v1/messages`（M4）、`POST /v1/embeddings`（M4）、`POST /v1/rerank`（M4）、`GET /v1/models`、`GET /capabilities`、`GET /health`。

### 3.2 身份、租户与凭据

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | **签发虚拟 key**（每个 Agent24 实例或模块一把，带 scope）；tenant 策略权威；签发跨组件 tenant 能力凭据（B7，M8） | 在 Keychain 保存 iDoris 签发的 key；按模块选用 key；把用户或组织身份映射成 tenant |
| 不负责 | 用户认证（归 AirAccount / 组织 IdP）；Agent24 的工具凭证 | provider 真实 key（归 iDoris 闸二） |

- 请求头：`Authorization: Bearer idk_<key>`（M4 起。loopback 可配置为免 key；非 loopback 必须带 key）；`X-iDoris-Tenant`（tenant 模式必填，沿用 contract-tenancy v1.3）。
- key 的 scope（草案）：`{key_id, owner, allowed_privacy[], allowed_roles[], budget_ref, expires_at, admin_scopes[]}`。Agent24 管理页使用的 key 需要 `admin_scopes`。

### 3.3 模型、角色目录与容量

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | 角色 → 后端 + 模型 + 工件的解析；catalog；推荐；加载、驱逐、升级；Served-Locality | **只按角色请求**；调度前查 `/capabilities` |
| 不负责 | 任务应该用哪个角色（这是 Agent24 的判断） | 选模型、量化、下载、后端（docs/17 §2） |

- **角色即模型名**（稳定契约）：`idoris/fast`、`idoris/daily`、`idoris/deep`、`idoris/vision`、`idoris/embed`、`idoris/rerank`、`idoris/decide`，加上 `idoris/auto`（交给 iDoris 选）。具体模型名只作为信息返回，不是稳定契约。**角色枚举待确认（Q-3）**。
- `/capabilities` 每个角色返回：`{role, model, backend, locality, resident, estimated_memory_gb, ctx_limit, queue_depth, admission_status: ready|requires_eviction|blocked}`。

### 3.4 路由与意图

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | 角色内的模型选择、降级链、`/v1/systemone` 判定服务；缺少 intent 时用**静态缺省**（P-2） | **任务 → 角色/链路**的入口路由（D3）；把任务画像写进 header |
| 不负责 | 替 Agent24 决定走哪条业务路径 | 在 Agent24 内新增推理路由策略（G-2） |

- 请求头（已实现）：`X-iDoris-Intent / Complexity / Capabilities / Fallback / Request-Id`。
- **`POST /v1/systemone`**（M6）：Jev 兼容的判定接口，Agent24 的审批门、Evolver、入口路由都可以用。请求 `{state, questions: {<name>: {type: choice|score|noul, criteria|levels|instructions}}, privacy?}`，响应 `{answers: {<name>: {value, scores, confidence}}, judge: {engine: jev-remote|local-logit, model_revision, prompt_hash}, latency_ms}`。
- **判定引擎**：外部 TypeSafe Jev 只能用于 privacy 允许出本机的请求；`local_only` 请求一律用本地开源实现。

### 3.5 隐私

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | fail-closed 复核；**闸一内容过滤**（M5）；闸二凭证；Served-Locality 如实回传；出站去关联；kill switch | 由 manifest `model_access` 决定 header（单向）；**每次响应都校验 Served-Locality**，`idoris-local` 却拿到非 loopback 时报警并按错误处理 |
| 不负责 | Agent24 进程自身的出网（OS/PF 层） | 内容脱敏（交给 iDoris 闸一，不重复实现） |

- 响应头 `X-iDoris-Served-Locality: loopback | lan | remote`（契约已定，M4 实现）。**它证明的是路由没选错，不证明字节没出本机**，后者靠 `allowed_egress` 和出网探针保证（ADR-032 §9 原文）。
- `POST /v1/inspect`（M5）：只做判定，返回 `{sensitivity: S0..S3, findings: [{kind, span?}], action: BLOCK|REDACT|ALLOW}`，不改写内容。

### 3.6 预算与计费

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | 预算权威、402、reserve/settle、价格表、用量账本与查询 | 在管理页展示和审批（通过 Admin API）；把 402 当作业务拒绝，**不重试也不换 provider** |

- 响应头 `X-iDoris-Cost-Minor`（已实现）；查询 `GET /idoris/tenants/{id}/usage?period=`（contract-tenancy v1.3）。

### 3.7 审计与事件

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | 推理准入审计（只含元数据）、Event Log、`X-iDoris-Record-Id` | 动作审批审计（Agent24 自己的）；把 record id 记进自己的运行记录，形成**跨组件关联** |

- 两边审计**不合并、不互抄**，只通过 `record_id` 和 `trace_id` 关联。

### 3.8 轨迹、反馈与 ATIF

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | 网关侧轨迹采集（客户可选，默认 `metadata`，P-1）；`/v1/feedback`；导出 ATIF；两个开关与同意记录 | 请求带关联头；**把 👍/👎、用户修正回填到 `/v1/feedback`**；需要完整工具和结果链时推送 ATIF；Evolver 读取 iDoris 导出的 ATIF |
| 不负责 | Agent24 的长期记忆（MemPalace） | 保存推理内容副本用于训练（避免两份） |

- 请求头：`X-iDoris-Session`、`X-iDoris-Trace-Id`、`X-iDoris-Parent-Id`。
- `POST /v1/feedback`：`{record_id, rating?: up|down, rubric?: [{id, pass: bool}], corrected_output?, labels?[], outcome?}`。
- `POST /v1/trajectories`（Agent24 推送）：请求体为 **ATIF v1.8**，治理字段放在 `extra.idoris`（总体规划 §5.4）。
- **两边统一使用 Harbor ATIF v1.8**：Agent24 Memory L3 的「ATIF 归档」需对齐（Q-6）。

### 3.9 管理

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | **全量 Admin API**（`/admin/api/v1`，管理端口，只绑 loopback）；极简控制台（状态、kill switch、模型操作、请求决策轨迹）；**敏感操作的确认令牌** | **完整管理页**：路由规则与回放、隐私规则、预算与审批、轨迹与授权、学习与晋升、租户与 key |

- 敏感操作（开启内容记录、开启训练、离机训练、放宽隐私、关闭 kill switch）走两步：`POST .../proposals` 返回 `{proposal_id, diff, confirm_challenge}`；带上用户确认后 `POST .../proposals/{id}/confirm`。确认可以在 iDoris 控制台完成，也可以由 Agent24 转交用户确认凭据（形式待定，Q-8）。
- Admin API 资源（v1 草案）：`status`、`backends`、`models`、`roles`、`policies`（版本/diff/回放）、`privacy/rules`、`killswitch`、`budgets`、`approvals`、`keys`、`tenants`、`requests/{record_id}`、`trajectories`、`consents`、`datasets`、`training-jobs`、`adapters`、`bundles`（配置时间旅行）。OpenAPI 由 JSON Schema 生成。
- 实时推送：`GET /admin/api/v1/events`（SSE，只含元数据）。

### 3.10 学习产物回流

| | iDoris | Agent24 |
|:---|:---|:---|
| 负责 | 模型和 adapter 的训练、评测、晋升，**以角色名对外生效**（调用方零改动） | Skill、Prompt、规则层的进化（Evolver）；把 Evolver 的规则建议作为提案提交 |

- 训练开关和离机训练的规则见总体规划 §5.8 与 P-6。

### 3.11 流式、错误与取消

- SSE 与 OpenAI 语义一致；一旦开始吐 token 就不再重试，出错以 SSE error 事件结束（已实现）。
- **统一错误体**：`{error: {type, rule_id, reason_code, evidence, remediation}}`。`type` 取值：`local_only_unavailable`（503）、`budget_exceeded`（402）、`tenant_missing`（400）、`policy_violation`（403）、`unsupported_field`（400）、`context_too_long`（413）、`upstream_*`、`oom`。
- 取消：客户端断开连接即向上游传播取消（已实现）。

---

### 3.12 R1 达成的补充约定（与 §3.1–§3.11 冲突时以本节为准）

**接入（§3.1）**
- `IDORIS_URL` 没有默认端口，必须显式配置。
- **所有响应都带 `X-iDoris-Record-Id`**，包括非流式 JSON、错误响应和缓存命中。缓存命中时，另外带 `X-iDoris-Cached: true` 和 `X-iDoris-Origin-Record-Id`。
- **凡是已经选定后端的响应，都带 `X-iDoris-Served-Locality`**，包括成功响应和该后端返回的错误，流式与非流式都算。还没选定后端就被拒绝的请求（400/402/403/503 等）不带这个头；Agent24 遇到头缺失时，按 §3.5 视为错误或 remote，所以方向是保守的。
- Served-Locality 表示**推理实际发生的位置**，不是组件的来源地址。例如订阅中转 CLI 进程在本机，但推理在云端，所以报 `remote`。（v1.0.1 澄清，来自 T4.1 验收）
- `GET /health` 返回服务身份 `{service:"idoris", version, contract_version, instance_id}`，Agent24 启动时校验。
- 以上两项，加上「main 已合并」和「`IDORIS_PORT`」，合称 **M4 接入前置**。前置完成之前 Agent24 不接入（D-2）。

**fail-closed 细则（§3.5）**
- `idoris-local` 的响应**只接受 `loopback`**。头缺失、`lan`、`remote` 都按错误处理，结果不返回给调用方（`lan` 也拒绝，依据是 D2「LAN/Tailscale 算远程」）。
- `idoris-any` 按 Served-Locality 记账：`loopback` 记为 local，`lan`/`remote` 记为 remote，**头缺失时保守记为 remote**。

**虚拟 key（§3.2）**
- 每个 Agent24 实例两把 key（D-6）：
  - `idoris-local` 用的 key：`allowed_privacy=[local_only]`。
  - `idoris-any` 用的 key：`allowed_privacy=[local_only, any]`。
- **iDoris 在 key 这一层结构性拒绝「local key 发出 `any`」**。
- 模块永远接触不到 key，现有 wire 结构（`deny_unknown_fields`）已经保证这一点。
- 原型阶段用环境变量 `IDORIS_API_KEY`，以后迁到 Keychain。

**调用方归因**
- 可选请求头 `X-iDoris-Caller: agent24/<module>`，**只作元数据，永不参与策略**。如果参与策略，就等于允许调用方自报排除项，会变成失败开放。

**角色（§3.3）**
- Agent24 **不在每次调用前预检 `/capabilities`**，因为预检和调用之间有 TOCTOU 窗口。
  - `/capabilities` 只用于展示，以及挂载期检查 `requires_models`。
  - 容量以 iDoris 返回的错误为准。
- `requires_models` 支持写角色名，Agent24 侧随 P4 实现。
- **`fast` 与 `daily` 的区分标准（回答 R1 Q-3）**：
  - `idoris/fast`（常驻 1–4B，**延迟优先**）：输出短或结构化、给内部环节用的调用，例如分类、抽取、摘要、工具选择、会话摘要。
  - `idoris/daily`（7–12B，**质量优先**，默认角色）：直接给人看的生成，例如对话、写作、邮件。
  - `idoris/deep`：`complexity: complex` 的请求。
- **Agent24 的任务 → 角色映射**：
  - `simple` → `daily`。
  - 内核内部的工具性调用（摘要器等）→ `fast`。
  - `complex` → `deep`。
  - Guardian 风险闸 → `decide`；M6 起改走 `/v1/systemone`。
- 长上下文、工具调用、结构化输出这类需求走 `X-iDoris-Capabilities`，**不新增角色**。
- **iDoris 侧待办**：catalog 现有角色 `fast/core/deep/temp` 统一到本规范的枚举。
  - `core` 改为 `daily`。
  - `temp` 不是角色，改用 LoadPolicy 的 `on_demand` 表达。

**请求头范围（§3.4）**
- 原型阶段 Agent24 只发三个头，外加 `model=idoris/<role>`：
  - `Privacy`（每个 provider 固定）；
  - `Complexity`；
  - `Request-Id`。
- `Intent`、`Capabilities` 等到有调用方真正需要时再加（加法兼容）。
- 关联头 `Session/Trace-Id/Request-Id` **由 Agent24 内核生成，不接受模块传入**。
- 近期不做流式响应。

**错误（§3.11）**
- 以下错误在 Agent24 侧是**终止错误**，不重试，也不换 provider：`402`、`403`、`413`、`400 unsupported_field`。
- `503 local_only_unavailable` 在 Agent24 侧如何处理，由 **D-1** 决定。
- `rule_id / evidence / remediation` 不透传给模块，只在管理页展示。

**轨迹（§3.8，修订）**
- **Evolver 的主数据源是 Agent24 自己的事件日志**（D-3）。
  - 需要模型侧元数据时，通过 `record_id`/`trace_id` 关联 iDoris 的记录。
  - **不复制内容**。
- Agent24 **默认不推送** `/v1/trajectories`。只有用户在 iDoris 侧打开 `full` 或训练开关、并完成确认后才推送。
- Agent24 的会话和事件日志属于运行期记忆，**不算**训练副本。
- ATIF 命名空间分开，**双方互不写对方的命名空间**：
  - iDoris 用 `extra.idoris`。
  - Agent24 用 `extra.agent24`（`run_id, module, approval_id, tier, served_by`）。
- `/v1/feedback` 的方向双方接受；Agent24 等有反馈 UI 时再接入。

**管理（§3.9）**
- Desktop（Electron）的 **main 进程直连** iDoris Admin 端口；admin key 只放在 main 进程（D-5）。
- 原型阶段只承诺 M4 状态卡片，其余页面按 Admin API 版本逐期批准（D-7）。
- **敏感确认原型阶段只在 iDoris 控制台完成**（D-4）。
  - Agent24 负责发起、展示 diff、给出深链接。
  - 以后如果要用 Touch ID，由 iDoris 进程调用 LocalAuthentication，不依赖 Agent24 的凭据。

**判定服务（§3.4）**
- 第一个用例是 **Guardian 风险闸**（choice 类问题，天然 local_only）。
- **本地判定器不可用时显式报错，不得静默降级**；`judge.engine` 必须如实回报。

**模型管理归属（回答 Q-9，双方一致）**
- 以下全部归 iDoris：
  - 模型下载、加载卸载、预热、常驻与驱逐；
  - 后端选择、量化；
  - catalog 与推荐（docs/09 R4 正式定为归 iDoris）；
  - 基准测试、升级与回滚；
  - 角色解析、容量 admission、embedding/rerank。
- Agent24 按以下路径退役自己的 oMLX 管理代码：
  - S0：直连 oMLX，维持现状。
  - S1：M4 接入前置完成后接入 iDoris。
  - S2：删除 Desktop 中的 oMLX 管理代码。
- Agent24 保留三项：
  - 任务 → 角色映射；
  - 授权 tier 门；
  - 按模块的用量账本：只计次数和 token；费用只记录 iDoris 回传的 `X-iDoris-Cost-Minor`，自己不算。

## 4. 数据规范清单（定稿后进入 `packages/contracts/schema/`）

| Schema | 状态 |
|:---|:---|
| task-profile / tenant / deploy-mode / provider / component-card / load-policy / routing-policy / adapter-manifest / training-sample | ✅ 已有 |
| `request-headers`、`response-headers`（含 Served-Locality、Record-Id、Degraded） | 新增 |
| `error-body` | 新增 |
| `capabilities`（角色版） | 修订 |
| `feedback` | 新增 |
| `systemone-request` / `systemone-response` | 新增 |
| `inspect-response` | 新增 |
| `virtual-key` | 新增 |
| `trajectory`（ATIF v1.8 + `extra.idoris`） | 新增，并替代 training-sample 的采集职能 |
| `admin/*`（资源、提案、确认） | 新增 |

---

## 5. 请 Agent24 回答的问题

| # | 问题 |
|:---|:---|
| Q-1 | 双逻辑 provider + 每次响应校验 Served-Locality：Agent24 侧的实现排期？ |
| Q-2 | 虚拟 key：按 Agent24 实例一把，还是每个模块一把？模块级 key 能否由内核代管（模块自己不接触）？ |
| Q-3 | 角色枚举 `idoris/fast\|daily\|deep\|vision\|embed\|rerank\|decide\|auto` 是否满足 Agent24 的任务画像？还缺什么角色？ |
| Q-4 | ME4-S2 的 `_a24/model/complete` 何时支持流式？模块经内核调 iDoris 时，内核能否透传 `X-iDoris-Session/Trace-Id`？ |
| Q-5 | `/v1/systemone` 在 Agent24 的第一个用例是什么（审批门的风险判定？Evolver？入口路由？） |
| Q-6 | Memory L3「ATIF 归档」能否对齐 Harbor ATIF v1.8？Evolver 从 iDoris 导出读取，还是 Agent24 自存一份？（建议前者，避免两份内容副本） |
| Q-7 | 完整管理页放在 Agent24 的哪个外壳（Desktop Electron、Pet0、Web）？M4 的状态卡片能否先做？ |
| Q-8 | 敏感操作的用户确认：Agent24 能否提供可验证的用户确认凭据（例如审批门签发的票据），还是统一跳转到 iDoris 控制台确认？ |
| Q-9 | **Agent24 正在评估「用哪个模型、哪个框架做模型管理」——结论是什么？** 本规范默认模型管理全部归 iDoris（docs/17 §2），若 Agent24 的结论与此不同，需要先对齐 |

---

## 6. 协商记录

| 轮次 | 日期 | 发起 | 内容 | 结论 |
|:---|:---|:---|:---|:---|
| R0 | 2026-09-27 | iDoris | 本草案 v0.1 发给 Agent24（agent24-13）review | Agent24 已确认收到，正在对照代码与 ADR-032/ME4-S2/A3 起草 Q-1…Q-9、§3 逐条意见与 ATIF 对齐评估；需 jason 拍板的点将单列；同意把 ADR-032 §9「一行都没合并」标为过期 |
| R1 | 2026-09-27 | Agent24 | 答复全文见 [`negotiation/R1-Agent24-答复.md`](negotiation/R1-Agent24-答复.md)。<br>• Q-9：同意模型管理全部归 iDoris。<br>• 更正两处事实：L3 从未实现；v0.1 漏列 Desktop 的 oMLX 管理。<br>• 分歧一处：Evolver 的数据源。<br>• §3 逐条意见。<br>• 另列 D-1…D-7 请 jason 拍板。 | — |
| R2 | 2026-09-27 | iDoris | **全部采纳** R1 的意见，包括撤回「Evolver 读 iDoris 导出」。<br>• 新增 §3.12。<br>• 回答 fast/daily 的区分标准。<br>• D-1…D-7：iDoris 同意 Agent24 的推荐选项（均为 (a)）。 | **待 jason 拍板 D-1…D-7**；拍板后转 JSON Schema，定稿 |
| R3 | 2026-09-27 | jason | **D-1…D-7 全部选 (a)**：配置 IDORIS_URL 后独占（不留直连 oMLX 的通道）；等 M4 前置完成再接入；Evolver 主数据源是 Agent24 事件日志；敏感确认只在 iDoris 控制台完成；Desktop main 进程直连 Admin 端口；每个实例两把 key；原型阶段只承诺 M4 状态卡片。同时同意 Agent24 那约 20 行文档修改 | 等 Agent24 回复「R2 无异议」后定稿 → 转 JSON Schema |
| R4 | 2026-09-27 | Agent24 | 「已读 §3.12，与我方 R1 一致，**R2 无异议**」。Agent24 开始修订自己的约 20 行文档：L3 改为 ATIF v1.8 交换格式；ADR-032 §9 标为过期；引用本规范。见 [Agent24 PR #540](https://github.com/iDoris-ai/Agent24/pull/540) | **定稿 v1.0** |
| R5 | 2026-09-27 | iDoris | v1.0.1 措辞澄清（T4.1 验收时发现）：Served-Locality 只在已选定后端时才带；它表示推理实际发生的位置；缓存命中带 Cached 和 Origin-Record-Id。都是加法兼容 | **Agent24 无异议**：接入时按 Cached + Origin-Record-Id 去重记账，这条写进它们的 P4 设计 |

# Agent24 对《iDoris × Agent24 分工边界与接口数据规范》v0.1 的答复（R1）

> 发起：Agent24（agent24-13）｜日期：2026-09-27｜对象：iDoris Draft PR #43
> 事实基线：Agent24 `main@f7fe994`（代码与已合并文档）。行号均指该提交。
> 前提：jason 已拍板的各项（两个独立组件 + 公开契约；iDoris 极简控制台 + Admin API，完整管理页归 Agent24；敏感开关最终确认在 iDoris；D3 入口路由归 Agent24；`/v1/systemone` 中 local_only 一律本地判定；轨迹默认 metadata、训练默认关、离机需确认 + 闸一；统一 Harbor ATIF v1.8；iDoris 用 TS；llama.cpp GGUF 进 M4、oMLX 仍是 Mac 默认），本答复不再讨论这些。
> 范围：当前是原型阶段，本答复**不扩大 Agent24 的实现承诺**。凡是会给 Agent24 增加开发量的条目，都标了【估算｜本轮是否建议做】。

---

## 0. 总体意见

1. **方向同意**。Agent24 做事，iDoris 负责推理准入；按权限域划分，双方之间只走版本化的 HTTP 契约。
2. **Q-9 结论与草案默认一致：模型管理全部归 iDoris。** 具体补充三点，见 A-Q9。
3. **有两处事实需要更正**，都属于 Agent24 侧：
   - Memory L3「ATIF 归档」**没有实现**，代码里没有这个符号，只有 PLAN 里的两行规划文字，因此不存在「从 DGM 风格 YAML 迁移」的问题（见 C）。
   - Agent24 今天**自己带有一套 oMLX 管理代码**（Desktop 负责启停和预热，另有 `omlx.sh`）。草案 §2 的复用清单没有列出它。接入 iDoris 后，这套代码应当退役，而不是和 iDoris 并存（见 A-Q9）。
4. **有一处分歧**：Evolver 的主数据源应当是 Agent24 自己的事件日志，不应是 iDoris 导出（见 A-Q6）。
5. Agent24 侧 P4（接入 iDoris provider）的排期**以 iDoris M4 的四项前置为门槛**，本轮不开工（见 A-Q1）。

---

## A. Q-1 … Q-9 逐条答复

### Q-1 双逻辑 provider + 逐响应校验 Served-Locality 的排期

**答：接口形状全部接受。开工以 iDoris M4 的四项前置到位为门槛，前置到齐后约 3–5 个工作日，拆成 2 个 PR。本轮不做。**

- **现状**
  - `ModelRouter::from_env()` 只构造 oMLX 和 Ollama 两个槽（`rust/crates/agent24-models/src/router.rs:258-298`）。`IDORIS_URL` 没有接线（ADR-032「已知缺口」，`docs/decision.md` ADR-032 节）。
  - provider 只会带 `bearer_auth`（`rust/crates/agent24-models/src/lib.rs:270`），不会注入自定义请求头，也不会读取响应头。
  - 请求固定为非流式，`"stream": false`（`lib.rs:552`）。
- **前置（iDoris 侧，全部来自 ADR-032 §9 与总体规划 §7 M4）**
  1. `preview → main` 已合并。
  2. 有 `IDORIS_PORT` 固定端口（FU-13）。Agent24 **不设** `IDORIS_URL` 的默认端口，必须显式配置（`INTEGRATION-AGENTEAR-IDORIS.md:257`）。
  3. `X-iDoris-Served-Locality` 已实现，并且**非流式 JSON 响应也要带上这个头**。Agent24 的 provider 目前是非流式的。
  4. `/health` 返回可识别的服务身份（`INTEGRATION-AGENTEAR-IDORIS.md:152`）。
- **Agent24 的工作量【估算约 500–600 行（含测试），3–5 个工作日｜本轮不做，前置到齐后仍需 jason 放行 P4 门】**
  - **PR-a（`agent24-models`）**
    - provider 支持按实例配置固定请求头。`idoris-local` 固定带 `X-iDoris-Privacy: local_only`，`idoris-any` 固定带 `any`。
    - 响应里读取 `Served-Locality` 和 `Record-Id`，放进 `CompletionResponse`。
    - `Served` 结构体（`router.rs:361` `complete_served`）增加「实际落点」字段。
  - **PR-b（`agentd`）**
    - `IDORIS_URL` / `IDORIS_API_KEY` 注册为两个逻辑 provider。
    - `/v1/models` 按 URL 去重。
    - 加入 `PASSTHROUGH_VARS`（`rust/apps/agent24-cli/src/service.rs:119`）。
    - 黑盒测试用 Python 桩，每条判据都带正对照。
- **校验规则（Agent24 侧，fail-closed）**
  - `idoris-local` 响应的 `Served-Locality` **只接受 `loopback`**：
    - 头缺失：按错误处理。
    - `lan` 或 `remote`：按错误处理，记 error 日志，**结果不返回给调用方**。这和 ME4-S2 的事后绊线同构（`rust/apps/agent24d/src/model_callback.rs:693`）。
    - `lan` 也拒绝，依据是 jason 的 D2 拍板「LAN/Tailscale 算远程」（`INTEGRATION-AGENTEAR-IDORIS.md:183`）。
  - `idoris-any` 按 Served-Locality **记账**：
    - `loopback` 记为 `local`。
    - `lan` 和 `remote` 记为 `remote`。
    - 头缺失时保守记为 `remote`。
    - 按模块的用量表 `served_by` 闭集不变，不需要迁移（ME4-S2 §6.1）。

### Q-2 虚拟 key：按实例还是按模块？模块级 key 能否由内核代管？

**答：原型阶段每个 Agent24 实例用两把 key，与两个逻辑 provider 一一对应。模块永远接触不到 key，这一点已经由现有 wire 结构保证。模块级 key 推迟。**

- **两把 key 的 scope**
  - `idoris-local` 的 key：`allowed_privacy=[local_only]`。
  - `idoris-any` 的 key：`allowed_privacy=[local_only, any]`。
  - 好处是 iDoris 在 key 这一层就能结构性地拒绝「local key 发出 `any`」，比只信任请求头多一道门，符合 G-3「凭据/进程边界即授权边界」的精神。
- **模块不接触 key**
  - `_a24/model/complete` 的参数是 `deny_unknown_fields`，没有 `provider/model/privacy/tier` 字段，`_meta` 永不被读取（`model_callback.rs:68-79`）。模块既不能选 provider，也拿不到任何凭据。
  - 隐私只来自 manifest 的 `model_access`（`model_callback.rs:468`）。内核代管 key 是现有结构的自然结果，不需要新增设计。
- **按模块归因**
  - Agent24 已有按模块的用量账本（`module_model_usage`，`rust/apps/agent24d/src/usage_recorder.rs:161`，`GET /api/v1/usage?module=`，`routes.rs:181`），原型阶段够用。
  - 如果 iDoris 也需要按模块归因，建议加一个**只作元数据、不参与策略**的请求头（例如 `X-iDoris-Caller: agent24/<module>`），由 iDoris 决定是否采纳。本轮不做。
- **Keychain**
  - Agent24 的 Rust 侧目前**没有任何 Keychain 代码**。
  - 原型阶段 key 走环境变量 `IDORIS_API_KEY`（docs/09 R1 已经这样命名），LaunchAgent 的 plist 已经是 owner-only（`service.rs:228`）。
  - 迁到 Keychain 另行排期【估算约 150 行｜本轮不做】。

### Q-3 角色枚举是否满足 Agent24 的任务画像？

**答：满足，而且目前还用不全。不需要新增角色。长上下文、工具调用、结构化输出应当用 `X-iDoris-Capabilities` 表达，不要做成角色。**

- **Agent24 今天的任务画像只有两个维度**：`TaskProfile{privacy: Any|LocalOnly, complexity: Simple|Complex}`（`router.rs:75-91`）。
- **现有调用方的建议映射**

| Agent24 调用方 | 现在的画像 | 建议的角色 |
|---|---|---|
| agent loop / `/api/v1/chat`（`routes.rs:319` 用 `TaskProfile::default()`） | Any + Simple | `idoris/daily` |
| 模块回调 `complexity: complex` | 由 manifest 决定 + Complex | `idoris/deep` |
| Guardian 风险闸（`rust/crates/agent24-policy/src/guardian.rs:232`） | LocalOnly + Simple，只输出 `low/high` | `idoris/decide`（M6 起改走 `/v1/systemone`，见 Q-5） |
| 会话摘要器（`rust/crates/agent24-agent/src/lib.rs:140-144`） | 与 chat 同画像 | `idoris/fast` |
| embedding（记忆检索） | 目前没有消费者（ML Worker 契约没有调用方） | `idoris/embed`（以后） |

- `vision`、`rerank`、`auto`：Agent24 目前没有消费者，保留即可。
- **请 iDoris 澄清两点**
  1. `fast` 和 `daily` 的区分标准：是按延迟，还是按质量或成本？Agent24 只有 simple/complex 两档，需要一条明确规则才能稳定映射。
  2. 总体规划 §1.3 的 catalog 角色是 `fast/core/deep/temp`，和本草案的枚举不一致，需要 iDoris 在自己一侧先统一。
- **Agent24 这边会连带改动一处**：manifest 的 `requires_models` 目前按**具体模型名**在挂载时做存在性检查（`rust/apps/agent24d/src/domain.rs:449`、`:1278`）。接入 iDoris 后需要允许写角色名，否则一换模型模块就挂载失败【估算约 50–80 行｜随 P4 做，本轮不做】。

### Q-4 `_a24/model/complete` 何时支持流式？内核能否透传 Session/Trace-Id？

**答：流式不在近期计划内。关联头可以由内核生成并注入，但不接受模块传入。**

- **流式**
  - ME4-S2 明确不做流式（`docs/design/ME4-S2-model-callback.md` §0）。ADR-032 把流式排在 v0.5.0 之后（P3）。A3 附着设计也把流式列为后续（`docs/design/A3-ATTACHED-MODULE.md:26`）。jason 已拍板 P2 可以先上非流式（ADR-032 D1）。
  - 流式需要单独设计，内容涉及背压、取消和计量（SPEC-ME3 §10-2）【估算 1000 行以上、1–2 周以上｜本轮不做】。
  - 另外，Agent24 → iDoris 这一跳目前也是非流式（`lib.rs:552`），所以流式要两段都补齐才有意义。
- **关联头**
  - 可以，但由**内核自己生成**：
    - `X-iDoris-Session` 取模块名 + 附着代次，或者 run id。
    - `X-iDoris-Trace-Id` 取内核的 run id 或回调 `request_id`（`model_callback.rs:77`）。
    - `X-iDoris-Request-Id` 每次调用唯一。
  - 模块**不能**自带这些值：wire 是 `deny_unknown_fields`，`_meta` 永不被读取。这是有意的设计，防止模块伪造关联关系。
  - 如果将来确实需要让模块传入父级关联，只能以「加字段」的方式做，而且内核要校验这个值属于该模块。
  - 【估算约 50 行，并入 Q-1 的 PR-a｜随 P4 做】

### Q-5 `/v1/systemone` 在 Agent24 的第一个用例

**答：Guardian 风险闸（D3 自动审批前置）。**

- Guardian 今天用一个 LocalOnly 小模型给工具调用打 `low|high` 分，只有明确的 `low` 才自动批准，其余全部升级给人（fail-closed，`guardian.rs:1-17`）。
- 它正好是一个 `choice` 问题，天然是 `local_only`，而且 `/v1/systemone` 响应里的 `confidence` 可以直接用作「低于阈值就升级」的门槛。
- 接入点现成：`RiskAssessor` trait 已经是一个抽象缝，新增一个 `SystemOneAssessor` 实现即可，不改审批流程。
- **Agent24 的要求**
  1. local_only 请求在本地判定器不可用时返回错误，**不得静默降级**。Agent24 侧会按 fail-closed 处理，也就是升级给人。
  2. 响应里的 `judge.engine` 必须真实回报，Agent24 会记进审计。
- **排期**：iDoris M6 之后。【估算约 150–200 行（含测试）｜本轮不做】
- 入口路由和 Evolver 排在后面：Evolver 尚未实现；入口路由目前只有 simple/complex 两档，还没有需要判定的场景。

### Q-6 Memory L3 能否对齐 ATIF v1.8？Evolver 从哪里读？

**答：能对齐，而且零迁移成本，因为 L3 还没有实现。但 Evolver 的主数据源不同意设为 iDoris 导出。** 格式细节见 C。

- **对齐**：Agent24 把 L3 的规划改为「交换/导出格式 = Harbor ATIF v1.8」。这是一处文档改动【约 10 行｜本轮可做，做成独立的 docs PR】。
- **Evolver 的数据源：分歧**
  1. **iDoris 看不到完整的工具链。** 它只看到推理调用，工具执行结果、审批决定、回执都在 Agent24。总体规划 §5.3 自己也写了「网关看不到工具执行结果」。
  2. **iDoris 默认只记 metadata（P-1）。** 默认配置下 iDoris 导出里**没有内容**，Evolver 从它那里读不到可以蒸馏的东西。
  3. **Agent24 本来就有权威事件日志。** 它属于 M-D 运行期记忆，不是为训练额外留的副本（`docs/architecture/kernel-boundary.html:401`：「trajectory 轨迹 · 内核 memory trace · 已建未接」）。
- **建议**
  - Evolver 以 Agent24 的事件日志为主数据源，Evolver 需要时现场投影成 ATIF。
  - 需要模型侧的元数据（路由决策、实际后端）时，通过 `record_id` 和 `trace_id` 与 iDoris 的记录关联，不复制内容。
  - 「避免两份内容副本」这个目标保留，但换一种落实方式：
    - Agent24 **不**默认向 `/v1/trajectories` 推送完整 ATIF。只有用户在 iDoris 侧打开 `full` 或训练开关、并完成确认后才推送。
    - Evolver 做的是不改权重的进化（SKILL.md），完全在本机进行，沿用 ADR-017「默认全本地」（`docs/decision.md:578`）。
- 这是一项需要 jason 拍板的事（D-3）。

### Q-7 完整管理页放在哪个外壳？M4 状态卡片能否先做？

**答：放在 Desktop（Electron，`apps/desktop`）。状态卡片能做，工作量很小，但应当等 iDoris Admin API v0 可用以后，和 Models 页的迁移一起做。本轮不做。**

- **为什么选 Desktop**：Desktop 已经有 Models 页和 oMLX 的 IPC（`apps/desktop/src/renderer/pages/Models.tsx`、`apps/desktop/src/main/ipc/index.ts:27-190`），这正是要改接 iDoris 的地方。Pet0 是桌宠，不适合做管理；Agent24 目前没有 Web 外壳。
- **调用路径建议**：Desktop 的 **main 进程直连** iDoris 的 Admin 端口（只绑 loopback），admin key 只放在 main 进程，renderer 拿不到；**不经过 agent24d 转发**，避免内核变成 Admin 代理。这一条需要拍板（D-5）。
- **状态卡片（M4）**：读 `status / backends / models`，只读，不包含任何写操作。【估算约 150–250 行 TS（含测试）｜等 iDoris Admin API v0 + 固定端口到位后做，本轮不做】
- **范围声明**：§3.9 列出的完整管理页（路由回放、隐私规则、预算审批、轨迹授权、学习晋升、租户与 key）是很大的前端承诺。Agent24 **接受这个方向上的归属**，但每一期都按 iDoris Admin API 的版本（v1/v2）分别由 jason 批准，**现在不承诺排期**（D-7）。

### Q-8 敏感操作的用户确认：Agent24 能否提供可验证的确认凭据？

**答：目前不能。原型阶段统一跳转到 iDoris 控制台确认。**

- Agent24 的审批决定只靠**一把 daemon 级 bearer token** 鉴权（`rust/apps/agent24d/src/approvals.rs:69` `decide_approval`；鉴权层在 `rust/apps/agent24d/src/server.rs:675-681`）。持有这把 token 的任何本机进程都能「批准」。Agent24 没有按用户签名的能力，也没有 Touch ID 或 WebAuthn。
- 如果由 Agent24 签发「确认票据」，iDoris 验证到的只是「某个持有 Agent24 token 的进程说用户同意了」。这会让「最终确认留在 iDoris」（G-7）实际上失效。
- **建议**
  - Agent24 管理页可以**发起** proposal，展示 diff 和状态，并给出跳转到 iDoris 控制台的深链接。
  - 确认动作只在 iDoris 自己的进程里完成。以后如果要做更顺滑的体验，也应当由 **iDoris 进程**调用 macOS LocalAuthentication（Touch ID），而不是依赖 Agent24 的凭据。
  - Agent24 侧工作量：仅限展示和跳转，并入状态卡片或管理页，无额外开发。

### Q-9 用哪个模型、用哪个框架做模型管理：Agent24 的评估结论

**结论：同意草案默认「模型管理全部归 iDoris」（docs/17 §2）。Agent24 不再发展任何模型管理能力，Desktop 现有的 oMLX 管理代码按下文路径退役。Agent24 对「用哪个核心模型」不持立场，交给 iDoris 决定（包括 catalog、推荐以及 docs/13 的 Qwen3.5-9B 结论）。**

**1. Agent24 现在自己做了什么（事实清单）**

| 能力 | 位置 | 性质 |
|---|---|---|
| `ModelRouter`：按 privacy 过滤 tier，按 complexity 排序，provider 健康冷却 | `router.rs:27-91`、`:319-420` | 授权 + 路由 |
| provider 槽：oMLX `127.0.0.1:8088` + Ollama `:11434`，默认模型 `DEFAULT_MODEL=Qwen3-8B-4bit` | `router.rs:258-298` | 选模型（**应迁走**） |
| `Tier` 判定：回环 + 不走代理 + 不跟随重定向才算 `Local`，判不准一律 `Remote` | `router.rs:163`、`lib.rs:243` | 授权 |
| 事后绊线：LocalOnly 却由非本地层服务时，不返回结果 | `model_callback.rs:693` | 授权 |
| 按模块用量账本（次数/token，`cost_usd` 恒为 `null`） | `usage_recorder.rs:161`、`routes.rs:181` | 归因 |
| manifest `model_access`（缺省 `local_only`）与 `requires_models`（具体模型名） | `model_callback.rs:468`、`domain.rs:449` | 授权 / 挂载检查 |
| **Desktop 的 oMLX 管理**：探测、列模型、`omlx serve` 启动、`pkill` 停止、发一条 chat 预热加载；Models 页里写死的模型目录 | `apps/desktop/src/main/ipc/index.ts:27-190`、`Models.tsx:3-18` | **模型管理（应迁走）** |
| `omlx.sh`（端口 8088 的启停脚本） | 仓库根目录 `omlx.sh` | **模型管理（应迁走）** |

**2. 交给 iDoris 的部分（Agent24 不再做）**：
- 模型下载，加载、卸载、预热、常驻与驱逐。
- 后端选择（oMLX / mlx_lm / llama.cpp）与后端进程的启停。
- 量化与工件选择。
- catalog 与硬件感知推荐：docs/09 R4 写的是「归属由 Agent24 定」，**Agent24 在此正式定为归 iDoris**。
- 基准测试，升级与回滚。
- 角色到模型的解析，容量与 admission。
- embedding 和 rerank（沿用 D4）。

**3. Agent24 保留的部分，以及为什么这不算「第二个策略源」**：
- **任务 → 角色的映射**（D3）：保留，映射表见 Q-3。
- **授权 tier 判定**：保留。
  - 包括 manifest 到 Privacy、Privacy 到哪个逻辑 provider、router 的 LocalOnly 硬门，以及 Served-Locality 绊线。
  - 这些属于 G-1 里「Agent24 管动作授权」的一侧，是和 iDoris fail-closed **同向叠加**的第二道门（ADR-032 §9「两道门同向叠加」）。它们**不涉及**选模型、预算或隐私内容过滤，因此不违反 G-2。
- **按模块用量账本**：保留。
  - 它回答的问题是「Agent24 里哪个模块用了多少」，只计次数和 token，**不计费用**（ME4-S2 §6.4），也**不是**预算权威。
  - iDoris 的账本按 tenant/key 记，是费用、预算和 402 的唯一权威。两本账的口径通过 Served-Locality 对齐：`idoris-any` 的 `served_by` 取实际落点，不取 tier（Q-1）。
  - 如果以后要显示按模块的费用，只记录 iDoris 回传的 `X-iDoris-Cost-Minor`，Agent24 不自带价目表。
- **直连 OpenAI 兼容 provider 的能力**：保留。没配置 `IDORIS_URL` 的用户行为零回归（docs/09 R1）。

**4. 迁移路径**

| 阶段 | 触发条件 | Agent24 的动作 | 工作量 |
|---|---|---|---|
| S0 现状（原型期） | — | 继续直连 oMLX，不动 | 0 |
| S1 接入 | iDoris M4 的四项前置到位（Q-1），且 jason 放行 P4 | Q-1 的 PR-a、PR-b；请求的 `model` 字段改为发角色名；配置了 `IDORIS_URL` 时，**直连 oMLX/Ollama 槽的处理方式由 D-1 决定**；Desktop 在检测到 iDoris 时**隐藏** oMLX 启停和预热按钮，Models 页改读 iDoris `/capabilities` | Rust 约 500–600 行 + Desktop 约 200 行 |
| S2 收口 | iDoris M5 Admin API v1 | `requires_models` 支持角色名；Desktop 删除 oMLX 管理代码，`omlx.sh` 从默认路径移除（保留给无 iDoris 的开发者） | 约 150 行，净删除为主 |

**关键风险（D-1 的理由）**：iDoris 管理的 oMLX 和 Agent24 直连的是**同一个 8088 实例**（D7）。如果配置了 iDoris 之后 Agent24 仍然直连 oMLX 并带上具体模型名，oMLX 会在 iDoris 不知情的情况下加载或驱逐模型。这会绕过 iDoris 的全局内存账本（总体规划 §4.3）和审计，Desktop 的「预热」按钮也属于同样的问题。

---

## B. 对 §3 各层「负责 / 不负责」的意见（只列有异议或需补充的条目）

| 条目 | 意见 | 理由 |
|---|---|---|
| §2 复用清单「Agent24 PLAN §3.3–3.4：当前是 DGM 风格 YAML，需修订」 | **修改** | L3 没有实现（`README.md:14`：「L3 ATIF 轨迹、SkillBank、自进化框架尚未实现（代码中无此符号）」），不存在「当前格式」。改为「规划文字，改成以 ATIF v1.8 为交换格式」（见 C） |
| §2「ADR-032 中 iDoris 未合并等描述已过期」 | **采纳** | Agent24 会在 ADR-032 §9 补一段更新说明（纯文档，约 10 行，可以和 L3 的文档修订放在同一个 PR） |
| §2 复用清单 | **补充** | 请把「Agent24 Desktop 的 oMLX 管理（`apps/desktop/src/main/ipc/index.ts:27-190`）+ `omlx.sh`」列为**待退役**条目（见 Q-9） |
| §3.1 Agent24 负责「把 iDoris 接成两个逻辑 provider」 | **采纳 + 补充** | ① `IDORIS_URL` 没有默认端口，必须显式配置；② Agent24 这一跳目前是**非流式**，请 iDoris 保证非流式 JSON 响应同样带 Served-Locality 和 Record-Id；③ 启动时校验 `/health` 的服务身份 |
| §3.2 Agent24「在 Keychain 保存 key」 | **修改** | 原型阶段用环境变量 `IDORIS_API_KEY`（plist owner-only），Keychain 以后再做；key 粒度见 Q-2（每个实例两把） |
| §3.2 Agent24「把用户/组织身份映射成 tenant」 | **修改（推迟）** | personal 模式不发 `X-iDoris-Tenant`；tenant 映射随 B7/M8 一起设计，本轮不承诺 |
| §3.3 Agent24「调度前查 `/capabilities`」 | **修改** | Agent24 **不在每次调用前预检**。预检和实际调用之间存在 TOCTOU 窗口，而且重复了 iDoris 的 admission；docs/17 §4 第 3 条也写了预检不能替代门禁。Agent24 只把 `/capabilities` 用于展示和挂载期的 `requires_models` 检查，容量不足时以 iDoris 返回的错误为准 |
| §3.3 Agent24「只按角色请求」 | **采纳 + 补充** | 连带改动：manifest `requires_models` 要支持角色名（Q-3） |
| §3.4 请求头 `Intent / Capabilities / Fallback` | **修改（范围）** | Agent24 目前只算得出 privacy 和 complexity。原型阶段只发 `Privacy`（每个 provider 固定）、`Complexity`、`Request-Id`，加上 `model=idoris/<role>`；`Intent` 和 `Capabilities` 等到有调用方真正需要时再加（加法兼容） |
| §3.4 Agent24「不在 Agent24 内新增推理路由策略」 | **采纳 + 澄清** | Agent24 保留的 tier 授权门、LocalOnly 硬门和 complexity 决定的 tier 顺序，都属于动作授权，**不算**推理路由策略（Q-9 第 3 点）。请在 G-2 的正文里写明这一点，避免以后误读 |
| §3.5 Agent24「逐响应校验 Served-Locality」 | **采纳 + 补充** | 写明 fail-closed 细则：`idoris-local` 只接受 `loopback`；头缺失、`lan`、`remote` 都按错误处理，结果不返回；`idoris-any` 头缺失时记账为 `remote`（Q-1） |
| §3.6 Agent24「402 不重试也不换 provider」 | **采纳 + 补充** | 现有代码已经满足：非 429 的 4xx 在 provider 层归为 `Provider`，是终止错误，不会切到下一个 provider（ME4-S2 §1 第 2 条）。另外请注意：`503 local_only_unavailable` 会被 Agent24 归为 `Unavailable`，从而让同层的下一个 Local provider 接手，这正是 D-1 要回答的问题 |
| §3.6 Agent24「在管理页展示和审批预算」 | **修改（排期）** | 属于 M5 管理页的范围，按 D-7 逐期批准，本轮不承诺 |
| §3.7 Agent24「把 record id 记进运行记录」 | **采纳（推迟）** | 随 P4 的 PR-a 读取 `Record-Id`；写进 run 记录约 30 行，随 P4 做 |
| §3.8 Agent24「把 👍/👎 和用户修正回填到 `/v1/feedback`」 | **修改（推迟）** | Agent24 目前**没有反馈 UI**（Desktop Chat 页没有 👍/👎）。方向接受，等有 UI 时再做，本轮不承诺 |
| §3.8 Agent24「Evolver 读取 iDoris 导出的 ATIF」 | **驳回（改为关联读取）** | 理由见 Q-6：主数据源是 Agent24 事件日志，通过 `record_id`/`trace_id` 与 iDoris 关联 |
| §3.8 Agent24「不保存推理内容副本用于训练」 | **采纳 + 澄清** | Agent24 的会话和事件日志属于运行期记忆（iDoris 在表中「不负责」里也写了 MemPalace），**不算**「训练副本」；Agent24 不为训练额外留存内容 |
| §3.8 ATIF 治理字段放在 `extra.idoris` | **补充** | Agent24 生成的 ATIF 用并列的命名空间 `extra.agent24`（run_id、module、approval_id、tier、served_by），双方互不写对方的命名空间 |
| §3.9 Agent24「完整管理页」 | **采纳方向，修改承诺** | 原型阶段只承诺 M4 状态卡片（Q-7），其余按 Admin API 版本逐期批准（D-7）；Desktop main 进程直连 Admin 端口（D-5） |
| §3.9 确认可以由 Agent24 转交用户确认凭据 | **修改** | 原型阶段只支持在 iDoris 控制台确认（Q-8） |
| §3.11 统一错误体 | **采纳 + 补充** | Agent24 把 `error.type` 映射到内核的错误闭集（ME4-S2 §7）；`rule_id / evidence / remediation` **不透传给模块**（ME4-S2 规定 provider 原文不出内核），只在管理页展示 |

---

## C. ATIF 对齐评估

**1. Agent24 的「现状格式」其实不存在。**
- `docs/PLAN.md:192` 只写了「L3 | ATIF 轨迹归档（DGM-style）| YAML 多文档」，`:202` 写了「ATIF Archive (results.log)」。
- 没有 schema，没有实现（`README.md:14`：「代码中无此符号」）。

**2. 两者的差异。** PLAN 原意是借鉴 DGM 的做法，这是按描述推断的：

| 维度 | PLAN 里的 L3（规划） | Harbor ATIF v1.8 |
|---|---|---|
| 粒度 | DGM 式 archive：一条记录对应一次任务或一个 agent 变体的结果与评分，用于谱系和择优 | 步骤级：`steps[]` 逐步记录 user/agent 消息、`tool_calls`、工具结果（observation）、metrics |
| 编码 | YAML 多文档，追加写入 `results.log` | JSON，带 `schema_version: "ATIF-v1.8"` |
| 关联 | 未定义 | `session_id` / `trajectory_id`，外加 `extra` 命名空间 |
| 校验 | 无 | 有 Harbor validator（总体规划 M6 的验收就用它） |

- 字段细节以 Harbor RFC 0001 原文为准。本表只用到总体规划 §5.4 已经列出的字段，以及 observation 这一概念。
- DGM 式 archive 和 ATIF **不是同一层的东西**：前者适合记录「Evolver 产出的 SKILL 版本谱系与评分」，后者是轨迹本身。

**3. 对齐的代价**
- 现在：**约等于 0**。只需要把 PLAN 的 L3 一行改为「交换格式 = ATIF v1.8；存储 = M-D 事件日志（权威）；ATIF 是按需导出的投影」【约 10 行文档｜本轮可做】。
- 将来真正实现时：做一个「M-D 事件日志 → ATIF v1.8」的导出器，在 CI 里接入 Harbor validator，Agent24 自己的字段放在 `extra.agent24`【估算 300–500 行｜本轮不做，等 Evolver 立项或用户开启推送时再做】。

**4. 建议（控制范围）**
- 本轮**只约定导出和交换格式**，不定义 Agent24 的存储格式，也不重写存储。
- 「是否推送到 `/v1/trajectories`」由 iDoris 侧的开关和同意记录决定，Agent24 默认不推送。
- 如果以后 Evolver 需要谱系 archive，把它作为 **SKILL 版本元数据**另行设计，和轨迹格式分开，不再挂 ATIF 的名字。

---

## D. 需要 jason 拍板的点（本答复不替他定）

| # | 问题 | 选项 | 推荐 |
|---|---|---|---|
| **D-1** | 配置 `IDORIS_URL` 之后，Agent24 是否还保留直连 oMLX/Ollama 的槽？ | (a) **排他**：配了 iDoris 就不注册直连槽，iDoris 挂掉时 local_only 请求返回不可用；(b) **兜底**：直连槽作为 Local 层，排在 idoris-local 之后，iDoris 不在线时直连；(c) 默认 (a)，另设一个显式环境变量开启 (b) | **(a)**。直连和 iDoris 用的是同一个 8088 oMLX，直连会绕过 iDoris 的全局内存账本和审计，而且会在 iDoris 不知情时加载或驱逐模型。原型期可用性要求不高，(a) 最简单，额外开发量为 0。(c) 需要增加约 30 行和一个开关，等真出现离线需求时再加 |
| **D-2** | Agent24 P4（接入 iDoris provider）何时开工？ | (a) 等 iDoris M4 四项前置（main 合并、`IDORIS_PORT`、Served-Locality 含非流式、`/health` 身份）全部到位再开工；(b) 现在就照契约用桩先写 | **(a)**。ADR-032 §9 已经约定「iDoris 已可依赖不写进时间表」；照着桩先写，容易在契约还会变动时白做 |
| **D-3** | Evolver 的主数据源 | (a) Agent24 事件日志为主，通过 `record_id`/`trace_id` 与 iDoris 关联；(b) iDoris 导出的 ATIF 为主（草案原提议） | **(a)**。iDoris 看不到工具链，而且默认只记元数据，(b) 在默认配置下读不到内容 |
| **D-4** | 敏感操作的用户确认 | (a) 原型阶段只在 iDoris 控制台确认，Agent24 负责发起和跳转；(b) Agent24 签发确认票据 | **(a)**。Agent24 目前只有 daemon 级 bearer token，没有按用户签名的能力，(b) 会让「最终确认留在 iDoris」失去意义 |
| **D-5** | Agent24 管理页调用 iDoris Admin API 的路径 | (a) Desktop main 进程直连 Admin 端口，key 只在 main 进程；(b) 经 agent24d 转发 | **(a)**。不让内核变成 Admin 代理，攻击面更小，agent24d 零改动 |
| **D-6** | 虚拟 key 的粒度 | (a) 每个实例两把，对应 idoris-local 和 idoris-any，scope 分别限定 privacy；(b) 每个实例一把；(c) 每个模块一把，由内核代管 | **(a)**。key 本身就带隐私边界，改动很小；(c) 等 iDoris 有调用方身份（FU-14）并且确实需要按模块做策略时再议 |
| **D-7** | 完整管理页的承诺范围 | (a) 原型阶段只做 M4 状态卡片，M5/M6 的页面按 Admin API 版本逐期再批；(b) 现在就按 §6.1 的 M4–M8 全表承诺 | **(a)**。符合「原型阶段不扩大实现承诺」 |

---

## 附：本答复涉及的 Agent24 开发量汇总（都不在本轮做，文档修订除外）

| 项 | 估算 | 本轮 |
|---|---|---|
| L3 规划改为 ATIF 交换格式 + ADR-032 §9 更新（纯文档） | 约 20 行 | **可做**（独立的 docs PR，需要 jason 同意） |
| P4 PR-a + PR-b（双逻辑 provider、请求头、Served-Locality、去重、关联头） | 500–600 行，3–5 天 | 否（D-2） |
| `requires_models` 支持角色名 | 50–80 行 | 否（随 P4） |
| Desktop：检测到 iDoris 时隐藏 oMLX 管理 + 状态卡片 | 150–250 行 TS | 否（等 Admin API v0） |
| Guardian 改用 `/v1/systemone` | 150–200 行 | 否（等 iDoris M6） |
| Keychain 保存 key | 约 150 行 | 否 |
| ATIF 导出器 + validator | 300–500 行 | 否 |
| `_a24/model` 流式 | 1000 行以上 | 否（v0.5.0 之后） |

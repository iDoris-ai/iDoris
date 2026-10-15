# iDoris × Agent24 分工边界与接口数据规范

> **文档修订 1.2，2026-10-15**：将 R0–R8 已接受条款合并到正文，消除“后补章节覆盖前文”的阅读方式；新增的 v0.3.1 范围及 R9 三项决策仍是 **OWNER REVIEW PENDING**，不借文档修订授权实现。
> **唯一跨仓规范权威**：本文件。iDoris 主持，Agent24 作为消费者审阅；Agent24 应保留短消费方指针、实现证据及反向硬件权威链接，不复制第二份契约。总体规划的项目里程碑仍见 [总体规划](../iDoris-总体规划.md)。

## 0. 版本、状态与证据边界

**两个独立组件，一份公开 HTTP 契约，无 crate/源码级耦合。** Agent24 管任务、动作授权、工具和运行期记忆；iDoris 管模型调用准入、推理策略、模型生命周期及其审计。规范条款不等于已实现功能；以下固定基线决定“实际”含义。

| 标识 | 含义与基线 |
|---|---|
| 文档修订 `1.2` | 本次文字整合；不是服务 wire version，不触发代码或 schema 升级 |
| iDoris release `v0.3.0` | 实现提交 `03c079c70611331a8c85a1666e32a75df6359d09`（下称 I0） |
| runtime `contract_version=1.0.1` | I0 的 `crates/idoris-contracts/src/version.rs::CONTRACT_VERSION`；TS `packages/contracts/src/version.ts` 同值 |
| Agent24 审计基线 | `6072fd373142eb3dbf1a6a4a3222e6eec82c74f6`（下称 A0，固定历史快照，非当前 main）；main 源码状态不等于 stable 已发给用户 |
| v0.3.1 | 拟议集成目标，尚未发布/验收；本规范不选择未来 wire version 或猜最低 Agent24 release |

除明确标为“提案/目标”的部分外，正文表达 R0–R8 的既有边界；§4 标明其实现差距。已有 JSON Schema 对它覆盖的 wire 结构是机器真源；未覆盖条款为文字规范，不能声称 schema 已齐全。发生实现差距时须如实失败或显示不可用，不能把目标字段当作当前 API。

**反向权威**：在同机组合中，主机事实与整机预算分配归 Agent24；公式和硬件能力计划只引用 [HW-TIERS §3.3/3.4（固定 961a6408）](https://github.com/iDoris-ai/Agent24/blob/961a64085576eec365d0442ed27d2a7422168200/docs/design/HW-TIERS-capability-hardware-matrix.md) 与 [ADR-036 提案（同一固定提交）](https://github.com/iDoris-ai/Agent24/blob/961a64085576eec365d0442ed27d2a7422168200/docs/decision.md)。A0 审计后，[#964](https://github.com/iDoris-ai/Agent24/pull/964) 已合并为 `622be5d7df0387b91028820624a778cb99cd6d43`，[#966](https://github.com/iDoris-ai/Agent24/pull/966) 已合并为 `cb11d7bc345ce8dfb5c438e4d6cf0d3663e76724`（2026-10-15 状态回读）；这不改变固定 A0，也不证明已发布给用户。HW-1 来源固定为 `f857ecfa4493b181d17a1d8e9323efde36ffe425`；其 `HardwareReport` 仅有 `profile/level/usable_bytes/reasons`，没有 `llm_allocated_bytes`。不直接依赖 agent24-hw，不照抄提案中已过时的“推荐器尚无分配额入参”。

动作授权与 Desktop 安全实现由 Agent24 的 [ADR-032/035（固定 A0）](https://github.com/iDoris-ai/Agent24/blob/6072fd373142eb3dbf1a6a4a3222e6eec82c74f6/docs/decision.md) 负责；跨仓边界以本文整合条款为准。

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
| G-8 | **按请求阶段划分职责**（v1.1，见 §1.1）。一句话：Agent24 决定「做什么、用什么工具、能不能做」；iDoris 决定「这一次模型调用用哪个模型、在哪里跑、花不花钱、记不记」。**本阶段视图不新增、也不改变 §3 的职责划分；如有冲突，以 §2–§4 为准** | R6/R7、J-1 |

### 1.1 请求阶段与负责方（G-8）

| 阶段 | 内容 | 负责方 |
|:---|:---|:---|
| S0 接入交互 | 渠道、会话 | Agent24 |
| S1 任务理解与规划 | 用户要做什么、拆成哪些步骤、选用哪些工具/技能/模块（**任务级意图**） | Agent24 |
| S2 动作授权 | tier 门、Guardian 风险闸、人工审批；可以调用 iDoris 的 `/v1/systemone` 取判定原语。**组合风险检测**（Lethal Trifecta 一类）归 Agent24，目前尚未实现，不写时间承诺（J-3） | Agent24 |
| S3 工具执行 | 非 LLM 的 HTTP、本地文件、CLI；第三方凭证经 vault 部署 (b) 注入（§2.2） | Agent24 |
| S4 发起模型调用 | 把任务映射为 role，设置 Privacy、Complexity 等请求头，产出请求并交给 iDoris | Agent24 |
| S5 模型调用内部 | **按 Agent24 声明的角色与能力需求选模型与路径**：隐私闸 → 角色/能力匹配 → 预算（只对付费候选做原子 reserve）→ 容量（加载/驱逐）→ 执行（本地运行时，或远程 + 凭证注入）→ 回传 Served-Locality 与 Record-Id。角色内可以降级；**跨角色**降级必须由 Agent24 用 `X-iDoris-Fallback` 声明，并在响应中如实回传（J-2） | iDoris |
| S6 结果返回 | 检查 Served-Locality 绊线，继续执行计划，写入记忆 | Agent24 |
| S7 记录与学习 | Agent24：事件日志 → Evolver 学技能。iDoris：审计元数据与可选轨迹 → 学路由与判定。两边以 `record_id` 关联 | 各自负责 |

**三个职责示例（含后续里程碑设计，不代表 v0.3.0 已全部实现）**
1. **总结今天的微信群聊**（敏感、只能在本机处理）。Agent24 先用本地工具取消息，再以 `idoris/daily` + `local_only` 发起调用。iDoris 过隐私闸后选本地 daily 模型，必要时驱逐空闲模型再加载；本地免费调用不涉及付费预算（仍受容量准入约束），响应回传 `loopback`。如果本地模型加载不起来，返回 **503 fail-closed**，**不会改走云端**。
2. **查资料写竞品分析报告**（公开数据，复杂任务）。Agent24 执行搜索和抓取工具，凭证由 vault (b) 注入。之后以 `idoris/deep` + `any` 发起调用。iDoris 选远程 deep 模型，对预算做原子 reserve（额度不足时仅在调用方显式声明 `X-iDoris-Fallback: next_in_chain` 后允许回落到免费候选，并如实回传降级；未声明或无可行候选则返回 402。I0 router 尚未接入此声明的预算回落，实际仍返回 402，见 §4），注入 provider 凭证（vault (a)），结算后回传 `X-iDoris-Cost-Minor`，Agent24 记入按模块的用量账本。
3. **转账或删除文件**（高风险动作）。Agent24 的 Guardian 调用 `/v1/systemone`（`local_only`），iDoris 用本地判定器返回概率，并如实回报 `judge.engine`。判定器不可用时**拒绝**（`judge_unavailable`），Agent24 转人工确认。资产类签名只能走 AirAccount + passkey（J-7）。双方以 `record_id` 与 `approval_id` 关联记录。

---

## 2. 合并后的边界契约

### 2.1 接入、生命周期与四类凭证

- Agent24 不拥有、启动或托管 iDoris daemon；iDoris daemon 不启动 Agent24。用户或可信外部 launcher 负责启动。launcher 宿主尚未决定（R9），不自动变成 Desktop 的 sidecar。
- `IDORIS_URL` 必须显式配置，无隐含端口。D-1(a)：配置后独占 iDoris，不留直连 oMLX/Ollama 回退；缺 key、服务不可用或身份不符都明确失败。
- 已接受完整模型是两个逻辑 provider：`idoris-local`=Local，固定 `local_only`；`idoris-any`=Remote，固定 `any`。按 URL 去重 models/容量，不能相加。D-6 两把 key 分别限定 `[local_only]` 与 `[local_only,any]`。§3 提议仅交付 local 子集，第二 provider/key 延后，不撤销 D-6。
- 模块 manifest 未声明或声明 `local_only` 时使用 `idoris-local`；只有 `remote_allowed` 才有资格进入 `idoris-any`，仍不能扩大调用方已授予的权限。
- 数据面默认必须有 virtual bearer，即使 loopback。`IDORIS_DEV_NO_KEY=1` 仅为显式 loopback 开发选项，无 key 只能走免费 loopback/local_only；错误 bearer 不能借此绕过。非 loopback 必须有有效 key。tenant 模式沿用 `X-iDoris-Tenant`；个人模式不伪造 tenant；tenant 模式的数据 key 必须绑定该 tenant_id。

| 凭证 | 权威/使用者 | 边界 |
|---|---|---|
| 数据面 virtual key `idk_…` | iDoris 签发；Agent24 内核持有 | Bearer 用于数据面；模块不接触；目标持久存储为 Keychain，不与 Admin 混用 |
| Admin session token | 每次启动的可信 launcher → iDoris、Electron main | 只经两条一次性匿名 pipe/FD（Windows 白名单 HANDLE）；不进入 argv/env/普通文件/stdout/stderr/log/renderer |
| oMLX 上游 key | iDoris → 后端 | 当前配置 `IDORIS_OMLX_API_KEY`；不是 Agent24 的 `OMLX_API_KEY`，不当 virtual key |
| Agent24 daemon token | Agent24 daemon ↔ 自身客户端 | 不属于 iDoris，不送进第三方 vault |

I0 `idoris key issue --spec-stdin` 的实际 spec 为 `owner, tenant_id?, allowed_privacy, allowed_roles, expires_at_ms?, admin_scopes`；`admin_scopes` 只能为空，未知字段拒绝。`budget_ref/expires_at` 等旧 scope 草案不是当前签发字段。scope 只能拒绝，不能放宽请求。完整角色权限才允许具体模型 ID 及 next_in_chain，最小 daily key 不能靠具体 ID 绕过限制；见 §4 安全候选的额外修复状态。

Admin v0 为独立 loopback listener，只有 `status/backends/models/runtimes/roles` 五个只读 GET 和只读控制台。Electron main 固定客户端；renderer 只得零参数 `idorisStatus()`，不得提供 URL/path/port/header/token/FD selector。沿用 ADR-035 已接受的 65-byte canonical frame（64 小写 hex + LF）、读后关闭、秘密不继承到后代；选择器是分开的 `--idoris-admin-token-fd <n>`，不是 `=n`。此处不新增 framing、bootstrap HTTP 或 Admin 写接口。无 handoff 显示 `handoff-missing`，不读 env/file 猜 token。producer launcher 尚未交付，已有 consumer 不等于组合交付。

### 2.2 凭证模块 vault（保留 J-4/J-5/J-7/J-8 的已接受设计，非 M4 已交付清单）

- **定位**：独立的 Rust crate，由 iDoris 开发，Agent24 作为消费方参与接口设计。一份加密存储（主密钥放在 Keychain，条目加密，只存密文），支持两种部署形态：
  - **(a) 嵌入 iDoris**：用于模型 provider 出站时注入凭证。**谁能用哪个 provider 由 iDoris 的虚拟 key 策略决定**（§2.1）。个人版仍然只有一个二进制。
  - **(b) 独立守护进程 / 出站代理**：供 Agent24 的工具、CLI、MCP 使用。**与 iDoris 网关是不同的进程/实例**。**谁能用哪把凭证由 Agent24 的 grant/审批签发调用方 token 与 scope 决定**，vault 只负责校验、注入和审计（单一策略源，G-1/G-2）。
- **覆盖范围（J-5）**：只管**出站第三方凭证**。Agent24 的内部令牌（daemon bearer、模块握手、A3 附着、Desktop host secret）和模块 actor key **不进 vault**。
- **凭证类别（J-7）**：
  - bearer 类：注入。
  - 身份/服务类签名（Nostr、SSH、HMAC/SigV4）：vault 代签，私钥不出模块。
  - **资产类**（钱包 owner key、EOA、AirAccount 签名）：只归 AirAccount（TEE KMS + passkey）。vault **不托管、不代签**；Agent24 只负责发起和过审批门，最终确认在用户的 passkey 上完成。
- **注入方式**：v1 先实现显式代理 + 请求头替换（OneCLI 模式：调用方只持有占位符）。之后补「启动时注入环境变量」模式，覆盖读环境变量的 MCP/CLI 和非 HTTP 协议。
- **fail-closed**：解析或注入失败时一律拒绝，**不回退去读环境变量**。
- **机制**：按次派发，短 TTL，落盘只存哈希；凭证优先级为 自有 key > 团队 key > 托管 key（借鉴 treg，只借鉴设计：treg 许可证有「禁止托管销售」条款）；余额不足返回结构化 402；审计只记元数据。
- **法律边界**：组织内部共享付费账号可以；**跨用户转售托管账号不做**（PGL「平台不充当资金池」）。
- **Agent24 侧前置（J-6）**：在 M4 接入 iDoris 之前，Agent24 修补 `shell_exec` 与 MCP 子进程继承环境变量的缺口（`env_clear` + 白名单，mcp.json 支持按 server 配置 env），防止 `IDORIS_API_KEY` 外泄。

### 2.3 角色、实际模型与容量

稳定模型角色名为 `idoris/fast|daily|deep|vision|embed|rerank|decide|auto`。Agent24 决定 task→role：simple→daily，内部摘要/抽取/工具性调用→fast，complex→deep，Guardian→decide（M6 才考虑 systemone）。长上下文、工具、结构化输出走能力画像，不发明额外角色。既有语义：fast 以延迟为先（常驻 1–4B），daily 以面向人的质量为先（7–12B）；当前 catalog 标签与此有差距，重标须 R9 决策，不能用目录首项或硬编码排名补齐。

**运维 role map 目标**：仅在 HTTP 后端组件卡使用 `extensions.idoris_role_models`，由 iDoris 运维/自己的工具写配置；Agent24 只通过 HTTP 请求角色、展示实际模型，不写 YAML、不直接调加载/驱逐，也不使用库 API 旁路。每个 role（包括 `auto`）都要显式绑定具体 backend model ID；`auto` 不等于“选发现列表第一个”。缺绑定、未知/不可用模型或不符 local_only 时 fail-closed（无本地可用候选为 503），不跨角色猜测、不默认上云。候选支持范围与交付限制见 §4；不承诺订阅/进程后端也支持该扩展。

iDoris 拥有 catalog、推荐、实际模型身份、后端/量化选择以及分配额内的加载和驱逐。下载/预热/升级/回滚的功能归属也在 iDoris，但归属不表示已有下载服务。推荐 ID、provider ID、角色名与真实权重 ID 不可混为一谈；调用成功应使 load、上游 POST 和 `response.model` 的身份一致。

`/capabilities` 用于展示、挂载期 `requires_models` 检查，**不逐次预检**（TOCTOU）；实际准入以调用结果为准。当前条目是 `{id,capability,resident,estimated_memory_gb,ctx_limit,queue_depth,admission_status}`，其中 admission 为 ready/requires_eviction/blocked。角色版 `{role,model,backend,locality,…}` 仅为目标，未实现，不据此生成 UI 假值。

### 2.4 主机事实、内存分配与费用

Agent24 拥有同机 host facts、core 占用与整机账，为 iDoris 划一个总 LLM allocation；iDoris 在该额度内用自己的全局 ledger 做模型准入，不让 Agent24 再做逐模型驱逐器。独立部署时 iDoris 可自探。不能把剩余额度冒充物理 `ram_gb`，min_ram 门仍按真实物理 RAM 判断。

已有启动输入 `IDORIS_ALLOCATED_BUDGET_BYTES` 为可选 u64 **整数字节**：缺省保留独立模式，0 是真零；畸形值启动失败；给了额度则推荐器不重复扣系统保留。内部模型 footprint 的十进制 GB 换算只在 iDoris 边界做一次。目标是推荐与加载准入均受同一额度约束；I0 只完成前者，不能声称后者已硬限。没有动态预算 HTTP API；未来预算桥/变更重启流程需单独交付。不发明当前不存在的 `agent24 hw budget` 或 `llm_allocated_bytes` 发送者。

费用账与内存账不同：iDoris 管付费 reserve/settle、价格、402 和 `X-iDoris-Cost-Minor`；Agent24 按模块计次数/token，只记录返回费用，不重算。402 为终态，不重试、不换 provider。

### 2.5 请求、隐私、响应与错误

- 原型请求为 `model=idoris/<role>` 加 `X-iDoris-Privacy`、`X-iDoris-Complexity: simple|complex`、`X-iDoris-Request-Id`。Intent/Capabilities/Fallback 等只按真实需求加法接入；关联 Session/Trace-Id/Request-Id 由内核生成，不信任模块。`X-iDoris-Caller` 可选且只作元数据，永不作为策略依据。近期 Agent24 接入不做流式。
- 隐私只能维持或收紧。声明角色的调用不从内容重猜 task/role；显式 auto 才委托角色选择，仍不放宽 privacy。未声明角色的其他调用方可由静态规则/判定器选角色。处理顺序：隐私 → 角色/能力 → 付费预算 → 容量 → 选择/声明的降级；预算不足不被静默回落掩盖；仅显式声明 `X-iDoris-Fallback: next_in_chain` 才允许 iDoris 回落到免费候选，并在响应中说明降级。此授权不允许 Agent24 改走另一 provider；I0 router 的实现差距见 §4。
- `X-iDoris-Served-Locality: loopback|lan|remote` 表示**推理实际位置**。仅选定后端后有此头，后端错误也带；选择前拒绝可缺失。idoris-local 只接受 loopback，缺失/lan/remote 都按错误，结果不返调用者。完整 any provider 的 lan 及缺头均保守计 remote。网关地址在 loopback 不证明推理在本机，亦不证明字节没有出网。
- 所有响应带 `X-iDoris-Record-Id`；缓存还带 `X-iDoris-Cached: true`、`X-iDoris-Origin-Record-Id` 供去重。`/health` 返回 service/version/contract_version/instance_id；开启开发免 key 时启动日志及 health 的 `dev_no_key_enabled` 会明示；活着不代表 backend/model ready。身份自报不是对抗恶意本机端点的认证。
- 错误体采用 `{error:{type,rule_id,reason_code,evidence,remediation}}` 的结构化方向，实际各端点字段以源码/fixture 为准；不得假定所有字段处处存在。402/403/413/400 unsupported field 为终止错误，不重试、不换 provider；503 local_only_unavailable 在独占路径明确失败。rule_id/evidence/remediation 仅在管理页展示，不透给模块。
- iDoris 已有 JSON/SSE 路径与取消处理；不得把未来 Agent24 非流式首片当作禁止 iDoris SSE。已开始 token 后不重试；字段支持必须逐项证明。I0 Supervisor 路径会拒绝 max_tokens，不能宣称完整 OpenAI 兼容。

### 2.6 审计、轨迹、敏感确认与后续能力

Agent24 事件日志是 Evolver 主数据源，运行期记忆不是训练副本。双方审计不合并、不互抄，仅 record_id/trace_id 关联。默认不推送 trajectories；iDoris 用户开启 full/训练且完成确认后才考虑 ATIF v1.8 交换，命名空间分别 extra.idoris/extra.agent24，互不写入。feedback 方向已接受，Agent24 等有反馈 UI 再接。

**M5 既定决定不变**：敏感开关只由用户在 iDoris 本地控制台完成一次性确认；Agent24 可发起/展示 diff/深链接，不能代理确认。未来“有持续时间的 caller consent”仍延期，不在本轮改成已接受机制。Admin proposals/confirm、inspect、vault、kill switch、训练/晋升与 systemone 均须依各里程碑/API 单独交付，不因职责表而成为 v0.3.0 能力。本地判定器不可用须显式报错，不静默降级；判定 engine 如实返回。

## 3. 拟议 v0.3.1 local_only 数据面切片（待联合验收）

1. 用户独立准备并启动 iDoris/后端/已安装权重；数据面与 launcher 独立。Agent24 显式配置 loopback IDORIS_URL 与 iDoris 签发的 local_only 数据 key，目标安全存储为 Keychain，不把 Admin token 当数据 key。未决定 keychain service/account、轮换 CLI 或期限默认值。
2. 配置后独占 idoris-local，无 oMLX/Ollama 旁路；只发约定角色和三个头，非流式，云端补位默认关闭。auto 不作为默认猜测；具体模型 override 仍受 key 权限约束。
3. iDoris 运维明确绑定 HTTP role→真实模型；iDoris 推荐并负责 load/evict。Desktop 展示绑定/可用性及真实 response.model/Record-Id/locality，缺模型显示不可用，安装流程见 R9。不能把静态推荐表、GUI 高亮或无行为 Download 按钮当已安装/已选中证据。
4. 每次结果必须通过 locality 绊线；负对照（无 key、缺本地、缺头/lan/remote、不兼容字段）明确失败且远程/直连后端计数为零。预算推荐和实际并发加载限制分别验收。
5. 未有配对 launcher 时 Admin 状态卡可继续 unavailable，数据面可独立验收。Admin 组合验收仍是完整 M4 readiness 的独立缺口；文档完成不授权启动 M5。M4 接入前置（main/IDORIS_PORT/响应头/health 身份）不等于完整用户路径完成。

**目标而非实现**：max_tokens 兑现及非法值校验、chat/load timeout 分离、角色版 capabilities、绑定模型可用性检查、结构化 ready/instance 校验与启动失败清理。未来字段、超时数值、ready 行格式和 runtime 版本均不得在本修订中当作冻结 API；不得照设计审计草稿新增 FD framing、write API、downloader 或角色标签。

## 4. 实际/候选/目标与工件映射

**预算回落实现差距（B1）**：R6/R7 已接受“显式声明 Fallback 可回落到免费候选并回传降级”。I0 `idoris-policy::pipeline::budget_stage` 有 `allow_fallback()` 与 `Degradation::BudgetFallback` 分支及测试，但 `idoris-router/src/dispatch.rs` 传入 `PolicyCtx { budget: None }`，实际报价后原子 reserve 失败直接返回402。因此 I0 router 在有无声明时都仍为402，不能把 policy 核心的已测能力写成运行时已接线；031验收要如实验证该差距，不能反转既有规范。

以下源码及测试名称按 I0/A0 固定对象静态核对；**本次未运行软件测试**，已有测试只证明对应层，不代表跨仓组合验收。

| 事实/差距 | 精确源码、工件与已有测试 | v0.3.1 所需证据 |
|---|---|---|
| wire 仍 1.0.1；health 含身份 | I0 `crates/idoris-contracts/src/version.rs`；`crates/idoris-router/src/lib.rs::health` / `health_has_the_pr46_shape_and_a_record_id_header`；`conformance/tests/response-headers.test.ts` | 明示支持版本与身份错误负对照，不猜 1.2.0 |
| 默认 virtual key 与开发限制 | I0 `crates/idoris-router/src/auth.rs::enforce_scope`、`key_issue.rs::IssueSpec`；`src/virtual_key_wiring_tests.rs::{scope_and_fallback_fail_403_before_any_upstream_call,dev_no_key_rejects_any_paid_remote_and_invalid_bearer_before_egress}` | 真实 daemon/CLI/Desktop/module 各路径 scope 校验 |
| role 被接受但未接真实模型 | I0 `crates/idoris-router/src/dispatch.rs::{candidate,backend_model_id}`：role 请求用 provider ID；`src/lib.rs::chat_completions_rejects_unknown_role` 仅证明未知 role 拒绝 | 有绑定 daily 成功，load/chat 同 ID；未绑定 auto/deep 503、零上游调用 |
| allocation 仅进入推荐 | I0 `crates/idoris-router/src/bin/idoris.rs::run`、`src/capabilities.rs::LiveCapabilitiesProvider`；`crates/idoris-recommender/src/recommend/resident.rs`；`src/runtime.rs::spawn_with_factory` 仍取默认 budget | 0/有限字节并发 load 的实际计数/ledger，不能用推荐结果替代 |
| capabilities 为旧条目 | I0 `crates/idoris-router/src/capabilities.rs::CapabilityEntry`；`tests/capabilities.rs::injected_provider_returns_top_level_array_with_reference_shape_and_record_id` | 角色版是未来字段，不冒充现有 schema |
| Admin 只读，已有 intake | I0 `crates/idoris-router/src/admin.rs::authenticated_admin_router_exposes_only_the_five_read_routes`；A0 `apps/desktop/src/main/idoris-admin.ts::{parseAdminTokenFd,parseTokenFrame,readAdminTokenFromFd}` | 可信 producer、错误 FD/继承/超时安全组合证据仍缺 |
| A0 未接 iDoris 数据面/预算发送 | A0 `rust/crates/agent24-models/src/router.rs::ModelRouter::from_env` 为 oMLX/Ollama；`apps/desktop/src/main/backend-manager.ts` 启 agent24d | 真实 provider 接线、独占、locality 绊线；修改 OMLX_URL 不是正式集成 |
| max_tokens/timeout 不匹配 | I0 `crates/idoris-router/src/lib.rs` unsupported_parameter 分支、`crates/idoris-upstream/src/omlx/mod.rs::OmlxAdapterConfig`（共用 call_timeout）；A0 `rust/apps/agent24d/src/model_callback.rs`、`rust/apps/agent24-documents/src/extract/run.rs`；iDoris FU-206/FU-25（`docs/agent/tasks.md`） | 参数保留到上游、非法值拒绝、长生成不被 load timeout 截断；仍是目标 |
| readiness 尚非结构化协议 | I0 `crates/idoris-router/src/bin/idoris.rs::run` 只打印 listening 文本；`tests/cli.rs::occupied_admin_port_fails_before_data_health_is_served` | 不发明 IDORIS_READY 为当前输出 |

### 4.1 I0 之后的源码与候选（不是 v0.3.0 发布能力）

| 候选 | 固定提交 | 证据范围/限制 |
|---|---|---|
| [#507](https://github.com/iDoris-ai/iDoris/pull/507) onboarding | `543b008121760f9b3eb1264c25803b5ca38cf6fc`（OPEN；该head已获PRD批准，后续base/head移动须复审） | `docs/安装与首次调用-v0.3.0.md`、`scripts/test_onboarding_smoke.py`：iDoris 归档 fixture，非 Agent24 真实进程联调 |
| [#508](https://github.com/iDoris-ai/iDoris/pull/508) key ceiling | `1c9c592ffb8f8226e873cfbbc242abf2fe24e747`（已合并至 `959337d`） | `crates/idoris-router/src/lib.rs`、`src/virtual_key_wiring_tests.rs`：防 personal key 提升与鉴权前事件泄露；已经进入 main，但不属于 I0 发布二进制，补丁版仍需完整验收 |
| [#510](https://github.com/iDoris-ai/iDoris/pull/510) role binding | `6a9b37737d807983f78d55d719326f9771f183b8`（OPEN；该head已获PRD批准，后续base/head移动须复审） | `crates/idoris-router/src/dispatch.rs::role_models` 与 `src/role_binding_tests.rs`：显式运维 map/auto、缺失 fail-closed；HTTP 多协议与 lifecycle 共享具体目标；无映射 fixed subscription 保留既有角色资格，执行身份仍固定；不是 Agent24 已联调的证据 |
| [#511](https://github.com/iDoris-ai/iDoris/pull/511) 精确容量基础 | `0c0d34347436f73f6816e161af8a687248666761`（已合并至 `9395a96760f450f7f88f916137659c05290f2008`） | `crates/idoris-backend/tests/runtime_exact_charge.rs`；已进入main，非I0产物，仍不等于启动预算已接线。 |
| [#512](https://github.com/iDoris-ai/iDoris/pull/512) 启动预算接线 | `f1da0c7aca7fa0c0f4204f09790a1c141fd48682`（OPEN；新head待复审） | `crates/idoris-router/tests/runtime_exact_growth.rs`；候选真实runtime额度接线，尚未合并。 |

**2026-10-15 后续状态**：#511 已以精确head批准、9绿CI、CLEAN普通合并为 main `9395a96760f450f7f88f916137659c05290f2008`；#512已转base main并同步到 `f1da0c7aca7fa0c0f4204f09790a1c141fd48682`，需新head复审。#479 nullable cap修复e3426f6已批准，父层安全修复同步后当前73051b1仍须以新head门禁为准。当前状态以GitHub为准，这些不是I0产物内容。

### 4.2 Schema 清单（I0 实际只有 15 份）

- 核心 9 份：`task-profile`、`tenant`、`deploy-mode`、`provider`、`component-card`、`load-policy`、`routing-policy`、`adapter-manifest`、`training-sample`，路径均为 `packages/contracts/schema/<name>.schema.json`。
- 另有 `role.schema.json` 与五份 `admin-v0-{status,backends,models,runtimes,roles}.schema.json`。Admin roles schema 不是角色版 capabilities。
- 机器一致性入口：`crates/idoris-contracts/tests/contract_drift.rs`、`tests/parity.rs`（均相对该 crate）；Admin wire 另由 `crates/idoris-router/src/admin.rs` 的 `*_wire_shape_matches_the_shared_v0_contract` 测试核对。它们不证明未存在的 schema。
- **不存在/未交付的目标工件**：request/response-headers、health、error-body、virtual-key、capabilities 角色版、feedback、systemone request/response、inspect、trajectory、通用 admin proposals/confirm schemas。旧表的“新增/修订”是待办，不是“七份 schema 已发布”。未来只为实际消费的接口补工件与 conformance，另行审阅，本文不创建 schema。

## 5. 历史承接与被取代条款

历史保留用于追溯，不能构成第二份当前契约。以下既有细则继续保留而非退役：未声明角色时回传所选角色；requires_models可声明角色名；外部Jev仅用于允许离机的请求；修订遵循“提议→双方同意→升版本”。这里只恢复规范承接，不宣称相应目标工件已全部实现。

| 历史材料 | 继承/更正 |
|---|---|
| [R1 全文](negotiation/R1-Agent24-答复.md)、下方 R0–R8 | D-1..D-7、J-1..J-8 保留；原 §3.12/3.13 的内容已合入 §2，不再后文覆盖前文 |
| I0 [原 v1.1 全文](https://github.com/iDoris-ai/iDoris/blob/03c079c70611331a8c85a1666e32a75df6359d09/docs/interfaces/iDoris-Agent24-边界与接口规范.md) | 旧“每次预检 capabilities”改为展示/挂载检查；Evolver 读 iDoris 导出改为 Agent24 自己事件日志；loopback 默认免 key 被 R8 取代 |
| [旧 09](../09-对Agent24的需求.md)、[旧 17](../17-模型能力设计-Agent24协作草案.md)、[旧 handoff](../agent/handoff-agent24.md) | 已废弃历史；模型/推荐归属继续有效，旧端口、TS smoke、旧模型排名不是 Rust release 验收 |
| [Agent24 旧入口路由设计（A0）](https://github.com/iDoris-ai/Agent24/blob/6072fd373142eb3dbf1a6a4a3222e6eec82c74f6/docs/iDoris-integration-and-entry-router.md)、[ADR-032 集成材料（A0）](https://github.com/iDoris-ai/Agent24/blob/6072fd373142eb3dbf1a6a4a3222e6eec82c74f6/docs/design/INTEGRATION-AGENTEAR-IDORIS.md) | 继承 LocalOnly/动作授权/Served-Locality/不拥有 daemon；“尚无入口/未合并”的历史文字过期；Agent24 短指针由其 owner 独立修改 |
| [M4 landing handoff](../agent/HANDOFF-2026-10-11-M4-LANDING-TAIL.md) | Admin load/unload 未交付，Admin v0 只读；不是全量 Admin API 的完成声明 |
| 总体规划 §4.2/§6.1 | 本次只纠正默认 key 与 Admin v0 只读；保留未来管理面设计及历史链接 |

## 6. R9 与剩余决定

见 [R9 协商提案](negotiation/R9-v1.2-协商提案.md)：launcher 宿主、同意后安装执行方式、fast/daily 标签重标均 **OWNER REVIEW PENDING**。未经决定不新增写 API、下载器、FD framing、角色标签或 M5 consent 机制。v0.3.1 local 子集待双方确认；数据面可以独立于 launcher 推进，完整 readiness 仍需组合与干净环境验收。

## 7. 协商记录（历史原样保留）

| 轮次 | 日期 | 发起 | 内容 | 结论 |
|:---|:---|:---|:---|:---|
| R0 | 2026-09-27 | iDoris | 本草案 v0.1 发给 Agent24（agent24-13）review | Agent24 已确认收到，正在对照代码与 ADR-032/ME4-S2/A3 起草 Q-1…Q-9、§3 逐条意见与 ATIF 对齐评估；需 jason 拍板的点将单列；同意把 ADR-032 §9「一行都没合并」标为过期 |
| R1 | 2026-09-27 | Agent24 | 答复全文见 [`negotiation/R1-Agent24-答复.md`](negotiation/R1-Agent24-答复.md)。<br>• Q-9：同意模型管理全部归 iDoris。<br>• 更正两处事实：L3 从未实现；v0.1 漏列 Desktop 的 oMLX 管理。<br>• 分歧一处：Evolver 的数据源。<br>• §3 逐条意见。<br>• 另列 D-1…D-7 请 jason 拍板。 | — |
| R2 | 2026-09-27 | iDoris | **全部采纳** R1 的意见，包括撤回「Evolver 读 iDoris 导出」。<br>• 新增 §3.12。<br>• 回答 fast/daily 的区分标准。<br>• D-1…D-7：iDoris 同意 Agent24 的推荐选项（均为 (a)）。 | **待 jason 拍板 D-1…D-7**；拍板后转 JSON Schema，定稿 |
| R3 | 2026-09-27 | jason | **D-1…D-7 全部选 (a)**：配置 IDORIS_URL 后独占（不留直连 oMLX 的通道）；等 M4 前置完成再接入；Evolver 主数据源是 Agent24 事件日志；敏感确认只在 iDoris 控制台完成；Desktop main 进程直连 Admin 端口；每个实例两把 key；原型阶段只承诺 M4 状态卡片。同时同意 Agent24 那约 20 行文档修改 | 等 Agent24 回复「R2 无异议」后定稿 → 转 JSON Schema |
| R4 | 2026-09-27 | Agent24 | 「已读 §3.12，与我方 R1 一致，**R2 无异议**」。Agent24 开始修订自己的约 20 行文档：L3 改为 ATIF v1.8 交换格式；ADR-032 §9 标为过期；引用本规范。见 [Agent24 PR #540](https://github.com/iDoris-ai/Agent24/pull/540) | **定稿 v1.0** |
| R5 | 2026-09-27 | iDoris | v1.0.1 措辞澄清（T4.1 验收时发现）：Served-Locality 只在已选定后端时才带；它表示推理实际发生的位置；缓存命中带 Cached 和 Origin-Record-Id。都是加法兼容 | **Agent24 无异议**：接入时按 Cached + Origin-Record-Id 去重记账，这条写进它们的 P4 设计 |
| R6 | 2026-09-27 | iDoris | jason 提出，iDoris 提议：请求阶段职责划分 S0–S7；统一凭证模块 vault | Agent24 方向同意，提出措辞修订与五个条件 |
| R7 | 2026-09-27 | 双方 | iDoris 采纳全部措辞修订与条件，并对 J-4、J-2 各加一处限定；Agent24 确认；jason 拍板 J-1…J-8 全部选 (a) | **v1.1** |
| R8 | 2026-09-27 | Agent24 | 审阅 v1.1：无异议；建议 loopback 免 key 仅作为显式开发者选项，免 key 时视为 local_only | 已采纳，写入 §3.2 |
| R9 | 2026-10-15 | iDoris | [文档修订 1.2 整合与三个 owner 决策](negotiation/R9-v1.2-协商提案.md)；local_only v0.3.1 最小数据面与 launcher 解耦 | **OWNER REVIEW PENDING**；不是新实现或双方最终同意 |

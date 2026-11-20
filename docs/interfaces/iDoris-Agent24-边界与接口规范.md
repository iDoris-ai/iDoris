# iDoris × Agent24 分工边界与接口数据规范

> **文档修订 1.6（拟议，wire 1.3.0，2026-11-20，待双方确认）**：Agent24 的 IDORIS-CORE（[Agent24#1937](https://github.com/iDoris-ai/Agent24/pull/1937)）要求 iDoris 成为 Agent24 的必选核心：动态端口与发现、登记与心跳、由 iDoris 托管 oMLX、embeddings 与预热。本修订新增 §2.10（登记协议 v1）与 §2.11（wire 契约 1.3.0，只新增），增补 §2.1、§2.2、§2.3、§2.5、§2.7、§2.8、§2.9、§4.2，协商记录见 R12。依据是 iDoris 侧评估与接口提案（[PR #614](https://github.com/iDoris-ai/iDoris/pull/614)，下称设计稿）和 Agent24 2026-11-20 对设计稿 §3.3 七个问题的答复。**本修订的所有条款都是拟议，待双方确认后冻结；冻结前不改变任何已冻结条款的承诺，增补处逐处标注，被取代的句子保留原文并注明。** 修订号说明：Agent24 方案里的「§2.7 修订 1.4」已被 §2.9（修订 1.4）占用，故本次记为修订 1.6。
>
> **文档修订 1.5，2026-11-19**：只改状态说法，不改契约内容。§2.9 中的推荐接口、本地模型列表和 oMLX 密钥顺序已在 iDoris v0.5.0 实现，wire 契约 1.2.0 随该版本生效；§2.9 的下载部分仍未实现，接口形状待另行修订。
>
> **文档修订 1.4，2026-11-15**：owner 要求装了 oMLX 的机器首次启动时按硬件自动选默认 chat 模型，由 iDoris 负责推荐。新增 §2.9（本机 chat 模型推荐、本地模型类别与下载，拟议 wire 1.2.0，只新增），档位表由 owner 拍板，iDoris 与 Agent24 两个会话各确认一次。iDoris 侧推荐、本地模型列表与密钥顺序已在 v0.5.0 实现（见修订 1.5）；下载部分仍未实现。
>
> **文档修订 1.3，2026-10-28**：owner 要求 Agent24 桌面端一站式「下载 → 运行 → 连接」iDoris，这定下了 R9 的 OD-1（launcher 宿主为 agent24d）和 OD-2（由用户在桌面端同意）。§2.1 第一条随之改写，新增 §2.7（launcher 托管契约，iDoris v0.4.0 起）与 §2.8（wire 契约 1.1.0）。已接受条款以正文为准。
>
> **文档修订 1.2，2026-10-15**：将 R0–R8 已接受条款合并到正文，消除“后补章节覆盖前文”的阅读方式；当时新增的 v0.3.1 范围及 R9 三项决策仍是 **OWNER REVIEW PENDING**。（其中 OD-1、OD-2 已由修订 1.3 决定，见 §2.7；fast/daily 标签重标仍待定。）
> **唯一跨仓规范权威**：本文件。iDoris 主持，Agent24 作为消费者审阅；Agent24 应保留短消费方指针、实现证据及反向硬件权威链接，不复制第二份契约。总体规划的项目里程碑仍见 [总体规划](../iDoris-总体规划.md)。

## 0. 版本、状态与证据边界

**两个独立组件，一份公开 HTTP 契约，无 crate/源码级耦合。** Agent24 管任务、动作授权、工具和运行期记忆；iDoris 管模型调用准入、推理策略、模型生命周期及其审计。规范条款不等于已实现功能；以下固定基线决定“实际”含义。

| 标识 | 含义与基线 |
|---|---|
| 文档修订 `1.5` | 文字修订；不是服务 wire version。wire 契约版本见 §2.8；§2.9 的 wire 1.2.0 随 iDoris v0.5.0 生效（下载部分除外，尚未实现） |
| 文档修订 `1.6`（拟议，2026-11-20，待双方确认） | 文字修订；不是服务 wire version。新增 §2.10、§2.11，增补 §2.1/2.2/2.3/2.5/2.7/2.8/2.9/4.2，协商记录 R12。条款拟议，待双方确认后冻结；冻结前以修订 1.5 及更早的已冻结条款为准，不得当作已发布契约，也不得据此宣称 iDoris 已实现 |
| iDoris release `v0.3.0` | 实现提交 `03c079c70611331a8c85a1666e32a75df6359d09`（下称 I0） |
| runtime `contract_version=1.0.1` | I0 的 `crates/idoris-contracts/src/version.rs::CONTRACT_VERSION`；TS `packages/contracts/src/version.ts` 同值 |
| Agent24 审计基线 | `6072fd373142eb3dbf1a6a4a3222e6eec82c74f6`（下称 A0，固定历史快照，非当前 main）；main 源码状态不等于 stable 已发给用户 |
| v0.3.1 / v0.3.2 | 已发布；launcher 托管从 v0.4.0 起，见 §2.7 |
| wire 契约 `1.3.0`（拟议） | iDoris v0.6.0 的目标版本，内容见 §2.11，只有新增；拟议，待双方确认后冻结。冻结并发版之前，`CONTRACT_VERSION` 仍为 `1.2.0`（v0.5.0），`/health.contract_version` 同值 |
| iDoris release `v0.5.0` | 提交 `014845472bc2d2d0efbc68d5739d293965aa00b2`（tag `v0.5.0`，wire 1.2.0）。修订 1.6 对 iDoris 现状的描述以该提交为读取基线，来自设计稿的静态阅读，未运行软件测试 |
| Agent24 IDORIS-CORE 基线 | `bf70404792dd8aecf128d26a7e3f19e737a97cff`（[#1937](https://github.com/iDoris-ai/Agent24/pull/1937) 的合并提交，2026-11-20），方案见 [IDORIS-CORE-PLAN](https://github.com/iDoris-ai/Agent24/blob/bf70404792dd8aecf128d26a7e3f19e737a97cff/docs/design/IDORIS-CORE-PLAN.md)；固定历史快照，不是当前 main |

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

- **修订 1.3**：agent24d 是 iDoris 的执行宿主，负责下载、安装、生成配置、签发 key、启动和停止，契约见 §2.7。iDoris daemon 不启动 Agent24。用户也可以自己手动启动 iDoris，这条路径继续支持。
- **修订 1.6（拟议，待双方确认后冻结）**：launcher 托管时，iDoris 还负责拉起并管理 oMLX 引擎（§2.7.6）；iDoris 仍不启动 Agent24。Agent24 完成 R12 的 Q7 迁移后，不再直连 oMLX，Agent24 源码里不再出现任何 oMLX 地址，所有模型调用只经 iDoris。
- `IDORIS_URL` 必须显式配置，无隐含端口。D-1(a)：配置后独占 iDoris，不留直连 oMLX/Ollama 回退；缺 key、服务不可用或身份不符都明确失败。〔修订 1.6，拟议，待双方确认后冻结：launcher 托管时 iDoris 绑定内核分配的动态端口（§2.7.2），agent24d 经发现文件（§2.7.3）与登记（§2.10）取得实际端口并自行构造数据面地址，所以仍然没有隐含端口；手工启动、固定端口的路径不变，Agent24 用什么开关显式给出该地址不属于本契约。〕
- 已接受完整模型是两个逻辑 provider：`idoris-local`=Local，固定 `local_only`；`idoris-any`=Remote，固定 `any`。按 URL 去重 models/容量，不能相加。D-6 两把 key 分别限定 `[local_only]` 与 `[local_only,any]`。§3 提议仅交付 local 子集，第二 provider/key 延后，不撤销 D-6。
- 模块 manifest 未声明或声明 `local_only` 时使用 `idoris-local`；只有 `remote_allowed` 才有资格进入 `idoris-any`，仍不能扩大调用方已授予的权限。
- 数据面默认必须有 virtual bearer，即使 loopback。`IDORIS_DEV_NO_KEY=1` 仅为显式 loopback 开发选项，无 key 只能走免费 loopback/local_only；错误 bearer 不能借此绕过。非 loopback 必须有有效 key。tenant 模式沿用 `X-iDoris-Tenant`；个人模式不伪造 tenant；tenant 模式的数据 key 必须绑定该 tenant_id。

| 凭证 | 权威/使用者 | 边界 |
|---|---|---|
| 数据面 virtual key `idk_…` | iDoris 签发；Agent24 内核持有 | Bearer 用于数据面；模块不接触；目标持久存储为 Keychain，不与 Admin 混用 |
| Admin session token | 每次启动的可信 launcher → iDoris、Electron main | 只经两条一次性匿名 pipe/FD（Windows 白名单 HANDLE）；不进入 argv/env/普通文件/stdout/stderr/log/renderer。〔修订 1.6，拟议，待双方确认后冻结：launcher 托管路径下 Electron main 不持有 Admin token，桌面端的 Admin 操作都经 agent24d 的 `/api/v1/idoris/*`（R12-Q3）；token 另可在已通过双向证明的登记会话里由 agent24d 用 `set_admin_token` 会话内轮换（§2.10.6），这是「只经两条…」之外唯一新增的传递通道，禁入项不变〕 |
| oMLX 上游 key | iDoris → 后端 | 当前配置 `IDORIS_OMLX_API_KEY`；不是 Agent24 的 `OMLX_API_KEY`，不当 virtual key。〔修订 1.6，拟议，待双方确认后冻结：iDoris 自己拉起的私有引擎另有每次拉起随机生成的私有 key，只在 iDoris 与该引擎之间，见 §2.7.7，待实测〕 |
| Agent24 daemon token | Agent24 daemon ↔ 自身客户端 | 不属于 iDoris，不送进第三方 vault |
| `instance_nonce`（修订 1.6，拟议，待双方确认后冻结） | iDoris 每次启动用 CSPRNG 生成 256 位（小写 hex 64 字符）；agent24d 读 0600 发现文件取得 | 只在 iDoris 进程内存和发现文件里；不进 `/health`、Admin、日志、argv、环境变量、错误信息，也不在 UDS 上明文传输，只作 HMAC 密钥（§2.10.5）。挡的是连错实例、读不到该目录的沙箱进程冒充，**不是**对抗同 UID 恶意进程的屏障，登记方身份由 Agent24 的 `LOCAL_PEERTOKEN` 与 codesign 校验兜底 |

修订 1.6（拟议，待双方确认后冻结）：上表因此多出第五行，标题里的「四类」沿用修订 1.5 的说法，冻结后再改标题，以免破坏引用锚点。`instance_id`（UUID v4，每个 iDoris 进程一个，`/health` 免鉴权公开）只是实例标识，不是凭据，用来核对「这个端口上答话的是不是登记的那个实例」，不能单独证明身份。

I0 `idoris key issue --spec-stdin` 的实际 spec 为 `owner, tenant_id?, allowed_privacy, allowed_roles, expires_at_ms?, admin_scopes`；`admin_scopes` 只能为空，未知字段拒绝。`budget_ref/expires_at` 等旧 scope 草案不是当前签发字段。scope 只能拒绝，不能放宽请求。完整角色权限才允许具体模型 ID 及 next_in_chain，最小 daily key 不能靠具体 ID 绕过限制；见 §4 安全候选的额外修复状态。

Admin v0 为独立 loopback listener，只有 `status/backends/models/runtimes/roles` 五个只读 GET 和只读控制台。Electron main 固定客户端；renderer 只得零参数 `idorisStatus()`，不得提供 URL/path/port/header/token/FD selector。沿用 ADR-035 已接受的 65-byte canonical frame（64 小写 hex + LF）、读后关闭、秘密不继承到后代；选择器是分开的 `--idoris-admin-token-fd <n>`，不是 `=n`。此处不新增 framing、bootstrap HTTP 或 Admin 写接口。无 handoff 显示 `handoff-missing`，不读 env/file 猜 token。producer launcher 尚未交付，已有 consumer 不等于组合交付。

**修订 1.6（拟议，待双方确认后冻结）取代上段两处**：「只有 `status/backends/models/runtimes/roles` 五个只读 GET 和只读控制台」，以及「此处不新增 … Admin 写接口」中的「Admin 写接口」一项。Admin 仍是独立 loopback listener，仍用同一个 Admin token，既有五个只读 GET 的形状不变；另新增只读 `GET /admin/api/v1/upstreams` 和受限写接口 `POST /admin/api/v1/upstreams/{id}/start|stop|restart`（§2.11.4，owner 已定）。写接口只作用于 iDoris 自己拥有的上游进程，不给外部进程发信号；模型 load/unload（[#611](https://github.com/iDoris-ai/iDoris/issues/611)）与 §2.9 的下载仍未交付，将沿用同一套 Admin 写接口通则（§2.11.4）另行修订。「不新增 framing、bootstrap HTTP」两项不变：`--launcher-stdin`（§2.7.1）是 launcher 到 iDoris 的 stdin 帧，不是 Admin 的 framing，上段的 ADR-035 65 字节帧与 `--idoris-admin-token-fd` 选择器不受影响。launcher 托管路径下 Electron main 不再持有 Admin token（R12-Q3），所以上段关于 Electron main 取 token 的 FD 交接在该路径下不再适用；ADR-035 本身是 Agent24 的内部决定，本修订不改它。§2.5「远程熔断开关」一条里的「当前 Admin v0 只读」同理，已在该处标注；§4 与 §5 表中「Admin 只读」的说法是对 I0 及历史材料的记述，不改。

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

**修订 1.6 增补（拟议，待双方确认后冻结）**：vault 仍然没有接线，下面两件事都不经过它。（1）oMLX 上游 key 的注入，由「`provider.id` 等于 `omlx`」改为「loopback 的 `omlx@` 卡」（判据与 `GET /v1/models` 现有的 loopback `omlx` 卡判据一致），这样 §2.3 的伴生卡 `omlx-embed` 才能带 key；key 的来源顺序仍是 §2.9「oMLX 密钥」，iDoris 私有引擎的 key 见 §2.7.7。（2）自定义 OpenAI 兼容上游（§2.3 的 `idoris_upstream`）在 v0.6.x 只做 loopback、不带密钥：非 oMLX 的卡没有任何凭据注入；需要密钥的上游，以及局域网、远程上游（隐私等级 `any`、egress 计数、同意流程），等 vault (a) 接线之后另行修订。

### 2.3 角色、实际模型与容量

稳定模型角色名为 `idoris/fast|daily|deep|vision|embed|rerank|decide|auto`。Agent24 决定 task→role：simple→daily，内部摘要/抽取/工具性调用→fast，complex→deep，Guardian→decide（M6 才考虑 systemone）。长上下文、工具、结构化输出走能力画像，不发明额外角色。既有语义：fast 以延迟为先（常驻 1–4B），daily 以面向人的质量为先（7–12B）；当前 catalog 标签与此有差距，重标须 R9 决策，不能用目录首项或硬编码排名补齐。

**运维 role map 目标**：仅在 HTTP 后端组件卡使用 `extensions.idoris_role_models`，由 iDoris 运维/自己的工具写配置；Agent24 只通过 HTTP 请求角色、展示实际模型，不写 YAML、不直接调加载/驱逐，也不使用库 API 旁路。每个 role（包括 `auto`）都要显式绑定具体 backend model ID；`auto` 不等于“选发现列表第一个”。缺绑定、未知/不可用模型或不符 local_only 时 fail-closed（无本地可用候选为 503），不跨角色猜测、不默认上云。候选支持范围与交付限制见 §4；不承诺订阅/进程后端也支持该扩展。

**修订 1.6 增补（拟议，待双方确认后冻结）**：卡的 `extensions` 新增两个键，同 `idoris_role_models` 一样必须带 `_degradation`。`idoris_managed: {engine: omlx, mode: auto}`：该卡的引擎由 iDoris 托管（§2.7.6）；可执行文件路径、模型目录这类宿主信息不进卡，端点取卡的 `endpoint`。`idoris_upstream: {kind: openai_compat, flavor: <字符串>}`：自定义 OpenAI 兼容上游（LM Studio 等）；`Form` 枚举与卡 schema 是冻结契约，不新增 kind，上游种类只放在这里；生成这类卡的 `setup upstream` 子命令后移 v0.6.1，本修订不冻结它的形状。`embed` 角色在 HTTP 路径上由一张伴生 resident 卡承载：`setup omlx` 在 `omlx.yaml` 之外再写 `omlx-embed.yaml`（`provider.id` 为 `omlx-embed`，`endpoint` 与 `omlx.yaml` 相同，能力只有 `embedding`，`load_policy` 为 `resident` 加 `keepalive.pinned: true` 加 `admission: coexist`；给了 `--bind embed=<模型>` 时，`extensions.idoris_role_models.embed` 绑定该具体 embedding 模型，未绑定时这张卡对 embeddings 请求 fail-closed，返回 503）。随包文件仍是 5 个，伴生卡由 `setup` 生成，并列入 `--force` 的整体替换范围（§2.7.8）。行为约定见 §2.11.3。

iDoris 拥有 catalog、推荐、实际模型身份、后端/量化选择以及分配额内的加载和驱逐。下载/预热/升级/回滚的功能归属也在 iDoris，但归属不表示已有下载服务。推荐 ID、provider ID、角色名与真实权重 ID 不可混为一谈；调用成功应使 load、上游 POST 和 `response.model` 的身份一致。

〔修订 1.6，拟议，待双方确认后冻结：上段「下载/预热/升级/回滚的功能归属也在 iDoris，但归属不表示已有下载服务」中，预热有了接口 `POST /v1/models/warmup`（§2.11.2），拟议随 iDoris v0.6.0；下载、升级、回滚仍然没有。该接口实现发版后，再以文字修订把上句的状态说法改为「预热已实现，下载仍未」。〕

`/capabilities` 用于展示、挂载期 `requires_models` 检查，**不逐次预检**（TOCTOU）；实际准入以调用结果为准。当前条目是 `{id,capability,resident,estimated_memory_gb,ctx_limit,queue_depth,admission_status}`，其中 admission 为 ready/requires_eviction/blocked。角色版 `{role,model,backend,locality,…}` 仅为目标，未实现，不据此生成 UI 假值。

### 2.4 主机事实、内存分配与费用

Agent24 拥有同机 host facts、core 占用与整机账，为 iDoris 划一个总 LLM allocation；iDoris 在该额度内用自己的全局 ledger 做模型准入，不让 Agent24 再做逐模型驱逐器。独立部署时 iDoris 可自探。不能把剩余额度冒充物理 `ram_gb`，min_ram 门仍按真实物理 RAM 判断。

已有启动输入 `IDORIS_ALLOCATED_BUDGET_BYTES` 为可选 u64 **整数字节**：缺省保留独立模式，0 是真零；畸形值启动失败；给了额度则推荐器不重复扣系统保留。内部模型 footprint 的十进制 GB 换算只在 iDoris 边界做一次。目标是推荐与加载准入均受同一额度约束；I0 只完成前者，不能声称后者已硬限。没有动态预算 HTTP API；未来预算桥/变更重启流程需单独交付。不发明当前不存在的 `agent24 hw budget` 或 `llm_allocated_bytes` 发送者。

费用账与内存账不同：iDoris 管付费 reserve/settle、价格、402 和 `X-iDoris-Cost-Minor`；Agent24 按模块计次数/token，只记录返回费用，不重算。402 为终态，不重试、不换 provider。

### 2.5 请求、隐私、响应与错误

- `POST /v1/inspect`（M5）：只做判定，返回 `{sensitivity: S0..S3, findings: [{kind, span?}], action: BLOCK|REDACT|ALLOW}`，不改写内容。
  - 契约已冻结（M5-01）：`packages/contracts/schema/inspect-request.schema.json`（`{input: string}`）与 `inspect-response.schema.json`，Rust 侧 `idoris_contracts::inspect`，两侧全部对象 `additionalProperties:false` / `deny_unknown_fields`。
  - `kind` 是封闭枚举（证件/手机/银行卡/统一社会信用代码/车牌/护照/港澳证件/邮箱/密钥类/凭据路径/`input_limit_exceeded`），新增类别属于契约变更。
  - `span = {start, length}`，按 **UTF-16 code unit** 计、半开区间 `[start, start+length)`，`length ≥ 1`；`input_limit_exceeded`（超限未扫描，按 X-1 直接 BLOCK）不带 span。
  - 响应里除枚举外没有任何字符串字段：命中原文、密钥子串不会出现在响应体，也不会经由响应进入日志。
  - 鉴权：与协议端点同一套 virtual key，但只看 key 的隐私上限，不看 role；`X-iDoris-Privacy` 缺省为 `local_only`。声明 `local_only`（或远程熔断已开启）时任何输入都判 `BLOCK`，只有声明 `any` 且 key 上限允许 `any` 才可能得到 `ALLOW`。调用方同意脱敏目前没有线上表示，因此该端点只会返回 `BLOCK`/`ALLOW`，不会返回 `REDACT`。

- **闸一在推理路径上的运行限制（I0 之后的 main 源码，不是 v0.3.0 发布能力；本节为静态源码核对，未作运行验收）**：
  - `/v1/chat/completions`、`/v1/embeddings`、`/v1/rerank`、`/v1/messages` 四个推理端点都在鉴权之后、路由策略（chat 另含意图识别）之前检查整个请求体——对象键、字符串与数字都会被提取、拼接后送入闸一判定。只要判定不是 `ALLOW`，就把这次请求钉成 `local_only`（只收紧，不放宽）；没有可用本地候选时上游收到 0 个请求，常见错误是 503 `local_only_unavailable`，也可能是 503 `no_eligible_candidate`（预算/准入/熔断排除），调用方不得假定只有单一 503 原因码。
  - 不透明内容的判定范围：闸一对上面提取并拼接的整段文本（含对象键，不限字符串值）做一次大小写不敏感的字面匹配 `data:…;base64,`，头部字符限定在 MIME token 字母表加 `/`、`;`、`=`，且要求逗号后有内容、其首字节是 ASCII 字母数字或 `+`/`/`；命中则整次请求按 S3/`BLOCK` 钉成本地。这条规则**不**声称覆盖全部 RFC 2397、浏览器或百分号编码写法。图片/音频/文件 content part（http(s) `image_url`、不带 `data:` 前缀的裸 base64 `source`/`input_audio.data`、文件 id）不会仅因其类型被钉成本地；token id 整数数组（任何长度、任何嵌套位置）也不会仅因是整数数组被钉成本地——数组里的每个数字值都按十进制文本、以 NUL 分隔后纳入检查，不代表其编码的原始内容已被还原或检查。以上都是当前实现的检测覆盖缺口，不是「已证明安全」：data URL 文本本身或解码后暴露的 PII 仍可能被其他规则独立命中并收紧，但不构成不透明性保证。需要「不透明即本地」的调用方应自行显式声明 `local_only`，不能依赖闸一兜底。
  - 配置了事件存储时，chat 与上述三个协议端点各在 `request.received` 之后、`profiled` 之前写一个 `inspected` 事件；写入失败在发送前返回 503。metadata 只有 `sensitivity`、`privacy`、`labels`（命中类别，可为空）与 `reason`；`reason` 优先级为扫描失败 `inspect_failed` > 远程熔断 `remote_kill_switch` > 内容收紧 `privacy_tightened_by_content`，不含原文或位置，也不区分「内容命中」与「不透明内容」两类收紧。
  - **远程熔断开关**：启动时 `IDORIS_REMOTE_KILL_SWITCH=1`（且只认这个精确值）让所有请求按 `local_only` 处理，同时跳过不可信远程卡的模型列举与启动探测；运行时切换依赖尚未定义的 Admin 写接口，当前 Admin v0 只读。〔修订 1.6，拟议，待双方确认后冻结：Admin 在 §2.11.4 之后有受限写接口，但只限 `upstreams` 一族；熔断开关的运行时切换仍未定义，本条其余不变。〕
  - **provider 断路器**：二进制默认关闭，`IDORIS_PROVIDER_BREAKER=1` 才启用（共享 conformance 仍约定“持续 5xx 原样透传”）。启用后，OPEN 窗口内不向该 provider 发送请求，选路改试其他候选；选择阶段若最终仍无可用候选，可能返回 503 `no_eligible_candidate`，但初始快照里全部候选都被断路器排除并不等同于必然如此——`select_with_gate_fallback` 的重试快照只对非 resident HTTP 候选保留 blocked，resident HTTP 候选会被重新纳入选路，因此仍可能选中一个初始被断路器排除的 resident HTTP 候选，随后才在代理发送前被拒，返回 503 `provider_circuit_open`。调用方不得假定选择阶段与代理阶段的断路器判定之间存在单一确定性映射，也不能把这个保证推广到 Supervisor 或订阅路径。
  - 未接线：响应侧检查（`idoris-policy::credentials::response`）与凭据 broker/`secret://` 注入授权（`idoris-policy::credentials`）目前都只是纯核心，没有 HTTP/流式调用方接入；不能据此声称响应内容或已知凭据注入已在运行时受检。

- 原型请求为 `model=idoris/<role>` 加 `X-iDoris-Privacy`、`X-iDoris-Complexity: simple|complex`、`X-iDoris-Request-Id`。Intent/Capabilities/Fallback 等只按真实需求加法接入；关联 Session/Trace-Id/Request-Id 由内核生成，不信任模块。`X-iDoris-Caller` 可选且只作元数据，永不作为策略依据。近期 Agent24 接入不做流式。
- 隐私只能维持或收紧。声明角色的调用不从内容重猜 task/role；显式 auto 才委托角色选择，仍不放宽 privacy。未声明角色的其他调用方可由静态规则/判定器选角色。处理顺序：隐私 → 角色/能力 → 付费预算 → 容量 → 选择/声明的降级；预算不足不被静默回落掩盖；仅显式声明 `X-iDoris-Fallback: next_in_chain` 才允许 iDoris 回落到免费候选，并在响应中说明降级。此授权不允许 Agent24 改走另一 provider；I0 router 的实现差距见 §4。
- `X-iDoris-Served-Locality: loopback|lan|remote` 表示**推理实际位置**。仅选定后端后有此头，后端错误也带；选择前拒绝可缺失。idoris-local 只接受 loopback，缺失/lan/remote 都按错误，结果不返调用者。完整 any provider 的 lan 及缺头均保守计 remote。网关地址在 loopback 不证明推理在本机，亦不证明字节没有出网。
- 所有响应带 `X-iDoris-Record-Id`；缓存还带 `X-iDoris-Cached: true`、`X-iDoris-Origin-Record-Id` 供去重。`/health` 返回 service/version/contract_version/instance_id；开启开发免 key 时启动日志及 health 的 `dev_no_key_enabled` 会明示；活着不代表 backend/model ready。身份自报不是对抗恶意本机端点的认证。〔修订 1.6，拟议，待双方确认后冻结：`/health` 在上述字段之外新增 `phase`、`ready`、`started_at_ms`、`registered`、`upstreams`（§2.11.1）。`status` 仍恒为 `"ok"`，含义仍是「进程活着并在服务」，**不是**「模型可用」；「模型可用」看 `ready`（等于 `phase=="ready"`）。`/health` 仍免鉴权，`instance_id` 仍是公开标识，`instance_nonce` 永远不出现在 `/health` 或任何响应里。〕
- 错误体采用 `{error:{type,rule_id,reason_code,evidence,remediation}}` 的结构化方向，实际各端点字段以源码/fixture 为准；不得假定所有字段处处存在。402/403/413/400 unsupported field 为终止错误，不重试、不换 provider；503 local_only_unavailable 在独占路径明确失败。rule_id/evidence/remediation 仅在管理页展示，不透给模块。
- iDoris 已有 JSON/SSE 路径与取消处理；不得把未来 Agent24 非流式首片当作禁止 iDoris SSE。已开始 token 后不重试；字段支持必须逐项证明。I0 Supervisor 路径会拒绝 max_tokens，不能宣称完整 OpenAI 兼容。

### 2.6 审计、轨迹、敏感确认与后续能力

Agent24 事件日志是 Evolver 主数据源，运行期记忆不是训练副本。双方审计不合并、不互抄，仅 record_id/trace_id 关联。默认不推送 trajectories；iDoris 用户开启 full/训练且完成确认后才考虑 ATIF v1.8 交换，命名空间分别 extra.idoris/extra.agent24，互不写入。feedback 方向已接受，Agent24 等有反馈 UI 再接。

**M5 既定决定不变**：敏感开关只由用户在 iDoris 本地控制台完成一次性确认；Agent24 可发起/展示 diff/深链接，不能代理确认。未来“有持续时间的 caller consent”仍延期，不在本轮改成已接受机制。Admin proposals/confirm、inspect、vault、kill switch、训练/晋升与 systemone 均须依各里程碑/API 单独交付，不因职责表而成为 v0.3.0 能力。本地判定器不可用须显式报错，不静默降级；判定 engine 如实返回。

### 2.7 launcher 托管契约（修订 1.3，iDoris v0.4.0 起，#588）

agent24d 托管 iDoris 时，按下表执行。表中都是已冻结的接口，改动必须走本规范的修订。（修订 1.6 的增补与被取代的句子见表后，拟议，待双方确认后冻结。）

| 环节 | 契约 |
|---|---|
| 下载 | Release 资产为 `idoris-v<ver>-aarch64-apple-darwin.tar.gz` 和同名 `.sha256`。launcher 锁定 tag 和 sha256，并校验 `codesign` 的 TeamIdentifier 为 `HYAP444FN9`。只支持 v0.4.0 及以上版本 |
| 布局 | 包里恰好有 5 个文件：`idoris`、`config/{catalog,routing-policy}.yaml`、`config/components/{omlx,subscription}.yaml`。配置路径相对可执行文件所在目录解析，与 cwd 无关 |
| 组件配置 | `idoris setup omlx --endpoint <loopback url> --out-dir <dir> [--bind role=model]… [--load-mode on_demand\|resident] [--api-key-stdin] [--force]`，输出 JSON。共 11 个错误码：`unreachable`、`auth_required`、`auth_failed`、`not_omlx`、`model_not_found`、`invalid_endpoint`、`invalid_role`、`out_dir_not_empty`、`validation_failed`、`write_failed`、`invalid_api_key_input`。只会写 out-dir 及其同级的 `.idoris-setup-*` 暂存目录。`--force` 的替换要么全部成功，要么全部不生效。key 只从 stdin 读入，只用于探测。运行时的 oMLX key 仍走 `IDORIS_OMLX_API_KEY`。Agent24 不写 YAML |
| 数据 key | `idoris key issue --spec-stdin` / `key list` / `key revoke <vk_…>`，离线运行，不需要 serve 在跑，也不取实例锁，但必须和 serve 使用同一个 `IDORIS_DB_PATH`。吊销立即生效，可以在线轮换。owner 字段建议用唯一值 `agent24d:<install_id>:<nonce>`，签发结果丢失时可以按 owner 找到对应的 key 再吊销 |
| 启动 | `idoris serve --admin-token-stdin`，进程常驻前台。Admin token 由 launcher 用 CSPRNG 生成，格式为 64 位小写 hex 加 LF，共 65 字节，10 秒内写入 stdin，不得出现在 argv、环境变量、文件或日志里 |
| 就绪 | 轮询 `GET /health`：`status=="ok"`，`version` 符合锁定的版本，`contract_version` 的 major 为 1，并且进程仍然存活。stdout 不属于契约。启动前应确认 oMLX 可达 |
| 停止 | SIGTERM 和 SIGINT 都会优雅停机，总时长约 15 秒（10 秒加上回收 runtime 子进程的 5 秒）。launcher 应先发 SIGTERM，17 秒后仍未退出再发 SIGKILL |
| 退出码 | 0 表示正常；1 表示启动失败或停止失败（停止超时会输出 `[idoris] 停止失败：…`）；3 表示同一数据库已有实例在运行（按 canonical 路径判断，符号链接视为同一个数据库） |
| 不承诺 | 自动升级、一致备份和回滚（M-IDL2 再做）；aarch64 以外的平台 |

**修订 1.6 增补（拟议，待双方确认后冻结；iDoris v0.6.0 目标）。** 上表各行的既有承诺全部保持。下面是增补，按上表的行对应；被取代的句子保留在上表原文里，在这里逐条注明。本修订引用设计稿 §3.2.0 的决策时一律写作「设计稿 Dn」，它与本规范既有的 D-1…D-7 是两套编号。

| 上表的行 | 修订 1.6 增补 | 被取代的句子 |
|---|---|---|
| 启动 | 新增 `idoris serve --launcher-stdin`（§2.7.1）；`--admin-token-stdin` 与 65 字节旧帧不变，二者互斥 | 无 |
| 就绪 | 两级就绪（§2.7.4）：「iDoris 可用」是本行判据加 Admin 可答，「模型可用」另看 `/health.phase` | 「启动前应确认 oMLX 可达」：对声明了托管的卡不再适用，iDoris 自己探测、拉起或报因（§2.7.6） |
| 停止 | 自有引擎一并停止，分配在原 17 秒预算内（§2.7.5） | 无 |
| 退出码 | 新增 4、5，并增加 1 的触发条件（§2.7.5） | 无，0、1、3 的含义不变 |
| 组件配置 | `setup omlx` 扩展（§2.7.8）；11 个既有错误码的语义不变 | 无 |
| 布局 | 随包仍恰好 5 个文件，伴生卡等由 `setup` 生成（§2.7.9） | 无 |
| 不承诺 | 更新（§2.7.9） | 无，只增加不承诺项 |
| （新增） | 动态端口（§2.7.2）、发现文件（§2.7.3）、登记（§2.10）、oMLX 生命周期（§2.7.6）、引擎密钥（§2.7.7） | — |

#### 2.7.1 `--launcher-stdin` 帧（拟议，待双方确认后冻结）

`idoris serve --launcher-stdin`，与 `--admin-token-stdin` 互斥，同时给出是用法错误，退出码 1。旧 flag 与旧 65 字节帧完全不变。

- 字节层：一行 UTF-8 JSON，以 LF 结尾，总长（含 LF）≤ 4096 字节，10 秒内读完。读到第一个 LF 就停，之后不再读 stdin。launcher 写完就关闭 stdin 是预期行为，**stdin 读到 EOF 不代表 launcher 已经不在**，iDoris 不据此判断孤儿，孤儿只看登记会话（§2.10.8）。读取位置与旧帧相同：端口绑定之后、对外服务之前，所以帧无效时在服务任何请求之前以退出码 1 退出。

| 字段 | 类型 | 必填 | 约束 |
|---|---|---|---|
| `v` | integer | 是 | 必须等于 1 |
| `admin_token` | string | 是 | 64 位小写 hex，同 `--admin-token-stdin` 的 token（上表「启动」行）；会话内可被 `set_admin_token` 替换（§2.10.6） |
| `discovery_path` | string | 否 | 绝对路径，≤ 1024 字节，不含 NUL，规则见 §2.7.3；缺省则不写发现文件 |
| `register_socket` | string | 否 | 绝对路径，≤ 103 字节（macOS `sun_path` 为 104 字节含 NUL），不含 NUL。**出现则 `discovery_path` 必填**，否则 agent24d 拿不到 `instance_nonce`；缺省则不登记 |
| `orphan_timeout_s` | integer | 否 | 0 到 86400；0 或缺省 = 不因「无登记会话」退出（§2.10.8）。**非 0 则 `register_socket` 必填**，否则永远登记不上，到时间就会自行退出。Agent24 取 900 |
| `instance_nonce` | string | 否 | 64 位小写 hex；缺省由 iDoris 生成，生产用法不给这个字段，只给确定性测试用 |
| `engine` | object | 否 | `{"omlx_executable"?: 绝对路径, "stop_on_exit"?: "owned" 或 "never"}`，未知子字段同样拒绝；缺省 `stop_on_exit` 为 `"owned"`（§2.7.6） |

- 解析严格：未知字段一律拒绝。launcher 与 iDoris 是锁版本一起发的，字段拼错而静默失效，比直接失败更危险（这与 §2.10 的 UDS 帧「忽略未知字段」是两种有意的不同）。任何错误都只输出固定文案 `[idoris] 启动失败：launcher 帧无效（<原因码>[：<字段名>]）`，退出码 1，**绝不回显字段值**（不得把解析库的默认错误信息带出，里面会有输入片段）。原因码取值：`frame_too_long`、`frame_not_json`、`unsupported_frame_version`、`field_invalid`、`unknown_field`、`timeout`。
- 已否决的做法：argv 传路径（`ps` 可见，且 `parse_args` 是精确匹配）、环境变量（会被 iDoris 自己的子进程继承）、替换旧 65 字节帧（破坏 ADR-035 与既有测试）。

示例（一行，token 只是占位，行末是一个 LF 字节）：

```
{"v":1,"admin_token":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","discovery_path":"/Users/x/.agent24/run/idoris.json","register_socket":"/Users/x/.agent24/run/idoris-register.sock","orphan_timeout_s":900}
```

#### 2.7.2 动态端口（拟议，待双方确认后冻结）

- launcher 托管路径下，数据面与 Admin 都可以绑定端口 0：`IDORIS_PORT=0` 与 `IDORIS_ADMIN_PORT=0` 被放行（v0.5.0 的解析会拒绝 0）；原有 1 到 65535 的取值和固定端口路径不变。端口 0 由内核分配，iDoris 以绑定之后的 `local_addr()` 为准；两个监听器仍然只绑 loopback。
- 实际端口只经发现文件（§2.7.3）和登记帧（§2.10.3）传出。stdout 不属于契约（上表「就绪」行），动态端口下不得靠它取端口；请求值 `IDORIS_PORT` 是 0，也不是端口。
- 不承诺 `idoris admin <资源> --token-stdin` 在动态端口下自动从发现文件取端口（§2.7.9）。

#### 2.7.3 发现文件（拟议，待双方确认后冻结）

发现文件是 iDoris 就绪后写给 launcher 的一份实例快照，**只是提示，不是事实来源**：同一个数据库只能有一个实例，由实例锁保证（退出码 3），孤儿接管不会因为发现文件而起出第二个实例。

- **路径**：launcher 在帧里给绝对路径（Agent24 约定 `<data_dir>/run/idoris.json`）。iDoris 没有默认路径，帧里没有 `discovery_path` 就不写：iDoris 不知道 Agent24 的数据目录，在自己的状态目录放默认文件会产生第二个事实来源。
- **路径校验**（都在写任何东西之前，失败即退出码 1，此时尚未对外服务）：（1）绝对路径、≤ 1024 字节、不含 NUL；（2）`parent = dirname(path)`，要求 `canonicalize(parent)` 与 `parent` 逐字节相等，即中间不允许有符号链接，launcher 必须传 canonical 路径（macOS 的 `/var` 实为 `/private/var`，传 `/var/folders/…` 会被拒）；（3）以不跟随符号链接的方式打开 `parent`，属主必须等于当前有效 uid，且组与其他人不可写（`mode & 0o022 == 0`，建议 0700）；（4）目标若已存在，必须是属主正确的普通文件（不是符号链接、目录、FIFO、socket），否则拒绝并退出，不删除；（5）iDoris 不创建 `parent`，由 launcher 负责，避免在错误位置建出权限不对的目录。
- **内容**：紧凑的一行 JSON 加 LF，≤ 4096 字节，UTF-8，权限 0600。

```
{"schema":1,"service":"idoris","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","instance_nonce":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","pid":41234,"data_port":51234,"admin_port":51235,"version":"0.6.0","contract_version":"1.3.0","started_at_ms":1763625330123,"launch":{"db_path":"/Users/x/.agent24/idoris/idoris.sqlite3","state_dir":"/Users/x/.agent24/idoris/state","components_dir":"/Users/x/.agent24/components/idoris-config","allocated_budget_bytes":17179869184}}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `schema` | integer | 固定 1 |
| `service` | string | 固定 `"idoris"` |
| `instance_id` | string | UUID v4，36 字符，等于 `/health.instance_id` |
| `instance_nonce` | string | 64 位小写 hex，**秘密**（§2.1） |
| `pid` | integer | iDoris 进程号 |
| `data_port`、`admin_port` | integer | 1 到 65535，取自绑定之后的 `local_addr()`，不是请求值 |
| `version`、`contract_version` | string | 等于 `/health` 同名字段 |
| `started_at_ms` | integer | iDoris 开始 `run()` 时的 Unix 毫秒（墙钟）。时间字段名用 `started_at_ms`，不用 Agent24 方案里的 `started_at`（R12-Q2） |
| `launch` | object，可选 | 启动输入快照：`db_path`（canonical 锁基路径）、`state_dir`、`components_dir`（canonical）、`allocated_budget_bytes`（u64 或 null）。供 agent24d 判断「这个孤儿的启动输入还是不是我要的」：不一致就重启，不接管 |

不含 `pid_start`：iDoris 取自己的内核启动时间需要不安全代码，而 workspace 禁用 unsafe。消费者自己读内核启动时间，与 `started_at_ms` 粗略比对（几秒内）即可；pid 复用由登记（HMAC）与 `/health.instance_id` 兜底。

- **原子写法**：在同目录创建不可预测名字的临时文件 `.idoris-discovery.<pid>.<16 位随机 hex>.tmp`（`O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`，权限 0600，并 `fchmod` 到 0600 以不受 umask 影响），写入并 `fsync`，`rename` 覆盖目标，再尽力 `fsync` 目录；失败路径删临时文件。读者要么看到旧文件，要么看到完整的新文件。不用 `F_FULLFSYNC`：重启后 pid 本来就没了，不需要掉电后文件仍在。
- **何时写**：同时满足（a）已持有实例锁，（b）数据面与 Admin 都已绑定，（c）服务已启动且 `/health` 能答（含 `phase:"starting"`）。登记（§2.10）在写文件之后。launcher 要求了发现文件而写失败，就是启动失败（退出码 1，固定文案），不允许「跑着但找不到」。
- **自愈**：每 10 秒（与是否已登记无关）`lstat` 目标并核对 `instance_id`，缺失或不一致就重写，`rm -rf` 误删可以恢复。
- **何时删**：优雅退出（SIGTERM、SIGINT、孤儿超时、退出码 5 的路径）时，在监听关闭之后、实例锁释放之前，**仅当文件里的 `instance_id` 等于自己**才删；读、比对、删除在同目录旁路锁文件 `<发现文件>.lock` 的独占 flock 下完成。SIGKILL、panic、掉电不删，即「崩溃残留」。
- **残留的识别**（消费者侧，建议顺序）：读文件（≤ 4 KiB、普通文件、属主正确、`schema==1`）；`kill(pid, 0)` 存活、uid 相同、可执行路径等于安装路径；`GET /health`（不带任何凭据，2 秒）返回 `service=="idoris"` 且 `instance_id` 相等；LISTEN 属主核对；最后以登记为准（§2.10.9）。任何一步失败即视为陈旧，pid 已死才可以删文件，删除前同样持旁路锁并复核 `instance_id`。
- **iDoris 自己遇到陈旧文件**：实例锁是唯一事实来源。拿到锁的新实例把已有文件视为陈旧并覆盖；没拿到锁（退出码 3）的实例**不碰**文件，因为它属于还活着的那个实例。

#### 2.7.4 两级就绪（拟议，待双方确认后冻结）

托管 oMLX 冷启动（Python 加 MLX 加载加扫描模型目录）可能超过 launcher 现有的就绪窗口，所以「iDoris 可用」与「模型可用」分开判断（R12-Q4）：

| 级 | 判据 | 用途 | 时限 |
|---|---|---|---|
| iDoris 可用 | 上表「就绪」行的判据（`status=="ok"`、`version` 符合锁定版本、`contract_version` 的 major 为 1、进程仍存活），并且 Admin `GET /admin/api/v1/status` 以 Admin token 能答 200 | Agent24 发布网关槽位（发布之前还须通过 §2.10.9 的核对） | 沿用 launcher 自己的就绪窗口（Agent24 现为 30 秒，属 Agent24 内部参数，不在本契约内冻结），超时按启动失败处理 |
| 模型可用 | `/health.phase=="ready"`，即 `ready==true`（§2.11.1） | Agent24 的 chat 放行 | `starting` 最长到引擎启动超时（默认 90 秒），期间界面显示「模型服务启动中」，**不得**因此把 iDoris 判为失败并重启 |

- `/health.status` 仍恒为 `"ok"`，只表示进程活着并在服务（设计稿 D8）；`phase` 与 `ready` 才表示模型可用。「iDoris 可用」之后、「模型可用」之前，数据面除 `/health` 外的路径返回 503 `idoris_starting`（§2.11.1）。
- Agent24 侧承诺：chat 在 `phase!="ready"` 时有界等待，上限 120 秒，超时向调用方返回 503 加 `retry_after`（这是 Agent24 自己的响应，不是 iDoris 的）。

#### 2.7.5 停止顺序与退出码（拟议，待双方确认后冻结）

**停止**：SIGTERM 与 SIGINT 仍走同一条优雅路径，总预算不变（iDoris 自己约 15 秒，即排空 10 秒加销毁 runtime 5 秒；launcher 17 秒后才 SIGKILL）。收到信号后并行执行：（1）向 agent24d 发 `bye`（至多 0.5 秒，§2.10.8）；（2）停止自己拥有的 oMLX（SIGTERM 整个进程组，8 秒宽限，之后 SIGKILL）；（3）HTTP 排空（沿用现有 10 秒上限）。然后：（4）删发现文件（监听关闭之后、实例锁释放之前，仅当 `instance_id` 匹配，§2.7.3）；（5）销毁 runtime（Drop 会 SIGKILL 剩余进程组，是最后一道兜底）；进程退出时内核释放实例锁。孤儿超时与退出码 5 走同一条路径。`engine.stop_on_exit` 为 `"never"` 时跳过（2），它只对 SIGTERM、SIGINT 的正常退出（退出码 0）有效；退出码 4、5 一律停止自己拥有的引擎。

**退出码**（0、1、3 的含义不变，新增 4、5）：

| 码 | 含义 | launcher 行为 |
|---|---|---|
| 0 | 正常停止（SIGTERM、SIGINT） | 按预期 |
| 1 | 启动失败或停止失败。修订 1.6 增加：launcher 帧无效、发现文件路径校验或写入失败 | 按崩溃处理，退避重启 |
| 3 | 同一数据库已有实例在运行 | 去接管，不要重启 |
| 4（新） | 孤儿超时：launcher 给了 `orphan_timeout_s`，超过该时间没有已 `ack` 的登记会话（§2.10.8），主动优雅停止 | 按预期停止，**不计入**崩溃熔断（R12-Q5） |
| 5（新） | iDoris 自己拉起（`owned`）的引擎失效，v0.6.0 无法在进程内恢复，优雅停止以让上层重启（§2.7.6） | **计入**崩溃熔断（R12-Q5），重启后走全新的启动门控 |

#### 2.7.6 oMLX 生命周期（拟议，待双方确认后冻结）

**范围与已定决策。** 「引擎」指 oMLX 服务进程，「模型」指引擎里的权重。现有 Supervisor 管模型（load、unload、chat），新增的引擎管理器在它之下，只管进程；`OmlxAdapter` 继续用 HTTP 与引擎对话，不变。owner 已定：（设计稿 D2a）adopt 优先，找不到才拉起，拉起的实例是 iDoris 私有的；（设计稿 D2b）v0.6.0 的引擎由用户自装，iDoris 探测并报 `engine_not_found`，由 Agent24 界面引导安装，附带或按需下载 oMLX 以后再议。**只对声明了 `extensions.idoris_managed` 的卡生效**（§2.3）；没有托管卡时完全走 v0.5.0 的现有路径，行为不变。

**启动决策。** 对每张托管卡，在创建 Supervisor 之前依次：

1. 读所有权记录（见下）。这一步必须先于探测：`kill -9` 之后遗留的自有引擎仍在监听，先探测会把它当成外部引擎，或者因为它的私有 key 而得到 401。记录存在且进程身份全部吻合，就接管自己遗留的引擎（`owned=true`，沿用记录里的 key，进入第 2 步用该 key 确认就绪）；记录存在但进程不吻合，丢弃记录；吻合但存在 `load.pending` 围栏时按下文处理。
2. 探测卡的 `endpoint`：`GET /v1/models`，key 按 §2.9「oMLX 密钥」的顺序解析（接管了遗留引擎时用记录里的 key）；判据同 `setup omlx`，另要求 `/v1/models/status` 与 `/api/status` 也是 oMLX 的形状。200 且形状是 oMLX：就绪；接管的遗留引擎 `owned=true`，其余是外部引擎，`owned=false`，**iDoris 永远不给外部引擎发信号，退出时也不停它**。401/403 且没有可用 key：`unavailable`，`reason:"auth_required"`（或 `auth_failed`），**不再拉起第二个**。连接被拒：进入第 3 步。其他，即端口被非 oMLX 的服务占用（含形状不符）：`failed`，`reason:"port_in_use"`。
3. 发现并校验可执行文件（见下）。找不到：该上游 `absent`，`reason:"engine_not_found"`，iDoris 本身继续服务，`/health` 给出原因，由 Agent24 界面引导用户安装。
4. 拉起（见下）并等就绪：探测同第 2 步，默认 90 秒超时，轮询间隔从 250 毫秒退避到 1 秒。

**可执行文件的发现与校验。** 发现顺序：（1）stdin 帧的 `engine.omlx_executable`（launcher 已校验，视为可信）；（2）`/Applications/oMLX.app/Contents/MacOS/omlx-cli`；（3）`~/Applications/oMLX.app/Contents/MacOS/omlx-cli`。**不使用 PATH 查找**：Agent24 启动 iDoris 时把 PATH 固定为 `/usr/bin:/bin:/usr/sbin:/sbin`，Homebrew 路径本来就不在里面；`~/.omlx/bin/omlx`、`/opt/homebrew/bin/omlx`、`/usr/local/bin/omlx` 只是指向 app 内 `omlx-cli` 的包装或链接，不作为发现目标。校验：`canonicalize` 之后必须是普通可执行文件；位于 `.app` 内时用 `/usr/bin/codesign --verify --strict` 并要求 `TeamIdentifier` 为 `PSK5Q5T46L`；launcher 显式给的路径不再校验团队。以上路径与团队标识来自设计稿附录 A 在 oMLX 0.6.4 上的只读实测，其他版本未实测，oMLX 换签名团队会使校验失败。模型目录：`~/Library/Application Support/oMLX/base-path` 存在且可读时，其内容作为 `OMLX_BASE_PATH` 传给引擎，否则不传，引擎用默认的 `~/.omlx/models`。

**启动命令**（flags 已对照 oMLX 0.6.4 的 `omlx serve --help`，其他版本未实测）：

| 项 | 取值 |
|---|---|
| 可执行文件与参数 | 上面解析出的 `omlx-cli`；`serve --host 127.0.0.1 --port <卡 endpoint 的端口>`；设置了 `IDORIS_ALLOCATED_BUDGET_BYTES` 且换算后（十进制 GB，向下取整）≥ 1 时再加 `--memory-guard-gb <整数>` |
| 环境 | `env_clear`，只放 `PATH=/usr/bin:/bin:/usr/sbin:/sbin`、`HOME`、`TMPDIR`、`LANG`、`LC_ALL`、`LC_CTYPE`、`TZ`、`OMLX_API_KEY`（§2.7.7）、`OMLX_BASE_PATH`（有才放）。iDoris 自己的 `IDORIS_*` 一个都不传给引擎 |
| 工作目录与 stdio | 工作目录为状态目录；stdin 为 null；stdout 与 stderr 追加到 `<IDORIS_STATE_DIR>/logs/omlx.log`（超过 10 MiB 滚动，保留 3 份） |
| 进程组 | 独立进程组；停止时 SIGTERM 整个组，8 秒宽限后 SIGKILL |

`--memory-guard-gb` 让 oMLX 自己的内存上限与 iDoris 的额度账本一致，这是 iDoris 托管比外部引擎更稳的一个具体理由；`omlx serve --help` 没写单位，取十进制整数 GB 向下取整，偏保守。额度为 0 时不拉起（`reason:"budget_zero"`），因为什么都装不下。不使用 `--api-key` 命令行参数（argv 同 UID 可见）。

**所有权记录与孤儿引擎。** iDoris 被 SIGKILL 时来不及停引擎，引擎成为孤儿，继续占着端口和内存，所以拉起成功后写所有权记录 `<IDORIS_STATE_DIR>/omlx-owned/<endpoint 编码>.json`（文件 0600、目录 0700，原子写法同 §2.7.3）：

```json
{"schema":1,"pid":51234,"port":19731,"started_at":"Fri Nov 20 15:14:05 2026","executable":"/Applications/oMLX.app/Contents/MacOS/omlx-cli","api_key":"<引擎私有 key>","launched_by_instance":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11"}
```

`started_at` 是 `ps -o lstart= -p <pid>` 的原文（用 `ps` 子进程取是实现细节，契约只要求下面的四项身份校验）。新 iDoris 启动时：pid、启动时间、命令、监听端口四项全部吻合才算同一个进程，就接管，沿用记录里的 key，不重启引擎，已加载的模型继续在内存里；读不出来不算不吻合（不发信号，记录保留）；记录存在但进程不吻合（pid 被复用或已死）则丢弃记录。吻合但存在 `load.pending` 围栏时，引擎可能正处于加载中：**对自己拥有的引擎，杀掉整个进程组、确认已消失、清围栏、重新拉起**。这是托管比外部引擎多出来的一项能力，也是 Agent24 的「`kill -9` iDoris 后自动恢复」在加载进行中被杀时能过的前提；外部引擎的围栏保持原样，仍需人工处理，因为没有证据证明加载已经停止。`engine.stop_on_exit` 为 `"never"` 时，正常退出也保留引擎和记录，由下一个 iDoris 接管（适合 iDoris 升级重启时不想重新加载大模型）；默认 `"owned"`，退出时停掉自己拉起的引擎，避免孤儿占内存（`"never"` 与退出码 4、5 的关系见 §2.7.5）。

**状态机、退避与就绪探测。** 状态即 `/health.upstreams[].state`（§2.11.1）：`starting` 到 `ready`；`ready` 探测失败或进程退出到 `restarting`（退避 1、2、4 秒起翻倍，封顶 30 秒，±20% 抖动；只适用于自有引擎，外部引擎探测失败只报 `unavailable`，iDoris 不重启它）；5 分钟内失败超过 5 次到 `failed`（`reason:"crash_loop"`），之后每 60 秒半开重试一次，不永久放弃（与 Agent24 方案 §3.1 同一套参数，两层口径一致）；Admin 显式停止到 `stopped`；找不到引擎到 `absent`。就绪探测：`GET /v1/models` 返回 200 且 JSON 里有 `data` 数组即可，空列表也算（模型目录可能是空的）；oMLX 没有 `/health`。

**与 Supervisor、围栏、`/health` 的关系。** Supervisor 在创建时只对引擎采样一次，失败就把启动错误粘滞住，之后 list、status、chat、load、unload 一直返回该错误，必须重启 iDoris（v0.5.0 的行为，所以现在要求「先起 oMLX 再起 iDoris」）。因此托管时必须「引擎就绪之后才创建 Supervisor」，而引擎可能要 90 秒才好，期间 iDoris 要能答 `/health`。做法是可替换的服务体：数据面与 Admin 两个监听器照旧最先绑定，各自服务一个可原子替换的 Router。（1）启动期服务「starting 体」：`/health` 返回 `phase:"starting"`，Admin `status` 可用，数据面其余路径 503 `idoris_starting` 并带 `Retry-After`；（2）引擎管理器按上面的启动决策把引擎带到就绪；（3）用真实的服务状态构建完整 Router 并替换，此时 Supervisor 面对的是一个已就绪的引擎，不会再锁住启动错误。没有托管卡时跳过（1）到（3）。托管引擎一直没有就绪（`absent`、`failed`、`unavailable`）时替换不会发生，数据面保持（1）的 503，`/health` 用 `phase`（`starting` 或 `degraded`）与 `upstreams[].reason` 说明原因；引擎后来就绪（用户装好 oMLX 后 `start`，或半开重试成功）再替换。**待定**：门控未成功期间，同一进程里其他卡（例如非托管的 resident 卡）的可用性，设计稿未写，S5 实现前由 iDoris 补充并回填本节。

**v0.6.0 与 v0.6.1 的边界。** v0.6.0 只做启动门控加拉起、停止、重启、孤儿接管（设计稿 3a）。运行期引擎换代（崩溃重启、外部引擎被用户重启）之后，Supervisor 的账本已过期，它以为 `Ready` 的模型其实已不在引擎里；热恢复（重建加替换，含围栏代际，设计稿 3b）后移 v0.6.1。v0.6.0 的兜底（设计稿 D4 的 B）：iDoris 自己拥有的引擎失效且无法在进程内恢复时，iDoris 优雅停止并以退出码 5 退出，由 agent24d 重启，重启后走全新的启动门控。外部引擎被用户重启这件事 v0.5.0 就存在，本修订不引入新问题，只在这里写明。退出码 5 的精确触发条件（进程一退出即退，还是 `restarting` 退避若干次之后才退）由实现定，发版时回填本节，launcher 不需要区分。

**停止、重启与启动**（经 Admin，§2.11.4）。`stop`：向自己拥有的引擎进程组发 SIGTERM，8 秒宽限后 SIGKILL，状态 `stopped`；对 `owned=false` 的上游一律拒绝（409 `upstream_not_owned`），iDoris 不去停别人的进程。`restart`：`stop` 之后重新走启动决策，对 `owned=false` 的上游同样拒绝。`start`：从 `stopped`、`failed`、`absent` 或 `unavailable` 重新走启动决策，已经 `ready` 时幂等。

#### 2.7.7 引擎密钥（拟议，待双方确认后冻结；设计稿 D10 待实测，结论回填）

- 拟议做法：iDoris 自己拥有的引擎，每次拉起时由 CSPRNG 生成一个随机 key，经环境变量 `OMLX_API_KEY` 交给引擎子进程（不用 `--api-key`，argv 同 UID 可见），并记入 0600 的所有权记录（§2.7.6）。别的进程不带这个 key 访问该引擎得到 401，「所有模型调用只经 iDoris」才成为技术约束，而不只是约定。iDoris 内部 oMLX key 的解析仍是单一入口：私有引擎的 key 优先，其次才是 §2.9「oMLX 密钥」的顺序；外部（adopt）引擎不适用，仍按 §2.9 的顺序。该 key 只在 iDoris 与该引擎之间，不是 virtual key，也不会交给 Agent24。
- **待实测，结论待回填**：环境变量 `OMLX_API_KEY` 与用户 `~/.omlx/settings.json` 里的 `auth.api_key` 同时存在时 oMLX 认哪个，尚未验证（设计稿起草时没有起引擎做实测）。验证办法：在一个空闲端口临时起 `omlx serve`，设环境变量，另用一份 settings.json，分别带两个 key 请求 `/v1/models`，看谁被接受，不加载任何模型。通过则启用上述做法；`settings.json` 优先则私有 key 不生效，退回现有 key 顺序，失去隔离，其余设计不变，并把本节改成「不隔离」。回填格式：结论（环境变量优先或 `settings.json` 优先）、oMLX 版本、日期。
- 结论：**待实测**。

#### 2.7.8 `setup omlx` 的扩展（拟议，待双方确认后冻结）

解除 Agent24 connect 流程对「oMLX 必须在线」的依赖，并让 Agent24 里不再出现任何 oMLX 地址（R12-Q7）。上表「组件配置」行的命令形状保持有效，新增：

```
idoris setup omlx --out-dir <dir> [--endpoint <url> | --auto] [--managed] [--managed-port <n>]
                  [--bind role=model]… [--load-mode on_demand|resident] [--api-key-stdin] [--force] [--no-probe]
```

- `--auto`：只读探测候选端点 `127.0.0.1` 的 8088、8000、8001（Agent24 现在在自己代码里探测的同一组，迁入 iDoris 之后 Agent24 才能真正删掉所有 oMLX 地址；多个同时可达时按这个顺序取第一个形状是 oMLX 的）。找到则等同 `--endpoint <找到的>`（adopt，卡不带 `idoris_managed`）；没找到则必须同时带 `--managed` 才继续，否则报 `unreachable`（保持现状）。
- `--managed`：卡带 `extensions.idoris_managed`，端点取 `--managed-port`；缺省时在 19000 到 19999 里挑一个此刻空闲的端口写进卡（私有端口，不占 oMLX 官方默认的 8000，避免用户之后启动 oMLX.app 时端口冲突）。`--managed` 与 `--auto` 联用且找到了外部 oMLX 时，本修订取保守的一边：adopt 外部引擎且不托管（待确认）。
- `--no-probe`（`--managed` 隐含）：不连引擎。`--bind role=model` 改为对照 `~/.omlx/models` 目录校验（`idoris models local` 已能离线读取），所以 `--bind embed=bge-m3-mlx-fp16` 在引擎没起时也能写。
- 新增错误码，现有 11 个不变：`engine_not_installed`（`--managed` 时找不到引擎可执行文件）、`port_in_use`（`--managed-port` 指定的端口已被占用）、`model_not_installed`（离线绑定时 `~/.omlx/models` 里没有该模型）。
- 输出 JSON 增加 `mode`（`"adopt"` 或 `"managed"`）与 `engine`（`{installed, executable, version?}`）；伴生 resident 卡 `omlx-embed.yaml` 随 `omlx.yaml` 一并写入，并列入 `--force` 的整体替换范围（§2.3）。
- Agent24 侧承诺（R12-Q7）：删除 `omlx_probe`，候选与配置全部改为调用 `idoris setup omlx --auto`（或 iDoris 另行提供的候选接口）；之后 Agent24 源码不再出现任何 oMLX 地址。

#### 2.7.9 布局与不承诺（拟议，待双方确认后冻结）

- 布局：随包仍恰好 5 个文件（上表「布局」行不变），Agent24 的锁文件里逐个列出的 5 个文件不变；伴生卡 `omlx-embed.yaml` 等都由 `setup` 生成在 `--out-dir`，不是随包文件。
- 不承诺（在上表「不承诺」行之外增加）：iDoris 下载或安装 oMLX 引擎（v0.6.0 由用户自装；附带、按需下载以后再议）；运行期引擎换代后的热恢复（v0.6.1，v0.6.0 以退出码 5 兜底）；`setup upstream`（自定义上游的生成器）、Admin 的模型 load/unload（#611）、§2.9 的下载（后移 v0.6.1）；动态端口下 `idoris admin <资源> --token-stdin` 自动从发现文件取端口；embeddings 并入 Supervisor 的内存账本（§2.11.3，设计稿 E2，以后再做）。

### 2.8 wire 契约 1.1.0（iDoris v0.4.0）

只有新增，1.0.x 的调用方不受影响。版本兼容规则：minor 版本只增加内容，不改变已有语义；破坏性变更会升 major。

- `GET /capabilities` 的条目可能带三个新字段：`degraded_alternative`（只出现在 `requires_eviction` 条目上，`{id, capability, estimated_memory_gb, quant, reason}` 或 `null`）、`degraded_alternatives`（只出现在 `blocked` 条目上，是数组）、`headroom_breach_option`（`{id, quant, estimated_memory_gb, ctx_limit}`，整份快照最多一个条目带这个字段，条目 id 唯一）。`reason` 是稳定的代码，例如 `fits_without_eviction`。解析时看哪个条目带了 `headroom_breach_option`，不要靠 `resident:true` 来定位。
- 用户选「清理后用最优模型」或「降级」时，直接请求对应的模型 id 即可，不需要新接口。
- **1GB 余量**：设置了 `IDORIS_ALLOCATED_BUDGET_BYTES` 时，生命周期加载默认最多用到额度减 1GB。`POST /v1/chat/completions` 带上 `X-iDoris-Headroom-Breach: confirmed`（只接受这一个值），表示用户已确认，这一次加载可以用满全部额度。确认只对单次请求有效，不会被记住。它是 Agent24 问过用户之后代为发出的约定，不是权限边界。
- 没带确认、而这次加载只有用掉余量才放得下时，返回 HTTP 503，`error.type` 与 `error.reason_code` 都是 `headroom_confirmation_required`，此时应询问用户。
- 生命周期繁忙时（包括另一个请求正在以不同的确认状态加载同一个模型），返回 HTTP 503，`error.type` 是 `local_only_unavailable`，`error.reason_code` 是 `supervisor_busy`。必须按 `reason_code` 区分，有限退避后用同一个请求头重试即可，不要询问用户。
- 带了确认也放不下（超过整个额度）时，`error.type` 是 `local_only_unavailable`，`reason_code` 不是 `headroom_confirmation_required`，为终态，不应弹出确认。其它 503 按原规则处理。
- `remediation` 是给人看的说明文字，不属于契约，不得用于匹配。

**修订 1.6 增补（拟议，待双方确认后冻结）：预热与余量确认。** `POST /v1/models/warmup`（§2.11.2）复用本节的约定：同一个请求头 `X-iDoris-Headroom-Breach: confirmed`，同一套 503 `reason_code`（`headroom_confirmation_required`、`supervisor_busy`，其余 `local_only_unavailable` 为终态）；确认仍只对单次请求有效，也仍是 Agent24 问过用户之后代为发出的约定，不是权限边界。预热是用户在设置里选大模型时最自然的触发点，所以 Agent24 已有的确认交互（ID-HR，R10）原样适用，仍按 `type` 与 `reason_code` 判定，不匹配文字。预热属于应用自己的流程，走数据面加数据 key；`unload`、固定（pin）、带策略的显式 load 属于运维操作，归 Admin（§2.11.4），两者不重叠。

### 2.9 本机 chat 模型推荐、本地模型类别与下载（wire 1.2.0，2026-11-15 双方确认；iDoris 侧推荐、列表与密钥顺序已在 v0.5.0 实现；下载待实现）

只有新增，1.1.x 的调用方不受影响。本节落地前，§2.3「下载/预热的功能归属在 iDoris，但归属不表示已有下载服务」仍然成立。

**分工**：iDoris 负责「这台机器的 chat 用哪个模型」——档位表、量化选择、是否已安装、内存是否放得下、下载与加载。Agent24 负责首次启动时调用、把用户的选择保存为默认 chat 模型、Settings 的模型选择器，以及 iDoris 未安装时回落到 `Qwen3-8B-4bit`。

**档位表（owner 2026-11-15 拍板，按物理内存）**

档位按 `host.ram_bytes`（物理内存字节数）落在哪个左闭右开区间判断，单位是 GiB（1 GiB = 1073741824 字节；Apple 标称的「16GB」就是 16 GiB）。模型体积与内存估算（`size_bytes`、`est_memory_bytes`）用十进制字节。每台机器恰好落在一行：

| `ram_bytes` 区间 | 档位推荐（oMLX 目录名） | `alternatives` | 档位 `reason` |
|---|---|---|---|
| [0, 16 GiB) | `Qwen3-8B-4bit`，**仅当它放得下**；放不下则 `model_id: null` | 无 | `tier_below_16gib` |
| [16 GiB, 48 GiB) | `Qwen3-8B-4bit` | 无 | `tier_16_48gib` |
| [48 GiB, 64 GiB) | `Qwen3.8-27B-OptiQ-4bit` | `Qwen3-8B-4bit` | `tier_48_64gib` |
| [64 GiB, ∞) | `Qwen3.8-27B-OptiQ-4bit` | `Qwen3.6-35B-A3B-OptiQ-4bit`、`Qwen3-8B-4bit` | `tier_64gib_plus` |

（owner 原话「≤8GB 不推荐本地 chat；16/24/32GB → 8B；48GB+ → 27B；64GB+ 可选 35B-A3B」。8 GiB 机器在第一行按放不下返回 `null`；18 / 36 GiB 等落在第二行。）

档位按真实物理 RAM 判断（与 §2.4 的 min_ram 门一致）。「放得下」（`fits`）另按 iDoris 的内存公式判断：设置了 `IDORIS_ALLOCATED_BUDGET_BYTES` 时按该额度，否则按 iDoris 自探的可用 GPU 内存。

**回落规则**：档位推荐的模型放不下时，按 `Qwen3.6-35B-A3B-OptiQ-4bit` → `Qwen3.8-27B-OptiQ-4bit` → `Qwen3-8B-4bit` 的顺序，从档位推荐的下一个起找第一个放得下的作为 `model_id`，此时 `reason` 为 `fallback_does_not_fit`；一个都放不下时 `model_id: null`，`reason` 为 `insufficient_memory`。`alternatives` 只列档位表里该行的备选，每项各自带 `fits`。

内存依据（实测，2026-11-15）：`Qwen3.8-27B-OptiQ-4bit` 权重 20.68 GB，加 8k 上下文 KV（全注意力上界约 2.1 GB）与约 1 GB 开销，约 23.8 GB；24 GiB 机器可用约 15.8–16.8 GB，32 GiB 约 21.1–22.4 GB，均放不下。

**`GET /v1/recommendation?role=chat`**（只读，需要数据 key，与 `/v1/models` 同级鉴权）

- 查询参数：`role`，本版只支持 `chat`，其他值返回 400，`error.reason_code` 为 `unsupported_role`。
- 响应 200：
  ```json
  {
    "role": "chat",
    "model_id": "Qwen3-8B-4bit",
    "installed": true,
    "size_bytes": 4620000000,
    "est_memory_bytes": 6100000000,
    "fits": true,
    "reason": "tier_16_48gib",
    "host": {"ram_bytes": 17179869184, "allocated_budget_bytes": null},
    "alternatives": [
      {"model_id": "…", "installed": false, "size_bytes": null, "est_memory_bytes": null, "fits": true, "reason": "…"}
    ]
  }
  ```
- `model_id` 是 oMLX 目录名（即 `/v1/chat/completions` 的 `model` 应填的值），可为 `null`。`installed` 表示 `~/.omlx/models/<model_id>` 已存在且完整。`size_bytes` 为磁盘/下载体积（未安装时为预计下载体积），`est_memory_bytes` 为加载后的内存估算；两者未知时都为 `null`（不用 0），`alternatives` 里的同名字段相同。`reason` 只取上面档位表与回落规则里的代码（`tier_below_16gib`、`tier_16_48gib`、`tier_48_64gib`、`tier_64gib_plus`、`fallback_does_not_fit`、`insufficient_memory`），供程序判断；给人看的说明另放 `remediation`，不属于契约。
- 本接口不加载、不下载、不改任何状态。Agent24 保存用户的选择，iDoris 不记忆。

**`idoris recommend --role chat --json`**（CLI，同一响应形状）

- 只需要 iDoris 可执行文件：不开数据库、不取实例锁、不启动 runtime、不需要 serve 在跑，也不需要完整 setup。自探硬件，读取 `~/.omlx/models` 判断是否已安装；不连接 oMLX，因此不读取任何 oMLX 密钥（下面「oMLX 密钥」一节只适用于 iDoris 连接 oMLX 的场景）；可用 `--allocated-budget-bytes <u64>` 覆盖额度。
- 退出码：0 成功（含 `model_id: null`）；1 失败（stderr 一行说明）。

**`GET /v1/models/local`**（只读，需要数据 key）

- 列出 `~/.omlx/models` 下的模型：`{"models": [{"omlx_id": "Qwen3-8B-4bit", "kind": "chat", "size_bytes": 4620000000, "catalog_id": "qwen3-8b"}]}`。
- `kind` 取值：`chat`、`vision`、`embedding`、`rerank`、`image`、`ocr`、`tts`、`unknown`。来源优先级：iDoris catalog（`config/catalog.yaml`，新增 `omlx_id` 与 `kind`）→ 按模型目录的 `config.json`（`architectures` / `model_type` 等）推断 → 推断不了为 `unknown`。`catalog_id` 不在 catalog 时为 `null`。
- Agent24 的 chat 选择器只列 `kind` 为 `chat` 或 `vision` 的条目；`unknown` 不列入。
- 同形状 CLI：`idoris models local --json`，运行条件与 `idoris recommend` 相同（只读文件系统，不连接 oMLX、不读密钥）。HTTP 版同样只读 `~/.omlx/models`，不为列表去调用 oMLX。

**下载（归 iDoris）**

- iDoris 负责下载与校验，拒绝下载内存放不下的模型（`fits: false`）。大文件下载必须由用户在 Agent24 界面确认后才开始，iDoris 不自动下载。
- 接口形状（下载进度、取消、错误码）在 iDoris 实现时另行修订本节，双方确认后再冻结；本版只冻结归属与「必须用户确认」这一条。Agent24 负责向用户说明体积、征得同意、显示进度。

**oMLX 密钥**

- 优先级：`IDORIS_OMLX_API_KEY`（§2.7，保持不变）→ `~/.omlx/settings.json` 的 `auth.api_key` → 无鉴权连接。
- `settings.json` 的 key 只发给 loopback 的 oMLX（base URL 非 loopback 时视为没有）；文件必须是常规文件（符号链接不算），key 取 trim 后的值，含内部空白或控制字符视为没有。环境变量的 key 维持原行为。
- 只从文件读取，不打印、不写日志、不进 argv 或响应体。不硬编码任何默认值（Agent24 侧同样不再假定默认 key）。
- oMLX 开了鉴权而两处都没有 key 时，iDoris 按 401 处理并在 `remediation` 说明，不重试。

**Agent24 的调用顺序**：iDoris serve 在跑且有数据 key → 走上面的 HTTP 接口；否则本机有 iDoris 可执行文件 → 走 CLI（`idoris recommend` / `idoris models local --json`）；都没有 → 回落 `Qwen3-8B-4bit`，选择器按 oMLX `/v1/models` 列出（不按 `kind` 过滤；Agent24 内部按名称启发式排除图像 / OCR / 语音类，属 Agent24 内部行为，不进本契约）。

**Agent24 侧的 oMLX 密钥**：与 iDoris 同一顺序的后两步——`~/.omlx/settings.json` 的 `auth.api_key` → 无鉴权；不再假定任何默认 key。

**不承诺**：`chat` 以外角色的推荐（embedding / vision / image 以后另行修订）；自动升级已安装的模型；Linux / x86。

**修订 1.6 增补（拟议，待双方确认后冻结）**：Agent24 在 R12-Q7 中承诺删除 `omlx_probe`，候选与配置全部改为调用 `idoris setup omlx --auto`（§2.7.8），之后不再直连 oMLX。迁移完成后，上文「Agent24 的调用顺序」末句里「都没有 → 回落 `Qwen3-8B-4bit`，选择器按 oMLX `/v1/models` 列出」的直连回落，以及「Agent24 侧的 oMLX 密钥」一段，都只对迁移前的 Agent24 版本有效；迁移后 iDoris 不可用时 Agent24 的降级行为由 Agent24 自己定义，不属于本契约。下载部分的归属与状态不变。

### 2.10 登记协议 v1（修订 1.6，拟议，待双方确认后冻结）

本节所有条款（含全部子节）拟议，待双方确认后冻结。

iDoris 是客户端，在 launcher 给的 Unix domain socket 上向 agent24d 登记，之后定期发心跳。它让 agent24d 知道「这个端口上跑的是哪个实例、是否健康」，也让 iDoris 知道有没有 launcher 还在。帧风格与 Agent24 已有的 `enroll.sock` 一致（一行一帧、单帧 ≤ 16 KiB、服务端先发 challenge、逐帧复核 `LOCAL_PEERTOKEN`），agent24d 可以复用同一套帧读取与复核代码。字节级测试向量见 §2.10.5。

**活性只看 `/health`，心跳只用于发现连接断开并重新握手。** 登记连接静默、心跳停止，都不是判卡死的依据：被 `kill -STOP` 的进程与「agent24d 自己卡住」无法区分。卡死判定与重启只由 agent24d 对 `/health` 的探测决定（探测间隔与超时是 Agent24 内部参数，不在本契约内冻结）。登记失败也永远不使 iDoris 退出，唯一例外是 launcher 显式给了非 0 的 `orphan_timeout_s`（§2.10.8）。

#### 2.10.1 传输与帧

- 传输：AF_UNIX、SOCK_STREAM，路径来自 launcher 帧的 `register_socket`（§2.7.1）。同一时刻 iDoris 只持有一个连接。
- 帧：一行一个 UTF-8 JSON 对象，以 LF 结尾，**每帧（含 LF）≤ 16384 字节**，不含 CR 和 BOM。超长、非 UTF-8、非对象、空行一律按 `bad_frame` 处理并关闭连接。接收方忽略未知字段（前向兼容），必填字段严格校验，`v` 必须等于 1。本协议最大的帧也只有约 350 字节。
- 选 NDJSON 而不是长度前缀：与 `enroll.sock` 一致，全是文本，用 `socat` 可以直接调试，没有二进制载荷。
- 两条与 `LOCAL_PEERTOKEN` 语义有关的义务：iDoris 在收到 `challenge` 之前不写任何字节（提前到达的字节可能被 agent24d 按 `identity_rejected` 拒绝并关闭连接）；UDS 的 fd 带 close-on-exec，不得被任何子进程（含 oMLX 引擎）继承。

#### 2.10.2 时序与计时

agent24d 先说话，iDoris 收到 `challenge` 之前不写任何字节：

```
iDoris                                   agent24d
  | -------- connect(register_socket) ----> |   (2 秒超时)
  | <-------- challenge ------------------- |   (5 秒内必须到)
  | --------- register -------------------> |   (收到 challenge 后 1 秒内发出)
  | <-------- ack ------------------------- |   (5 秒内)
  | --------- heartbeat seq=1 -------------> |   (ack 后立即发，之后每 heartbeat_ms)
  | <-------- heartbeat_ack seq=1 ---------- |   (5 秒内)
  | ...                                      |
  | --------- bye (优雅停止时) -------------> |
```

| 项 | 值 |
|---|---|
| 连接超时 | 2 秒 |
| 等 `challenge` | 5 秒 |
| 收到 `challenge` 后发出 `register` | 1 秒内 |
| 等 `ack` | 5 秒 |
| 心跳间隔 | `ack.heartbeat_ms`，缺省 10 秒，±10% 抖动 |
| 等 `heartbeat_ack` | 5 秒 |

#### 2.10.3 消息与字段

| `op` | 方向 | 何时 | 必填字段（除 `v`、`op`） |
|---|---|---|---|
| `challenge` | agent24d → iDoris | 连接建立后，agent24d 先发 | `conn_id`、`nonce` |
| `register` | iDoris → agent24d | 收到 `challenge` 后 1 秒内 | `nonce`、`instance_id`、`pid`、`version`、`contract_version`、`data_port`、`admin_port`、`started_at_ms`、`phase`、`resume`、`proof` |
| `ack` | agent24d → iDoris | 校验通过后 5 秒内 | `session_id`、`proof`；`heartbeat_ms` 可选 |
| `heartbeat` | iDoris → agent24d | `ack` 之后立即发，之后每 `heartbeat_ms` | `seq`、`phase`、`ready` |
| `heartbeat_ack` | agent24d → iDoris | 收到 `heartbeat` 后 5 秒内 | `seq` |
| `set_admin_token` | agent24d → iDoris | 仅在 `ack.proof` 已通过的会话上（§2.10.6） | `token` |
| `admin_token_ack` | iDoris → agent24d | iDoris 接受并替换 token 之后 | 无 |
| `bye` | iDoris → agent24d | 优雅停止时，尽力发送，至多 0.5 秒（§2.10.8） | `reason` |
| `error` | 双向 | 之后关闭连接 | `code`、`message` |

`challenge`：`conn_id` 为 16 个小写 hex，`nonce` 为 32 个小写 hex（128 位，同 `enroll.sock` 的 challenge）。

```
{"v":1,"op":"challenge","conn_id":"0123456789abcdef","nonce":"00112233445566778899aabbccddeeff"}
```

`register`：

```
{"v":1,"op":"register","nonce":"00112233445566778899aabbccddeeff","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","pid":41234,"version":"0.6.0","contract_version":"1.3.0","data_port":51234,"admin_port":51235,"started_at_ms":1763625330123,"phase":"ready","resume":false,"proof":"386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884"}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `nonce` | string | 原样回显 `challenge` 的 `nonce`；缺失或不符 = `identity_rejected` |
| `instance_id`、`pid`、`version`、`contract_version`、`data_port`、`admin_port`、`started_at_ms` | 同发现文件 | 服务端应与发现文件交叉核对；`pid` 只供日志与交叉核对，**不作身份依据** |
| `phase` | string | `starting`、`ready` 或 `degraded`，同 `/health.phase` |
| `resume` | boolean | 本进程此前已登记过（这是重连）则为 true，仅供日志与统计 |
| `proof` | string | 64 位小写 hex，见 §2.10.5 |

`ack`：

```
{"v":1,"op":"ack","session_id":"a1b2c3d4e5f60718293a4b5c6d7e8f90","heartbeat_ms":10000,"proof":"099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967"}
```

`session_id` 为 32 个小写 hex，是会话标识，不是凭据。`heartbeat_ms` 是心跳间隔，缺省 10000，iDoris 夹到 2000 到 60000。`proof` 是**服务端对 iDoris 的证明**（双向握手，§2.10.5）：读得到发现文件才算得出来。iDoris 校验失败就关闭连接，不发心跳，按退避重连（§2.10.7），仍继续服务。

`heartbeat` 与 `heartbeat_ack`：`seq` 是 u64，从 1 起每次加 1，`heartbeat_ack` 必须回显同一个 `seq`；`phase`、`ready` 同 `/health`，只是信息，不是活性依据。

```
{"v":1,"op":"heartbeat","seq":1,"phase":"ready","ready":true}
{"v":1,"op":"heartbeat_ack","seq":1}
```

`bye` 与 `error`（`message` ≤ 200 字符，不含秘密）：

```
{"v":1,"op":"bye","reason":"shutdown"}
{"v":1,"op":"error","code":"identity_rejected","message":"…"}
```

#### 2.10.4 版本协商与错误码

- **版本协商**：`v` 必须是 1；服务端不支持就回 `unsupported_version` 并关闭。以后升 `v` 属于破坏性变更，必须走本规范的修订。`contract_version` 是 HTTP wire 版本，服务端按它的锁（major 为 1，加 Agent24 要求的最低 minor）判断兼容，不兼容回 `contract_incompatible`。
- **会话规则**：iDoris 只处理 `challenge`、`ack`、`heartbeat_ack`、`error`、`set_admin_token` 五种服务端帧，其他 `op` 与未知字段一律忽略；服务端正常关闭连接（EOF）时 iDoris 立即重连（§2.10.7）。
- **错误码**（`error.code`）：

| `code` | 谁发 | 含义 | iDoris 收到后 |
|---|---|---|---|
| `identity_rejected` | agent24d | 登记方身份校验未过（`LOCAL_PEERTOKEN`、codesign，或 `register.nonce` 缺失、不符，或提前写了字节） | 关闭连接，60 秒后重连 |
| `proof_invalid` | 双向 | agent24d 发：`register.proof` 不符；iDoris 发：`ack.proof` 不符 | 收到时关闭连接，30 秒后重连；iDoris 自己发出后关闭连接，按默认退避重连 |
| `unsupported_version` | agent24d | 不支持 `v` | 关闭连接，60 秒后重连 |
| `contract_incompatible` | agent24d | `contract_version` 的 major 不是 1，或低于 Agent24 要求的最低 minor | 关闭连接，60 秒后重连 |
| `bad_frame` | 双向 | 帧不合规 | 关闭连接，按默认退避重连 |
| `timeout` | agent24d | 等待 iDoris 的帧超时 | 关闭连接，按默认退避重连 |
| `busy` | agent24d | 并发已满 | 关闭连接，5 秒后重连 |
| `instance_conflict` | agent24d | 实例冲突（确切语义待 Agent24 补充，见 R12 待确认项） | 关闭连接，按默认退避重连 |
| `internal` | 双向 | 内部错误 | 关闭连接，按默认退避重连 |

iDoris 自己发 `error` 只用 `bad_frame`（服务端帧不合规）、`proof_invalid`（`ack.proof` 校验失败）、`internal`，而且只在已经收到 `challenge` 之后才发，尽力发送、不等应答，之后关闭连接。等 `challenge`、`ack`、`heartbeat_ack` 超时时 iDoris 不发 `error`，直接关闭连接：收到 `challenge` 之前它不写任何字节。agent24d 收到任何 `error` 都应结束该会话。

#### 2.10.5 `proof`：不让秘密上线（HMAC-SHA256）与测试向量

`register.proof = HMAC-SHA256(key, transcript)`。`key` 是 `instance_nonce` 解码后的 **32 个原始字节**，不是 64 个 hex 字符的文本。`transcript` 是下列各项按顺序拼接的 ASCII 字符串，项与项之间只有一个 LF（0x0A）、**没有空格**、末尾没有 LF，数字用十进制、无前导零：`idoris-register/v1`（字面量）、`conn_id`、`challenge_nonce`、`instance_id`、`pid`、`data_port`、`admin_port`、`started_at_ms`。

`ack.proof = HMAC-SHA256(key, transcript)`，同一个 `key`，`transcript` 的各项为：`idoris-ack/v1`（字面量）、`conn_id`、`challenge_nonce`、`session_id`，同样以单个 LF 分隔、末尾没有 LF。

- `conn_id` 与 `challenge_nonce` 每个连接都新，所以不能重放；端口与 pid 进了 transcript，中途不能篡改；服务端用常量时间比较。冒充的服务端只能看到公开信息（`instance_id`、端口、pid），拿不到 `instance_nonce`。
- 双向：`register.proof` 是 iDoris 对服务端的证明（我就是写了这份发现文件的实例），`ack.proof` 是服务端对 iDoris 的证明（我读得到发现文件）。
- 边界：`instance_nonce` 在 0600 文件里，挡不住同 UID 的恶意进程；它挡的是（1）连错实例、读到陈旧文件，（2）读不到该目录的沙箱进程（例如 Agent24 的模块沙箱）冒充。真正的身份闸门是 Agent24 对登记方的 `LOCAL_PEERTOKEN` 与 codesign 校验（§2.10.9），两者叠加。

**测试向量**（两端实现都必须得到同样的值；下面 transcript 里的 `\n` 表示 LF 一个字节，不是反斜杠加 n）。输入均为 ASCII：

```
instance_nonce  = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f   (hex 解码后 32 字节作为 HMAC key)
conn_id         = 0123456789abcdef
challenge nonce = 00112233445566778899aabbccddeeff
instance_id     = 3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11
pid=41234  data_port=51234  admin_port=51235  started_at_ms=1763625330123
session_id      = a1b2c3d4e5f60718293a4b5c6d7e8f90
```

`register` 的 transcript（137 字节，末尾无 LF）：

```
idoris-register/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\n3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\n41234\n51234\n51235\n1763625330123
```

`proof` = `386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884`

`ack` 的 transcript（96 字节，末尾无 LF）：

```
idoris-ack/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\na1b2c3d4e5f60718293a4b5c6d7e8f90
```

`proof` = `099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967`

可用 Python `hmac.new(bytes.fromhex(nonce), transcript, hashlib.sha256).hexdigest()` 复算；起草本节时已用这段代码核对上面两个值。

#### 2.10.6 会话内轮换 Admin token：`set_admin_token`

问题：接管孤儿 iDoris 的新 agent24d 没有旧的 Admin token。办法是 agent24d 生成新 token，经登记会话设给 iDoris，旧 token 立即失效，再由 agent24d 自己使用。

```
{"v":1,"op":"set_admin_token","token":"<64 位小写 hex>"}
{"v":1,"op":"admin_token_ack"}
```

- 只在 `ack.proof` 已通过的会话上接受；在 `ack` 之前或 `ack.proof` 未通过的连接上收到 `set_admin_token`，一律不处理并关闭连接。
- `token` 的校验同 launcher 的 token（64 位小写 hex）；每 10 秒至多接受一次。校验失败或超频：iDoris 不替换，token 保持原值，先尽力发 `error`（`code` 取 `bad_frame`）再关闭该会话。
- 替换是进程内存里的原子替换：之后任何带旧 token 的 Admin 请求得到 401，已经在处理的请求不受影响。
- 这是 §2.1 里 Admin token「只经两条一次性匿名 pipe/FD」之外唯一新增的传递通道。ADR-035 的约束不变：token 不进 argv、环境变量、文件、日志、`error.message`。
- Agent24 侧：接管后新 agent24d 生成新 token，用本帧设给 iDoris；token 之后只在 agent24d 内部使用，桌面端的 Admin 操作都经 agent24d 的 `/api/v1/idoris/*`，Electron main 不持有 token（R12-Q3）。

#### 2.10.7 失联判定、重连与退避

| 项 | 值 |
|---|---|
| iDoris 判失联 | 连续 3 次心跳未应答（约 30 秒），或任何读写错误，或 EOF |
| 服务端判失联（建议） | 3 倍 `heartbeat_ms`（缺省 30 秒）没有收到任何帧 |
| 重连退避 | 第 1 次立即；之后 0.25、0.5、1、2、4、8 秒，再之后固定 10 秒；每次 ±20% 抖动；会话持续满 60 秒后清零 |
| 特殊原因的固定等待 | `identity_rejected`、`unsupported_version`、`contract_incompatible`：60 秒；`proof_invalid`：30 秒；`busy`：5 秒 |
| 日志 | 只记状态变化，以及首次失败和之后每第 10 次失败，不刷屏（Agent24 会把子进程输出管道读走，别写满） |

- 退避上限取 10 秒而不是常见的 30 秒：本机 UDS 连接几乎没有成本，不存在惊群，而 agent24d 被 launchd 重启之后希望尽快重新握手，延迟上限就是退避上限。
- **agent24d 拒绝或不在时，iDoris 继续服务，永不因为登记失败而退出**：`/health.registered` 为 false，数据面与 Admin 不受影响，按上表退避重试；agent24d 被重启之后，iDoris 重连、重新握手，`resume` 为 true。

#### 2.10.8 孤儿策略与 `bye`

- **孤儿计时**：launcher 在帧里给了非 0 的 `orphan_timeout_s` 才启用（Agent24 取 900，R12-Q5）。从「没有已 `ack` 的登记会话」开始计时（包含进程启动后一直没登记上的情形），任何一次成功的 `ack` 把计时清零。超过 `orphan_timeout_s` 就走优雅停机（§2.7.5），退出码 4，日志写 `idoris: 无启动器登记超过 N 秒，已停止`。这条补上的缺口是：agent24d 被永久卸载或停掉之后，孤儿 iDoris 连同它托管的 oMLX 和已加载的模型会一直占着内存。手工运行（帧里没给）不受影响。
- **`bye`**：优雅停止（SIGTERM、SIGINT、孤儿超时、退出码 5 的路径）时，iDoris 在已有的登记会话上尽力发 `bye`，至多 0.5 秒，不等应答，让 agent24d 区分「预期停止」与「崩溃」，对应 Agent24 方案里卡死与崩溃分开记录的要求。`reason` 本版只定义 `shutdown`，具体退出原因以退出码为准（§2.7.5）。

#### 2.10.9 登记方校验与核对顺序（Agent24 承诺，R12-Q1，2026-11-20）

登记方不看 pid 数字。agent24d 的核对顺序如下，都是 Agent24 侧的承诺：

1. **登记方身份**：用 `LOCAL_PEERTOKEN`（每帧复核）取得对端，校验 codesign：identifier `ai.idoris.cli`，Team `HYAP444FN9`。这与 iDoris 发布 CLI 的签名一致（hardened runtime）；iDoris 保持该标识符稳定，改动须同步 Agent24 的允许清单，也就须走本规范的修订。iDoris 建议的等价要求串：`identifier "ai.idoris.cli" and anchor apple generic and certificate leaf[subject.OU] = "HYAP444FN9"`，并建议再按安装路径核对可执行文件。
2. **端口属主**：经 `LOCAL_PEERTOKEN` 校验过的那个进程，确实持有 `data_port` 与 `admin_port` 的 LISTEN。
3. **实例标识**：`GET /health`（**不带任何凭据**）返回的 `instance_id` 等于登记的 `instance_id`。
4. **最后才发数据 key**：以上全部通过之后，才把数据 key 发给该数据面地址并发布网关槽位。`/health` 免鉴权，带上 key 反而可能把 key 发给占着端口的冒充者，所以 `/health` 请求永不带 key。
5. **开发构建**：只有 agent24d 本身是非打包的开发构建，并且显式设置了 `AGENT24_DEV_ALLOW_UNSIGNED_IDORIS=1`，才接受未签名的登记方；打包版一律拒绝。iDoris 不提供任何绕过。

另有一点：新 agent24d 没有旧 nonce，`instance_nonce` 从发现文件读，不经 stdin 传递（nonce 由 iDoris 生成，设计稿 D1）。

### 2.11 wire 契约 1.3.0（修订 1.6，拟议，待双方确认后冻结）

本节所有条款（含全部子节）拟议，待双方确认后冻结。只有新增，1.2.x 的调用方不受影响；版本兼容规则同 §2.8：minor 版本只增加内容，不改变已有语义。`CONTRACT_VERSION` 在 iDoris v0.6.0 发版时升到 `1.3.0`（Rust、TS 与 conformance 三处一致，`pnpm check:contract-drift` 校验）；发版之前 `/health.contract_version` 仍是 `1.2.0`，调用方不得据本节假定新增字段已经存在。

#### 2.11.1 `/health` 新字段

保持不变（Agent24 的就绪校验依赖它们）：`status`（恒为 `"ok"`，语义仍是「进程活着并在服务」）、`service`、`version`、`contract_version`、`instance_id`、`components`、`dev_no_key_enabled`。新增：

```json
{"status":"ok","service":"idoris","version":"0.6.0","contract_version":"1.3.0","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","components":2,"dev_no_key_enabled":false,
 "phase":"ready","ready":true,"started_at_ms":1763625330123,"registered":true,
 "upstreams":[{"id":"omlx","state":"ready","managed":true,"owned":true},
              {"id":"omlx-embed","state":"ready","managed":false,"owned":false}]}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `phase` | string | `starting`：已绑定、已在答 `/health`，但启动门控未完成（正在探测或拉起 oMLX）；`ready`：至少一个 upstream 为 `ready`；`degraded`：进程正常，但没有 upstream 为 `ready` |
| `ready` | boolean | 等于 `phase=="ready"` |
| `started_at_ms` | integer | 同发现文件（§2.7.3） |
| `registered` | boolean | 当前是否有已 `ack` 的登记会话；没有 launcher 时恒为 false |
| `upstreams` | array | 上游列表：Supervisor 管理的卡与 resident 的 HTTP 卡。订阅中转这类进程卡是否列入待定，S1 实现前由 iDoris 确认；`components` 仍计所有卡 |
| `upstreams[].id` | string | 卡的 `provider.id` |
| `upstreams[].state` | string | `ready`、`starting`、`unavailable`、`restarting`、`failed`、`stopped`、`absent`；状态机见 §2.7.6 |
| `upstreams[].reason` | string，可选 | 稳定原因码：`engine_not_found`、`port_in_use`、`auth_required`、`auth_failed`、`startup_timeout`、`crash_loop`、`fence_pending`、`budget_zero`、`startup_latched`。给人看的文字另放，不属于契约 |
| `upstreams[].managed` | boolean | 卡声明了由 iDoris 管理引擎（`extensions.idoris_managed`，§2.3） |
| `upstreams[].owned` | boolean | 引擎进程是 iDoris 自己拉起的；外部引擎、被接管的外部引擎为 false，iDoris 永远不会给它发信号 |

- 示例里的伴生卡 `omlx-embed` 与 `omlx` 共用同一个 endpoint，但不带 `idoris_managed`，所以 `managed` 与 `owned` 都是 false，状态由探测器给出；引擎的启停只经托管卡 `omlx`，对 `omlx-embed` 的 `stop`、`restart` 会被拒绝（§2.11.4）。
- **`/health` 必须 O(1)，不做任何上游 I/O，响应 ≤ 200 毫秒。** 状态来自后台探测器与引擎管理器写入的快照。Agent24 每 5 秒探测、2 秒超时、连续 3 次超时就判卡死并 SIGTERM（Agent24 方案 §3.2，Agent24 内部参数），一个会被慢上游拖住的 `/health` 会造成误杀与重启风暴。
- 由 Supervisor 管理的卡，状态取自 Supervisor，带 200 毫秒超时；Supervisor 返回启动错误时报 `unavailable` 加 `reason:"startup_latched"`，**不能**因为 `/v1/models` 能通就报 `ready`（聊天实际是 503）。resident 卡由探测器周期探测：`GET /v1/models`，key 按 §2.9「oMLX 密钥」的顺序解析，间隔 5 秒、2 秒超时；401/403 报 `auth_required` 或 `auth_failed`。
- 调用方按 `state` 判可用性，按 `reason` 判原因，不要假定二者一一对应；`reason` 出现时 `state` 不是 `ready`。已定的配对：`engine_not_found` 对应 `absent`，`startup_latched` 与 `auth_required`、`auth_failed` 对应 `unavailable`，`crash_loop`、`port_in_use` 对应 `failed`；其余原因码的配对以实现为准。
- **启动期行为**（§2.7.6）：`phase:"starting"` 期间，数据面除 `/health` 外的路径返回 503，`error.type` 与 `error.reason_code` 都是 `idoris_starting`（写法同 §2.8 的 `headroom_confirmation_required`），带 `Retry-After`；Admin 的 `GET /admin/api/v1/status` 照常可用。

#### 2.11.2 `POST /v1/models/warmup`

数据面、数据 key，不是 Admin：

```
POST /v1/models/warmup
Authorization: Bearer idk_…
X-iDoris-Privacy: local_only            (缺省 local_only)
X-iDoris-Request-Id: <可选>
X-iDoris-Headroom-Breach: confirmed     (可选，只接受这一个值，语义同 §2.8)
Content-Type: application/json

{"model":"idoris/daily","timeout_ms":120000}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `model` | 是 | 角色别名（`idoris/<role>`）或具体模型 id，选择与角色绑定逻辑同聊天 |
| `timeout_ms` | 否 | 1000 到 300000，缺省 120000；未知字段一律 400 `unsupported_field`（G-6「不静默」） |

- 鉴权与 key 范围同聊天：聊天在该模型上会被拒的 key，预热也被拒。
- 选卡同聊天。**生命周期卡（oMLX）**：复用聊天路径里的加载前置（取 catalog、加载前占用检查、带余量确认的 load），加载完成即返回，**不生成任何 token**。**resident 卡**：没有生命周期可管，按能力发一个最小的真实请求（chat 用 `max_tokens:1`，embedding 用输入 `"warmup"`），受 `timeout_ms` 约束。
- 200 响应带 `X-iDoris-Record-Id` 与 `X-iDoris-Served-Locality`：

```json
{"model":"<解析后的具体 id>","provider_id":"omlx","state":"ready","already_loaded":false,"load_ms":4210}
```

- 错误沿用聊天的信封与代码（§2.5、§2.8）：400 `invalid_json`、`unsupported_field`；401；403；404 `model_not_found`；503 `headroom_confirmation_required`（界面询问用户，确认后带 `X-iDoris-Headroom-Breach: confirmed` 重发）；503 `local_only_unavailable` 且 `reason_code:"supervisor_busy"`（有限退避后重试）；503 `local_only_unavailable` 的其他原因（终态，例如超过整个额度）；**新增** 504 `warmup_timeout`（超过 `timeout_ms`，加载可能仍在引擎里继续，不等于失败；`error.type` 与 `error.reason_code` 都是 `warmup_timeout`，写法同 §2.8）。
- 副作用：不进用量与预算记录（本地免费）；配置了事件存储时写一条 `warmup` 审计事件；幂等；同一模型的并发预热由 Supervisor 现有的「同 id、同策略」合并处理。
- **实现前提**（不是 wire 字段）：把 oMLX 的 load 调用超时从一般管理调用超时里分离出来。现在 load 与其他管理调用共用 `call_timeout`（默认 10 秒），Supervisor 对 load 另包一层 30 秒；超时按「结果未知」处理，并留下 `load.pending` 围栏。需要新增独立的 `load_timeout`，缺省 180 秒、可配置，只作用于 load 这一次调用，其他管理调用不变；否则冷加载大模型会被当作结果未知，首次启动预热反而让 iDoris 不可用。Qwen3-8B 冷加载实测 3.8 秒（`spike/u0/U0-LOG.md`），§2.9 档位表在 48 GiB 以上推荐的 27B 冷加载时间未实测。

#### 2.11.3 `/v1/embeddings`：`idoris/embed` 与 key 范围

路由、鉴权、透传和 `local_only` 语义 v0.5.0 就已存在，缺的是默认安装下 oMLX 卡走不通：`/v1/embeddings` 只在 resident 卡里选候选，而默认 oMLX 卡是 `on_demand`，候选为空，返回 503。v0.6.0 用 §2.3 的伴生 resident 卡解决，行为约定如下：

- **请求写 `model:"idoris/embed"`，数据 key 的 `allowed_roles` 含 `embed`。** Agent24 的本地数据 key 角色为 `daily`、`deep`、`embed`（R12-Q6，最小权限，不发全角色 key）。写具体模型名（如 Agent24 现在的 `bge-m3-mlx-fp16`）会被当作无角色请求，无角色请求要求 key 具备全部角色，否则 403 `role_forbidden`。
- `local_only`：缺省即 `local_only`；只有可信本地卡（loopback、`local_only`、egress 只有 none 或 loopback）有资格；响应带 `X-iDoris-Served-Locality: loopback`，Agent24 的 embedding 客户端要求这个值。远程或局域网卡必须 `X-iDoris-Privacy: any` 且 key 允许 `any`；付费卡直接 503 `paid_proxy_unavailable`。多张 embedding 卡走现有的确定性选择管线，不做负载均衡。
- 已知限制（要写进发行说明）：（a）resident 路径绕过 Supervisor，embedding 模型的内存不在 iDoris 账本里，由 oMLX 自己的内存上限兜底（托管引擎有 `--memory-guard-gb`，§2.7.6）；（b）冷加载 embedding 模型可能超过代理的 10 秒 header 超时，所以首次启用向量召回前应先调预热（§2.11.2），iDoris 另拟把 embeddings 路径的上游响应头超时放宽到 30 秒（实现参数，不是 wire 契约）；（c）Agent24 一批最多 64 条，响应远小于代理的 8 MiB 上限。
- 以后再做：给 Supervisor 加 embed 命令（带在途计数、驱逐感知、容量记账），账本最准确但改动大，v0.7 以后（设计稿 E2）。
- **v0.5.0 的过渡写法**（不属于 wire 1.3.0，现在就能用）：`setup omlx --load-mode resident --bind embed=bge-m3-mlx-fp16`，2026-11-20 实测返回 200；代价是整张 oMLX 卡变成 resident，chat 失去生命周期内存管理。v0.6.0 起改用伴生 resident 卡，chat 保持 `on_demand`。

#### 2.11.4 Admin：`upstreams` 读与 `start|stop|restart` 写

Admin 从「只有只读接口」改为「有受限写接口」（§2.1 已标注被取代的句子，owner 已定）。路径前缀与既有五个资源相同，鉴权同 Admin token：

| 方法与路径 | 说明 |
|---|---|
| `GET /admin/api/v1/upstreams` | 只读；响应 `{"upstreams":[…]}`，元素字段同 `/health.upstreams[]`（§2.11.1） |
| `POST /admin/api/v1/upstreams/{id}/start` | 从 `stopped`、`failed`、`absent` 或 `unavailable` 重新走启动决策（§2.7.6） |
| `POST /admin/api/v1/upstreams/{id}/stop` | 停止 iDoris 自己拥有的引擎，状态 `stopped` |
| `POST /admin/api/v1/upstreams/{id}/restart` | 先 `stop`，再重新走启动决策 |

**Admin 写接口通则**（#611 的模型 load/unload 与 §2.9 的下载另行修订时沿用，本版不冻结它们的路径与字段）：

- 方法 POST，不需要请求体；路径形如 `/admin/api/v1/<资源>/{id}/<动作>`；鉴权同其余 Admin 路由（Admin token）。
- 幂等：目标状态已达成时，重复请求仍返回 202 与当前状态。
- 返回 **202** 加该资源的当前状态对象（字段同对应的 GET）；动作可能异步完成，调用方轮询 `GET` 或看 `/health` 到终态。
- 只作用于 iDoris 自己拥有或托管的对象：`owned` 为 false 的上游，`stop` 与 `restart` 一律 409 `upstream_not_owned`，`managed` 为 false 的上游，`start` 同样 409 `upstream_not_owned`；iDoris 不给别人的进程发信号。`id` 不存在则 404 `upstream_not_found`。各错误体字段以实现为准（§2.5）。

#### 2.11.5 工件与验收

本节没有 JSON Schema（§4.2）：`/health` 新字段的形状由 conformance 用例锁定，warmup 的状态码矩阵和 Admin `upstreams` 由各自的集成测试覆盖，都在实现片里补，不在本修订里创建。发版时 `CONTRACT_VERSION` 升到 `1.3.0`。

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
- **修订 1.6 增补（拟议，待双方确认后冻结）**：本修订新增的 launcher 帧、发现文件、登记协议帧、`/health` 新字段、`POST /v1/models/warmup` 和 Admin `upstreams` 都没有 JSON Schema，以文字规范为准，登记协议另附测试向量（§2.10.5）；`/health` 仍无 schema，新增字段的形状只靠 conformance 用例锁定。机器真源仍只限于已有 schema 覆盖的 wire 结构。

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

见 [R9 协商提案](negotiation/R9-v1.2-协商提案.md)。launcher 宿主（OD-1：agent24d）和同意后安装执行方式（OD-2：由用户在桌面端同意）已由修订 1.3 决定，契约见 §2.7，以 §2.7 为准（**OD-2 被修订 1.6 取代**：owner 2026-11-20 定 iDoris 为 Agent24 必选核心，随 Agent24 安装包自动安装，不再单独征求用户同意；模型、引擎等大文件下载仍须用户确认，不受影响）；fast/daily 标签重标仍 **OWNER REVIEW PENDING**，未经决定不新增角色标签或 M5 consent 机制。数据面可以独立于 launcher 推进；完整 readiness 仍需组合与干净环境验收。

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
| R9 | 2026-10-15 | iDoris | [文档修订 1.2 整合与三个 owner 决策](negotiation/R9-v1.2-协商提案.md)；local_only v0.3.1 最小数据面与 launcher 解耦 | OD-1/OD-2 已由修订 1.3 决定（见 §2.7）；fast/daily 标签重标仍 **OWNER REVIEW PENDING** |
| R10 | 2026-10-28 | jason / 双方 | 文档修订 1.3：owner 要求 Agent24 桌面端一站式「下载 → 运行 → 连接」，由此定下 OD-1（宿主为 agent24d）、OD-2（用户在桌面端同意）；新增 §2.7 launcher 托管契约（iDoris v0.4.0 起）、§2.8 wire 契约 1.1.0 | launcher 设计已合入 [Agent24#1437](https://github.com/iDoris-ai/Agent24/pull/1437)；按 v0.4.0 的实现与验收是 Agent24 的 IL-01 起各项，尚未完成。其中「额度紧时 503 `headroom_confirmation_required` → 用户确认 → 重发」的确认交互登记为 ID-HR，见 [Agent24#1442](https://github.com/iDoris-ai/Agent24/pull/1442) |
| R11 | 2026-11-15 | jason / 双方 | 文档修订 1.4：新增 §2.9 本机 chat 模型推荐（`GET /v1/recommendation?role=chat`、`idoris recommend --role chat --json`）、本地模型类别（`GET /v1/models/local`、`kind`）、下载归 iDoris 且须用户确认、oMLX 密钥读取顺序；档位 16/24/32GB → Qwen3-8B-4bit，48GB+ → Qwen3.8-27B-OptiQ-4bit，64GB+ 可选 Qwen3.6-35B-A3B-OptiQ-4bit | iDoris 与 Agent24 会话各确认一次（2026-11-15）；iDoris 侧推荐、列表与密钥顺序已在 v0.5.0 实现（#604、#605，文档修订 1.5）；下载未实现；Agent24 先做不依赖 iDoris 的部分 |
| R12 | 2026-11-20 | iDoris / Agent24 | 文档修订 1.6（拟议，wire 1.3.0）：IDORIS-CORE（[Agent24#1937](https://github.com/iDoris-ai/Agent24/pull/1937)）。iDoris 提案见设计稿（[PR #614](https://github.com/iDoris-ai/iDoris/pull/614)）；新增 §2.10 登记协议 v1、§2.11 wire 契约 1.3.0，增补 §2.1/2.2/2.3/2.5/2.7/2.8/2.9/4.2。Agent24 2026-11-20 对设计稿 §3.3 七个问题的答复：<br>• Q1：登记方允许清单含 identifier `ai.idoris.cli`（Team `HYAP444FN9`）；只有 agent24d 本身是非打包的开发构建、且显式设置 `AGENT24_DEV_ALLOW_UNSIGNED_IDORIS=1` 时才接受未签名登记方，打包版一律拒绝。<br>• Q2：发现文件的时间字段用 `started_at_ms`（毫秒整数），不用 Agent24 方案里的 `started_at`。<br>• Q3：Electron main 不持有 Admin token，桌面端所有 Admin 操作走 agent24d 的 `/api/v1/idoris/*`，`set_admin_token` 轮换只是 agent24d 内部的事。<br>• Q4：两级就绪：`status=="ok"` 且 Admin 可答为 iDoris 可用（Agent24 发布网关槽位），`phase=="ready"` 为模型可用，`starting` 最长 90 秒；Agent24 的 chat 在 `phase!="ready"` 时有界等待，上限 120 秒，超时 503 加 `retry_after`。<br>• Q5：退出码 4（孤儿超时）不计入 Agent24 崩溃熔断，5（自有引擎无法恢复）计入。<br>• Q6：embeddings 请求发 `model:"idoris/embed"`，数据 key 的角色为 `daily`、`deep`、`embed`（最小权限，不发全角色 key）；v0.5.0 的过渡写法见 §2.11.3，v0.6.0 用伴生 resident 卡，chat 保持 `on_demand`。<br>• Q7：Agent24 删除 `omlx_probe`，候选与配置全部改为调用 `idoris setup omlx --auto`（或 iDoris 提供的候选接口），之后 Agent24 源码不再出现任何 oMLX 地址。<br>另已商定：nonce 由 iDoris 生成，写进 0600 发现文件，登记用 HMAC 证明，nonce 不上线；`/health` 只返回公开的 `instance_id`；Agent24 的核对顺序是先 `LOCAL_PEERTOKEN` 与端口属主，再 `/health.instance_id`，最后才发数据 key，`/health` 不带 key；孤儿超时 `orphan_timeout_s` 由 launcher 给（Agent24 取 900）；Admin 新增受限写接口 `upstreams/{id}/start\ owner 2026-11-20：OD-2（安装 iDoris 须用户在桌面端同意）被取代，iDoris 随 Agent24 自动安装、不弹同意。 |stop\|restart`；引擎在 v0.6.0 由用户自装，iDoris 报 `engine_not_found` | **拟议，待 Agent24 评审**。上列 Q1–Q7 是 Agent24 已给出的答复，规范文字本身评审后才冻结；冻结前不改变任何已冻结条款，也不是 iDoris 已实现的功能。Agent24 方案里的「§2.7 修订 1.4」编号改记为修订 1.6 |

**R12 待确认项（拟议，请 Agent24 评审时回答）**：

1. `phase=="degraded"` 且所有上游的 `reason` 都是终态原因（`engine_not_found`、`auth_required`、`auth_failed`、`budget_zero`）时，chat 是否仍等满 120 秒？iDoris 建议只在 `starting`，或存在 `restarting` 的上游时才等待（§2.7.4）。
2. Q7 的「候选接口」：`idoris setup omlx --auto` 只按 8088、8000、8001 的顺序取第一个可达的 oMLX（§2.7.8）。桌面端若需要列出全部候选供用户选择，请给出字段需求，在 iDoris 实现 `--auto` 之前补进 §2.7.8。
3. `setup omlx --managed` 的可执行文件发现只看默认位置；而 `serve` 另有帧字段 `engine.omlx_executable`。Agent24 若要指定位置（例如将来随包附带 oMLX.app），`setup omlx` 是否也要接受一个对应的 `--omlx-executable <path>`？
4. 登记错误码 `instance_conflict` 的确切语义（设计稿只列了码，§2.10.4），请 Agent24 补充；发现文件残留的删除是否同样持旁路锁并复核 `instance_id`（§2.7.3，建议如此）。

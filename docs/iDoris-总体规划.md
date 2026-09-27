# iDoris 总体规划（最终版 v1.0）

> **状态**：待拍板（2026-09-27）。拍板后作为**唯一现行规划**，其余规划/设计文档标记为「已废弃，保留作决策追溯」（清单见 §11）。
> **维护者**：iDoris.ai / @jhfnetboy
> **证据附录**：[`research/blog-调研记录-2026-09-27.md`](research/blog-调研记录-2026-09-27.md)（blog MCP 11 层 129 个关键词、106 篇精读逐篇笔记 + 全网核实）。本文每个决策后以 `〔来源〕` 标注主要借鉴文章，文章均可在 `https://blog.mushroom.cv/blog/<id>/` 找到。
> **范围**：本阶段只做规划、设计、调研，**不开发**。

---

## 0. 一句话

> **iDoris 是本地优先的「模型与策略层」**：对上游只有一个地址（OpenAI / Anthropic 兼容 + 决策端点 + 反馈端点）；对内管理多个异构模型运行时；每个请求按**隐私 → 预算 → 意图 → 容量**的固定顺序确定性地准入与路由；全程记录可审计的元数据；**客户可选**把请求轨迹沉淀下来，反哺自有小模型。
>
> iDoris 的边界按**权限**划定，不按「智能」划定：它决定「谁、在什么隐私与预算约束下、能用哪些推理资源」，不做 agent 执行、不做长期记忆、不做聊天产品。

---

## 1. 现状（2026-09-26 的架构说明，按要求录入）

### 1.1 交付状态

- `preview` 上 M1/M2/M3 共 **38 个 task 全部合入**（PR #9–#40），7 个包，门禁全绿：lint / typecheck / contract-drift / build / **375 条测试** / smoke。
- `preview → main` 的发布 PR **#41 已开、CI 全绿**，被 main 的 ruleset 挡住：需要 1 个有效批准，且仓库管理员也不在豁免名单里（bypass 列表为空）。
- 仍开着的跟进项：FU-13（没有生产默认端口）、FU-14（personal 模式没有调用方身份）、FU-15（新增有状态组件必须过 tenancy 层）、FU-16（`version_pin: omlx@0.6.4`，但适配器是照 v0.4.3 写的，端点未复测）。

### 1.2 整体架构（现有）

```
  业务 / Agent24 / AgentEar / 微信 ──▶ http://127.0.0.1:PORT/v1  (OpenAI-compat)
                                            │
                                     ┌──────▼──────┐
                                     │ iDoris Router│  隐私门 / 预算闸 / 意图路由 / 审计
                                     └──┬────┬────┬┘
                         ①订阅中转 ─────┘    │    └──── ③本地模型
                         claude/codex CLI    │          oMLX（已实现）· vLLM 槽 · llama.cpp 槽（空）
                         （仅 personal）   ②外部 API（OpenAI-compat 槽位）
```

包的职责：`contracts`（契约 + zod + 组件卡校验器）· `adapters`（引擎无关后端）· `router`（编排）· `tenancy`（deploy_mode、租户硬隔离、预算）· `recommender`（选型）· `growth`（数据湖 + MLX-LoRA）· `federation`（联邦聚合 + DP）。

两条贯穿全局的设计：
- **控制面走 header，不走 prompt**：`X-iDoris-Privacy/Intent/Complexity/Capabilities/Fallback/Tenant/Request-Id`。执行顺序固定为**隐私判定 → 预算闸门 → 意图/能力匹配**。privacy 缺省是 `local_only`；tenant 模式缺 tenant header 直接 400。
- **fail-closed 靠代码保证**：`local_only` 请求在本地跑不了时返回 503，不会降级到外部；降级候选还会二次复核 `privacy_class` 和 `allowed_egress`。

### 1.3 模型怎么管：三层

| 层 | 管什么 | 现在的实现 |
|:---|:---|:---|
| **选哪个** | `config/catalog.yaml` 按架构参数现算 KV，`min_ram_gb` 是硬门槛；角色分 `fast/core/deep/temp`（T4.2 已更名：`core`→`daily`，`temp` 不是角色改用 `load_hint: on_demand`；完整枚举见 docs/interfaces/iDoris-Agent24-边界与接口规范.md §3.3/§3.12） | `recommender` 包 |
| **怎么装卸** | `LoadPolicy{mode: resident\|on_demand\|evict_to_load, keepalive, admission}`，加一把每个后端独立的驱逐互斥锁 | `contracts` + `router/evict-lock` |
| **谁执行** | `ModelBackend` 接口（list/load/unload/admission/status/chat），Router 核心里不出现任何引擎名 | `adapters`：mock + oMLX |

### 1.4 与 oMLX 的关系

**oMLX 是 macOS 上的默认后端，可以替换，不是依赖。**

- 两边通过进程外 HTTP 通信（`127.0.0.1:8088`，D7 定的端口）。
- 按平台探测后端，MLX 只占 macOS 这一格；vLLM 和 llama.cpp 的槽位接口已经齐了，实现还是空的。
- 适配器只做语义映射，比如 `resident → is_pinned`。抽象层的 `Pressure` 类型借用了 oMLX 的 ok/soft/hard/ceiling 分级，这是抽象层对具体引擎依赖最深的一处。
- MLX 还用在 `growth` 包的 LoRA 训练上，那是训练侧的另一件事。

### 1.5 已实现 / 未实现（诚实清单）

| 已实现 | 未实现（本规划要做的） |
|:---|:---|
| 对外 4 个端点：`/health`、`/v1/models`、`/capabilities`、`/v1/chat/completions` | `/v1/messages`、`/v1/embeddings`、`/v1/systemone`、`/v1/feedback`，以及用量/审计查询接口 |
| 控制面 header 解析、声明式 policy、fail-closed、降级链 | **闸一内容级隐私过滤**（`INSPECTED` 阶段，docs/15 已设计，代码为零） |
| 流式、重试、幂等、取消；驱逐锁；出网启动断言 | **闸二凭证代理**（Keychain）；能力②的真实 provider |
| 预算终态拒绝、租户硬隔离、账期时区、审计字段黑名单 | 调用方身份与虚拟 key（FU-14）、生产端口（FU-13） |
| 语义意图兜底（embedding 余弦） | 入口判定层（System-1 决策模型）、决策账本 |
| oMLX 适配器、跨平台探测、推荐器 | 多后端同时编排、全局内存账本、GGUF 后端、模型升级/回滚流程 |
| 数据湖（仅合成数据）、MLX-LoRA、AdapterManifest、联邦骨架 + DP | **轨迹记录**、数据集策展、真实数据的学习闭环 |
| — | **管理界面**（只有 CLI 与配置文件） |

---

## 2. 不变式（统筹后的「不可动摇边界」）

原有的边界全部保留，另外加入这轮调研得出的几条新纪律（★ 为新增）。

1. **顺序即语义**：隐私 → 预算 → 意图 → admission → 降级。顺序反了就是漏洞，必须有测试。
2. **隐私只能收紧，不能放宽**：`local_only` fail-closed。不管是意图推断、模型判定、下层策略还是调用方自述，都只能让隐私要求更严，不能让它更松。★
3. **预算是拒绝不是降级**；★ **价格未知 ≠ 免费**：远程模型价格未知时，要么拒绝，要么按保守上限估算。
4. **租户硬隔离**在数据访问层实现；FU-15：新增有状态组件时必须主动接入 tenancy 层。
5. **审计账本只存元数据**（字段白名单 + 黑名单闸门）。★ 轨迹是另一个模块，客户可选，与审计物理分离（§5）。
6. **策略是数据**，★ 而且所有写入都走「**提议 → diff → 人批准 → 版本化 → 可撤销**」。〔exxperts、Virtual AI Infra Team〕
7. ★ **可信网关 / 不可信执行 / 确定性策略**：凭证和策略留在网关侧；后端和订阅 CLI 都视为不可信执行。〔openclaw-gateway〕
8. ★ **不静默**：不支持的请求参数要么真实兑现，要么显式 400；配置写了但不会生效，就拒绝启动；降级必须在响应头里回传。〔localagi 拆解、Pipelock 实测、Rapid-MLX、Krill〕
9. ★ **日志即运行时**：每个请求一条追加式事件流。审计、用量、UI、轨迹、回放都是这条流的**投影**，不各写一份。〔maka「Log is the Runtime」〕
10. ★ **模型只提议，代码判决**：路由学习、模型升级、adapter 晋升，最终结论都由确定性代码给出；**没有合格候选时保留现状，这算正常结果**。〔virtual-ai-infra-team〕
11. ★ **本地优先不等于只用本地**：按敏感度分路由；远程路径要去关联，不带用户和租户标识出站。〔vitalik-ai-survival-guide〕
12. ★ **授权结构化**：调用方的自我声明永远不能放宽授权；只接白名单 provider，未审核的中转站不得注册。〔pentest-harness 拆穿〕
13. **License 红线**（扩充）：LiteLLM `enterprise/`、Dify 多租户、ComfyUI GPL、★ LobeHub 社区许可、HugAgentOS 的「禁竞争性多租户 SaaS」条款，都不得引入代码。
14. **进程边界即授权边界**；不 vendor 第三方源码；版本一律钉死。

---

## 3. 目标架构

```
┌────────────────────────── 上游消费者（只认一个地址）──────────────────────────┐
│ Agent24 AI Layer · AgentEar 理解层 · 业务模块 · LobeHub/OpenWebUI · 手机(Tailscale) · VM 沙箱内 agent │
└───────────────┬───────────────────────────────────────────────┬──────────────┘
    数据面 :PORT（虚拟 key 鉴权）                     管理面 :ADMIN_PORT（仅 loopback + 会话令牌）
   /v1/chat/completions  /v1/messages               /admin/api/v1/*  ←  Web 控制台 / `idoris` CLI
   /v1/embeddings /v1/rerank  /v1/systemone          （kill switch、策略写入、晋升只在这里）
   /v1/feedback  /v1/models  /capabilities
                │
┌───────────────▼──────────────────────────── iDoris Router ─────────────────────────────┐
│ ① 接入：协议适配 · 字段兑现矩阵 · 虚拟 key → 调用方身份 · trace 关联头                         │
│ ② 入口判定层 INSPECT：L0 确定性规则 → L1 System-1 判定（intent/privacy/complexity，校准概率）  │
│ ③ 策略引擎：隐私 → 预算(reserve) → 意图/能力 → admission → 降级（policy 版本化）                │
│ ④ 调度：会话亲和(保 KV/前缀缓存) · 三粒度断路器(provider/connection/model) · 驱逐锁            │
│ ⑤ 执行：闸二凭证注入点（只见 secret:// 引用）· 出站去关联 · 流式回填 · 预算 settle           │
│ ⑥ Event Log（追加式）──投影──▶ 审计账本 · 用量账本 · 决策账本 · 运维指标(OTel) · 轨迹金库(可选) │
└───────┬──────────────────────────────┬───────────────────────────┬─────────────────────┘
        │ Runtime Supervisor           │ 能力② 外部 API             │ 能力① 订阅 CLI（沙箱，personal）
        ▼                              ▼                             ▼
  oMLX(默认) · mlx_lm.server(保底) · llama.cpp GGUF(长上下文/Windows) · Apple FM(系统小模型) · 嵌入/重排进程
        ▲
        │ adapter 晋升（仅经评测门禁）
┌───────┴──────────────── 学习层（离线、空闲调度）────────────────┐
│ 策展 → 数据集卡(血缘) → 训练(mlx-lm LoRA/OPD/KTO/DPO) → 评测门禁 → 晋升/回滚 │
└──────────────────────────────────────────────────────────────┘
```

**数据面与控制面分离**：推理流量和管理操作走不同端口。数据面端口上根本没有关闭 kill switch 或修改策略的入口。〔Pipelock、wemux〕

---

## 4. 分层详细设计

### 4.1 接入与协议层

| 设计 | 说明 | 来源 |
|:---|:---|:---|
| 协议面 | OpenAI `/v1/chat/completions`（已有），加上 **Anthropic `/v1/messages`**（DeepSeek、Kimi、智谱等国内 Coding Plan 普遍走 Anthropic 协议），以及 `/v1/embeddings`、`/v1/rerank` | proma、osaurus、krill |
| **字段兑现矩阵** | 每个请求字段都要明确：兑现 / 显式拒绝 / 透传。`usage` 必须真实填写，因为预算和轨迹都依赖它 | localagi（7 个字段被静默忽略、usage 恒为 0） |
| 虚拟模型名 | `idoris/auto`、`idoris/fast`、`idoris/private` 作为 header 之外的第二个意图入口，给改不了 header 的客户端用。**模型名只能收紧隐私，不能放宽** | omniroute、clawrouter |
| 响应头 | 已有：`X-iDoris-Provider/Reason`。新增：`X-iDoris-Record-Id`、`X-iDoris-Locality`、`X-iDoris-Route`、`X-iDoris-Degraded`、`X-iDoris-Cost-Minor`（都不含内容） | tare、docs/17 D2 |
| 统一错误体 | `{type, rule_id, reason_code, evidence(不含内容), remediation}`，违规时告诉调用方该怎么改 | hugagentos |
| 真实上下文窗口 | 从后端探测或组件卡声明；超长请求显式返回 413，不让后端静默截断 | exxperts（128000 默认值导致静默截断） |
| 推理内容分离 | `<think>` 内容不进审计；进轨迹时单独放 `reasoning_content` 字段 | rapid-mlx |

### 4.2 身份、租户与授权

| 设计 | 说明 | 来源 |
|:---|:---|:---|
| **虚拟 key = 调用方身份**（解 FU-14） | iDoris 给每个上游（Agent24 的某个模块、AgentEar、手机）签发一把本地 key。policy 可以按 key 区分；key 由 iDoris 签发，不是调用方自称，因此不走 caller header | archestra、onecli |
| 绑定规则 | loopback 默认可不带 key；**非 loopback 绑定且没有 key 时拒绝启动**；Tailscale 只绑 tailnet 接口，不绑 0.0.0.0；拒绝伪造 Host 和代理头（防 DNS rebinding） | krill、openminis 反例、exxperts |
| 层级 | Org(tenant) → Workspace/Project → Key(member/agent)；**下层只能收紧上层策略** | platypus、OpenAI 治理白皮书 |
| Blueprint | 组织级策略模板（路由 + 隐私规则 + 预算），应用到 workspace 时「增量、幂等、快照」 | platypus |
| 临时授权 | policy 绑定可以带到期时间（例如「本周允许某项目用 Opus」），到期自动失效 | authentik |
| 组织认证 | 不自建 IdP，对接 OIDC（authentik/Keycloak/企业 SSO）；iDoris 只做角色映射与 key 签发。角色：member / operator / policy-author / admin | authentik、trinity |
| 跨组件 tenant 凭据（B7） | iDoris 签发短期、签名的能力凭据，Agent24 和 MemPalace 负责验证。第二个组件开始消费 tenant 时启用 | ecosystem-boundaries |

### 4.3 模型管理层

**目标**：一台 Mac 上同时编排多个异构后端，用一本全局内存账本统一记账；升级和切换有证据、可回滚。

| 设计 | 说明 | 来源 |
|:---|:---|:---|
| **Runtime Supervisor** | 从「一个平台一个后端」改为「**一个平台多个后端，按任务分派**」：oMLX 跑大模型、mlx_lm.server 做 Apple 官方保底、**llama.cpp GGUF 跑长上下文和 Windows**（MLX 的 `--kv-bits` 会禁用批处理）、Apple Foundation Models 当系统自带的零预算小脑、嵌入/重排进程单独跑 | M1 Max 64GB 方案（用户本人）、WWDC26、docs/13 §3.3 |
| **全局内存账本** | oMLX 的 `model_memory_max` 只管它自己；Supervisor 汇总所有后端占用，作为 admission 的唯一依据 | M1 Max 64GB 方案 |
| catalog 增维 | 新增 `task`（chat/embed/rerank/asr/ocr/guard/decide）、`format×platform` 可用矩阵、`agent/tool_use` 能力维度、「自报 vs 实测」标记；发布前核对实际文件是否存在 | SIE、QUASAR、spark-x、needle2 |
| 会话亲和 | 同一会话路由到同一个已加载实例，保住 prefix/KV 缓存；隐私脱敏必须确定性（同一实体用同一个占位符），否则会破坏缓存 | LIM、omniroute cache-optimized、tare |
| **内置基准并存库** | tok/s、TTFT、峰值内存、并发曲线，记录均值 ± 95% CI 和测试条件，按硬件 × 量化 × 版本存档，用来校准推荐器 | llm-dock、ferrum |
| **升级/切换流程** | 候选 → 预检 → 基线 → **计划冻结** → 维护窗口 → 实验 → Selector 判决（门槛取最严，**默认保留现状**）→ 同端口晋升 → 在线复验 → 提交或回滚；门禁包含**贪心输出逐 token 比对**和场景评测；全程产出可复算的证据包 | virtual-ai-infra-team |
| 真模型 live 测试层 | 适配器除了 mock 黄金测试，还要有每周或手动触发的真模型一致性测试（oMLX 在场时才跑）。这是 FU-16 的长期解 | krill（日常 CI 绿、真模型测试连续 6 周失败） |
| **配置 bundle 时间旅行** | policy + 组件卡 + catalog + LoadPolicy + 模型 pin + adapter + tenant 策略 + key 表，作为一个版本化 bundle，可以快照、diff、回滚到 known-good | cohesity agent-resilience、exxperts |
| 训练与推理抢内存 | 训练任务也走 admission：只在 pressure=ok 且没有在途请求时启动；推理请求到来时可以暂停训练 | macaron MetaClaw |

### 4.4 路由与意图

**D3 的现状要说清楚**：D3（2026-09-20）定的是「入口路由归 Agent24，iDoris 不做意图推断，header 缺失时用静态缺省」，但 T2.4 已经实现了语义意图兜底。本规划的处理见 §10 P-2。

| 设计 | 说明 | 来源 |
|:---|:---|:---|
| 分清两件事 | **路径路由**（这件事该怎么处理）和**模型选择**（强模型还是弱模型）是两层。iDoris 只管后者和兜底 | semantic-router guide |
| 分阶段的判定器 | ① 规则/显式 header（永远优先）→ ② embedding 相似度（T2.4 已有）→ ③ **本地 logit 读取型 System-1**：复用常驻小模型，一次前缀并行出 intent/privacy/complexity 多个分布 → ④ 用自己的决策轨迹训练专用决策头 | SemIf、agentjev、jev、kev、opensquilla |
| **决策点命名化** | `route.intent`、`route.privacy`、`route.complexity`、`gate.budget`、`gate.egress`…；每个决策点可以单独配置判定器（rules/embed/local-logit/remote），并支持级联（本地先判，不确定再升级） | mu 判断核 |
| 校准与兜底 | 输出校准概率，低于阈值走保守分支；候选里加「都不是/不确定」选项；阈值必须在自有数据上标定 | needle2、nimble（概率 ≠ 正确率） |
| **置换一致性测试** | 候选顺序不应影响结果，作为所有判定器的测试断言 | kev `/permute`、LLM-as-a-Verifier |
| **shadow 模式** | 新判定器先只记录、不生效，用决策账本对比现行判定器，达标后再切换。这也补了 FU-8「测试不承重」的缺口 | mu |
| 复杂度 × 成本档位 | 路由表的数据形状是 `SIMPLE/MEDIUM/COMPLEX/REASONING × ECO/AUTO/PREMIUM` | clawrouter |
| 三粒度弹性 | provider 断路器（HALF-OPEN 探测）/ connection 冷却（遵守 Retry-After）/ **单模型锁定** | omniroute |
| ⚠️ 需核查 | T2.4 用的 embedding 模型和阈值是在什么模型上标定的。中文请求配英文嵌入模型会静默失效；embedding 必须显式指定为多语种/中文模型 | all-MiniLM 拆解 |

### 4.5 隐私过滤（闸一 · 闸二）

docs/15 的设计**保留并作为落地依据**。调研补充如下：

| 设计 | 说明 | 来源 |
|:---|:---|:---|
| `INSPECTED` 阶段 | 插在 `RECEIVED` 之后、读 header 之前；L0 机械规则（中文规则自建，带 Luhn 等校验和防误报）→ L1 System-1 Noul 条件（PII/健康/财务/凭据/商业机密，**只能收紧**）→ L2 策略；动作 BLOCK / REDACT / ALLOW | docs/15、kalm-jev、hindsight（端口号被误判为信用卡） |
| **REDACT 三件套** | 稳定占位符 `[PERSON_1]` + 流式回复在本地实时还原；**脱敏后再扫一遍，仍有遗漏就阻止发送**；UI 显示「云端实际收到的字节」 | osaurus |
| 出站去关联 | 远程请求剥离 tenant/user/key 标识，每个请求用新的上游 request-id；远期把 ZK-API/TEE 作为远程通道选项 | vitalik |
| **kill switch** | 一键「切断一切远程」：只能从管理端口/UI 触发，触发后所有请求强制按 local_only 处理 | pipelock |
| 闸二 | provider 真实 key 存 macOS Keychain（docs/15 §2 与 research-secret-brokering 已定），上游只拿虚拟 key，实际上是占位符 | onecli |
| 边界声明 | Router 只管 **LLM 流量**。上游 agent 自己的其他出网归 Agent24/OS 层（PF 按用户过滤）管，写进边界，不假装全管 | pipelock（macOS 上不走代理的 socket 直连出网） |

### 4.6 预算与计费

| 设计 | 说明 | 来源 |
|:---|:---|:---|
| **reserve → settle 两阶段** | 请求前按估算上限预留，防止并发击穿预算；完成后按实际 usage 结算并退回差额；被拒、失败、缓存命中都不计费 | loopx |
| 价格表 | LiteLLM 价格 JSON（MIT）每日刷新 + 离线快照 + 版本号；**价格未知的远程模型要么拒绝，要么按上限估算** | tokentracker |
| 本地影子价格 | 折旧 + 电费，按实测 TPS 折算；只进报表和组织内部分摊，不拦截请求（避免把本地请求挡掉） | hermes-local-rig-accounting |
| 维度 | tenant × env × provider 的硬上限；provider 级滑动窗口限速/配额 | archestra、pipelock |
| 超额审批队列 | 超预算、提额请求进 Operator Queue 由人审批（staged 动作） | trinity、claude-financial-advisors |
| 自己生成 request_id | 网关计量是权威口径，不存在重复计数；上游 usage 缺失（订阅 CLI）时标 `usage.source=estimated` | tokentracker（reqId 去重超算 1.6–3.7 倍） |

### 4.7 审计与可观测

| 设计 | 说明 | 来源 |
|:---|:---|:---|
| **Event Log 为唯一真相源** | 事件序列：`request.received → inspected → profiled → decided → budget.reserved → dispatched → stream.* → completed → budget.settled → feedback.*`；审计、用量、UI、轨迹都是投影 | maka |
| 审计投影 | 维持现有字段白名单 + 黑名单闸门；append-only，管理员只读 | trinity |
| 结构化 reason | reason 用枚举码 + 参数，不用散文，保证可复现、可统计 | Harness Engineering 论文（自由文本是方差来源） |
| **决策账本** | 每次判定记录 `{decision_point, judge, model_revision, prompt_hash, scores, latency}`，同时是审计证据和训练样本 | SemIf、mu |
| OTel 导出 | 按 GenAI 语义约定导出（`gen_ai.*`，默认不含内容）。**这套约定截至 2026-07 仍是 Development 状态，必须钉死版本** | agentsight、aegra、WEB 核实 |
| **回放** | 用历史事件回放新 policy，看决策差异。路由是确定性的，回放成本低、价值高 | braintrust 六代 eval |
| 变更记录带预测 | 每次 policy/模型变更附一条可证伪的预测（例如「X 类请求本地率上升 Y%」），下个周期自动验证 | AHE |

### 4.8 学习层（自有模型）

**顺序**：先有评测，再训练；先做不需要 GPU 的（规则/Prompt/Skill），再做 LoRA-SFT/偏好，最后做 RL。〔harvey、macaron、reef〕

| 路线 | 说明 | 来源 |
|:---|:---|:---|
| ① **决策模型先行** | 意图/隐私/复杂度判定的奖励就是准确率 + 校准度，最容易闭环；决策账本直接当训练数据 | jev、jevembed（7.9 万样本 30%→84%）、opensquilla 数据飞轮 |
| ② **本地教师蒸馏（OPD）** | 本机的 deep 档（例如 35B-A3B MoE）当老师，fast/daily 档当学生。**老师和数据都不出机器**，是 personal 模式下最干净的自增长路径 | arle（4B 0.518→0.792）、miniCPM5 RL+OPD |
| ③ 编译型学习 | 高频、结构化的请求类型只在「编译期」请教强模型生成标注，运行期全部走本地小 adapter。**请教老师这一步同样过隐私闸** | compile-by-training、castform |
| ④ 反馈偏好 | 👍/👎 → KTO 非配对格式；用户修正 → DPO 配对格式 | TRL（WEB 核实）、立项 §3.4 |
| ⑤ RL | 有稳定场景评测和可计算奖励之后再做；奖励不给中间过程加分，「不知道」优于幻觉，同时计入成本 | grpo 指南、harvey |
| 晋升门禁 | 场景评测（docs/16）+ 贪心逐 token 比对 + **安全/越狱回归**（微调可能削弱对齐）+ 位置偏差测试 | ai-infra-guard、virtual-ai-infra-team |
| 按角色分 adapter | 按角色、按租户训练独立 adapter，不追求一个万能的 iDoris 模型；**租户数据只训租户自己的 adapter** | harvey、areal |
| 训练位置 | 默认 Mac 上用 mlx-lm LoRA；**离机训练（租 GPU）可选，须用户明确确认 + 数据全部过闸一隐私过滤**（P-6） | unsloth、ART |

---

## 5. 轨迹记录方案（客户可选项）

### 5.1 定位

- **是什么**：对经过 iDoris 的每个请求，按客户选择的粒度留存完整交互（输入、输出、路由决策、结果、反馈），作为**企业自有数据和业务场景的训练素材**。
- **是可选项**：由部署方（personal）或租户管理员（tenant）开关，粒度三档：`off` / `metadata`（只挂在审计上的决策与结果，不含内容）/ `full`（含内容）。
- **不是什么**：不是审计（审计永远只存元数据）；不是 Agent24/MemPalace 的运行期记忆（那是给 agent 当下用的，这里是离线学习语料）；也不覆盖绕过 iDoris 的边缘推理（AgentEar 本地 ASR 等可以选择主动推送 ATIF）。
- **为什么放在网关**：Agent Lightning、AReaL 2.0、MetaClaw、TencentDB Memory Proxy 都把采集点放在「替换 base URL 的透明代理」位置，上游不用改一行代码。iDoris 天然就在这个位置。〔agent-lightning、areal、macaron〕

### 5.2 两个开关，两个目的

依据：泰国 PDPC 2026 AI 指南草案（中国 PIPL 的目的限制与之同向）：

| 开关 | 目的 | 合法依据 | 默认 |
|:---|:---|:---|:---|
| `trajectory.retention` | 留存（排障、回放、评测、合规举证） | 部署方/租户的业务需要或合同 | 客户选择 |
| `trajectory.training` | **用于训练模型**（离机训练另需逐次确认 + 闸一过滤，P-6） | **单独的、可撤回的同意**，或组织与其客户之间的 DPA 授权 | **关**，必须单独开启 |

- 记录不等于训练。PDPC 草案明确：prompt、embedding、交互日志都属于个人数据；把数据转用于训练需要新的合法依据，原依据是同意的要**重新取得同意**；DPA 应包含删除模型权重与向量库的条款。〔WEB：PDPC 草案〕
- 因此删除必须**级联到派生物**（§5.6）。

### 5.3 采集点与关联

| 机制 | 说明 | 来源 |
|:---|:---|:---|
| `X-iDoris-Record-Id`（响应头） | 每个请求一个 record id，流式在首帧/末帧返回 | reef |
| `X-iDoris-Session` / `X-iDoris-Trace-Id` / `X-iDoris-Parent-Id`（请求头） | 上游把一次任务的多次调用串成一条轨迹；网关看不到工具执行结果，只能靠下一轮 messages 里出现的 tool 结果和关联头来补全 | agent-lightning |
| **`POST /v1/feedback`** | `{record_id, rating \| binary_rubric[], corrected_output?, labels[], outcome?}`，**可以随时补到已有记录上（延迟绑定奖励）** | reef、areal（delayed reward）、美团（二元 rubric） |
| 上游推送 ATIF | Agent24 Evolver、AgentEar 可推送完整 ATIF 轨迹，补上工具与结果 | areal Data Proxy |
| 历史会话导入（可选） | 只读导入本机已有的 Claude Code/Codex/OpenCode 会话，标记 `source=imported`。订阅产出的内容能否用于训练属于服务条款灰区，由用户逐源决定（P-7） | wake |

### 5.4 数据结构

**规范交换格式 = ATIF v1.8**（Harbor RFC 0001，NVIDIA NeMo Agent Toolkit 已内置支持）。ATIF 没有覆盖的治理字段放进 `extra.idoris` 命名空间，字段设计取自 AReaL ATDP 的四原则：信用可归因、延迟奖励、版本化可回放、受治理的可观测。〔WEB：Harbor ATIF、areal〕

```jsonc
{
  "schema_version": "ATIF-v1.8",
  "session_id": "<X-iDoris-Session>",
  "trajectory_id": "<uuid>",
  "agent": { "name": "<上游 key 对应的调用方>", "version": "...", "model_name": "idoris/<role>" },
  "steps": [
    { "step_id": 1, "source": "user", "message": "...", "timestamp": "..." },
    { "step_id": 2, "source": "agent", "model_name": "qwen3.5-9b@mlx-8bit",
      "message": "...", "reasoning_content": "...(若有)",
      "tool_calls": [{ "tool_call_id": "...", "function_name": "...", "arguments": {} }],
      "metrics": { "prompt_tokens": 0, "completion_tokens": 0, "cached_tokens": 0, "cost_usd": 0,
                   "completion_token_ids": [/* 仅本地后端 */], "logprobs": [/* 仅本地后端，可选 */] },
      "extra": { "idoris": {
        "record_id": "...", "tenant_id": "...|null", "key_id": "...", "deploy_mode": "personal|tenant",
        "route":   { "decisions": [{ "point": "route.intent", "judge": "local-logit", "scores": {}, "model_revision": "...", "prompt_hash": "..." }],
                     "candidates": [], "chosen": "...", "reason_code": "...", "rule_id": "...", "policy_version": "..." },
        "privacy": { "declared": "local_only|any", "inspected": "S0..S3", "actions": ["REDACT:PERSON"], "egress": "loopback|remote" },
        "budget":  { "reserved_minor": 0, "settled_minor": 0, "price_version": "..." },
        "backend": { "runtime": "omlx@0.6.4", "model_digest": "...", "adapter": "...|null", "quant": "8bit",
                     "tokenizer_fingerprint": "...", "sampling": { "temperature": 0, "seed": null } },
        "governance": { "source": "live|upstream_atif|imported|synthetic|teacher", "data_class": "synthetic|anonymized|real",
                        "sensitivity": "S0|S1|S2|S3", "retention_until": "...", "training_eligible": false, "consent_ref": "...|null" },
        "outcome": { "status": "success|failure|aborted|rejected|degraded", "failure_mode": "F1..F8|null" },
        "reward":  { "feedback": [], "auto_verifier": [] }   // 人工与自动来源分开存，不合成一个信号
      } } }
  ],
  "final_metrics": { },
  "extra": { "idoris": { "bundle_version": "...", "lineage": [] } }
}
```

两条关键约定：
- **双表示**：规范层存 messages，**与基座和分词器无关**（立项 §9：价值留在数据里，换基座可复用）。只有本地后端服务的请求才额外存 `completion_token_ids` + 分词器指纹，供日后对同一基座做 RL/OPD（TITO：避免重新分词造成的漂移）。〔idoris-project-launch §9、miles〕
- **原件不可变**：raw 轨迹只追加、写一次。修正和脱敏以派生新版本的方式进行；唯一例外是用户行使删除权时硬删除并留下墓碑。`is_copied_context=true` 的步骤和被拒产物**不进 SFT**。〔wikiskill、maka、ATIF、opensquilla〕

### 5.5 分类体系（多轴标签，采集时自动打）

| 轴 | 取值 | 谁来打 |
|:---|:---|:---|
| 来源 `source` | live / upstream_atif / imported / synthetic / teacher | 采集点 |
| 数据等级 `data_class` | synthetic / anonymized / real（沿用 T3.1.1） | 采集点 |
| 敏感度 `sensitivity` | S0 公开 / S1 内部 / S2 个人 / S3 特殊类别（健康/金融/证件/凭据/未成年），对应产品隐私分级 0–3 | 闸一 L0/L1 |
| 任务 | intent（chat/coding/writing/extract/classify/decide/translate/summarize/tool_use…）+ 角色（docs/16）+ 场景 | 入口判定层 |
| 模态 | text / image / audio / tool | 采集点 |
| 结果 | success / failure / aborted / rejected / degraded；`failure_mode` 取 docs/16 的 F1–F8，加 BrainTrust 的 retrieval miss / bad tool choice / unsafe action / cost blowup | 事件流 + 反馈 |
| 反馈 | none / thumbs / binary_rubric / correction / pair | `/v1/feedback` |
| 质量层 | **L0 raw → L1 eligible（通过资格筛选）→ L2 curated（人工或规则批准）→ L3 gold（进评测集）** | 策展流水线 |
| 用途 | sft / preference(KTO/DPO) / rl_env / decision_train / router_train / eval | 导出时确定 |

〔reef（学习资格）、美团（二元 rubric、Bad Case 飞轮）、miniCPM5（L0–L3 质量分层）、deeptutor（轨迹→摘要→综合）〕

### 5.6 生命周期

```
采集 ─▶ 自动分类(闸一 + 判定层) ─▶ 资格筛选(同意 ∧ 等级 ∧ 非 S3 或显式授权)
     ─▶ 策展(去重 / 质量过滤 / 人工批准队列 / 对比样本生成) ─▶ 数据集卡(版本、来源、租户、许可、样本数、筛查记录)
     ─▶ 导出 ─▶ 训练 ─▶ 评测门禁 ─▶ 晋升/回滚 ─▶ 退役
删除/撤回授权 ─▶ 墓碑 ─▶ 血缘表查出受影响的数据集与 adapter ─▶ 标记「撤回/需重建」
```

- **导出器**：mlx-lm `chat{messages}` / `tools{messages,tools}` JSONL（train/valid/test，支持 `--mask-prompt`）；TRL KTO `{prompt, completion, label}` 与 DPO `{prompt, chosen, rejected}`；原样 ATIF（给 RL 和 Agent24 Evolver）。〔WEB：mlx-lm LORA.md、TRL〕
- **血缘表**：轨迹 → 数据集版本 → adapter → 部署，每一层都记录 `sources[]`。〔tencentdb-agent-memory、conversationalvoice〕
- **对比样本**：对同一请求改掉一个关键事实（例如去掉证件号），让标签翻转，得到成对样本，用于训练隐私/意图判定器，不需要真实敏感数据。〔nimble〕
- **自动预评分**：没有人工反馈的轨迹可以由本地 verifier 预打分，存为 `auto_verifier`，与人工反馈分开存放。〔llm-as-a-verifier〕

### 5.8 「训练」到底指什么（P-6 的上下文）

**训练 = 用积累下来的轨迹去改变模型权重**，也就是训出新的 adapter 或小模型。这是「自进化」里改模型的那一半。

| 训练什么 | 用什么数据 | 做什么动作 | 产出 |
|:---|:---|:---|:---|
| ① 决策/路由小模型 | 请求文本 + 判定结果 + 结果反馈（入口判定需要文本；只做「哪个模型更合适」的路由优化，元数据就够） | 微调一个 0.6–4B 判定模型或决策头 | 更准的意图/隐私/复杂度判定 |
| ② 角色 LoRA（SFT） | 被批准的「请求 → 好回答」对 | 在基座上训练低秩适配器 | 更像你/你的组织的写法、格式、术语 |
| ③ 偏好对齐（KTO/DPO） | 👍/👎、用户修正 | 同上，偏好目标 | 更符合偏好的回答 |
| ④ 本地蒸馏（OPD） | 学生自己的输出 + 本地大模型当老师 | 大教小，数据和老师都在本机 | 小模型接近大模型的能力 |
| ⑤ RL | 有可计算奖励的任务轨迹 | GRPO 等 | 多步任务成功率提升 |

**不属于训练开关管辖的**：不改权重的进化（规则、Skill、Prompt、路由策略的改进建议），归 Agent24 Evolver 或 iDoris 的 policy 提案，仍走「提议 → 审批」。

**开关含义：**
- **训练开关关**：轨迹只用于回放、排障、评测。任何模型的权重都不会因为这些轨迹而改变。
- **训练开关开**：标记为 `training_eligible` 的轨迹可以被策展成数据集，并**在本机**训练上表各类产物。
- **离机训练**（P-6）：要额外满足两个条件：用户对该数据集版本明确确认；数据全部经过闸一过滤后的派生版本才可导出。

**只用元数据训练会不会泄露隐私？** 基本不会。元数据里没有内容，只能训练路由优化，例如「哪类请求在哪个模型上成功率高、成本低」。

**用含内容的轨迹在本地训练，风险在哪？** 数据不出机器，没有外泄；**真正的风险是模型记住内容再说出来**：
- adapter 可能在被问到时复述训练数据里的姓名、号码。
- 在组织里，用多个员工的数据训一个共享 adapter，可能让 A 从模型输出里看到 B 的内容。
- adapter 一旦离开本机（分享、联邦、离机训练产物回传），就成了泄露载体。

对策：
- 凭据和 S3 类内容**即使本地训练也先脱敏或排除**。
- adapter 继承其数据的最高敏感度和访问范围，按人或按租户分开训练。
- 训练前插入金丝雀样本，训练后检测记忆（membership / canary 测试），不通过不准晋升。
- 离开设备只允许 DP 处理后的 LoRA（沿用 F3.4）。
- 删除一条轨迹要级联到用了它的 adapter。

**普通机器扛得住吗？** 能扛住「小模型 + LoRA」，扛不住「大模型 + RL」。量级如下（来自调研文章，**未在本机实测，M7 前先做一次基准**）：

| 机器 | 可行的训练 | 依据 |
|:---|:---|:---|
| 16GB Mac | ≤2B 的 LoRA、决策头；0.5B 级模型数小时 | kev（0.5B 在 M5 上约 1h45m） |
| 24–32GB | 4B LoRA（学生约 4GB）；9B QLoRA 偏紧 | arle OPD |
| 64GB（M1 Max 这一档） | **9B QLoRA 舒适**（约 14GB），夜间跑完几千条样本 | idoris 立项文 §3.2 |
| RL / 14B 以上 | 建议 GPU（单张 H100 不到一天；ART·E 约 $80）→ 走 P-6 离机训练 | GRPO 指南 |

数据量不是瓶颈：Agent Lightning 用 6K 样本就有明显提升，JevEmbed 用 7.9 万条把判定准确率从 30% 拉到 84%。

### 5.7 存储与安全

| 形态 | 存储 | 加密 |
|:---|:---|:---|
| personal | 按天分片的追加式 JSONL（内容）+ SQLCipher 加密的 SQLite 索引（元数据，FTS5 trigram 中文检索仅在解锁时建） | 数据密钥由 Keychain 主密钥包裹 |
| tenant | Postgres + 对象存储；**每个租户独立的包裹密钥** | 信封加密；轨迹不跨租户、不进联邦（联邦只传 DP 后的 LoRA，沿用 F3.4） |

- 轨迹库是**高敏感资产**：每次读取都记访问审计；导出需要审批；默认不出机。〔agentsight、Agent24 ADR-017〕
- 量级估算：每天 1,000 次请求 × 平均约 6KB 内容 ≈ 6MB/天，约 2.2GB/年（压缩前），单机完全可承受。
- 反例提醒：Proma 只加密 API key、其余明文；Hindsight 做不到逐条删除。这两点都不能照搬。

---

## 6. 管理界面方案

### 6.1 定位与分期（P-3 已拍板）

**关系：两个独立组件 + 深度定制契约。** iDoris 是后台服务，自带极简控制台；**完整管理页由 Agent24 提供**，通过 iDoris 的 Admin API 实现。

| 选项 | 评估 |
|:---|:---|
| A. 深度合并：iDoris 只起后台，**所有**管理都走 Agent24 | ❌ iDoris 还服务 AgentEar、业务模块、LobeHub、手机端，没装 Agent24 的用户会失去管理手段；Agent24 挂掉时连 kill switch 都没有入口；安全开关的最终确认也会落到另一个进程，违背「进程边界 = 授权边界」 |
| B. 两边各做一套完整管理界面 | ❌ 重复建设，两套 UI 必然漂移 |
| **C. iDoris 自带极简控制台 + 全量 Admin API，Agent24 做完整管理页（推荐，已采纳）** | ✅ iDoris 可以独立使用、应急可达；Agent24 负责体验最好的那一层；接口只有一份 |

**深度定制是什么意思**：Agent24 是 iDoris 的**头号消费者**，享有专门的契约（header 协议、`idoris-local`/`idoris-any` 双逻辑 provider、Served-Locality、反馈与 ATIF、`/v1/systemone`、Admin API）。但它们都是**版本化的公开契约**，不是私有耦合。Agent24 仍然可以直连任何 OpenAI 兼容的外部 provider，只是那样拿不到 iDoris 的路由、隐私、预算、审计和轨迹保证。

**分工：**
- **iDoris 自带控制台**只做四件事：健康/内存/后端状态；**安全姿态与 kill switch**；模型加载、常驻、驱逐；最近请求的决策轨迹。
- **Agent24 管理页**做全部其余能力：路由规则编辑与回放、隐私规则、预算与审批、轨迹与授权、学习与晋升、租户与密钥。
- **敏感操作的最终确认留在 iDoris**：开启内容记录、开启训练、离机训练、放宽隐私、关闭 kill switch。Agent24 可以发起，但 iDoris 要求一次性确认令牌并记入审计。确认既可以在 iDoris 控制台完成，也可以用 Agent24 转交的用户确认凭据完成，具体凭据形式在边界规范里定。

| 阶段 | iDoris 侧 | Agent24 侧 |
|:---|:---|:---|
| M4 | `idoris` CLI + Admin API v0（只读 + 模型操作）+ 极简控制台；模型下载仍沿用 oMLX 自带界面 | 状态卡片（读 Admin API） |
| M5 | Admin API v1（隐私、预算、审批、kill switch） | 管理页：隐私、预算、审批 |
| M6–M7 | Admin API v2（轨迹、授权、学习、晋升） | 管理页：轨迹、学习、路由回放 |
| M8 | 组织版 Admin API（OIDC/RBAC） | 组织管理页 |

### 6.2 页面结构

| 页 | 内容 |
|:---|:---|
| **总览** | 健康、全局内存压力、各后端状态、在途请求、今日用量/成本、告警、**当前安全姿态**（绑定地址、key 策略、kill switch、出站开关） |
| **模型** | 目录（按角色/任务）、已加载实例、推荐组合及理由、**基准历史**、升级候选及证据包、晋升/回滚按钮 |
| **路由** | policy 版本与 diff、决策点配置、**回放模拟器**（新 policy 放在过去 N 天的流量上跑一遍）、shadow 对比 |
| **隐私** | 规则集、命中统计、**实际出站字节**、脱敏样例（本地可见）、kill switch |
| **预算** | 租户/key/provider 限额、价格表版本、影子价格、审批队列 |
| **请求** | 请求列表 → 详情页：以可读形式展示「判定 → 隐私 → 预算 → 路由 → 后端 → 结果」的决策轨迹 |
| **轨迹与学习** | 采集开关与粒度、同意记录、数据集与数据集卡、训练任务、评测结果、adapter 血缘 |
| **租户与密钥** | 虚拟 key、Blueprint、临时授权、角色 |
| **配置历史** | bundle 版本时间线、diff、回滚 |

〔llm-dock、virtual-ai-infra-team、staffdeck（执行记录即可读决策轨迹）、exxperts（History 与时间旅行）〕

### 6.3 交互原则

1. 所有写操作：**提议 → diff → 二次确认 → 版本化 → 可撤销**；写入前做乐观并发校验。〔exxperts、virtual-ai-infra-team〕
2. **LUI 驱动 GUI**：用一句话描述规则（例如「健康相关的永远不出本机」），编译成 policy diff，在界面里确认后生效。〔fliggy 读后感〕
3. 实时推送（SSE）在应用顶层全局挂载，切换页面不丢事件。〔proma〕
4. 高级参数原样透传，旁边附参考说明，不强行做成表单。〔llm-dock〕
5. 默认只绑 loopback；拒绝伪造 Host 与代理头；数据端口上没有任何管理入口。〔exxperts、pipelock〕

### 6.4 技术选型

极简控制台用 React + Vite + TypeScript，由 Router 进程托管静态资源（体量保持很小）。完整管理页的技术栈归 Agent24（其 Desktop/外壳）。**先做 API**：`/admin/api/v1` 的 OpenAPI 由 JSON Schema 真源生成（沿用 D-3），CLI、Web、未来的 Tauri 共用同一套 API。〔openmaple 三入口一套资源 API〕

---

## 7. 路线图（M1–M3 已完成，新增 M4–M8）

每个里程碑都是**端到端可验证的纵切**，不一次建完。验收以用户视角表述，每条都配可机器验证的命令和负对照（沿用 FU-8 纪律）。

### M4 可托付的底座（约 4–6 周）
- 合并 `preview → main`；解决 FU-13（生产端口，`IDORIS_PORT`）、**FU-14（虚拟 key + 绑定规则）**、**FU-16（oMLX 0.6.4 端点复测）**、FU-15 落成 lint/测试护栏。
- Runtime Supervisor：oMLX + **mlx_lm.server 保底** + **llama.cpp GGUF**，加全局内存账本和会话亲和。
- 协议面：`/v1/messages`、`/v1/embeddings`、`/v1/rerank`；字段兑现矩阵测试；统一错误体（rule_id + remediation）。
- **Event Log 骨架** + `X-iDoris-Record-Id` + trace 关联头 + **`/v1/feedback`**；审计改为 Event Log 的投影。
- Admin API v0 + `idoris` CLI + iDoris 极简控制台；Agent24 状态卡片（§6.1）。
- **验收**：换掉任一后端或核心模型，调用方零改动；非 loopback 且无 key 时拒绝启动；任一请求都能由 record id 查到完整决策链。

### M5 隐私与预算真正落地
- **闸一 `INSPECTED`**：L0 中文规则 + 校验和 + 二次扫描 fail-closed；REDACT 稳定占位符 + 流式还原；**kill switch**；出站字节可视化。
- **闸二**：Keychain 凭证代理；能力②真实 provider（Anthropic/OpenAI/DeepSeek/Qwen 白名单）；出站去关联。
- 预算 reserve/settle + 价格表 + 价格未知 fail-closed + 影子价格 + 审批队列；三粒度断路器。
- Admin API v1；Agent24 管理页：隐私、预算、审批；iDoris 控制台加 kill switch 与请求决策轨迹。
- **验收**：往请求里注入身份证号或手机号，远程出站为 0 字节或已被脱敏（出站计数器验证）；并发 100 个请求不会击穿预算；kill switch 触发后远程请求为 0。

### M6 轨迹金库与入口判定
- **Trajectory Vault**：ATIF v1.8 + `extra.idoris`；三档粒度开关、两个目的开关；加密存储；自动分类；删除/撤回级联；导出器（mlx-lm / TRL / ATIF）。
- **入口判定层**：本地 logit 型 System-1（复用常驻小模型），决策点命名，**shadow 模式**，决策账本，置换一致性测试。
- 对外提供 `/v1/systemone` 决策端点（Agent24 审批门、Evolver、业务模块都可以用）。
- 管理面：轨迹与授权页、路由回放。
- **验收**：关闭采集时，除审计元数据外磁盘上没有任何内容；开启后任一请求都能导出为合法 ATIF（通过 Harbor validator）；删除一条记录后，受影响的数据集被标记为待重建；新判定器在 shadow 模式下与现行判定器的一致率可以在账本上查到。

### M7 数据飞轮与自有模型
- 策展流水线 + 数据集卡 + 血缘表；对比样本生成；本地 verifier 预评分。
- 按顺序训练：**决策模型** → fast/daily 角色的**本地教师 OPD** → KTO/DPO 偏好 → 有了可计算奖励再做 RL。
- 晋升门禁：docs/16 场景评测 + 贪心逐 token 比对 + 安全/越狱回归 + 位置偏差；空闲/夜间调度，训练让位于推理。
- Admin API v2；Agent24 管理页：轨迹、学习、路由回放。
- **验收**：用自有轨迹训练的判定器在自有测试集上，校准后 ECE 和准确率优于基线，并且比较结论跨过 docs/16 ST-2 的统计门槛；晋升失败时自动回到 known-good。

### M8 组织版与联邦
- OIDC、RBAC、Blueprint、临时授权、PAM 式审批；租户信封加密；Postgres 存储，支持从 SQLite 非破坏性迁移。
- B7 跨组件签名 tenant 凭据；联邦真实数据接入（F3.4 隐私层之后）。
- Windows 客户端（W-1b：随 GGUF 后端一起做）。
- **验收**：A 租户的管理员看不到 B 租户的任何记录或轨迹；租户撤回训练授权后，其数据不再出现在任何新的数据集版本中。

〔Trinity 非破坏性迁移、platypus、authentik；原 roadmap 的 F3.x 与 acceptance 七条判据并入〕

---

## 8. 对上游的服务目录

| 能力 | 接口 | 时间 | 主要消费者 |
|:---|:---|:---|:---|
| 文本推理（OpenAI 协议） | `/v1/chat/completions` | ✅ 已有 | Agent24、AgentEar、业务模块 |
| 文本推理（Anthropic 协议） | `/v1/messages` | M4 | 走 Anthropic 协议的 harness |
| 嵌入 / 重排 | `/v1/embeddings` `/v1/rerank` | M4 | RAG、MemPalace |
| 模型/角色目录与容量 | `/v1/models` `/capabilities` | ✅ 已有（M4 增加 locality 列） | Agent24 |
| 反馈 | `/v1/feedback` | M4 | 所有带 👍👎 或修正能力的上游 |
| 隐私检测（只判定、不改写） | `/v1/inspect` | M5 | 需要先判断再决定是否外发的上游 |
| **快速决策（System-1）** | `/v1/systemone` | M6 | Agent24 审批门/Evolver、业务分流 |
| 用量/预算/审计查询 | `/idoris/tenants/{id}/…` | M4 | 下游计费 |
| 轨迹导出 | `/admin/api/v1/trajectories/export` | M6 | Agent24 Evolver（统一使用 ATIF v1.8） |

**对 Agent24 的新增需求（写入 docs/09 的后续版本）**：
1. AI Layer 的路由改为**通过 header 委托给 iDoris**，Agent24 自身不再新增路由策略，避免出现两个策略源。
2. Agent24 现行的「ATIF 归档」（DGM 风格 YAML）**对齐到 Harbor ATIF v1.8**。
3. 👍👎 与用户修正回填到 `/v1/feedback`。
4. 请求时带上 `X-iDoris-Session/Trace-Id`。

---

## 9. 与原规划的差异汇总（本版做出的变更）

| # | 原结论 | 本版 | 理由 |
|:---|:---|:---|:---|
| C-1 | 一个平台一个后端（`detectBackend`） | 一个平台多个后端，Supervisor 统一编排，全局内存账本 | 用户本人的 64GB 方案就是 oMLX + apfel + Ollama 并存 |
| C-2 | 真实数据在 F3.4 隐私层就位前不得进入任何管线 | **在本地加密留存真实数据**，由客户开关决定；**离开设备**（联邦、云端训练）仍受 F3.4 门禁约束 | 轨迹是客户可选项；原门禁本意是防止数据离机 |
| C-3 | 审计只存元数据是唯一的记录形态 | 审计不变；另设**客户可选的轨迹模块**，两者物理分离 | 用户澄清：泰国合同是定制开发，通用产品提供可选项 |
| C-4 | 本地模型 `cost=0` | 路由与预算仍按 0 处理，报表另计影子价格 | 本地成本不是零，但不能因此拦截本地请求 |
| C-5 | 能力② 只保留槽位 | M5 接入真实白名单 provider，加闸二凭证代理 | 云端强模型当「编译期老师」需要它 |
| C-6 | 意图路由只由 Agent24 负责（D3） | 待拍板（P-2） | T2.4 已经实现了兜底，且用户这次把「意图」列为 iDoris 的能力 |
| C-7 | 管理面没有规划 | §6 四阶段 | 本次新需求 |

---

## 10. 待你拍板

| # | 问题 | 建议 |
|:---|:---|:---|
| **P-1** ✅ | 轨迹采集的**出厂默认值** | **已拍板（2026-09-27）**：personal 与 tenant **默认都是 `metadata`（开，只记元数据）**；记录内容（`full`）须用户/租户管理员主动开启；训练开关一律默认关（含义见 §5.8） |
| **P-2** ✅ | D3 与 T2.4 的冲突：iDoris 要不要做意图推断 | **已拍板（2026-09-27）**：显式 header 永远优先；判定层作为能力对外提供（`/v1/systemone`），**判定引擎可选 Jev 类模型：外部 API（TypeSafe Jev，仅限 privacy 允许出本机的请求）或本地开源实现（SemIf/LLM2Jev/Kev/AgentJev 类，`local_only` 请求只能用本地）**；自身兜底推断是 policy 开关，默认静态缺省。**附加要求**：规划确认后，按层界定与 Agent24 的分工边界和接口数据规范，经跨会话与 Agent24 多轮协商定稿（见 §12） |
| **P-3** ✅ | 管理界面形态与 iDoris×Agent24 关系 | **已拍板（2026-09-27）**：两个独立组件 + 深度定制契约。iDoris 自带**极简控制台**（状态、应急、安全开关）；**全部管理能力以 Admin API 暴露**，完整管理页由 Agent24 提供。iDoris 不再做 Tauri 壳。详见 §6 |
| **P-4** ✅ | 实现语言 | **已拍板**：用 TypeScript；Rust 重写以后再议 |
| **P-5** ✅ | llama.cpp GGUF 后端放进 M4 | **已拍板**：放进 M4。注意这**不是把模型管理框架换成 llama.cpp**——框架是 iDoris 自己的 Runtime Supervisor + `ModelBackend` 抽象，oMLX 仍是 Mac 默认后端；llama.cpp 是第二个后端，理由：① MLX 的 KV 量化会禁用连续批处理，长上下文/高并发要靠它（docs/13 §3.3 已核实）；② Windows/Linux 无 MLX；③ GGUF 生态覆盖最广 |
| **P-6** ✅ | 训练能否离机（租 GPU） | **已拍板（2026-09-27）**：**可选离机**，但须满足两条硬条件：① 每次离机都要**用户明确确认许可**（逐次或按数据集版本授权，记入同意记录与血缘）；② **离机数据必须全部经过闸一隐私过滤**（BLOCK/REDACT 后的派生版本才可导出，二次扫描不过则拒绝）。缺任一条件 → 拒绝导出 |
| **P-7** ✅ | 导入本机历史会话（Claude Code/Codex） | **已拍板：不导入**。iDoris 的方向不是编程场景 |
| 沿用 | B6（动作审批 sunset 条件）、B7（跨组件 tenant 凭据）、W-1 | 不变，M8 前定 |

---

## 11. 文档治理

- **本文件是唯一现行规划**。拍板后，执行台账（`agent/tasks.md` / `agent/progress.md`）按 §7 拆出 M4 起的 task，继续用 pilot 推进。
- **仍然有效、不废弃**：`agent/tasks.md`、`agent/progress.md`（执行台账）；`agent/contract-tenancy.md`（**已冻结的对外契约 v1.3**，下游正在照此实现；后续变更走 v2）；`spike/u0/U0-LOG.md`（实测原始记录）；`research/` 与 `docs/research-*.md`（调研证据）。
- **已废弃（保留作决策追溯，冲突时以本文件为准）**：`docs/00`–`docs/17`、`docs/iDoris-主模型-设计与规划汇总.md`、`docs/Prism-report.md`、`agent/architecture.md`、`agent/spec.md`、`agent/roadmap.md`、`agent/research.md`、`agent/acceptance.md`、`agent/ecosystem-boundaries.md`、`agent/handoff-agent24.md`、`agent/voice-*.md`，以及根目录的三份 2026-09-19 报告。
  - 仍在引用的**设计细节**（契约字段、docs/15 的中文规则集、docs/16 的评测框架、docs/13 的模型数据、ecosystem-boundaries 的权限域划分）按「本文件引用 → 原文作为附录」的方式继续使用，不重复抄写。
- docs/00 §3 冲突清单中的 S-1～S-10 已在本文件中消化；S-3（主力模型从 Ornith 改为 Qwen3.5-9B）继续有效。

---

## 12. 下一步：与 Agent24 的分工边界与接口规范（P-2 附加要求）

规划确认后执行，顺序固定：

1. **起草**《iDoris × Agent24 分工边界与接口数据规范》，**按层**逐一界定（接入协议、身份与租户凭据、模型/角色目录、路由与意图、隐私、预算、审计与 Event Log、轨迹与反馈/ATIF、决策端点、学习产物回流），每层写清：谁负责、谁不负责、接口（端点/header/错误体）、数据规范（JSON Schema 真源）、版本与兼容规则。
2. **跨会话推送**给 Agent24（连同本规划中与它相关的章节、docs/17 协作草案、docs/09 需求），请其 review。
3. **多轮协商**：每轮的分歧、采纳、驳回留档于规范文件的「协商记录」节，直到双方确认定稿。
4. 定稿规范以 JSON Schema 形式进入 `packages/contracts`，并作为 M4 起相关 task 的验收依据。

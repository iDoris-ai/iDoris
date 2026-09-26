# iDoris 统一模型服务 · 立项调研 — research

> 「为什么做、凭什么做」。本文件是 `docs/01~10` 十篇调研文档的**决策蒸馏**，不重复其论证过程。
> 原始论证见 [`../01-统一模型服务-架构设计.md`](../01-统一模型服务-架构设计.md) 起的十篇；实测见 [`../../spike/u0/U0-LOG.md`](../../spike/u0/U0-LOG.md)。
> 记录日期：2026-09-07（原始调研 2026-07-30，U0 实测同日）

## 五步框架

### 1. 要解决的问题
个人/社区/城市三级用户想用 AI，但今天只有两个选择：把数据交给云平台（丧失数字主权），或者自己拼一堆本地工具（每个业务各接各的模型，没有统一入口、没有隐私分级、没有降级链、换个模型要改一遍所有调用方）。

具体痛点（来自 iDoris.ai 自身与 Agent24 的真实使用）：
- 已付费的 Claude Code / Codex **订阅算力**无法被自己的业务代码复用，只能在 CLI 里手敲。
- Mac 统一内存有限（部署目标 Mac mini M4 24GB），常驻大模型和临时小模型**抢内存**，没有编排就只能人肉起停。
- 业务（banner / blog / chat / 微信 / Nostr / Agent24）各自选模型，**隐私敏感任务没有机制保证不出设备**。
- 换一个模型/一个路由器 = 改所有调用方，因为**没有契约层**。

### 2. 现有方案全景
见下方开源全景表。结论：**三条腿都有成熟开源支撑**，没有一处需要从零造轮子。

### 2.5 定位：个人网关，同时是组织大脑（2026-09-07 R0 拍板）
原始文档 `01` 把 iDoris 定位为「**个人** AI 网关」。R0 之后这个定位**扩展而非推翻**：个人是形态之一，不是全部。iDoris 同时是**组织的大脑**——未来为组织提供托管服务，故多租户（用量/预算/审计按 tenant 隔离）是定位本身包含的一等能力，不是为某个业务开的后门。

两种形态共用同一套契约与代码路径，差别只在 `deploy_mode` 与 tenant 维度是否生效。**合规边界不因此松动**：多租户只作用于能力②③，能力①订阅中转在 `deploy_mode=tenant` 下**拒绝注册**——多租户与订阅红线是正交的两件事。

### 3. 差异化立足点
把「订阅中转 + 隐私路由 + 业务意图路由 + 自增长/联邦」这四件**别人没缝合过**的事，缝进同一层薄编排。
- LiteLLM 解决「多 provider 统一」，不懂隐私分级和业务意图。
- llama-swap / oMLX 解决「本地多模型 + 内存编排」，不懂 provider 家族与订阅态。
- Flower / OpenFedLLM 解决「联邦训练」，与推理网关完全脱节。
- **没有任何一个项目**把「个人订阅算力 + 本地模型 + 隐私 fail-closed + 个人→社区→城市联邦」做成同一个本地 URL。

护城河不在单个模型多强，而在「**数据在体系内自增长 + 隐私前提下的集体进化**」这套闭环——关系图谱/个人化模型/网络效应归属用户与社区，与 Mycelium「数字公共物品 + 数字主权」使命同构。

### 4. 可复用 vs 要自建
| 层 | 决策 | 依据 |
|:---|:---|:---|
| 本地模型编排（能力③）| **复用 oMLX**（macOS）/ vLLM·llama.cpp·Ollama（Win/Linux）| U0 实测：oMLX 原生具备多模型 + LRU + memory-guard + KV 估算，**不装 llama-swap** |
| 外部 API 统一（能力②）| **延后选型**，只留 provider 槽位 | local-first；无 Anthropic/Gemini key，无真实消费者 |
| 订阅中转（能力①）| **自建薄封装**，参考 agent-cli-to-api 模式 | U0 实测 `claude -p` / `codex exec` 中转可行；无现成项目符合 loopback+单用户门禁要求 |
| 契约层（ProviderDescriptor / LoadPolicy / AdapterManifest / routing policy）| **必须自建** | 这是整个体系的资产，没有任何上游提供 |
| iDoris Router | **必须自建**（TypeScript / pnpm 起，验证后内化进 Agent24 Rust）| 业务语义层，上游都不懂 |
| 联邦训练 | **复用 Flower + PEFT + mlx-lm** | 标准工具链，LoRA-only 是被验证的低成本高隐私路径 |

### 5. License / 合规边界
- **不 vendor 第三方源码**：组件以二进制/CLI/容器引入，版本 pin。与 Agent24 SPEC-001 §10「进程边界 = 授权边界」同源——这条同时解决许可洁净（可合法集成 GPL/非商用上游）与可替换性。
- **能力① 订阅中转的合规立场**（2026-07-30 定稿）：个人把**自己的**订阅转成本地 API 供**自己**用，属对自有资产的正当使用（开源、非商业、不干预服务方、不转售），ToS 上是未明确禁止的灰区。据此**默认 loopback + 单用户**；用户可自行放开到 Tailscale 私网多设备。
- **硬边界**：社区端 / 城市端**绝不转发个人订阅**——多租户转发超出自用范畴、触碰服务方明确红线。社区/城市端用自己的 API（能力②）或本地模型（能力③）。
- **联邦硬门禁**：F0 阶段**只准用合成/脱敏数据**；真实个人数据必须等隐私层（DP-FedLoRA + 安全聚合）就位。否则「联邦」退化成「上传数据」，违背整个体系初衷。
- 本仓库 Apache 2.0；引入的 adapter 必须在 `AdapterManifest.license` 声明，走 cargo-deny 式许可门。

## 开源全景表

| 项目 | 能力 | 可借鉴 | License | 本项目决策 |
|:---|:---|:---|:---|:---|
| [oMLX](https://github.com/) v0.4.3 | MLX 推理服务器，多模型 + LRU + memory-guard + `/v1/messages` + `/load` `/unload` | 直接采用为 macOS 的能力③实现 | — | **采用**（U0 实测通过）|
| [llama-swap](https://github.com/mostlygeek/llama-swap) | 引擎无关代理，per-model TTL | LoadPolicy 的另一种实现 | MIT | **不装**；仅当要混非-MLX 引擎时评估 |
| [LiteLLM](https://github.com/BerriAI/litellm) | 140+ provider / 1892 模型，Anthropic·Gemini 原生翻译 | 能力②候选之一 | MIT | **延后**，等真实消费者 |
| ClawRouter / OmniRoute | TS 网关，免费层 / 多 provider | 能力②候选 | — | **延后** |
| [agent-cli-to-api](https://github.com/leeguooooo/agent-cli-to-api) | 多 CLI → OpenAI-compat `/v1` | 能力①的直接范式 | — | **借鉴模式**，自建薄封装 |
| CLIProxyAPI / claudecodex | OAuth 订阅态接入 | 备选路径 | — | 备选 |
| [semantic-router](https://github.com/aurelio-labs/semantic-router) | 嵌入式语义路由，无 LLM 调用 | 入口意图路由首选（见 [`../10-入口路由模型-调研对比.md`](../10-入口路由模型-调研对比.md)）| MIT | **M2 引入** |
| [Flower](https://github.com/adap/flower) + PEFT + mlx-lm | 联邦学习框架 | 联邦层直接采用 | Apache 2.0 | **M3 采用** |
| onecli 模式 | Rust MITM 凭证网关 | CredentialProvider 的默认实现 | — | **BACKLOG**（保留抽象接口，实现延后）|
| [Ornith-1.0](https://github.com/) 9B | 自改进脚手架 RL，9B 打 35B | 常驻核心模型 | MIT | **选定为常驻核心** |
| Agents-A1 35B MoE | Horizon Scaling | 32GB+ 机型的常驻候选 | 开源 | 24GB **不作默认** |

## 结构性空白（差异化）
1. **订阅算力 ≠ API 算力，但没人把它当成一等 provider**。业界网关只认 API key，不认「本机已登录的 CLI 订阅态」。
2. **隐私是路由判据，不是文档承诺**。现有网关的 fallback 链会在本地模型 OOM 时静默转到云端——对 `LocalOnly` 任务这是数据泄漏。iDoris 把 `fail_closed` 写进组件卡强制字段。
3. **容量是接口**。24GB 机器上「能不能共存 / 要不要驱逐 / 会不会超延迟」是业务必须知道的，但没有网关把它暴露成 `/capabilities`。
4. **推理网关与联邦训练分属两个世界**。iDoris 让同一个节点既是个人核心，又是别人的「外部 API 上游」，靠联邦把个人→社区→城市串成一张自增长的网。
5. **「个人隐私网关」与「组织多租户网关」被当成两类产品**。业界要么做单机隐私工具（无租户概念），要么做企业网关（无本地优先与隐私 fail-closed）。iDoris 用同一套契约覆盖两者：个人形态守 loopback 红线，组织形态守租户隔离，而**隐私 fail-closed 在两种形态下都是同一段代码**。

## 结论
**做**。切入点是 M1「统一网关 MVP」：先把契约（06 §10）落成可执行代码 + oMLX 适配 + 薄 Router，让 Agent24 能通过一个 `IDORIS_URL` 调到本地模型且隐私 fail-closed 生效。第一个里程碑指向「**一个 URL、三能力可路由、LocalOnly 绝不外泄**」，而非任何模型能力指标。

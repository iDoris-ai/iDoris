# iDoris 主模型 —— 跨仓库设计与规划汇总

> 文档类型：跨仓库综合（synthesis）
> 编制日期：2026-07-30 ｜ 维护者：iDoris.ai / @jhfnetboy
> 范围：把散落在 8+ 仓库里关于「iDoris 主 AI / 主模型」的设计、构想、契约、现状**汇总为一处**，作为"接入 iDoris 主 AI（替换 placeholder）"这项工作的权威参照。
>
> **一句话现状**：iDoris 主模型的**设计蓝图非常完整且各仓库一致**（本地优先 · 三层拓扑 · 隐私联邦 · 自进化），但**具体推理端点 / 适配器尚未实现**——目前是"契约 + 占位"，运行时用 oMLX（Qwen3-8B-4bit）本地模型作事实替身。

---

## 0. TL;DR（给赶时间的人）

| 问题 | 结论 | 出处 |
|---|---|---|
| iDoris 主模型是什么？ | 一个**本地优先、隐私保护、分层协同**的个人/社区 AI —— 定位 "Community Brain"（协作大脑），不是通用聊天机器人 | iDoris/README、AuraAI、Prism 分析 |
| 架构范式 | **"中型模型 + 丰富数据"**（逆转云端"大模型+碎片数据"），跨域数据产生"涌现洞察" | Prism 分析 §1 |
| 模型分层 | 端侧 `<3B` → 家庭端 `9B–35B MoE`（Qwen3.5-MoE）→ 社区端（算力池 + 集体 LoRA） | Prism §2、AuraAI product-vision |
| 隐私机制 | 架构级隐私（本地推理 + 语义联邦，传摘要不传原始数据，实测 125.5x 压缩）+ 隐私分级 0–3 | Prism §2/§3、AuraAI product-vision |
| 软件契约 | `@auraaihq/idoris` 定义 **Adapter + Bridge**（provider: `idoris\|claude\|openai\|local`），路由/fallback/隐私层 | auraai-packages/packages/idoris |
| 运行时消费者 | Agent24 Rust 内核 `ModelRouter`（Local/Remote/Lora 三层 + health/cooldown + 隐私标签强制本地） | Agent24 rust/agent24-models |
| 现状差距 | **无 `@auraaihq/ai-idoris` 具体适配器；Agent24 Remote 层未接线；Brood 里 iDoris 推理 API 标"待完善"** | 见 §8 |

---

## 1. 相关仓库总览（每个贡献了什么）

| 仓库 | 对"主模型"的贡献 | 关键文件 |
|---|---|---|
| **iDoris** | 定位与命名：*"Community Brain for cooperation and coordination like Mycelium"*；深度架构参照（Prism 分析报告） | `README.md`、`_Prism 项目深度分析报告 .md` |
| **AuraAI** | 产品化愿景（OpenAgent Network）+ 数据主权起源构想 + 模型分层选型 + 五大护城河 + 隐私分级 | `docs/product-vision.md`、`_个人数据主权 AI 架构探讨 .md`、`docs/repo-architecture.md` |
| **auraai-packages** | **AI 网关契约实装**：`@auraaihq/idoris`（Adapter/Bridge/路由/错误分类/隐私层预留） | `packages/idoris/src/{adapter,bridge,in-process-adapter,index}.ts` |
| **Agent24** | **运行时消费者**：Rust `ModelRouter` 三层路由 + `OpenAiCompatProvider`（oMLX/Ollama）+ TaskProfile 隐私/复杂度；ADR-016 / PLAN 里的 "iDoris(placeholder)" | `rust/crates/agent24-models/{lib,router}.rs`、`docs/ADR-016`、`docs/PLAN.md` |
| **iDoris-SDK** | **接入渠道层**（主模型如何触达用户）：多平台桥接（微信/Telegram/...），Agent 接口，OpenAI-compat adapter | `docs/ARCHITECTURE.md`、`docs/DESIGN.md` |
| **Brood** | 组织级对外契约（iDoris 是生态"AI 能力层"）；**iDoris 推理接口标记为"待完善"** | `orgs/auraai/{PROFILE,INTERFACES}.md`、`protocol/MISSION.md` |
| **AgentSocial** | 理论支撑：多 agent 协作 / 记忆 / 社会仿真 论文阅读（社区大脑的学术背景） | `paper1-reading-notes/*` |
| **simple-agent / AgentSocial / iDoris-website** | 周边（agent 范例 / 官网 / Doris 形象），非主模型核心 | — |

---

## 2. 设计源头：Prism 三层拓扑（架构参照）

iDoris 主模型的架构思想直接借鉴并演进自开源研究项目 **Prism**（*Cross-Domain Personal Data Integration on Consumer Hardware Produces Emergent Insights*）。

### 2.1 核心范式转移
- 云端 AI = **大模型 + 小数据**（隐私限制下只能看到碎片上下文）。
- Prism / iDoris = **中型模型 + 丰富数据**（Medium Model + Rich Data）：把金融、饮食、情绪、阅读等跨域深度数据喂给本地中型模型，产生单域 AI 触不到的**"涌现洞察"**（Insight Increment Ratio，跨域整合平均 1.48×，危机场景达 1.53×）。

### 2.2 三层设备拓扑（计算压力 ⟂ 数据敏感度解耦）
| 层 | 设备 | 模型规模 | 职责 |
|---|---|---|---|
| **Tier 0 采集/端侧** | 手机/IoT | `<2–3B` | 原生数据采集、即时意图识别、隐私过滤、初步摘要 |
| **Tier 1 家庭端** | MacBook / 消费级 PC | `9B` | 单域日常查询，平衡性能/功耗 |
| **Tier 2 全景端** | Mac Studio（大内存）| `35B MoE` | 联邦汇总全域摘要、复杂"全景推理"、闲时增量微调（LoRA）|

### 2.3 关键实现原语（借鉴点）
- **模型选型**：`Qwen3.5-35B-A3B (MoE)` —— "35B 知识容量、3B 推理成本"，配 `llama.cpp` / `MLX`（Apple Silicon 统一内存）。
- **LAN 联邦协议**：节点间只暴露标准化 JSON Schema **摘要**（趋势/统计值），**严禁传原始记录**；实测 **125.5× 压缩**——语义级聚合本身即天然脱敏。
- **隐私审计**：记录原始数据量 vs 传输字节，**用架构强制隐私边界**而非政策声明。
- **两阶段输出**（解决"行动力悖论"：数据越全建议越空泛）：先出深度洞察 → 再针对性生成行动清单。

---

## 3. 产品化愿景：OpenAgent Network / Community Brain

AuraAI 把 Prism 的技术构想产品化为 **OpenAgent Network**——"以物理硬件为载体、本地优先、隐私保护的 Agent 网络"，并演进出 iDoris 特有的**社区端**。

### 3.1 从"三层拓扑"到"三层协同体系"
| 层 | 命名 | 定位 | 模型 |
|---|---|---|---|
| 用户端 | **主权入口（Sovereign Edge）** | 唯一入口、跨平台、模型透明、轻量感知 | `<3B` 端侧（Gemma/Qwen-Mobile/Whisper 等，量化后 `<500MB`）|
| 家庭端 | **私人知识中心** | 高频私人计算、全量数据本地存储、深度洞察 | `9B–35B`（Qwen3.5-9B/35B-MoE），后端 Ollama/vLLM + 本地向量库 |
| 社区端 | **共治大脑（Community Intelligence）** | 算力池化 + 技术托管降级 + 脱敏集体 LoRA 训练反哺 | 分布式集群，Sovereign-Sync 联邦协议 |

> **社区端的双角色**：①技术托管——用户一键同步最新模型/适配器，不必自己追模型、写 API；②集体进化——用户签署脱敏授权后上传**语义摘要**（非原始数据），社区大算力做集体 LoRA，产出"通用经验插件"反哺每个人。副产品是一个社区共有的个体模型训练能力。

### 3.2 五大护城河（AuraAI product-vision）
1. **本地隐私过滤/安全模型**（专门训练，非通用 AI；持续训练的商业资产）。
2. **Token-free 24h 常驻 Agent**（本地推理无 token 成本，"一直在场"而非"问一次答一次"，主动关注）。
3. **自进化算法**（用户授权下用私有数据持续微调本地模型，"越用越懂你"且只属于你，可加密迁移）。
4. **Agent 账户与硅基人网络**（每设备唯一 Agent ID / DID，Agent 间互认与协作）。
5. **目标协作协议 + 信用体系**（真实生活场景协作，信誉积累非虚拟币）。

### 3.3 隐私数据分级（产品级落地）
```
级别 0  永不离开设备：生物特征、位置历史、家庭成员信息
级别 1  仅限本地网络（设备↔PC/Mobile）：对话内容、行为习惯
级别 2  可选同步（明确授权）：技能包使用统计（匿名）
级别 3  公开网络（协议层）：Agent ID（去身份化）、信用积分
```
> 该分级与 Agent24 `ModelRouter` 的 `Privacy::LocalOnly` 强制本地路由**同源同向**，是把隐私分级落进代码的接口。

---

## 4. 软件契约：`@auraaihq/idoris`（AI 网关）

主模型不是"某一个模型"，而是一层**可插拔网关**——业务只调 `complete()`，网关决定用哪个 provider、如何 fallback、如何脱敏。这是把"模型透明"落地的关键抽象。

### 4.1 Adapter 契约（每个 provider 实现）
```ts
type ProviderFamily = 'idoris' | 'claude' | 'openai' | 'local' | 'other' | (string & {})

interface Adapter {
  metadata: { id; name; provider: ProviderFamily; local: boolean; maxConcurrency? }
  complete(prompt, options?): Promise<{ text; adapterId?; usage? }>   // M1 只有 complete；chat/stream/tools 在 M2
}
```
- **`local: boolean`** —— provider 是否完全在设备内（无网络），是隐私路由的判据。
- **`maxConcurrency`** —— 单会话模型运行器（llama.cpp 绑定）必须设 1，防并发死锁/输出污染。

### 4.2 Bridge 路由/回退策略
- 主 adapter 抛出**跨适配器可恢复**错误（`rate_limit / timeout / network / context_overflow / unsupported`）→ 沿链 fallback 到下一个 adapter。
- **跨适配器致命**错误（`auth / invalid_request / aborted / unknown`）→ 直接上抛（换 adapter 无益）。
- 错误 `cause` 非枚举、不经 `JSON.stringify` 泄漏——避免把 prompt/token/header 意外外泄。
- 路线：M2 加 streaming、能力匹配、best-of-N、成本感知路由。

### 4.3 隐私层
`@auraaihq/idoris` 描述职责含 **"privacy layer（PII 脱敏、元数据剥离）"**（M1 预留接口、M2 实装），对齐 ADR-016 iDoris 网关层设计。

> ⚠️ **注意**：`@auraaihq/idoris` 是**契约 + 路由 + 测试用 in-process adapter**。文件头明确写："Concrete impls (`@auraaihq/ai-claude`, `@auraaihq/ai-local`, etc.) ship as separate packages (landing in M1+)"。**这些具体适配器包（含 `@auraaihq/ai-idoris`）目前不存在**（`packages/` 下只有 `idoris`，无 `ai-*`）。

---

## 5. 运行时消费者：Agent24 Rust `ModelRouter`

Agent24 内核（当前主后端为 Rust）已实现一套与上述设计同构的路由层（M-D / D2）。

### 5.1 三层 + 隐私/复杂度路由
- `Tier { Local, Remote, Lora }`（Lora 视为本地）。
- `TaskProfile { privacy: Any|LocalOnly, complexity: Simple|Complex }` 决定 tier 偏好序：
  - `LocalOnly` → **只走 [Local, Lora]，永不 Remote**（即便本地全挂也 fail-closed 报错，不泄漏）。
  - `Any + Simple` → `[Local, Lora, Remote]`；`Any + Complex` → `[Remote, Local, Lora]`。
- **health/cooldown 闭环**：provider 返回 `Unavailable` → 指数退避冷却跳过；成功清零。
- **locality 校验**：`OMLX_URL` 指向非 loopback 会被降级为 Remote，防"伪本地"泄漏。

### 5.2 当前 provider = oMLX / Ollama（事实替身）
`from_env()` 只接线两个**本地** provider：
- oMLX `http://127.0.0.1:8088`（默认模型 `Qwen3-8B-4bit`，`OMLX_URL/OMLX_API_KEY/DEFAULT_MODEL` 可覆盖）
- Ollama `http://127.0.0.1:11434`
- 二者都用 `OpenAiCompatProvider`（OpenAI-compat HTTP）。
- **Remote / Lora 层未接线**——注释原文："A remote/lora provider is added by the daemon when configured"。**这就是 "iDoris 主 AI(placeholder)" 的技术落点**：Remote/主-AI 层是留好的空位。

### 5.3 配套（已就绪，等主模型接入即生效）
- **D3 Guardian**（`agent24-policy`）：本地小模型评估 `{risk_level, rationale}` 自动放行低风险——本身也是一个"iDoris 类"本地模型的消费场景。
- **D1/D5b 记忆**：CanonicalSession 上下文载入/回写/压缩。
- **ADR-026 结论**：**跑 LLM 不需要 Python**（oMLX 走 OpenAI-compat HTTP 即可）；Python worker 只为 Embedding/Whisper/LoRA，且"先有消费者再有提供者"。

---

## 6. 接入渠道：iDoris-SDK（主模型如何触达用户）

主模型要成为"个人 AI 伙伴"，必须能从熟悉渠道对话。iDoris-SDK 提供**平台无关的 Agent 桥接**：

- 三层：Platform Bridge（WeChat 已完成 / Telegram·Twitter·Discord 规划）→ `@idoris/core`（`PlatformBridge` + `Agent` + `IncomingMessage` 统一模型）→ Agent 层。
- **一个 Agent 接口，接入任意平台**：`Agent.chat(IncomingMessage) → AgentResponse`，平台协议细节全隐藏在 Bridge。
- 内置 adapter：Claude / OpenAI-compat（DeepSeek/Qwen/任意兼容）/ 自定义。
- 微信走腾讯官方 iLink ClawBot（`ilinkai.weixin.qq.com`，个人号扫码），协议层以 submodule 引入可自控。
- 与 Agent24 的关系：Agent24 已独立实现了 `packages/wechat-bridge`（F3，接 agent24d v1 HTTP）与 `packages/nostr-bridge`（F4）——即渠道层已在 Agent24 落地，iDoris-SDK 是同一理念的独立/更早实现。

---

## 7. 设计一致性检查（跨仓库是否自洽）

| 设计主张 | Prism | AuraAI | @auraaihq/idoris | Agent24 | 一致？ |
|---|---|---|---|---|---|
| 本地优先 / 隐私保护 | ✅ 本地推理 | ✅ Local-first | ✅ `local` flag + 隐私层 | ✅ `LocalOnly` 强制 | ✅ |
| 分层模型（端/家庭/社区）| ✅ Tier 0/1/2 | ✅ Edge/Home/Community | ✅ 多 adapter 可路由 | ✅ Local/Remote/Lora | ✅ |
| 主 AI 本地 + 远程备 | ✅ 本地 9B + 社区分流 | ✅ 本地 + PC 补充 | ✅ fallback 链 | ✅ 隐私→本地/复杂→远程 | ✅ |
| 自进化 / LoRA | ✅ Tier2 闲时 LoRA | ✅ 护城河 3 | ⏳ 契约预留 | ✅ Lora tier（未接线）| ✅（未实装）|
| 联邦/脱敏 | ✅ 语义摘要 125.5x | ✅ 分级 0-3 | ✅ 隐私层（预留）| ✅ 路由级 fail-closed | ✅ |

**结论**：设计层面高度自洽，是同一套思想在不同层的投影。缺的只有"最后一公里"实装。

---

## 8. 现状 vs 差距（"接入 iDoris 主 AI" 到底缺什么）

### 已就绪 ✅
- 网关契约（`@auraaihq/idoris`：Adapter/Bridge/错误分类/隐私层接口）。
- 运行时路由（Agent24 `ModelRouter` 三层 + 隐私强制 + health/cooldown）。
- 事实替身（oMLX `Qwen3-8B-4bit` 本地跑得通，chat/runs/guardian 都在用）。
- 渠道（微信/Nostr 桥接）、记忆（D1）、审批门（C4/D3）。

### 缺口 / 阻塞 ⚠️
1. **没有 iDoris 推理端点规范**：Brood `INTERFACES.md` 明确把"iDoris AI 推理接口（边缘计算 AI 模型 API 规范）"列为**"待完善/规划中"**。→ *主模型没有官方线协议可对接*。
2. **没有 `@auraaihq/ai-idoris` 具体适配器**：`packages/` 下只有网关 `idoris`，无任何 `ai-*` concrete impl。
3. **Agent24 Remote/主-AI 层未接线**：`from_env()` 只有 oMLX+Ollama；`iDoris (placeholder)` 名副其实。
4. **"iDoris 模型"本身尚无独立 checkpoint**：现用通用 Qwen 权重，尚无 iDoris 特化/微调模型或其分发方式。

### 因此："接入 iDoris 主 AI" 的真实含义（现阶段）
> **不是**把内核指向一个已存在的 iDoris API（它还不存在），
> **而是**把"主 AI provider"这条**可插拔接线**修好——让 iDoris 端点/模型一旦出现即可零改动切换，并把当前真实主 AI（本地 oMLX）正确落成"主 provider"。

---

## 9. 落地规划（建议里程碑，供拍板）

> 按"先有消费者再有提供者 + 契约先行"的一贯原则，从内向外、可验证地推进。**每步都不越 Agent24 的产品门（对外分发/签名仍需单独拍板）**。

- **I0｜规范先行**：在 Brood `INTERFACES.md` 落定 **iDoris 推理接口契约**（首版直接采用 OpenAI-compat `/v1/chat/completions`，与全生态既有 provider 一致，零新协议）。产出：接口契约 + 版本号。
- **I1｜Agent24 主-provider 接线**：`ModelRouter::from_env()` 增加 `IDORIS_URL/IDORIS_API_KEY/IDORIS_MODEL` 可选 provider，locality 自动判 tier；未配置时行为不变（纯加法、零回归）。把 router.rs:203 的"remote/lora 由 daemon 配置"从注释变成实现。
- **I2｜网关具体适配器**：新建 `@auraaihq/ai-idoris`（实现 `Adapter`，OpenAI-compat 之上），并让 Bridge 把 `provider:'idoris'` 作为首选、`claude/local` 作 fallback。
- **I3｜隐私层实装**：`@auraaihq/idoris` 的 PII 脱敏/元数据剥离从"预留"变"实装"，与隐私分级 0–3 / `LocalOnly` 打通。
- **I4｜iDoris 特化模型**：定义"iDoris 模型"是什么——先以本地 Qwen(MoE) + 系统提示/persona 起步，再按护城河 3 引入用户授权下的本地 LoRA 自进化；分发走模型目录（`~/.omlx/models/`）。
- **I5｜社区端（远期，产品门后）**：算力池 + 集体 LoRA + Sovereign-Sync 联邦——属 M4/M5 生态，需单独拍板。

> **门约束**：I0–I3 是内部接线/契约，风险低；I4 触及模型训练与分发；I5 是对外生态（跨用户/社区），与 Agent24 P4 门后 4 项（跨用户共享、Nostr 分发、iDoris 主 AI、模块签名）同域，越门需用户拍板。

---

## 10. 术语表

| 术语 | 含义 |
|---|---|
| **主模型 / 主 AI** | iDoris 作为默认首选 AI provider（隐私敏感优先它，高推理才上远程），非"唯一模型"，而是网关首选项 |
| **Community Brain** | iDoris 的定位：为协作与协调服务的社区大脑（呼应 Mycelium 菌丝网络） |
| **IIR** | Insight Increment Ratio，跨域整合洞察相对单域的增益比 |
| **联邦协议 / Sovereign-Sync** | 节点间只传语义摘要（非原始数据）的隐私保护同步协议 |
| **LoRA** | 低秩增量微调；家庭端闲时/社区端集体训练的个性化"补丁" |
| **Adapter / Bridge** | `@auraaihq/idoris` 的网关抽象：Adapter=单 provider 实现；Bridge=多 provider 路由/回退 |
| **Tier (Local/Remote/Lora)** | Agent24 `ModelRouter` 的路由层；Lora/Local 属"本地"，满足 `LocalOnly` |
| **placeholder** | 当前"iDoris 主 AI"在 PLAN/ADR/代码中的占位状态——契约就位、具体实装缺席 |

---

## 附录 A：源文件索引（可追溯）

| 主张来源 | 路径 |
|---|---|
| Prism 三层拓扑 / MoE / 联邦 / IIR | `iDoris/_Prism 项目深度分析报告 .md` |
| iDoris 定位 "Community Brain" | `iDoris/README.md` |
| 产品愿景 / 模型选型 / 五护城河 / 隐私分级 | `AuraAI/docs/product-vision.md` |
| 数据主权起源构想 | `AuraAI/_个人数据主权 AI 架构探讨 .md` |
| OpenAgent 仓库矩阵 | `AuraAI/docs/repo-architecture.md` |
| AI 网关契约（Adapter/Bridge/错误/隐私） | `auraai-packages/packages/idoris/src/{adapter,bridge,in-process-adapter,index}.ts` |
| iDoris 网关层设计 / 迁移路径 | `Agent24/docs/ADR-016-consume-auraai-packages.md` |
| 主 AI 解耦（iDoris主/Claude备/LLaVA）| `Agent24/docs/PLAN.md` |
| Rust 三层路由 + 隐私强制 | `Agent24/rust/crates/agent24-models/{lib.rs,router.rs}` |
| ADR-026：跑 LLM 不需要 Python | `Agent24/docs/ADR-026-rust-core-polyglot.md`、`Agent24/docs/specs/TASKS.md` §M-D |
| 渠道桥接（Agent/PlatformBridge） | `iDoris-SDK/docs/{ARCHITECTURE,DESIGN}.md` |
| 组织定位 + 推理接口"待完善" | `Brood/orgs/auraai/{PROFILE,INTERFACES}.md` |

---

*本文档为跨仓库汇总，所有论断均可回溯至附录 A 源文件；如源文件更新，请同步修订本文件。*

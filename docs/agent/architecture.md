# iDoris 统一模型服务 · 架构 — 技术判断与骨架

> 「怎么搭」。定义契约与**不可动摇的边界**。数据细节见 [`spec.md`](spec.md)。
> 完整论证见 [`../01-统一模型服务-架构设计.md`](../01-统一模型服务-架构设计.md)、[`../05-集成与技术栈协调规划.md`](../05-集成与技术栈协调规划.md)、[`../06-组件接口契约与互换标准.md`](../06-组件接口契约与互换标准.md)。
> 记录日期：2026-09-07

## 核心判断

1. **集成契约 = REST(OpenAI-compat HTTP) + 进程边界，不是 import、不是同语言。**
   生态本就是 polyglot（Agent24 Rust+TS、agent-speaker Go、auraai-packages TS）。强求同语言不可能也没必要。进程边界同时买到四样东西：稳定（组件崩溃不波及）、许可干净（不 vendor 源码即可合法集成 GPL 上游）、可换（契约后随便替）、可取消可审批（跨进程天然可 kill）。与 Agent24 SPEC-001 §10「进程边界 = 授权边界」同源。

2. **能力③ 用 oMLX，不装 llama-swap（U0 实测后的决策变更）。**
   原计划用 llama-swap 做「常驻/临时 + 一个 URL + 内存受控」，探测发现 `omlx serve` 本身就是 LRU-based 多模型服务器，原生具备且实测通过（8B 热命中 0.39s、16GB guard 下精确核算、模型级驱逐实测确认、内置 KV 估算与 07 §1 公式一致）。**纯 MLX 场景下 llama-swap 是多余一层**——anti-over-engineering。

3. **但契约不写死 oMLX。** oMLX 仅 Apple Silicon，而 Agent24 要分发 Windows/Linux/macOS。故 capability③ 的实现**按平台探测选择**，全部在 `LoadPolicy` 抽象之后。oMLX 是「macOS 的默认实现」，不是 iDoris 的固定依赖。

4. **local-first 的能力优先级**：`③ 本地模型(核心) > ① 订阅中转(可选/best-effort) > ② 外部 API(罕用逃生口)`。
   能力①**绝不**作为核心正确性的必需 fallback；能力②这一轮只留 provider 槽位 + OpenAI-compat 冒烟。

5. **策略是数据不是代码。** 路由/隐私/fallback 逻辑若硬编码进 Router，换 Router 就要移植隐藏策略。故 `routing_policy` 是版本化的声明式 YAML，Router 只是**解释引擎**。

6. **Router 第一版 TypeScript / pnpm**（2026-09-07 拍板）。与 auraai-packages / iDoris-SDK 同栈，oMLX 与 CLI 中转都是 HTTP/subprocess，TS 起最快。验证后按 06 契约内化进 Agent24 Rust `ModelRouter`——**契约不变，实现可换**这条对我们自己也成立。

7. **iDoris 是组织大脑，多租户是一等能力**（2026-09-07 拍板 R0）。
   iDoris 的定位不止「个人 AI 网关」——**它同时是组织的大脑**，未来要为组织提供托管服务。故 Router 有两种部署形态，由 `deploy_mode` 决定：
   - `personal`：单人自用。三能力全开（含能力①订阅中转），无 tenant 维度。
   - `tenant`：**组织大脑**。为多个租户托管，每次调用带 `tenant`，**用量 / 预算 / 审计按 tenant 硬隔离**。
   多租户**只作用于能力②③**（外部 API / 本地模型）。**能力①的 loopback + 单用户红线不因多租户而松动**——恰恰相反：`deploy_mode=tenant` 时订阅中转 provider **直接拒绝注册**，代码层面无法启用。多租户与订阅红线是正交的两件事，不是此消彼长。
   > 这条来自跨仓库需求 R0（iDoris-website 泰国业务托管多客户）。原本 `products/gateway/` 与本层重复实现，现**并回 iDoris**：对方降级为消费者，其 `routing.py` / `audit.py` / `egress_guard.py`（Apache-2.0，含变异测试）整体移交。

## 系统骨架

**两种部署形态共用同一套契约与代码路径**，差别只在 `deploy_mode` 开关与 tenant 维度是否生效：

| | `deploy_mode: personal` | `deploy_mode: tenant`（组织大脑）|
|:---|:---|:---|
| 服务对象 | 单人自用 | 多个租户（组织托管）|
| `X-iDoris-Tenant` | 忽略（无此维度）| **必填**，缺失即 400 |
| 能力① 订阅中转 | 可用（loopback + 单用户）| **拒绝注册**，启动即报错 |
| 能力②③ | 可用 | 可用，按 tenant 隔离用量/预算/审计 |
| 绑定地址 | loopback（可选放开 Tailscale 私网）| 按组织部署决定，但订阅 provider 永不参与 |


```
              ┌──────── 对外：唯一入口 http://127.0.0.1:PORT/v1 (OpenAI-compat) ────────┐
 Agent24 /    │  + 控制面 header  X-iDoris-Privacy / Intent / Complexity / Capabilities │
 渠道 / 业务 ─▶└──────────────────────────────┬──────────────────────────────────────────┘
                                ┌─────────────▼──────────────┐
                                │   iDoris Router (TS 进程)   │
                                │  ┌──────────────────────┐  │
                                │  │ 控制面解析 → TaskProfile│  │
                                │  │ routing_policy 解释引擎 │  │  ← 策略是 YAML，不是代码
                                │  │ ProviderRegistry(组件卡)│  │  ← 缺策略字段拒绝注册
                                │  │ fail_closed 隐私门禁    │  │  ← LocalOnly 无本地可用即报错
                                │  │ tenant 隔离: 用量/预算/审计│  │  ← tenant 模式；预算超支=终态拒绝
                                │  │ 降级链 / reason 可解释   │  │  ← 每次决策必须答得出「为什么」
                                │  └──────────────────────┘  │
                                └──┬───────────┬──────────┬───┘
                     REST ─────────┘    REST   │  subprocess+REST └────────┐
                       ▼                       ▼                            ▼
        ┌──────────────────────────┐ ┌────────────────────┐ ┌────────────────────────────┐
        │ 能力③ 本地模型（核心）    │ │ 能力② 外部 API      │ │ 能力① 订阅中转（可选）      │
        │ LoadPolicy 适配器:        │ │ (槽位，本轮只冒烟)  │ │ spawn `claude -p`/`codex   │
        │  macOS → oMLX :8088      │ │ OpenAI-compat 上游  │ │ exec` → 封 OpenAI-compat   │
        │  Win/Linux → vLLM/       │ │ 选型 BLOCKED        │ │ loopback + 单用户 硬绑定    │
        │   llama.cpp/Ollama       │ └────────────────────┘ └────────────────────────────┘
        └──────────────────────────┘
                                    ┌──────────────────────────────────┐
                                    │ M2: HardwareAwareModelRecommender │  ← 读 /api/status 做 admission
                                    │ M3: 联邦训练进程(Flower+PEFT+mlx) │  ← 离线批处理，按需拉起
                                    └──────────────────────────────────┘
```

## 契约 / 接口（实现与调用分离）

四份契约是**本项目的核心资产**，组件是可替换耗材。完整字段见 [`spec.md`](spec.md)，语义来源见 06 §10。

| 契约 | 作用 | 成熟度目标 |
|:---|:---|:---|
| `ProviderDescriptor` | 统一 provider 家族/tier/capability/privacy_class/locality 五套词汇 | M1 达 L1 骨架 → L2 完整 schema |
| `ComponentCard` | 组件注册单元，**强制**含 `privacy_class` `allowed_egress` `fallback_policy` `fail_closed` | M1 L1 + 校验器 |
| `LoadPolicy` / `ModelLease` | 抽象「常驻/临时/驱逐载入」，引擎无关 | M1 L1 + oMLX 适配 → M2 L3 黄金测试 |
| `RoutingPolicy` | 声明式路由规则（if privacy/intent/complexity → then tiers/capability/fail_closed）| M1 L1 |
| `TenantContext` | 租户身份 + 预算 + 配额；tenant 模式下每次调用必带 | M1 L1 |
| `AdapterManifest` | LoRA 的 base/tokenizer 指纹 + framework + 隐私处理 + 聚合兼容性 | M3 |

**控制面（06 §10.5）**：意图/隐私/复杂度**不走 prompt、不走 model 名**，走扩展 header（对 OpenAI-compat 透明）：
```
X-iDoris-Privacy: local_only | any
X-iDoris-Intent: banner | blog | reasoning | coding | chat
X-iDoris-Complexity: simple | complex
X-iDoris-Capabilities: vision,asr
X-iDoris-Fallback: fail_closed | next_in_chain
X-iDoris-Tenant: <tenant_id>     # deploy_mode=tenant 时必填，缺失即 400；personal 模式忽略
```
备选方式 B：侧端点 `POST /idoris/route` 返回选定 provider 后再调 `/v1`。

**契约成熟度分级（06 §10.10）**：L0 命名 → L1 骨架（强制字段+语义）→ L2 完整 I/O schema → L3 黄金一致性测试。**「可替换」这个承诺只对达到 L2+L3 的能力成立**；未达的诚实标注，不假装。

## 不可动摇的边界

- **LocalOnly fail-closed**：`privacy_class: local_only` 的任务，本地无可用 provider 时**报错**，绝不降级到 loopback 以外的任何目的地。这是产品承诺，不是最佳实践。
- **组件卡缺策略字段即拒绝注册**：协议兼容 ≠ 路由安全。没有 `privacy_class`/`allowed_egress`/`fallback_policy`/`fail_closed` 的组件不进 registry。
- **能力①只绑 loopback + 单用户，且 `deploy_mode != personal` 时拒绝注册**：组织/社区/城市端配置下**代码层面无法启用**订阅中转（不是文档劝告）。用户可显式放开到 Tailscale 私网，属个人自用延伸。**多租户不是放开这条红线的理由**——组织租户用组织自己的 API（能力②）或本地模型（能力③）。
- **tenant 隔离是硬隔离**：A 租户的用量、预算、审计记录，B 租户**查不到任何一条**；不是靠查询时加 where 条件，是数据访问层就带 tenant 作用域，缺 tenant 上下文的查询**直接报错**而非返回全量。
- **预算耗尽是拒绝，不是降级**：预算是商业约束，与技术性降级（忙/OOM/超时）走不同出口。错误信息必须说清「是预算不是故障」，否则客户会以为服务坏了。
- **审计只存元数据，绝不存内容**：Router 是跨租户集中组件，一旦存内容就成了「所有客户的会议记录、合同、客服对话」的集中数据库。防线是字段名黑名单闸门（命中即**抛错拒绝写入**，不是静默丢弃）+ 单字段长度上限，不是靠自觉。
- **每次路由决策必须可解释**：决策返回带非空 `reason`，能区分「隐私强制 / 预算 / 意图匹配 / 降级」四类。没有 reason，路由错了只能靠猜，客户问「为什么用了贵的那个」也答不上来。
- **出网控制是启动断言，不是运行时警告**：存在会导致遥测外发的环境变量时**拒绝启动**（进程退出非 0）。启动不了人一定会看；数据流出去人不一定会知道。扫描用**前缀匹配而非点名已知变量**——点名是失败开放，上游下个版本加个新变量名，清单不会自己长出来。
- **不 vendor 第三方源码**：二进制/CLI/容器引入，一切经 pin 的版本（二进制版本号 / 容器 tag / commit / 模型指纹），杜绝悄悄升级破坏兼容。需 patch 的协议层才用 submodule 固定 commit。
- **不写死单一推理后端**：任何 `omlx` 字样只能出现在 `adapters/omlx/` 下；Router 核心只认 LoadPolicy 契约。
- **base 指纹不匹配拒绝聚合/挂载**（M3）：防止一次静默升级毁掉整批 LoRA。
- **联邦真实数据门禁**（M3）：隐私层（DP-FedLoRA + 安全聚合）未就位时，真实个人数据不得进入联邦；F0 只用合成/脱敏数据。
- **License 红线（自下游尽调引入）**：**LiteLLM 的 `enterprise/` 目录绝不引用**；**Dify 禁多租户**——多租户已归 iDoris，此条直接排除它作为选型；ComfyUI GPL 只能以独立进程调用，不得传染。
- **能力①的适配器必须沙箱化，且价值主张要按沙箱后的实际能力表述**：`claude` / `codex` 是具备工具与工作区能力的 agent 程序，不是纯模型 provider。不沙箱化，它就是一条绕过 Agent24 审批门的通道。
  - **沙箱后它还剩什么**：对注入的上下文做摘要/分类/抽取/翻译/起草、生成计划或候选答案、结构化输入→结构化输出。**没有了**：自主读真实工作区、调业务工具、改文件、执行工作流、核验生成内容是否符合现实状态。
  - **所以价值主张只能写成「复用订阅额度获得模型推理」，不能写成「复用现成 Agent 能力」。** 若某个用例的卖点来自 Claude Code / Codex 的工具使用与仓库操作，沙箱确实把它掏空了——那个用例就不该走能力①。
  - **「无任意子进程」要操作化**，否则要么跑不起来、要么仍是旁路：允许启动**固定的那个二进制及其必要运行时**（CLI 本身要联网、要读订阅凭据、可能有内部子进程假设），同时限制**网络目的地、输入目录、凭据范围、输出大小**。
- **战略平台依赖不抽象**：Nostr（去中心通信）与 AirAccount DID（身份信任根）是 Mycelium 的既定赌注，直接依赖，**不套可替换抽象层**——把它们当可换组件反而增加无谓复杂度（06 §10.9）。

## 运行形态

- **iDoris Router**：常驻 Node 进程（pnpm workspace）。`personal` 模式监听 `127.0.0.1:PORT`，测试期跑个人电脑，部署到 Mac mini 24h 常驻，经 Tailscale 从任意地点访问同址。`tenant` 模式（组织大脑）由组织部署决定绑定地址，但订阅 provider 永不参与、且启动时做出网断言。
- **能力③ 后端**：独立进程（macOS 上是 oMLX .app / `omlx serve`），Router 通过 HTTP 调用与 `/load` `/unload` 显式控制。
- **能力① 中转**：按请求 spawn `claude -p` / `codex exec`，非常驻。
- **联邦训练**（M3）：Python 进程，离线批处理，按需拉起，不常驻。
- **跨仓库**：Agent24 把 iDoris 统一 URL 作为一个 provider 接入（`IDORIS_URL`，纯加法零回归）；需 Nostr 时 subprocess 驱动 agent-speaker(Go) CLI。生态 lingua franca = **OpenAI-compat HTTP + Nostr + DID**。

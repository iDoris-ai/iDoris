# iDoris 统一模型服务 Roadmap — Milestone → Feature

> 「未来要做什么」。具体怎么做+验收见 [`tasks.md`](tasks.md)。
> 编号：M<里程碑> → F<里程碑>.<序号>。记录日期：2026-09-07
> 与原始文档的对应：M1 ≈ 01 §6 的 U0/U1/U2/U3；M2 ≈ U4 + 07 §5 + 09 R1–R6；M3 ≈ 03 §5 的 F0/F1/F2。

## M1 — 交付给第一个真实消费者的最小可用底座
目标：**一条极薄的真实纵切跑通，而不是一次建完整底座**。
2026-09-07 按下游阻塞重排：泰国业务的 Documents D7 / Creative C2 / Assistant A4 全部卡在「经 Gateway 计费与审计」，那是它们里程碑的出口判据——所以审计与用量查询**从 M2 上移进 M1**；recommender / 语义路由 / 容量接口留在 M2。

**纵切顺序（每一刀都端到端可验证，不做完再接）**：
```
① 单租户一次本地文本推理跑通（契约→Router→oMLX→回包）
② 加租户身份 + 预算拒绝（402）
③ 加审计 + reason 关联
④ 加第二个租户，验证隔离
⑤ 加用量查询 + 账期时区凭据（下游据此计费）
```
> 反面教材是「先把租户、路由、密钥、语音、发行五套设计一起建完再接消费者」——iDoris 现在零代码，一次背五套未验证设计必然返工。

- **F1.1 契约层落地** — 把 06 §10 的五份契约从 YAML 片段变成可执行的 TS 类型 + zod schema + 校验器；组件卡缺策略字段即拒绝注册。这是整个体系的资产，先于任何实现。
- **F1.2 能力③ 本地模型编排** — LoadPolicy 抽象 + oMLX 适配器（U0 已实测其 knob）+ 跨平台后端探测骨架。**不写死 oMLX**。
- **F1.3 iDoris Router 薄编排层** — 统一 `/v1` 出口、控制面 header 解析、声明式 policy 解释引擎、fail-closed 隐私门禁、降级链。
- **F1.4 能力① 订阅中转（可选/best-effort）** — spawn `claude -p` / `codex exec` 封 OpenAI-compat，loopback + 单用户硬绑定。**绝不作为核心正确性的必需 fallback**。
- **F1.6 审计与用量账本（自 M2 上移）** — 审计记录（元数据白名单 + `reason`）、按 tenant 的用量聚合与账期时区凭据。**下游的计费依据，不进 M1 消费者就用不了。**
- **F1.5 多租户基线（组织大脑）** — `deploy_mode: personal | tenant`；`TenantContext` 契约、tenant 硬隔离的用量/预算/审计、预算终态拒绝、决策 `reason` 可解释、出网启动断言。**2026-09-07 拍板 R0 后新增**：多租户是 iDoris 定位的一部分，不是为单个业务开的后门。

## M2 — 硬件感知与可运营
目标：**在 24GB 机器上自己会安排内存，并被 Agent24 真正用起来**。容量成为接口，推荐有理由，接入零回归。
（审计与计费已上移 M1；本里程碑不再包含它们。）

- **F2.1 HardwareAwareModelRecommender** — 按 07 §5 的公式与伪代码实现：硬件探测 + 模型目录 + 打分 → 常驻/临时组合 + 警告 + sysctl 建议 + 可读 tradeoff。
- **F2.2 容量接口与可观测** — `GET /capabilities` 暴露 `resident/estimated_memory_gb/ctx_limit/queue_depth/admission_status`；路由决策审计日志。
- **F2.3 Agent24 集成交接** — 按 [`../09-对Agent24的需求.md`](../09-对Agent24的需求.md) R1–R6 交出需求与联调；iDoris 侧只提供 provider 与契约，**不改 Agent24 内核**。
- **F2.4 语义意图路由** — 引入 semantic-router（[`../10-入口路由模型-调研对比.md`](../10-入口路由模型-调研对比.md) 首选），把 `X-iDoris-Intent` 从「调用方声明」升级为「可自动识别」。
- **F2.5 能力② 外部 API 槽位** — 仅 OpenAI-compat 冒烟证明槽位可接；OmniRoute/ClawRouter/LiteLLM 三选一与保真度矩阵 **BLOCKED**（缺 Anthropic/Gemini key + 无真实消费者）。

## M3 — 自增长与联邦（F0 → F1 → F2）
目标：**用得越久越懂我，且隐私是接入真实数据的硬门槛**。严格按「先跑通 → 隐私 → 效率」推进。

- **F3.1 个人本地自增长闭环（对应 03 的 F0 个人端）** — 使用 → 数据湖 → 提炼 → MLX-LoRA → 热挂载，全程不出设备。**只用合成/脱敏数据**。
- **F3.2 AdapterManifest 与一致性门禁** — base/tokenizer 指纹校验，不匹配拒绝挂载/聚合；许可门。
- **F3.3 个人↔社区最小联邦（对应 03 的 F0 联邦端）** — Flower 起最小联邦，同 base、只传 LoRA、FedAvg 反哺。仍**只用合成数据**。
- **F3.4 隐私层（对应 03 的 F1）** — DP-FedLoRA 加噪 + 安全聚合 + 授权协议 + 隐私级别 0 硬隔离 + 审计。**这一层就位前，真实个人数据不得进入联邦**。
- **F3.5 效率与规模化（对应 03 的 F2）** — 通信压缩、客户端择优参与、闲时调度、增量聚合、non-IID 缓解。

## 已明确不做（本轮）

- **不装 llama-swap** —— U0 实测 oMLX 原生具备常驻/临时 + 内存受控（决策见 [`architecture.md`](architecture.md) 核心判断 2）。仅当将来要混非-MLX 引擎时重新评估。
- **不做 Anthropic/Gemini 保真度矩阵** —— local-first，且缺凭证、无真实消费者。等消费者出现再做（先有消费者再有提供者）。
- **不做凭证网关（onecli 式 MITM）** —— 2026-09-07 拍板记为 BACKLOG；只在 architecture 里保留 `CredentialProvider` 抽象接口位。
- **不改 Agent24 内核** —— iDoris 只提需求（09 R1–R6），实现由 Agent24 侧决定。
- **不承载下游业务语义** —— 租户侧的业务动作、字数规则、审批队列留在消费方；Router 只管「哪个模型、能不能调、记什么账」。
- **不为语音引入非-MLX 后端**（本轮）—— 语音走控制面/数据面分离，`speech-worker` 的运行时归 agentEar；iDoris 只签发授权与记账。见 [`ecosystem-boundaries.md`](ecosystem-boundaries.md) §4.2。
- **不重写 Agent24 已有的 ModelRouter** —— iDoris 作为它的一个 provider 接入（纯加法）；策略权威归 iDoris，但 Agent24 侧不做迁移改造。
- **不整体承担密钥管理** —— 只管 provider key；DID 私钥、Nostr 签名钥、工具凭证、设备权限各归其位（「统一管理」与「统一爆炸半径」只差一步）。
- **不为多租户放开能力①的订阅红线** —— `deploy_mode=tenant` 下订阅中转直接拒绝注册，组织租户用组织自己的 API 或本地模型。
- **不把 Nostr / AirAccount DID 抽象成可替换组件** —— 它们是战略平台依赖（06 §10.9）。

---

> 当前聚焦：**M1 / F1.1**（F1.5 多租户基线与 F1.3 Router 并行设计，接口契约先行——下游 iDoris-website 已挂起等我们的契约）。每个 Feature 的 Task 拆分与状态见 [`tasks.md`](tasks.md)。

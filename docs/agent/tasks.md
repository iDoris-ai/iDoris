# iDoris 统一模型服务 · 任务台账 — Task

> 前置：[`roadmap.md`](roadmap.md)（M→F）· [`architecture.md`](architecture.md) · [`spec.md`](spec.md) · [`acceptance.md`](acceptance.md)
> 每个 Task 自包含，可独立开发与验收。**验收标准必须可机器验证**（跑命令能判定）。
> 状态：BACKLOG · READY · IN_PROGRESS · BLOCKED · PR_OPEN · CHANGES_REQUESTED · APPROVED · DONE
> 记录日期：2026-09-07
> 台账同步：2026-09-22（F1.x/F2.x 共 32 个 Task 已落 PR #9–#32，均为栈内 OPEN；F3.x 与 T2.5.2 未动）

---

## F1.1 — 契约层落地

### T1.1.1 pnpm workspace 骨架 + 门禁流水线  `PR_OPEN`
- **优先级**：high
- **目标**：起一个能跑 lint/typecheck/build/test 的 TS monorepo，后续所有 task 有落点。
- **开发范围**：`pnpm-workspace.yaml`；`packages/contracts` `packages/router` 两个空包；tsconfig（strict）、eslint、vitest、`.github/workflows/ci.yml` 跑同一套门禁。
- **明确不做**：不写任何业务逻辑；不引入 oMLX/HTTP 依赖；不配置发布流程。
- **依赖**：无
- **交付物**：`pnpm-workspace.yaml`、`package.json`（scripts: lint/typecheck/build/test）、两个包的骨架、CI workflow
- **验收命令**：`pnpm install && pnpm lint && pnpm typecheck && pnpm build && pnpm test`（全部退出 0；test 允许 0 用例但脚本必须存在且成功）
- **涉及文件**：仓库根、`packages/contracts/`、`packages/router/`、`.github/workflows/ci.yml`
- **风险/回滚**：无（纯新增）
- **证据**：PR #9（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.1.2 五份契约的 TS 类型 + zod schema  `PR_OPEN`
- **优先级**：high
- **目标**：把 06 §10 的契约片段变成可执行、可校验的类型。
- **开发范围**：`ProviderDescriptor` / `ComponentCard` / `LoadPolicy` / `RoutingPolicy` / `TaskProfile` 五份的 TS 类型与 zod schema，字段与取值严格按 [`spec.md`](spec.md) 数据模型章节。
- **明确不做**：不做 `AdapterManifest`（M3）；不做校验器的错误信息本地化；不实现任何解释/执行逻辑。
- **依赖**：T1.1.1
- **交付物**：`packages/contracts/src/{provider,component-card,load-policy,routing-policy,task-profile}.ts` + 导出的 zod schema
- **验收命令**：`pnpm --filter @idoris/contracts test`（每份契约至少 1 条合法样例通过 + 1 条缺必填字段样例被拒绝）
- **涉及文件**：`packages/contracts/`
- **风险/回滚**：契约变更影响所有下游 → 本 task 内定版为 v1，后续改动走版本化
- **证据**：PR #10（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.1.3 组件卡策略校验器（缺字段即拒绝注册）  `PR_OPEN`
- **优先级**：high
- **目标**：让「协议兼容 ≠ 路由安全」成为代码保证——缺策略字段的组件卡进不了 registry。
- **开发范围**：`validateComponentCard()`：强制 `privacy_class` / `allowed_egress` / `fallback_policy` / `fail_closed` / `version_pin` 存在；**交叉规则**：`privacy_class=local_only` 必须 `fail_closed=true`；`tier=local` 且 `locality=remote` 为非法；`allowed_egress` 含 `internet` 时 `privacy_class` 不得为 `local_only`。
- **明确不做**：不做 registry 本身（T1.3.1）；不做运行时 health 探测。
- **依赖**：T1.1.2
- **交付物**：`packages/contracts/src/validate.ts` + 非法样例表
- **验收命令**：`pnpm --filter @idoris/contracts test:contract`（每条交叉规则至少 1 条反例被拒绝，断言错误类型而非仅断言抛错）
- **涉及文件**：`packages/contracts/src/validate.ts`
- **风险/回滚**：**涉安全**——校验器漏判等于隐私门禁失效；反例测试是唯一凭证，不得跳过
- **证据**：PR #11（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.1.4 契约成熟度标注与 README  `PR_OPEN`
- **优先级**：low
- **目标**：按 06 §10.10 诚实标注每份契约当前是 L0/L1/L2/L3，不假装可替换。
- **开发范围**：`packages/contracts/README.md` 成熟度表 + 每份 schema 顶部注释标注级别
- **依赖**：T1.1.2
- **验收命令**：`test -f packages/contracts/README.md && grep -qE 'L[0-3]' packages/contracts/README.md`
- **证据**：PR #13（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F1.2 — 能力③ 本地模型编排

### T1.2.1 LoadPolicy 抽象接口 + mock 适配器  `PR_OPEN`
- **优先级**：high
- **目标**：先定义引擎无关的 `ModelBackend` 接口并用 mock 实现，保证 Router 不依赖任何具体引擎。
- **开发范围**：接口 `list() / load(id) / unload(id) / status() / chat(req)`；mock 适配器维护内存中的 loaded 集合 + 可配置内存上限，模拟 `ok/soft/hard/ceiling` 压力分级与 LRU 驱逐。
- **明确不做**：不碰 oMLX；不做真实推理。
- **依赖**：T1.1.2
- **交付物**：`packages/adapters/src/backend.ts`、`packages/adapters/mock/`
- **验收命令**：`pnpm --filter @idoris/adapters test`（断言：pinned 模型在 ceiling 压力下不被驱逐；unpinned 按 LRU 驱逐；`admission` 正确返回 `coexist|requires_eviction`）
- **涉及文件**：`packages/adapters/`
- **风险/回滚**：无
- **证据**：PR #14（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.2.2 oMLX 适配器  `PR_OPEN`
- **优先级**：high
- **目标**：把 LoadPolicy 抽象映射到 oMLX v0.4.3 的真实 knob（U0 已实测全部端点）。
- **开发范围**：`mode:resident → model_settings.is_pinned=true`；`on_demand → unpinned`；`evict_to_load → 依赖 ProcessMemoryEnforcer`；显式 `POST /v1/models/{id}/load` `/unload`；`admission` 读 `GET /api/status` 的 `model_memory_max` 与 loaded 列表。
- **明确不做**：不封装 oMLX 的 `/v1/embeddings` `/v1/rerank` `/v1/responses`（M3 再说）；不处理 oMLX 升级（手动换 .app，用户操作）。
- **依赖**：T1.2.1
- **交付物**：`packages/adapters/omlx/`
- **验收命令**：`pnpm --filter @idoris/adapters test:integration`（本机有 oMLX 时真起 `omlx serve --memory-guard balanced --memory-guard-gb 16` 跑 load→warm-hit→evict 序列；**无 oMLX 时必须打印 SKIPPED 并以非零以外方式明示跳过，不得静默通过**）
- **涉及文件**：`packages/adapters/omlx/`
- **风险/回滚**：v0.4.3 已知小限制——VLM 引擎 guard 传播告警（`could not resolve scheduler for VLMBatchedEngine`），不阻塞，记 followup
- **证据**：PR #16（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.2.3 跨平台后端探测骨架  `PR_OPEN`
- **优先级**：mid
- **目标**：启动时按 OS/硬件选后端，**Router 核心不出现 `omlx` 字样**。
- **开发范围**：`detectBackend()`：macOS+Apple Silicon → oMLX；Win/Linux+NVIDIA → vLLM 槽位；否则 llama.cpp/Ollama 槽位。非 macOS 的实现可先是 `NotImplemented` 占位，但**接口必须齐**。
- **明确不做**：不实现 vLLM/llama.cpp 适配器本体（等真实平台需求）。
- **依赖**：T1.2.2
- **交付物**：`packages/adapters/src/detect.ts`
- **验收命令**：`pnpm --filter @idoris/adapters test` + `! grep -rn "omlx" packages/router/src`（Router 核心零引用即通过）
- **涉及文件**：`packages/adapters/src/detect.ts`
- **风险/回滚**：无
- **证据**：PR #15（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.2.4 LoadPolicy 黄金一致性测试（L3）  `PR_OPEN`
- **优先级**：mid
- **目标**：让「可替换」从承诺变成可验证的凭证——同一组序列打 mock 与 oMLX 语义等价。
- **开发范围**：一份共享测试套件，参数化跑在两个适配器上。
- **依赖**：T1.2.2、T1.2.1
- **验收命令**：`pnpm test:golden`
- **证据**：PR #17（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F1.3 — iDoris Router 薄编排层

### T1.3.1 Router 骨架：HTTP 服务 + ProviderRegistry + `/v1/models`  `PR_OPEN`
- **优先级**：high
- **目标**：起一个监听 `127.0.0.1:PORT` 的进程，从 `config/components/*.yaml` 加载组件卡（经 T1.1.3 校验）并暴露 `/v1/models`。
- **开发范围**：HTTP server；组件卡加载 + 校验 + registry；`/v1/models` 聚合各 backend 的模型清单；health/cooldown（连续 3 次失败进 30s cooldown）。
- **明确不做**：不做路由决策（T1.3.2）；不监听非 loopback 地址。
- **依赖**：T1.1.3、T1.2.1
- **交付物**：`packages/router/src/{server,registry,health}.ts`、`config/components/omlx.yaml`
- **验收命令**：`pnpm --filter @idoris/router test && pnpm smoke`（`curl -s localhost:$PORT/v1/models | jq -e '.data|length>0'`；且注入一张缺 `privacy_class` 的组件卡时**启动失败并退出非 0**）
- **涉及文件**：`packages/router/`、`config/components/`
- **风险/回滚**：**涉安全**——绑定地址必须硬编码 loopback，测试断言不监听 `0.0.0.0`
- **证据**：PR #18（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.3.2 控制面 header 解析 + routing policy 解释引擎  `PR_OPEN`
- **优先级**：high
- **目标**：意图/隐私/复杂度走 header 不走 prompt；路由规则是 YAML 数据不是代码。
- **开发范围**：解析 `X-iDoris-Privacy/Intent/Complexity/Capabilities/Fallback/Tenant` → `TaskProfile` + `TenantContext`（**缺省 privacy = `local_only`**，保守默认；`deploy_mode=tenant` 时缺 `X-iDoris-Tenant` → 400，**不得回落到「默认租户」**）；加载 `config/routing-policy.yaml`，按序匹配首条命中，`default` 必填；**严格按 [`spec.md`](spec.md)「路由决策的执行顺序」实现：隐私判定 → 预算闸门 → 意图/能力匹配**；产出候选 provider 列表。
- **明确不做**：不做语义自动识别意图（M2/T2.4.1）；不把策略逻辑写进代码分支；不做预算的持久化（T1.5.2）。
- **依赖**：T1.3.1、T1.5.1
- **交付物**：`packages/router/src/{profile,policy}.ts`、`config/routing-policy.yaml`
- **验收命令**：`pnpm --filter @idoris/router test`（覆盖：header 缺省 → local_only；非法值 → 400；规则按序首条命中；无 `default` 的 policy 文件加载失败；**tenant 模式缺 tenant header → 400 而非默认租户**）+ `pnpm test:privacy` 的顺序用例（`privacy=local_only` 且意图指向外部能力 → 走本地或报错，**证明隐私判定排在意图匹配之前**）
- **涉及文件**：`packages/router/src/`、`config/routing-policy.yaml`
- **风险/回滚**：策略文件版本化，破坏性改动升 `version`
- **证据**：PR #19（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.3.3 fail-closed 隐私门禁 + 降级链  `PR_OPEN`
- **优先级**：high
- **目标**：`local_only` 任务在本地不可用时**报错而非外泄**——这是产品承诺的落地点。
- **开发范围**：按 [`spec.md`](spec.md) 的请求路由状态机实现：`NO_CANDIDATE` 时若 `fail_closed` → 终态 503 `local_only_unavailable`；否则按 `next_in_chain` 走降级，且降级候选必须通过 `privacy_class` 与 `allowed_egress` 复核（**不信任 policy，二次校验**）。
- **明确不做**：不做重试（T1.3.4）；不做审计落盘（M2/T2.2.3）。
- **依赖**：T1.3.2
- **交付物**：`packages/router/src/dispatch.ts`
- **验收命令**：`pnpm test:privacy` —— 停掉全部本地 provider，发 20 条 `local_only` 请求，断言 **20 条全部 503 且假上游出站计数器 == 0**；任一条出站即失败
- **涉及文件**：`packages/router/src/dispatch.ts`
- **风险/回滚**：**阻断级**——此测试失败不接受「已知问题」标注，必须修到过
- **证据**：PR #20（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.3.4 `/v1/chat/completions` 转发：streaming、重试、幂等、取消  `PR_OPEN`
- **优先级**：high
- **目标**：让标准 openai SDK 不改代码即可调通，且失败行为可预期。
- **开发范围**：非流式 + SSE 流式转发；非流式最多 2 次退避重试（250ms→1s + jitter），**流式已吐 token 后不重试**，以 SSE error 事件终止；可选 `X-iDoris-Request-Id` 幂等键（60s 窗口）；客户端断开时向上游传播取消。
- **明确不做**：不做 tool-calling 的语义翻译（能力②相关，延后）；不做多模态输入。
- **依赖**：T1.3.3
- **交付物**：`packages/router/src/proxy.ts`
- **验收命令**：`pnpm smoke`（用 openai SDK 跑非流式 + 流式各一次，断言首 token 到达；断开连接后断言上游收到 abort）
- **涉及文件**：`packages/router/src/proxy.ts`
- **风险/回滚**：流式重试会导致重复 token —— 测试显式断言「已吐 token 后不重试」
- **证据**：PR #25（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.3.5 驱逐竞态互斥锁  `PR_OPEN`
- **优先级**：mid
- **目标**：防止两个请求同时驱逐彼此需要的模型形成活锁。
- **开发范围**：per-backend 互斥锁，`evict_to_load` 全程持锁；等待超时 10s → 返回 `oom` 而非无限等待。
- **依赖**：T1.3.4、T1.2.1
- **验收命令**：`pnpm --filter @idoris/router test`（并发 10 个需要互相驱逐的请求，断言无死锁、无超过 10s 的等待、全部有终态）
- **证据**：PR #26（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.3.6 出网启动断言（启动期部署配置，与运行期路由是两个洞）  `PR_OPEN`
- **优先级**：mid
- **目标**：证明 Router 进程在**处理任何请求之前**不会自己出网——`test:privacy` 测的是运行期路由，这条测的是启动期部署配置，两者抓不到对方的问题。
- **开发范围**：socket 层打桩拒绝所有非本机连接，启动 Router 后断言零出网；**必须配正对照**：一个刻意出网的用例要被探针抓到——抓不到出网的探针，它报的「零出网」什么都不证明。
- **明确不做**：不做运行期出站管控（那是 T1.4.2 的 egress-guard）。
- **依赖**：T1.3.1
- **交付物**：`packages/router/test/egress-probe.ts`（含正对照用例）
- **验收命令**：`pnpm test:egress`（零出网断言通过 **且** 正对照用例确实被探针捕获；正对照不红即判本 task 未完成）
- **涉及文件**：`packages/router/test/`
- **风险/回滚**：**涉隐私**——参考实测：某些库不设变量时零出网，但设了 tracing 类环境变量后会连外部端点。风险不在库，在部署配置
- **证据**：PR #23（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F1.4 — 能力① 订阅中转（可选 / best-effort）

### T1.4.1 subprocess 中转适配器  `PR_OPEN`
- **优先级**：mid
- **目标**：把已登录的 `claude` / `codex` 订阅态封成 OpenAI-compat provider（U0 已验证 `claude -p` 与 `codex exec` 可行）。
- **开发范围**：spawn CLI → 解析输出 → 封 OpenAI-compat 响应；120s 超时；取消/超时走 `SIGTERM → 5s → SIGKILL`，**不留孤儿进程**。
- **🔴 沙箱硬要求（评审 5.1，涉安全）**：`claude` / `codex` **不是纯模型 provider，是具备工具与工作区能力的 agent 程序**。若直接当 provider 放进推理路径，调用方就获得一条**绕过 Agent24 审批门执行工具**的通道（审批门管不到 iDoris spawn 出来的 CLI）。故该适配器**必须**运行在无工具、无工作区写权限、无任意子进程能力的沙箱里；**做不到就不得放进可信推理路径**。
- **明确不做**：不做 streaming（CLI 非交互模式先按整块返回）；不做 OAuth 路径（CLIProxyAPI 模式，备选）。
- **依赖**：T1.3.1
- **交付物**：`packages/adapters/subscription/`
- **验收命令**：`pnpm --filter @idoris/adapters test:integration`（① 本机有 `claude` 时断言 `claude -p "reply with exactly: IDORIS_RELAY_OK"` 经网关返回该字符串；② 进程清理断言 `ps` 无残留子进程；③ **沙箱断言**：喂一个诱导写文件/调工具的 prompt，断言文件系统无变化；**子进程断言是「只有那个固定二进制及其必要运行时被 spawn」，不是「零子进程」**（CLI 自己要联网读凭据，零子进程跑不起来）；另断言网络目的地、输入目录、凭据范围、输出大小四项限制生效；④ 无 CLI 时打印 SKIPPED）
- **涉及文件**：`packages/adapters/subscription/`
- **风险/回滚**：孤儿进程会吃满机器 —— 清理断言是硬性验收项
- **证据**：PR #30（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.4.2 loopback + 单用户门禁（合规红线落地）  `PR_OPEN`
- **优先级**：high
- **目标**：让「社区端/城市端绝不转发个人订阅」成为代码约束而非文档劝告。
- **开发范围**：订阅 provider 的组件卡固定 `allowed_egress: [loopback]`；Router 在 dispatch 前复核请求来源为 loopback（或用户显式开启的 Tailscale 私网白名单）；**`deploy_mode != personal`（即 `tenant` / `community` / `city`）时该 provider 拒绝注册并明确报错**——多租户不是放开这条红线的理由，组织租户用组织自己的 API（能力②）或本地模型（能力③）。
- **明确不做**：不做租户鉴权本身（T1.5.1 负责 TenantContext 的来源与校验）。
- **依赖**：T1.4.1、T1.1.3
- **交付物**：`packages/router/src/egress-guard.ts`、`config/components/subscription.yaml`
- **验收命令**：`pnpm --filter @idoris/router test`（断言：非 loopback 来源的订阅请求被拒；`IDORIS_DEPLOY_MODE` 取 `tenant` / `community` / `city` **三者中任一**时，启动即拒绝注册订阅 provider 并退出非 0）
- **涉及文件**：`packages/router/src/egress-guard.ts`
- **风险/回滚**：**涉合规**——此门禁是能力①得以存在的前提，测试不得跳过
- **证据**：PR #30（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.4.3 订阅中转不作必需 fallback 的断言  `PR_OPEN`
- **优先级**：mid
- **目标**：落实 U0 决策 S3——能力①是 best-effort，核心正确性不依赖它。
- **开发范围**：routing policy 中订阅 provider 永不出现在 `default` 链；测试断言禁用订阅 provider 后所有非订阅场景仍全绿。
- **依赖**：T1.4.2
- **验收命令**：`IDORIS_DISABLE_SUBSCRIPTION=1 pnpm test`（全绿）
- **证据**：PR #30（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F1.5 — 多租户基线（组织大脑）

> 2026-09-07 拍板 R0 新增：多租户是 iDoris 定位的一部分（「iDoris 是组织大脑，未来为组织提供服务」），不是为单个业务开的后门。
> **下游 iDoris-website 已把自己的 `products/gateway/` 降级为消费者并挂起等本节的接口契约**，故 T1.5.1 优先级最高。

### T1.5.1 `deploy_mode` + TenantContext 契约  `PR_OPEN`
- **优先级**：high
- **目标**：先把**接口契约**定下来对外发布——下游已停工等它，契约不定他们改完还要再改一遍。
- **开发范围**：`deploy_mode: personal | tenant` 配置项；`TenantContext` 的 TS 类型 + zod schema（`tenant_id` / `budget{limit_minor,spent_minor,scope}` / `billing_timezone` / `quota`）；`X-iDoris-Tenant` header 语义（tenant 模式必填，缺失 400，**不得回落默认租户**；personal 模式忽略）；产出一份对外契约文档。
- **明确不做**：不实现预算扣减（T1.5.2）、不实现隔离存储（T1.5.3）、不做租户鉴权/发证（组织侧负责，本层只消费已验明的 tenant_id）。
- **依赖**：T1.1.2
- **交付物**：`packages/contracts/src/tenant.ts`（**待做**）；`docs/agent/contract-tenancy.md`（对外契约，**已随本规划 PR 交付 v1**，下游可据此改薄客户端）
- **验收命令**：`pnpm --filter @idoris/contracts test:contract`（合法/非法 TenantContext 各若干；`billing_timezone` 缺失或非 IANA 名 → 拒绝；`budget.scope` 非枚举值 → 拒绝）且 `test -f docs/agent/contract-tenancy.md`
- **涉及文件**：`packages/contracts/src/tenant.ts`
- **风险/回滚**：契约发布后下游会照着写，破坏性改动要升版本 —— 本 task 内定版 v1
- **证据**：PR #12（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.5.2 预算闸门（终态拒绝，非降级）  `PR_OPEN`
- **优先级**：high
- **目标**：预算耗尽 → 拒绝调用并返回明确错误，**不产生任何计费调用**，也不自动降级到便宜档。
- **开发范围**：`BUDGET_CHECK` 节点置于 `POLICY_MATCHED` 之后、`CANDIDATES` 之前；超限返回 402 `budget_exceeded`，错误信息**必须说清「是预算不是故障」**；实现 `budget.scope` 两种语义——`paid_only`（默认，只闸 `cost>0` 的候选，本地模型不受影响）与 `all`（一律拒绝）。
- **明确不做**：不做预算充值/管理界面；不做限流（quota 另议）。
- **依赖**：T1.5.1、T1.3.2
- **交付物**：`packages/tenancy/src/budget.ts`
- **验收命令**：`pnpm --filter @idoris/tenancy test`（① 超预算 → 402 且**假上游计费计数器 == 0**；② 错误体含明确的预算语义标识，不与 5xx 故障混淆；③ `scope=paid_only` 时超预算仍可调本地零成本模型；④ `scope=all` 时一律拒绝；⑤ 变异测试：把「超预算拒绝」改成「降级到便宜档」必须变红）
- **风险/回滚**：**涉钱**——闸门失效等于替客户花钱；计费计数器断言不可省
- **证据**：PR #22（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.5.3 租户硬隔离的数据访问层  `PR_OPEN`
- **优先级**：high
- **目标**：A 租户查不到 B 租户的**任何一条**用量/预算/审计记录。
- **开发范围**：用量/预算/审计三类数据的访问层强制携带 tenant 作用域；**缺 tenant 上下文的查询直接抛错，而非返回全量**——靠调用方每次记得加 `where tenant_id = ?` 是失败开放。
- **明确不做**：不做跨租户聚合报表（组织管理员视角，另议）；**不替其他组件完成隔离**——iDoris 只隔离预算/用量/provider 凭证，Agent24 的会话与审批记录、Hyphae 的收件箱、agentEar 的录音缓冲、MemPalace 的记忆命名空间**各自负责各自的租户隔离**（见 [`ecosystem-boundaries.md`](ecosystem-boundaries.md) §5.2）。这条要写进对外契约，否则「多租户底座」只隔离了账单没隔离数据。
- **依赖**：T1.5.1
- **交付物**：`packages/tenancy/src/store.ts`
- **验收命令**：`pnpm test:tenancy`（① 造两个 tenant 的三类数据，断言 A 查不到 B 的任何一条；② **不带 tenant 上下文的查询抛错**而非返回全量；③ 变异测试：把作用域校验去掉必须变红）
- **风险/回滚**：**涉隐私/涉钱**——隔离失效等于跨客户数据泄漏
- **证据**：PR #21（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T1.5.4 决策 reason 可解释  `PR_OPEN`
- **优先级**：mid
- **目标**：每次路由决策带非空 `reason`，能区分「隐私强制 / 预算 / 意图匹配 / 降级」四类。
- **开发范围**：决策链各节点产出结构化 reason，进审计记录并可在响应头回传。
- **明确不做**：不做自然语言解释生成（枚举 + 结构化字段即可）。
- **依赖**：T1.3.3
- **验收命令**：`pnpm --filter @idoris/router test`（四类 reason 各一条用例；**reason 为空的决策被拒绝**；变异测试：把 reason 置空必须变红）
- **证据**：PR #24（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F2.1 — HardwareAwareModelRecommender

### T2.1.1 硬件探测 + 内存公式  `PR_OPEN`
- **优先级**：high
- **目标**：实现 07 §1–§3 的公式：`footprint = params × bpp + KV(ctx) + 开销`，Apple 预算三档。
- **开发范围**：`system_profiler`/`sysctl`/`os.cpus` 探测 `ram_gb/chip/gpu_cores`；量化 bpp 与质量表；KV 估算（层数×KV头×head_dim×ctx×kv_quant）。
- **明确不做**：不做推荐决策（T2.1.2）。
- **依赖**：T1.1.2
- **交付物**：`packages/recommender/src/{probe,memory}.ts`
- **验收命令**：`pnpm --filter @idoris/recommender test`（对照 U0 实测值断言：Qwen3-8B 4bit ≈4.48GB、VL-7B ≈8.50GB、8B 的 KV ≈9.00MB/64token，误差 <5%）
- **涉及文件**：`packages/recommender/`
- **风险/回滚**：公式偏差会导致 OOM —— 用 U0 实测数据作回归基线
- **证据**：PR #27（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T2.1.2 打分与推荐算法 + `IDORIS_CORE_MODEL` override  `PR_OPEN`
- **优先级**：high
- **目标**：按 07 §5.3 伪代码选出常驻 + 临时组合。
- **开发范围**：`catalog.yaml` 加载；常驻选 `capability.reasoning × quant.quality` 最高且放得下的；临时按需求能力逐个 admission；不下则标 `requires_eviction`；`IDORIS_CORE_MODEL` 强制时推荐模块让路但仍输出警告。
- **依赖**：T2.1.1
- **交付物**：`packages/recommender/src/recommend.ts`、`config/catalog.yaml`
- **验收命令**：`pnpm --filter @idoris/recommender test`（断言 M4/24GB profile 输出 `resident=ornith-1.0-9b@q6_k` 且 `agents-a1-35b` 判 `BLOCKED`；16GB 降到 q4/q5；32GB 允许 35B）
- **风险/回滚**：目录数据错误会推荐出跑不动的组合 —— catalog 每条附 `min_ram_gb` 硬门槛
- **证据**：PR #27（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T2.1.3 可读 tradeoff 输出 + sysctl 建议  `PR_OPEN`
- **优先级**：mid
- **目标**：推荐不是黑箱打分，要能解释为什么这么选。
- **依赖**：T2.1.2
- **验收命令**：`pnpm --filter @idoris/recommender test`（断言输出含 `warnings[]`、`recommended_sysctl.iogpu_wired_limit_mb`、非空 `tradeoff` 文本）
- **证据**：PR #27（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F2.2 — 容量接口与可观测

### T2.2.1 `GET /capabilities`  `PR_OPEN`
- **优先级**：high
- **目标**：把容量变成接口（06 §10.8），业务据此知道能否共存/要不要驱逐。
- **开发范围**：每个能力附 `resident/estimated_memory_gb/ctx_limit/queue_depth/admission_status(ready|requires_eviction|blocked)`；数据源为 recommender + backend `status()`。
- **依赖**：T2.1.2、T1.3.1
- **验收命令**：`pnpm smoke`（`curl /capabilities | jq -e '.[]|select(.admission_status)'` 非空且取值在枚举内）
- **证据**：PR #28（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T2.2.2 native 特性收进 extensions 命名空间  `PR_OPEN`
- **优先级**：low
- **目标**：防 provider 锁定（06 §10.7）——原生特性不得裸用，必须带降级声明。
- **依赖**：T1.1.2
- **验收命令**：`pnpm --filter @idoris/contracts test:contract`（无 `_degradation` 声明的 extension 被拒绝）
- **证据**：PR #28（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T2.2.3 路由决策审计日志  `PR_OPEN`
- **优先级**：mid
- **目标**：每次路由留下「选了谁、为什么、是否降级」的记录（acceptance「可审计」），且 tenant 模式下按租户隔离。
- **开发范围**：结构化审计记录，字段**穷举白名单**见 [`spec.md`](spec.md)「审计记录（AuditRecord）」：`request_id / tenant_id / component / intent / privacy / tier / provider_id / model_id / tokens_in / tokens_out / cost_minor / latency_ms / status / reason / ts_utc`。**`reason` 必须非空且能区分四类**（`privacy_enforced` / `budget` / `intent_match` / `degraded`）——一句「routed」不合格。写入走 tenant 作用域（见 T1.5.3）。**两道防线**（来自 iDoris-website 实现，Apache-2.0 可直接移植）：① 写入前对记录的**字段名**逐个比对黑名单（`prompt/prompts/input/content/text/body/messages/document/file/payload` 等 frozenset），命中即抛 `ContentLeakError` **拒绝写入**——不是静默丢弃（静默丢弃会让人以为内容被存下来了）；② 单字段 500 字符上限——长文本出现在元数据里，本身就是「有人把内容塞进来了」的信号。
- **明确不做**：**绝不记录请求或响应内容**（仅元数据）。
- **依赖**：T1.3.3、T1.5.3
- **验收命令**：`pnpm test:audit`（① 哨兵字符串不出现在日志；② 含黑名单字段名的记录**抛错**而非被清洗后写入；③ 超 500 字符的字段被拒绝。对照 iDoris-website 的两条变异测试：「字段名不再比对禁用清单」「取消 500 字符上限」，改坏后必须变红）；④ **`reason` 为空的记录被拒绝**，且四类 reason 各有一条用例
- **风险/回滚**：**涉隐私**——内容入日志等于隐私承诺作废，哨兵测试是硬性验收项
- **证据**：PR #28（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F2.3 — Agent24 集成交接

### T2.3.1 交出 R1–R6 需求并确认接口  `PR_OPEN`
- **优先级**：mid
- **目标**：按 [`../09-对Agent24的需求.md`](../09-对Agent24的需求.md) 与 Agent24 侧确认 `IDORIS_URL` provider 接入方式（纯加法零回归）。
- **明确不做**：**不改 Agent24 内核代码**——iDoris 只提需求。
- **依赖**：T1.3.4
- **验收命令**：`test -f docs/agent/handoff-agent24.md`（含 R1–R6 逐条的接口约定与联调命令）
- **证据**：PR #31（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T2.3.2 端到端联调冒烟  `PR_OPEN`
- **优先级**：mid
- **目标**：Agent24 把 `IDORIS_URL` 指过来后原有功能零回归且能用到本地模型。
- **依赖**：T2.3.1
- **验收命令**：`pnpm smoke:agent24`（Agent24 provider 列表出现 iDoris；一次 `local_only` 调用落到本地模型）
- **证据**：PR #31（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F2.4 — 语义意图路由

### T2.4.1 引入 semantic-router 做意图识别  `PR_OPEN`
- **优先级**：low
- **目标**：把 `X-iDoris-Intent` 从「调用方必须声明」升级为「未声明时可自动识别」（[`../10-入口路由模型-调研对比.md`](../10-入口路由模型-调研对比.md) 首选方案）。
- **明确不做**：不替代 header —— **显式声明永远优先于识别结果**；隐私字段绝不自动推断。
- **依赖**：T1.3.2
- **验收命令**：`pnpm --filter @idoris/router test`（断言：显式 header 存在时识别结果被忽略；`privacy` 永不被自动推断）
- **证据**：PR #32（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F2.5 — 能力② 外部 API 槽位

### T2.5.1 OpenAI-compat 上游槽位 + 冒烟  `PR_OPEN`
- **优先级**：low
- **目标**：只证明「外部槽位可接」，不做选型。
- **开发范围**：通用 OpenAI-compat 上游适配器；用 `.env` 的 `OPENAI_API_KEY` 做一次冒烟。
- **明确不做**：不做 Anthropic/Gemini 翻译；不引入 LiteLLM/ClawRouter/OmniRoute。
- **依赖**：T1.3.4
- **验收命令**：`pnpm smoke:external`（有 key 时调通一次；无 key 时打印 SKIPPED 而非失败）
- **证据**：PR #31（栈内 OPEN，未合并；本机门禁 14 项全绿）

### T2.5.2 三路由保真度矩阵（OmniRoute / ClawRouter / LiteLLM）  `BLOCKED`
- **优先级**：low
- **目标**：产出「provider × 特性 × 是否统一可用」矩阵，作为能力②的验收基线。
- **阻塞原因**：① 需要 Anthropic + Gemini API key（用户凭证，尚未提供）；② 尚无真实消费者 —— 按「先有消费者再有提供者」原则不提前做。
- **解除条件**：用户提供两把 key，**或**明确「只测 ClawRouter 免费层 + 本地模型」并接受结论范围受限。
- **依赖**：T2.5.1
- **验收命令**：`test -f docs/agent/fidelity-matrix.md`（含 streaming / tool-calling / 多模态 / thinking 四项 × 各 provider）
- **证据**：<…>

---

## F2.6 — 计费与账期

### T2.6.1 按 tenant 的月度用量聚合 + 显式账期时区  `PR_OPEN`
- **优先级**：mid
- **目标**：下游按此计费，所以「换台机器账单就变」是不可接受的失败模式。
- **开发范围**：按 `tenant_id` 聚合月度用量与成本；月份边界**用租户显式配置的 `billing_timezone`**（不取服务器时区，也不接受调用方在查询里另指定），`ts_utc` 存 UTC epoch，聚合时才做时区换算；提供余额与月度用量查询接口。**响应必须回显 `billing_timezone` 与解析出的 `range_utc` 边界**——让调用方能断言而不是只能信任。
- **明确不做**：不做发票/支付；不做跨租户账单汇总。
- **依赖**：T1.5.3、T2.2.3
- **交付物**：`packages/tenancy/src/billing.ts`
- **验收命令**：`pnpm test:billing` —— 同一批数据在 `TZ=UTC` / `TZ=Asia/Bangkok` / `TZ=Pacific/Midway` 下聚合，断言 `totals` **与 `range_utc` 都完全一致**（只断言 totals 不够：两个错误的边界也可能凑出相同的总数）。测试**必须真的切换进程时区**（`TZ` + `tzset()`）造数据；只在测试内部造时间戳的写法抓不到这个 bug。变异测试：把边界换算改成服务器时区必须变红
- **风险/回滚**：**涉钱**——上游 iDoris-website 踩过真坑：月份边界用本地时区而时间戳存 UTC，同一笔曼谷 10-01 06:00 的调用在 UTC 算 9 月、在曼谷算 10 月，换机器账单就变且无任何报错；更阴的是当时测试也用本地时区造时间戳，两边一起漂，在任何时区都自洽地全绿
- **证据**：PR #29（栈内 OPEN，未合并；本机门禁 14 项全绿）

---

## F3.x — 自增长与联邦（M3，全部 BACKLOG）

> 严格按 03 §5 的 F0 → F1 → F2 推进。**硬门禁：F3.4 隐私层就位前，真实个人数据不得进入联邦；F3.1/F3.3 只用合成/脱敏数据。**

### T3.1.1 本地数据湖 + 提炼管线（合成数据）  `PR_OPEN`
- **优先级**：low ｜ **依赖**：T2.2.3 ｜ **验收命令**：`pnpm --filter @idoris/growth test`（合成语料跑通 使用→数据湖→提炼 三段，断言产出的训练样本 schema 合法且**不含真实数据标记**）

- **证据**：PR #35（栈内 OPEN，未合并；本机门禁全绿）
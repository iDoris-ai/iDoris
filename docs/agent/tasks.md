# iDoris 统一模型服务 · 任务台账 — Task

> 前置：[`roadmap.md`](roadmap.md)（M→F）· [`architecture.md`](architecture.md) · [`spec.md`](spec.md) · [`acceptance.md`](acceptance.md)
> 每个 Task 自包含，可独立开发与验收。**验收标准必须可机器验证**（跑命令能判定）。
> 状态：BACKLOG · READY · IN_PROGRESS · BLOCKED · PR_OPEN · CHANGES_REQUESTED · APPROVED · DONE
> 记录日期：2026-09-07
> 台账同步：2026-09-22（F1.x/F2.x/F3.x 共 38 个 Task 已落 PR #9–#37，均为栈内 OPEN；T2.5.2 仍 BLOCKED）

---

## F1.1 — 契约层落地

### T1.1.1 pnpm workspace 骨架 + 门禁流水线  `DONE`
- **优先级**：high
- **目标**：起一个能跑 lint/typecheck/build/test 的 TS monorepo，后续所有 task 有落点。
- **开发范围**：`pnpm-workspace.yaml`；`packages/contracts` `packages/router` 两个空包；tsconfig（strict）、eslint、vitest、`.github/workflows/ci.yml` 跑同一套门禁。
- **明确不做**：不写任何业务逻辑；不引入 oMLX/HTTP 依赖；不配置发布流程。
- **依赖**：无
- **交付物**：`pnpm-workspace.yaml`、`package.json`（scripts: lint/typecheck/build/test）、两个包的骨架、CI workflow
- **验收命令**：`pnpm install && pnpm lint && pnpm typecheck && pnpm build && pnpm test`（全部退出 0；test 允许 0 用例但脚本必须存在且成功）
- **涉及文件**：仓库根、`packages/contracts/`、`packages/router/`、`.github/workflows/ci.yml`
- **风险/回滚**：无（纯新增）
- **证据**：PR #9（已合并进 `preview`；门禁全绿）

### T1.1.2 五份契约的 TS 类型 + zod schema  `DONE`
- **优先级**：high
- **目标**：把 06 §10 的契约片段变成可执行、可校验的类型。
- **开发范围**：`ProviderDescriptor` / `ComponentCard` / `LoadPolicy` / `RoutingPolicy` / `TaskProfile` 五份的 TS 类型与 zod schema，字段与取值严格按 [`spec.md`](spec.md) 数据模型章节。
- **明确不做**：不做 `AdapterManifest`（M3）；不做校验器的错误信息本地化；不实现任何解释/执行逻辑。
- **依赖**：T1.1.1
- **交付物**：`packages/contracts/src/{provider,component-card,load-policy,routing-policy,task-profile}.ts` + 导出的 zod schema
- **验收命令**：`pnpm --filter @idoris/contracts test`（每份契约至少 1 条合法样例通过 + 1 条缺必填字段样例被拒绝）
- **涉及文件**：`packages/contracts/`
- **风险/回滚**：契约变更影响所有下游 → 本 task 内定版为 v1，后续改动走版本化
- **证据**：PR #10（已合并进 `preview`；门禁全绿）

### T1.1.3 组件卡策略校验器（缺字段即拒绝注册）  `DONE`
- **优先级**：high
- **目标**：让「协议兼容 ≠ 路由安全」成为代码保证——缺策略字段的组件卡进不了 registry。
- **开发范围**：`validateComponentCard()`：强制 `privacy_class` / `allowed_egress` / `fallback_policy` / `fail_closed` / `version_pin` 存在；**交叉规则**：`privacy_class=local_only` 必须 `fail_closed=true`；`tier=local` 且 `locality=remote` 为非法；`allowed_egress` 含 `internet` 时 `privacy_class` 不得为 `local_only`。
- **明确不做**：不做 registry 本身（T1.3.1）；不做运行时 health 探测。
- **依赖**：T1.1.2
- **交付物**：`packages/contracts/src/validate.ts` + 非法样例表
- **验收命令**：`pnpm --filter @idoris/contracts test:contract`（每条交叉规则至少 1 条反例被拒绝，断言错误类型而非仅断言抛错）
- **涉及文件**：`packages/contracts/src/validate.ts`
- **风险/回滚**：**涉安全**——校验器漏判等于隐私门禁失效；反例测试是唯一凭证，不得跳过
- **证据**：PR #11（已合并进 `preview`；门禁全绿）

### T1.1.4 契约成熟度标注与 README  `DONE`
- **优先级**：low
- **目标**：按 06 §10.10 诚实标注每份契约当前是 L0/L1/L2/L3，不假装可替换。
- **开发范围**：`packages/contracts/README.md` 成熟度表 + 每份 schema 顶部注释标注级别
- **依赖**：T1.1.2
- **验收命令**：`test -f packages/contracts/README.md && grep -qE 'L[0-3]' packages/contracts/README.md`
- **证据**：PR #13（已合并进 `preview`；门禁全绿）

---

## F1.2 — 能力③ 本地模型编排

### T1.2.1 LoadPolicy 抽象接口 + mock 适配器  `DONE`
- **优先级**：high
- **目标**：先定义引擎无关的 `ModelBackend` 接口并用 mock 实现，保证 Router 不依赖任何具体引擎。
- **开发范围**：接口 `list() / load(id) / unload(id) / status() / chat(req)`；mock 适配器维护内存中的 loaded 集合 + 可配置内存上限，模拟 `ok/soft/hard/ceiling` 压力分级与 LRU 驱逐。
- **明确不做**：不碰 oMLX；不做真实推理。
- **依赖**：T1.1.2
- **交付物**：`packages/adapters/src/backend.ts`、`packages/adapters/mock/`
- **验收命令**：`pnpm --filter @idoris/adapters test`（断言：pinned 模型在 ceiling 压力下不被驱逐；unpinned 按 LRU 驱逐；`admission` 正确返回 `coexist|requires_eviction`）
- **涉及文件**：`packages/adapters/`
- **风险/回滚**：无
- **证据**：PR #14（已合并进 `preview`；门禁全绿）

### T1.2.2 oMLX 适配器 —— 0.6.4 已复测，pin/pressure 未通过  `DONE`

> ⚠️ **退化说明（2026-09-27，FU-16 复测 + Opus 验收 CHANGES_REQUESTED 修复后）**：本任务台账状态集合没有 `PARTIAL`，这里继续标 `DONE` 但**不代表"0.6.4 上全部功能已验证可用"**——`is_pinned`（pin/常驻）与 `pressure`（压力分级）这两项在 0.6.4 上**未验证通过**，见下方明细与 FU-17/FU-18。只有 `list/load/unload/chat/model_memory_max|used` 这几项是「0.4.3 与 0.6.4 均已实测」。

- **优先级**：high
- **目标**：把 LoadPolicy 抽象映射到 oMLX 的真实 knob。`GET /v1/models`、显式 `load`/`unload`、`chat completions`（流式+非流式）、`/api/status` 的 `model_memory_max`/`model_memory_used`（换算成 GiB 后）**0.4.3 与 0.6.4 均已实测**；`is_pinned` 与 `pressure` **仅在 0.4.3 上有过实测证据，0.6.4 上未验证通过**（见下）。
- **开发范围**：`mode:resident → is_pinned=true`（PUT 成功后还要用严格的 `verifyModelState()` 复核 `loaded===true && pinned===true` 才算数，M1；0.6.4 上因 401 会抛 `OmlxPinUnavailableError`，见下）；`on_demand`/`evict_to_load`/未传 policy → 不主动调用 pin 端点（unpinned 是 0.6.4 上刚 load 完的模型的默认状态，已实测确认，调用端点只会白白触发必 401 的 admin 校验），但会用同一个 `verifyModelState()` **严格**核对该模型的 `loaded`/`pinned` 状态——要求恰好一条匹配条目、`loaded`/`pinned` 都是 boolean 且 `loaded===true`，任何一步不满足（响应畸形、找不到条目、并发卸载）都抛 `OmlxVerificationError`，**fail-closed，不当成"未被 pin"**（Codex 复审 H1，修复了此前的 fail-open）；核对到确实被 pin 住则抛 `OmlxUnexpectedlyPinnedError`（H-a）；显式 `POST /v1/models/{id}/load` `/unload`；`admission` 读 `GET /api/status` 的 `model_memory_max`（已换算 GiB）与已加载列表。**0.6.4 复测发现并修复的漂移**：① `/api/status` 已加载列表字段名是 `loaded_models`（不是 0.4.3 假设的 `loaded`），缺失/类型不对时抛错，**元素不是字符串也抛错**（不再用 `filter` 静默丢弃，M-a）；② `model_memory_max`/`model_memory_used` 要求严格是 `number` 类型、有限、`>=0`，不做 `Number(value)` 宽松转换（不再接受数字字符串/布尔/数组/负数，Codex 复审 M3；缺失/非法时抛错、不当成 0，L-a）；③ `pressure` 缺失时返回显式 `"unknown"`，不在白名单里的值 **warn 一行后按 `"unknown"` 处理**（不抛错，避免连带炸掉同次 `status()` 里已解析好的 `loaded`，M-c）；④ 设置 `is_pinned` 从 `POST /admin/settings`（0.6.4 上已 404）搬到 `PUT /admin/api/models/{id}/settings`（body 按 openapi schema 拍平，**未实测跑通**）。**所有错误信息与 `console.warn` 只写字段名/期望类型/实际类型或下标，不把后端返回的原始值字符串化写进去，也不拼接下游抛出的 `cause.message`/`String(cause)`**（第一轮 Codex 复审 H2 先堵了字段解析这条路；`OmlxPinUnavailableError` 构造函数当时仍在拼 `cause.message`，第二轮 Codex 复审又抓出这条剩下的泄露点——已改成只暴露 `causeErrorName`/`causeHttpStatus` 这类安全的结构化元数据，不再从 `cause` 拼字符串；`verifyModelState()` 还补了顶层响应校验，`response 是 null/数字/字符串/数组时抛 `OmlxVerificationError(reason=response_invalid)` 而不是直接抛 `TypeError`）；`PUT` 成功后若复核本身失败，改抛新增的 `OmlxPinStateUnverifiedError`（状态未知），不再误判成 `OmlxPinUnavailableError`（确认失败）；四个错误类型（`OmlxPinUnavailableError`/`OmlxPinStateUnverifiedError`/`OmlxUnexpectedlyPinnedError`/`OmlxVerificationError`）都从 `@idoris/adapters` 包根导出，各带稳定的 `code` 字段。
- **已知缺口（未通过，本次未修复，见 FU-17/FU-18）**：`PUT /admin/api/models/{id}/settings` 在 0.6.4 上要求独立 admin 会话认证，仅推理 API key 会被拒绝（401，已实测），所以 `load(id,{mode:"resident"})` 在 0.6.4 上必定抛 `OmlxPinUnavailableError`——**pin/常驻语义在 0.6.4 上完全不可用**，不是"改个端点就好了"；对称地，**unpin 也不可用**——`on_demand` 检测到模型被外部 pin 住后只能抛 `OmlxUnexpectedlyPinnedError`，无法自动纠正。`pressure` 在 `--memory-guard` 模式下的字段名是否不变，本次也**未验证**（会连带卸载用户正在用的模型，未做）；`Pressure` 的 `"unknown"` 值要求消费方按保守方向处理，但目前只是类型注释里的约定，没有机制强制。
- **明确不做**：不封装 oMLX 的 `/v1/embeddings` `/v1/rerank` `/v1/responses`（Milestone M3 再说，与本轮评审的 M3 编号无关）；不处理 oMLX 升级（手动换 .app，用户操作）；本次不实现 admin 会话认证能力（跟进见 FU-17）。
- **依赖**：T1.2.1
- **交付物**：`packages/adapters/omlx/`
- **验收命令**：`pnpm --filter @idoris/adapters test:integration`（本机有 oMLX 时真起 `omlx serve --memory-guard balanced --memory-guard-gb 16` 跑 load→warm-hit→evict 序列；**无 oMLX 时必须打印 SKIPPED 并以非零以外方式明示跳过，不得静默通过**）
- **涉及文件**：`packages/adapters/omlx/`
- **风险/回滚**：v0.4.3 已知小限制——VLM 引擎 guard 传播告警（`could not resolve scheduler for VLMBatchedEngine`），不阻塞，记 FU-1。**v0.6.4 已知缺口（未通过）**：① pin (`is_pinned`) 端点需要 admin 会话认证，适配器暂无该能力，`load(id,{mode:"resident"})` 会抛 `OmlxPinUnavailableError`（FU-17）；② 对称地，`on_demand` 遇到外部已 pin 的模型会抛 `OmlxUnexpectedlyPinnedError` 但无法自动 unpin（FU-17）；③ `pressure` 在 memory-guard 模式下的字段名未验证，且 `"unknown"` 的保守处理约定没有机制强制（FU-18）
- **证据**：PR #16（已合并进 `preview`；门禁全绿）；0.6.4 复测证据见 FU-16 对应 PR #44（两轮 Opus CHANGES_REQUESTED 修复）

### T1.2.3 跨平台后端探测骨架  `DONE`
- **优先级**：mid
- **目标**：启动时按 OS/硬件选后端，**Router 核心不出现 `omlx` 字样**。
- **开发范围**：`detectBackend()`：macOS+Apple Silicon → oMLX；Win/Linux+NVIDIA → vLLM 槽位；否则 llama.cpp/Ollama 槽位。非 macOS 的实现可先是 `NotImplemented` 占位，但**接口必须齐**。
- **明确不做**：不实现 vLLM/llama.cpp 适配器本体（等真实平台需求）。
- **依赖**：T1.2.2
- **交付物**：`packages/adapters/src/detect.ts`
- **验收命令**：`pnpm --filter @idoris/adapters test` + `! grep -rn "omlx" packages/router/src`（Router 核心零引用即通过）
- **涉及文件**：`packages/adapters/src/detect.ts`
- **风险/回滚**：无
- **证据**：PR #15（已合并进 `preview`；门禁全绿）

### T1.2.4 LoadPolicy 黄金一致性测试（L3）  `DONE`
- **优先级**：mid
- **目标**：让「可替换」从承诺变成可验证的凭证——同一组序列打 mock 与 oMLX 语义等价。
- **开发范围**：一份共享测试套件，参数化跑在两个适配器上。
- **依赖**：T1.2.2、T1.2.1
- **验收命令**：`pnpm test:golden`
- **证据**：PR #17（已合并进 `preview`；门禁全绿）

---

## F1.3 — iDoris Router 薄编排层

### T1.3.1 Router 骨架：HTTP 服务 + ProviderRegistry + `/v1/models`  `DONE`
- **优先级**：high
- **目标**：起一个监听 `127.0.0.1:PORT` 的进程，从 `config/components/*.yaml` 加载组件卡（经 T1.1.3 校验）并暴露 `/v1/models`。
- **开发范围**：HTTP server；组件卡加载 + 校验 + registry；`/v1/models` 聚合各 backend 的模型清单；health/cooldown（连续 3 次失败进 30s cooldown）。
- **明确不做**：不做路由决策（T1.3.2）；不监听非 loopback 地址。
- **依赖**：T1.1.3、T1.2.1
- **交付物**：`packages/router/src/{server,registry,health}.ts`、`config/components/omlx.yaml`
- **验收命令**：`pnpm --filter @idoris/router test && pnpm smoke`（`curl -s localhost:$PORT/v1/models | jq -e '.data|length>0'`；且注入一张缺 `privacy_class` 的组件卡时**启动失败并退出非 0**）
- **涉及文件**：`packages/router/`、`config/components/`
- **风险/回滚**：**涉安全**——绑定地址必须硬编码 loopback，测试断言不监听 `0.0.0.0`
- **证据**：PR #18（已合并进 `preview`；门禁全绿）

### T1.3.2 控制面 header 解析 + routing policy 解释引擎  `DONE`
- **优先级**：high
- **目标**：意图/隐私/复杂度走 header 不走 prompt；路由规则是 YAML 数据不是代码。
- **开发范围**：解析 `X-iDoris-Privacy/Intent/Complexity/Capabilities/Fallback/Tenant` → `TaskProfile` + `TenantContext`（**缺省 privacy = `local_only`**，保守默认；`deploy_mode=tenant` 时缺 `X-iDoris-Tenant` → 400，**不得回落到「默认租户」**）；加载 `config/routing-policy.yaml`，按序匹配首条命中，`default` 必填；**严格按 [`spec.md`](spec.md)「路由决策的执行顺序」实现：隐私判定 → 预算闸门 → 意图/能力匹配**；产出候选 provider 列表。
- **明确不做**：不做语义自动识别意图（M2/T2.4.1）；不把策略逻辑写进代码分支；不做预算的持久化（T1.5.2）。
- **依赖**：T1.3.1、T1.5.1
- **交付物**：`packages/router/src/{profile,policy}.ts`、`config/routing-policy.yaml`
- **验收命令**：`pnpm --filter @idoris/router test`（覆盖：header 缺省 → local_only；非法值 → 400；规则按序首条命中；无 `default` 的 policy 文件加载失败；**tenant 模式缺 tenant header → 400 而非默认租户**）+ `pnpm test:privacy` 的顺序用例（`privacy=local_only` 且意图指向外部能力 → 走本地或报错，**证明隐私判定排在意图匹配之前**）
- **涉及文件**：`packages/router/src/`、`config/routing-policy.yaml`
- **风险/回滚**：策略文件版本化，破坏性改动升 `version`
- **证据**：PR #19（已合并进 `preview`；门禁全绿）

### T1.3.3 fail-closed 隐私门禁 + 降级链  `DONE`
- **优先级**：high
- **目标**：`local_only` 任务在本地不可用时**报错而非外泄**——这是产品承诺的落地点。
- **开发范围**：按 [`spec.md`](spec.md) 的请求路由状态机实现：`NO_CANDIDATE` 时若 `fail_closed` → 终态 503 `local_only_unavailable`；否则按 `next_in_chain` 走降级，且降级候选必须通过 `privacy_class` 与 `allowed_egress` 复核（**不信任 policy，二次校验**）。
- **明确不做**：不做重试（T1.3.4）；不做审计落盘（M2/T2.2.3）。
- **依赖**：T1.3.2
- **交付物**：`packages/router/src/dispatch.ts`
- **验收命令**：`pnpm test:privacy` —— 停掉全部本地 provider，发 20 条 `local_only` 请求，断言 **20 条全部 503 且假上游出站计数器 == 0**；任一条出站即失败
- **涉及文件**：`packages/router/src/dispatch.ts`
- **风险/回滚**：**阻断级**——此测试失败不接受「已知问题」标注，必须修到过
- **证据**：PR #20（已合并进 `preview`；门禁全绿）

### T1.3.4 `/v1/chat/completions` 转发：streaming、重试、幂等、取消  `DONE`
- **优先级**：high
- **目标**：让标准 openai SDK 不改代码即可调通，且失败行为可预期。
- **开发范围**：非流式 + SSE 流式转发；非流式最多 2 次退避重试（250ms→1s + jitter），**流式已吐 token 后不重试**，以 SSE error 事件终止；可选 `X-iDoris-Request-Id` 幂等键（60s 窗口）；客户端断开时向上游传播取消。
- **明确不做**：不做 tool-calling 的语义翻译（能力②相关，延后）；不做多模态输入。
- **依赖**：T1.3.3
- **交付物**：`packages/router/src/proxy.ts`
- **验收命令**：`pnpm smoke`（用 openai SDK 跑非流式 + 流式各一次，断言首 token 到达；断开连接后断言上游收到 abort）
- **涉及文件**：`packages/router/src/proxy.ts`
- **风险/回滚**：流式重试会导致重复 token —— 测试显式断言「已吐 token 后不重试」
- **证据**：PR #25（已合并进 `preview`；门禁全绿）

### T1.3.5 驱逐竞态互斥锁  `DONE`
- **优先级**：mid
- **目标**：防止两个请求同时驱逐彼此需要的模型形成活锁。
- **开发范围**：per-backend 互斥锁，`evict_to_load` 全程持锁；等待超时 10s → 返回 `oom` 而非无限等待。
- **依赖**：T1.3.4、T1.2.1
- **验收命令**：`pnpm --filter @idoris/router test`（并发 10 个需要互相驱逐的请求，断言无死锁、无超过 10s 的等待、全部有终态）
- **证据**：PR #26（已合并进 `preview`；门禁全绿）

### T1.3.6 出网启动断言（启动期部署配置，与运行期路由是两个洞）  `DONE`
- **优先级**：mid
- **目标**：证明 Router 进程在**处理任何请求之前**不会自己出网——`test:privacy` 测的是运行期路由，这条测的是启动期部署配置，两者抓不到对方的问题。
- **开发范围**：socket 层打桩拒绝所有非本机连接，启动 Router 后断言零出网；**必须配正对照**：一个刻意出网的用例要被探针抓到——抓不到出网的探针，它报的「零出网」什么都不证明。
- **明确不做**：不做运行期出站管控（那是 T1.4.2 的 egress-guard）。
- **依赖**：T1.3.1
- **交付物**：`packages/router/test/egress-probe.ts`（含正对照用例）
- **验收命令**：`pnpm test:egress`（零出网断言通过 **且** 正对照用例确实被探针捕获；正对照不红即判本 task 未完成）
- **涉及文件**：`packages/router/test/`
- **风险/回滚**：**涉隐私**——参考实测：某些库不设变量时零出网，但设了 tracing 类环境变量后会连外部端点。风险不在库，在部署配置
- **证据**：PR #23（已合并进 `preview`；门禁全绿）

---

## F1.4 — 能力① 订阅中转（可选 / best-effort）

### T1.4.1 subprocess 中转适配器  `DONE`
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
- **证据**：PR #30（已合并进 `preview`；门禁全绿）

### T1.4.2 loopback + 单用户门禁（合规红线落地）  `DONE`
- **优先级**：high
- **目标**：让「社区端/城市端绝不转发个人订阅」成为代码约束而非文档劝告。
- **开发范围**：订阅 provider 的组件卡固定 `allowed_egress: [loopback]`；Router 在 dispatch 前复核请求来源为 loopback（或用户显式开启的 Tailscale 私网白名单）；**`deploy_mode != personal`（即 `tenant` / `community` / `city`）时该 provider 拒绝注册并明确报错**——多租户不是放开这条红线的理由，组织租户用组织自己的 API（能力②）或本地模型（能力③）。
- **明确不做**：不做租户鉴权本身（T1.5.1 负责 TenantContext 的来源与校验）。
- **依赖**：T1.4.1、T1.1.3
- **交付物**：`packages/router/src/egress-guard.ts`、`config/components/subscription.yaml`
- **验收命令**：`pnpm --filter @idoris/router test`（断言：非 loopback 来源的订阅请求被拒；`IDORIS_DEPLOY_MODE` 取 `tenant` / `community` / `city` **三者中任一**时，启动即拒绝注册订阅 provider 并退出非 0）
- **涉及文件**：`packages/router/src/egress-guard.ts`
- **风险/回滚**：**涉合规**——此门禁是能力①得以存在的前提，测试不得跳过
- **证据**：PR #30（已合并进 `preview`；门禁全绿）

### T1.4.3 订阅中转不作必需 fallback 的断言  `DONE`
- **优先级**：mid
- **目标**：落实 U0 决策 S3——能力①是 best-effort，核心正确性不依赖它。
- **开发范围**：routing policy 中订阅 provider 永不出现在 `default` 链；测试断言禁用订阅 provider 后所有非订阅场景仍全绿。
- **依赖**：T1.4.2
- **验收命令**：`IDORIS_DISABLE_SUBSCRIPTION=1 pnpm test`（全绿）
- **证据**：PR #30（已合并进 `preview`；门禁全绿）

---

## F1.5 — 多租户基线（组织大脑）

> 2026-09-07 拍板 R0 新增：多租户是 iDoris 定位的一部分（「iDoris 是组织大脑，未来为组织提供服务」），不是为单个业务开的后门。
> **下游 iDoris-website 已把自己的 `products/gateway/` 降级为消费者并挂起等本节的接口契约**，故 T1.5.1 优先级最高。

### T1.5.1 `deploy_mode` + TenantContext 契约  `DONE`
- **优先级**：high
- **目标**：先把**接口契约**定下来对外发布——下游已停工等它，契约不定他们改完还要再改一遍。
- **开发范围**：`deploy_mode: personal | tenant` 配置项；`TenantContext` 的 TS 类型 + zod schema（`tenant_id` / `budget{limit_minor,spent_minor,scope}` / `billing_timezone` / `quota`）；`X-iDoris-Tenant` header 语义（tenant 模式必填，缺失 400，**不得回落默认租户**；personal 模式忽略）；产出一份对外契约文档。
- **明确不做**：不实现预算扣减（T1.5.2）、不实现隔离存储（T1.5.3）、不做租户鉴权/发证（组织侧负责，本层只消费已验明的 tenant_id）。
- **依赖**：T1.1.2
- **交付物**：`packages/contracts/src/tenant.ts`（**待做**）；`docs/agent/contract-tenancy.md`（对外契约，**已随本规划 PR 交付 v1**，下游可据此改薄客户端）
- **验收命令**：`pnpm --filter @idoris/contracts test:contract`（合法/非法 TenantContext 各若干；`billing_timezone` 缺失或非 IANA 名 → 拒绝；`budget.scope` 非枚举值 → 拒绝）且 `test -f docs/agent/contract-tenancy.md`
- **涉及文件**：`packages/contracts/src/tenant.ts`
- **风险/回滚**：契约发布后下游会照着写，破坏性改动要升版本 —— 本 task 内定版 v1
- **证据**：PR #12（已合并进 `preview`；门禁全绿）

### T1.5.2 预算闸门（终态拒绝，非降级）  `DONE`
- **优先级**：high
- **目标**：预算耗尽 → 拒绝调用并返回明确错误，**不产生任何计费调用**，也不自动降级到便宜档。
- **开发范围**：`BUDGET_CHECK` 节点置于 `POLICY_MATCHED` 之后、`CANDIDATES` 之前；超限返回 402 `budget_exceeded`，错误信息**必须说清「是预算不是故障」**；实现 `budget.scope` 两种语义——`paid_only`（默认，只闸 `cost>0` 的候选，本地模型不受影响）与 `all`（一律拒绝）。
- **明确不做**：不做预算充值/管理界面；不做限流（quota 另议）。
- **依赖**：T1.5.1、T1.3.2
- **交付物**：`packages/tenancy/src/budget.ts`
- **验收命令**：`pnpm --filter @idoris/tenancy test`（① 超预算 → 402 且**假上游计费计数器 == 0**；② 错误体含明确的预算语义标识，不与 5xx 故障混淆；③ `scope=paid_only` 时超预算仍可调本地零成本模型；④ `scope=all` 时一律拒绝；⑤ 变异测试：把「超预算拒绝」改成「降级到便宜档」必须变红）
- **风险/回滚**：**涉钱**——闸门失效等于替客户花钱；计费计数器断言不可省
- **证据**：PR #22（已合并进 `preview`；门禁全绿）

### T1.5.3 租户硬隔离的数据访问层  `DONE`
- **优先级**：high
- **目标**：A 租户查不到 B 租户的**任何一条**用量/预算/审计记录。
- **开发范围**：用量/预算/审计三类数据的访问层强制携带 tenant 作用域；**缺 tenant 上下文的查询直接抛错，而非返回全量**——靠调用方每次记得加 `where tenant_id = ?` 是失败开放。
- **明确不做**：不做跨租户聚合报表（组织管理员视角，另议）；**不替其他组件完成隔离**——iDoris 只隔离预算/用量/provider 凭证，Agent24 的会话与审批记录、Hyphae 的收件箱、agentEar 的录音缓冲、MemPalace 的记忆命名空间**各自负责各自的租户隔离**（见 [`ecosystem-boundaries.md`](ecosystem-boundaries.md) §5.2）。这条要写进对外契约，否则「多租户底座」只隔离了账单没隔离数据。
- **依赖**：T1.5.1
- **交付物**：`packages/tenancy/src/store.ts`
- **验收命令**：`pnpm test:tenancy`（① 造两个 tenant 的三类数据，断言 A 查不到 B 的任何一条；② **不带 tenant 上下文的查询抛错**而非返回全量；③ 变异测试：把作用域校验去掉必须变红）
- **风险/回滚**：**涉隐私/涉钱**——隔离失效等于跨客户数据泄漏
- **证据**：PR #21（已合并进 `preview`；门禁全绿）

### T1.5.4 决策 reason 可解释  `DONE`
- **优先级**：mid
- **目标**：每次路由决策带非空 `reason`，能区分「隐私强制 / 预算 / 意图匹配 / 降级」四类。
- **开发范围**：决策链各节点产出结构化 reason，进审计记录并可在响应头回传。
- **明确不做**：不做自然语言解释生成（枚举 + 结构化字段即可）。
- **依赖**：T1.3.3
- **验收命令**：`pnpm --filter @idoris/router test`（四类 reason 各一条用例；**reason 为空的决策被拒绝**；变异测试：把 reason 置空必须变红）
- **证据**：PR #24（已合并进 `preview`；门禁全绿）

---

## F2.1 — HardwareAwareModelRecommender

### T2.1.1 硬件探测 + 内存公式  `DONE`
- **优先级**：high
- **目标**：实现 07 §1–§3 的公式：`footprint = params × bpp + KV(ctx) + 开销`，Apple 预算三档。
- **开发范围**：`system_profiler`/`sysctl`/`os.cpus` 探测 `ram_gb/chip/gpu_cores`；量化 bpp 与质量表；KV 估算（层数×KV头×head_dim×ctx×kv_quant）。
- **明确不做**：不做推荐决策（T2.1.2）。
- **依赖**：T1.1.2
- **交付物**：`packages/recommender/src/{probe,memory}.ts`
- **验收命令**：`pnpm --filter @idoris/recommender test`（对照 U0 实测值断言：Qwen3-8B 4bit ≈4.48GB、VL-7B ≈8.50GB、8B 的 KV ≈9.00MB/64token，误差 <5%）
- **涉及文件**：`packages/recommender/`
- **风险/回滚**：公式偏差会导致 OOM —— 用 U0 实测数据作回归基线
- **证据**：PR #27（已合并进 `preview`；门禁全绿）

### T2.1.2 打分与推荐算法 + `IDORIS_CORE_MODEL` override  `DONE`
- **优先级**：high
- **目标**：按 07 §5.3 伪代码选出常驻 + 临时组合。
- **开发范围**：`catalog.yaml` 加载；常驻选 `capability.reasoning × quant.quality` 最高且放得下的；临时按需求能力逐个 admission；不下则标 `requires_eviction`；`IDORIS_CORE_MODEL` 强制时推荐模块让路但仍输出警告。
- **依赖**：T2.1.1
- **交付物**：`packages/recommender/src/recommend.ts`、`config/catalog.yaml`
- **验收命令**：`pnpm --filter @idoris/recommender test`（断言 M4/24GB profile 输出 `resident=ornith-1.0-9b@q6_k` 且 `agents-a1-35b` 判 `BLOCKED`；16GB 降到 q4/q5；32GB 允许 35B）
- **风险/回滚**：目录数据错误会推荐出跑不动的组合 —— catalog 每条附 `min_ram_gb` 硬门槛
- **证据**：PR #27（已合并进 `preview`；门禁全绿）

### T2.1.3 可读 tradeoff 输出 + sysctl 建议  `DONE`
- **优先级**：mid
- **目标**：推荐不是黑箱打分，要能解释为什么这么选。
- **依赖**：T2.1.2
- **验收命令**：`pnpm --filter @idoris/recommender test`（断言输出含 `warnings[]`、`recommended_sysctl.iogpu_wired_limit_mb`、非空 `tradeoff` 文本）
- **证据**：PR #27（已合并进 `preview`；门禁全绿）

---

## F2.2 — 容量接口与可观测

### T2.2.1 `GET /capabilities`  `DONE`
- **优先级**：high
- **目标**：把容量变成接口（06 §10.8），业务据此知道能否共存/要不要驱逐。
- **开发范围**：每个能力附 `resident/estimated_memory_gb/ctx_limit/queue_depth/admission_status(ready|requires_eviction|blocked)`；数据源为 recommender + backend `status()`。
- **依赖**：T2.1.2、T1.3.1
- **验收命令**：`pnpm smoke`（`curl /capabilities | jq -e '.[]|select(.admission_status)'` 非空且取值在枚举内）
- **证据**：PR #28（已合并进 `preview`；门禁全绿）

### T2.2.2 native 特性收进 extensions 命名空间  `DONE`
- **优先级**：low
- **目标**：防 provider 锁定（06 §10.7）——原生特性不得裸用，必须带降级声明。
- **依赖**：T1.1.2
- **验收命令**：`pnpm --filter @idoris/contracts test:contract`（无 `_degradation` 声明的 extension 被拒绝）
- **证据**：PR #28（已合并进 `preview`；门禁全绿）

### T2.2.3 路由决策审计日志  `DONE`
- **优先级**：mid
- **目标**：每次路由留下「选了谁、为什么、是否降级」的记录（acceptance「可审计」），且 tenant 模式下按租户隔离。
- **开发范围**：结构化审计记录，字段**穷举白名单**见 [`spec.md`](spec.md)「审计记录（AuditRecord）」：`request_id / tenant_id / component / intent / privacy / tier / provider_id / model_id / tokens_in / tokens_out / cost_minor / latency_ms / status / reason / ts_utc`。**`reason` 必须非空且能区分四类**（`privacy_enforced` / `budget` / `intent_match` / `degraded`）——一句「routed」不合格。写入走 tenant 作用域（见 T1.5.3）。**两道防线**（来自 iDoris-website 实现，Apache-2.0 可直接移植）：① 写入前对记录的**字段名**逐个比对黑名单（`prompt/prompts/input/content/text/body/messages/document/file/payload` 等 frozenset），命中即抛 `ContentLeakError` **拒绝写入**——不是静默丢弃（静默丢弃会让人以为内容被存下来了）；② 单字段 500 字符上限——长文本出现在元数据里，本身就是「有人把内容塞进来了」的信号。
- **明确不做**：**绝不记录请求或响应内容**（仅元数据）。
- **依赖**：T1.3.3、T1.5.3
- **验收命令**：`pnpm test:audit`（① 哨兵字符串不出现在日志；② 含黑名单字段名的记录**抛错**而非被清洗后写入；③ 超 500 字符的字段被拒绝。对照 iDoris-website 的两条变异测试：「字段名不再比对禁用清单」「取消 500 字符上限」，改坏后必须变红）；④ **`reason` 为空的记录被拒绝**，且四类 reason 各有一条用例
- **风险/回滚**：**涉隐私**——内容入日志等于隐私承诺作废，哨兵测试是硬性验收项
- **证据**：PR #28（已合并进 `preview`；门禁全绿）

---

## F2.3 — Agent24 集成交接

### T2.3.1 交出 R1–R6 需求并确认接口  `DONE`
- **优先级**：mid
- **目标**：按 [`../09-对Agent24的需求.md`](../09-对Agent24的需求.md) 与 Agent24 侧确认 `IDORIS_URL` provider 接入方式（纯加法零回归）。
- **明确不做**：**不改 Agent24 内核代码**——iDoris 只提需求。
- **依赖**：T1.3.4
- **验收命令**：`test -f docs/agent/handoff-agent24.md`（含 R1–R6 逐条的接口约定与联调命令）
- **证据**：PR #31（已合并进 `preview`；门禁全绿）

### T2.3.2 端到端联调冒烟  `DONE`
- **优先级**：mid
- **目标**：Agent24 把 `IDORIS_URL` 指过来后原有功能零回归且能用到本地模型。
- **依赖**：T2.3.1
- **验收命令**：`pnpm smoke:agent24`（Agent24 provider 列表出现 iDoris；一次 `local_only` 调用落到本地模型）
- **证据**：PR #31（已合并进 `preview`；门禁全绿）

---

## F2.4 — 语义意图路由

### T2.4.1 引入 semantic-router 做意图识别  `DONE`
- **优先级**：low
- **目标**：把 `X-iDoris-Intent` 从「调用方必须声明」升级为「未声明时可自动识别」（[`../10-入口路由模型-调研对比.md`](../10-入口路由模型-调研对比.md) 首选方案）。
- **明确不做**：不替代 header —— **显式声明永远优先于识别结果**；隐私字段绝不自动推断。
- **依赖**：T1.3.2
- **验收命令**：`pnpm --filter @idoris/router test`（断言：显式 header 存在时识别结果被忽略；`privacy` 永不被自动推断）
- **证据**：PR #32（已合并进 `preview`；门禁全绿）

---

## F2.5 — 能力② 外部 API 槽位

### T2.5.1 OpenAI-compat 上游槽位 + 冒烟  `DONE`
- **优先级**：low
- **目标**：只证明「外部槽位可接」，不做选型。
- **开发范围**：通用 OpenAI-compat 上游适配器；用 `.env` 的 `OPENAI_API_KEY` 做一次冒烟。
- **明确不做**：不做 Anthropic/Gemini 翻译；不引入 LiteLLM/ClawRouter/OmniRoute。
- **依赖**：T1.3.4
- **验收命令**：`pnpm smoke:external`（有 key 时调通一次；无 key 时打印 SKIPPED 而非失败）
- **证据**：PR #31（已合并进 `preview`；门禁全绿）

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

### T2.6.1 按 tenant 的月度用量聚合 + 显式账期时区  `DONE`
- **优先级**：mid
- **目标**：下游按此计费，所以「换台机器账单就变」是不可接受的失败模式。
- **开发范围**：按 `tenant_id` 聚合月度用量与成本；月份边界**用租户显式配置的 `billing_timezone`**（不取服务器时区，也不接受调用方在查询里另指定），`ts_utc` 存 UTC epoch，聚合时才做时区换算；提供余额与月度用量查询接口。**响应必须回显 `billing_timezone` 与解析出的 `range_utc` 边界**——让调用方能断言而不是只能信任。
- **明确不做**：不做发票/支付；不做跨租户账单汇总。
- **依赖**：T1.5.3、T2.2.3
- **交付物**：`packages/tenancy/src/billing.ts`
- **验收命令**：`pnpm test:billing` —— 同一批数据在 `TZ=UTC` / `TZ=Asia/Bangkok` / `TZ=Pacific/Midway` 下聚合，断言 `totals` **与 `range_utc` 都完全一致**（只断言 totals 不够：两个错误的边界也可能凑出相同的总数）。测试**必须真的切换进程时区**（`TZ` + `tzset()`）造数据；只在测试内部造时间戳的写法抓不到这个 bug。变异测试：把边界换算改成服务器时区必须变红
- **风险/回滚**：**涉钱**——上游 iDoris-website 踩过真坑：月份边界用本地时区而时间戳存 UTC，同一笔曼谷 10-01 06:00 的调用在 UTC 算 9 月、在曼谷算 10 月，换机器账单就变且无任何报错；更阴的是当时测试也用本地时区造时间戳，两边一起漂，在任何时区都自洽地全绿
- **证据**：PR #29（已合并进 `preview`；门禁全绿）

---

## F3.x — 自增长与联邦（M3）

> 严格按 03 §5 的 F0 → F1 → F2 推进。**硬门禁：F3.4 隐私层就位前，真实个人数据不得进入联邦；F3.1/F3.3 只用合成/脱敏数据。**

### T3.1.1 本地数据湖 + 提炼管线（合成数据）  `DONE`
- **优先级**：low ｜ **依赖**：T2.2.3 ｜ **验收命令**：`pnpm --filter @idoris/growth test`（合成语料跑通 使用→数据湖→提炼 三段，断言产出的训练样本 schema 合法且**不含真实数据标记**）
- **证据**：PR #35（已合并进 `preview`；门禁全绿）

### T3.1.2 MLX-LoRA 本地训练 + 热挂载  `DONE`
- **优先级**：low ｜ **依赖**：T3.1.1、T3.2.1 ｜ **风险**：python 3.9.6 可能不满足 mlx-lm（需 3.10+），U0 已记 ｜ **验收命令**：`pnpm --filter @idoris/growth test:integration`（训练出一个 rank=16 的 adapter 并经 Router 热挂载后可推理；无 MLX 环境打印 SKIPPED）
- **证据**：PR #36（已合并进 `preview`；门禁全绿）

### T3.2.1 AdapterManifest + base 一致性门禁  `DONE`
- **优先级**：low ｜ **依赖**：T1.1.2 ｜ **风险**：**涉正确性**——指纹校验失效会「一次静默升级毁掉整批 LoRA」 ｜ **验收命令**：`pnpm --filter @idoris/contracts test:contract`（断言 base/tokenizer digest 不匹配的 manifest 被拒绝挂载与聚合）
- **证据**：PR #34（已合并进 `preview`；门禁全绿）

### T3.3.1 Flower 最小联邦（同 base、只传 LoRA、FedAvg）  `DONE`
- **优先级**：low ｜ **依赖**：T3.1.2、T3.2.1 ｜ **验收命令**：`pnpm --filter @idoris/federation test:integration`（两个本地客户端 + 合成数据跑通一轮聚合；断言传输载荷**只含 adapter 权重、不含原始样本**）
- **证据**：PR #37（已合并进 `preview`；门禁全绿）

### T3.4.1 DP-FedLoRA 加噪 + 安全聚合  `DONE`
- **优先级**：low ｜ **依赖**：T3.3.1 ｜ **验收命令**：`pnpm --filter @idoris/federation test tests/dp.test.ts`（① ε 越小 σ 越大且与解析式一致；② 同 seed 同噪声、σ=0 原样返回；③ 掩码上传量 ≠ 明文而求和与明文和一致，即 δ_ij=-δ_ji 真的抵消）
- **证据**：PR #37（已合并进 `preview`；门禁全绿）

### T3.4.2 真实数据准入门禁  `DONE`
- **优先级**：low ｜ **依赖**：T3.4.1 ｜ **验收命令**：`pnpm --filter @idoris/federation test`（断言隐私层未启用时，标记为真实数据的样本**无法**进入联邦管线，报错而非跳过）
- **证据**：PR #37（已合并进 `preview`；门禁全绿）

---

## 跟进项账本（followups）

| # | 来源 | 内容 | 状态 |
|:---|:---|:---|:---|
| FU-1 | U0 实测 | oMLX v0.4.3 VLM 引擎 guard 传播告警（`could not resolve scheduler for VLMBatchedEngine`），不阻塞，等新版 .app。**2026-09-27 复测**：本机已是 0.6.4（`/Applications/oMLX.app`），该告警未复现验证（本次复测未跑 memory-guard 场景，见 FU-16/FU-18） | OPEN |
| FU-2 | U0 环境探测 | 本机 python 3.9.6，mlx-lm 训练可能需 3.10+，M3 开工前处理 | OPEN |
| FU-3 | 05 §8 第 5 条 | 凭证网关（onecli 式 MITM）2026-09-07 拍板记 BACKLOG；architecture 已留 `CredentialProvider` 抽象位 | OPEN |
| FU-4 | 跨仓库 | iDoris-website PR #4 的 R0（网关归属 + 多租户语义）**已拍板归 iDoris**（2026-09-07），落为 F1.5 + F2.6；下游 `products/gateway/` 降级为消费者 | CLOSED |
| FU-5 | 跨仓库 R6 | 账期/时区：R0 归 iDoris 后，多租户用量聚合就在本层，**已从跟进项升为正式 task T2.6.1** | CLOSED |
| FU-6 | 跨仓库 R1 | `budget_exceeded` 终态已写进 spec 状态机，**已落为 T1.5.2**；本仓库另加了一处细化：`budget.scope` 区分 `paid_only`/`all`，因为 iDoris 有零成本本地模型，一刀切会让超预算租户连不花钱的本地推理都用不了 | CLOSED |
| FU-7 | 跨仓库移交 | 接收 iDoris-website 的 `routing.py`(10 条变异) / `audit.py` / `egress_guard.py`(16 条变异)，Apache-2.0；对方保留一份直到我方跑通，避免出现「两边都没有」的窗口 | OPEN |
| FU-9 | 生态边界 | [`ecosystem-boundaries.md`](ecosystem-boundaries.md) §7 四条待拍板：B1 双 harness 二选一（**最迫近**，下游已在跑 B）· B2 MemPalace 独立与否 · B3 agentEar 立项与归属 · B4 模型制品层时机 | OPEN |
| FU-10 | 跨仓库 | iDoris-website `docs/business/INDEX-产品设计总览.md` §3 的「113 条变异」与「没有一条连过模型」范围不符：Documents 72 + Creative 15 = **87** 才是该句点名的范围；Assistant 16 + Gateway 10 那 26 条不在「连没连过模型」这个轴上（测的是启动期环境变量与路由顺序），被那句话罩住反显得更空。已转告作者 | OPEN |
| FU-13 | 接口缺口（本轮核 Agent24 ADR-032 时暴露）| **没有生产默认端口**：`startRouter` 的 `port` 默认 `0`（随机，供测试并行），于是下游无法据此写 `IDORIS_URL`，而我们既没有 `IDORIS_PORT` 也没有固定默认值。部署方目前只能自己钉死端口 | PR_OPEN（T4.1）：新增 `packages/router/src/cli.ts`（bin `idoris-router serve`），读 `IDORIS_PORT`（缺省 `8740`，非法值直接启动失败非 0 退出，不静默回落）/`IDORIS_COMPONENTS_DIR`（缺省 `config/components`）/`IDORIS_ROUTING_POLICY`（可选）；`startRouter` 本身的 `port` 缺省仍是 `0`，测试行为不变 |
| FU-14 | 架构缺口 | **personal 模式没有「调用方身份」概念**：loopback 绑定限制的是**可达范围**不是授权，同机任何进程都能调、不需任何凭据。所以「同一实例按调用方区分策略」（如本人聊天可用订阅、Agent24 模块不可用）目前做不到，只能靠起第二个 Router 实例（进程边界=授权边界）。要做就得在 routing policy 加规则维度并升契约版本，**不走 caller header**（调用方自带排除项是失败开放） | OPEN |
| FU-15 | 护栏（三次同类事故后立的）| **新增带状态的组件必须主动过一遍 tenancy 层**。#21/#22 建立的「租户硬隔离」只覆盖数据访问层(usage/budget/audit)；此后 #25 的响应缓存、以及合并 #32 时的 deployMode 透传，都是**全新状态没有自动继承那套隔离**。硬隔离目前是一条**约定**，不是新代码会自动撞上的护栏 —— 在它成为机械强制之前，每新增一处状态都要手工核对 | OPEN |
| FU-16 | 版本漂移（本轮评审发现）| **`version_pin` 指向一个从未复测过端点的版本**：`config/components/omlx.yaml` pin 的是 `omlx@0.6.4`（与本机 `/Applications/oMLX.app` 实测一致），但 T1.2.2 的适配器是照 **v0.4.3** 的 knob 写的，U0 实测日志也只覆盖 0.4.3。两者之间隔着两个 minor 版本，没有任何证据说明端点未变。**待办**：拿 API key 重跑一遍 U0 的端点清单打在 0.6.4 上，确认 `is_pinned` / `/v1/models/{id}/load|unload` / `/api/status` 的 `model_memory_max` 语义没变，然后把 T1.2.2、FU-1、`spike/u0/U0-LOG.md` 的版本号一并更正；在此之前，「已实测全部端点」这句只对 0.4.3 成立。**2026-09-27 复测结论**：`GET /v1/models`、`/v1/models/{id}/load\|unload`、chat completions（流式+非流式）未变；`/api/status` 的 `model_memory_max`/`model_memory_used` 字段名未变但**单位是字节**（已修，换算成 GiB，见 T1.2.2）。发现并修复的漂移：① `/api/status` 已加载列表字段名从（假设的）`loaded` 变成 `loaded_models`，且缺失/类型不对时改成抛错而不是静默返回 `[]`；② `pressure` 缺失时改成显式 `"unknown"`（不是 fail-open 的 `"ok"`），非法值**不抛错**——warn 一行（固定脱敏文案，只报告类型）后按 `"unknown"` 处理，避免连带炸掉同次 `status()` 里已解析好的 `loaded`；③ 设置 `is_pinned` 的端点从 `POST /admin/settings`（0.6.4 已 404）搬到 `PUT /admin/api/models/{id}/settings`（body 仅按 openapi schema 编写，未实测跑通）。**未通过、未来跟进拆分到 FU-17（admin 会话能力，pin 在 0.6.4 上完全不可用）和 FU-18（`pressure` 在 memory-guard 模式下的字段名未验证）**。详见 `spike/u0/U0-LOG.md`「0.6.4 复测（2026-09-27）」与 PR #44（历经两轮 Opus CHANGES_REQUESTED + 一轮 Codex CHANGES_REQUESTED 修复） | PR_OPEN |
| FU-17 | FU-16 复测暴露（Opus 验收 High 项，第二轮 CHANGES_REQUESTED 补充）| **oMLX 0.6.4 的 pin (`is_pinned`) 语义完全不可用，且这是双向的**：① 设置 pin 的端点 `PUT /admin/api/models/{id}/settings` 要求独立的 admin 会话认证（`/admin/api/login` 一类），仅推理用的 `Authorization: Bearer <API key>` 会被拒绝（401 `Admin authentication required`，已实测），`load(id,{mode:"resident"})` 会抛 `OmlxPinUnavailableError`（模型已加载但未 pin）；② **反向的 unpin 同样不可用**：`on_demand`/`evict_to_load` 加载完成后，适配器会读 `GET /v1/models/status`（**已实测确认**这个端点用推理 API key 就能读，200，只读，不受 admin 会话限制）核对该模型是否被外部 pin 住（用户在 oMLX 管理页手动 pin、pin 状态跨重启持久化、或曾经切换成过 resident），若确实是 pinned 就抛 `OmlxUnexpectedlyPinnedError`——但适配器**检测到之后没有能力去 unpin 它**，因为 unpin 走的还是同一个需要 admin 会话的端点。也就是说：pin 得上但读不出真状态是一种缺口，pin 上了想摘不掉是另一种缺口，两者都指向同一个根因（缺 admin 会话）。**待办**：调研 oMLX admin 会话的建立方式（`/admin/api/login` 需要什么凭证、会话如何保持/续期、是否有等价的 API-key 路径），评估是否值得给 `OmlxBackend` 加一个 admin 会话能力；在此之前 `mode:"resident"` 在 0.6.4 上不可用，`on_demand` 遇到外部已 pin 的模型也只能报错、不能自动纠正 | OPEN |
| FU-18 | FU-16 复测暴露（Opus 验收 Medium 项，第二轮 CHANGES_REQUESTED 补充一条）| **`pressure`（ok/soft/hard/ceiling）在 0.6.4 memory-guard 模式下的字段名未验证**：2026-09-27 复测用的实例是不带 `--memory-guard` 启动的（避免重启卸载用户正在用的模型），`/api/status` 响应里完全没有 `pressure` 字段，适配器按此返回 `"unknown"`。但 0.4.3 时代该字段名是否在 0.6.4 的 memory-guard 模式下保持不变，尚无实测证据。**待办**：找机会（新起一个隔离的 `omlx serve --memory-guard ...` 进程，不影响用户当前实例；或者征得用户同意临时重启）验证 `pressure` 字段名与取值集合在 0.6.4 上是否与 0.4.3 一致。**L-c 补记**：`Pressure` 类型里 `"unknown"` 的语义是"消费方必须把它视为至少 `soft`"（保守方向），但**目前没有任何机制强制这一点**——`packages/adapters/src/backend.ts` 只在类型注释里写了这句话，第一个真正读 `.pressure` 做决策的消费方出现时，必须自己遵守这条约定，没有编译期或运行期的校验会替它把关 | OPEN |
| FU-12 | 评审 | codex 指出 B1 最可能在六个月后被推翻，**触发条件很低**：出现第一条同时依赖内容+收件人+渠道+副作用的策略即可（如「金额超 ฿10,000 或群聊含非客户成员时，发账单必须人工批准」）。届时会改成「Python 提供签名的领域校验证据，Rust 持唯一授权状态机与最终否决权」——本稿已按这个形状写，但要盯着别退回「动作/输出互不重叠」的旧说法 | OPEN |
| FU-11 | License 红线（自下游 `oss-due-diligence.md` 引入）| **LiteLLM `enterprise/` 目录绝不引用**（若将来做能力②）· **Dify 禁多租户**——多租户现已归 iDoris，此条直接约束选型 · ComfyUI GPL 只能隔离进程调用 | OPEN |
| FU-8 | 验收方法论 | **「绿灯不代表你以为的那件事成立」**——两半：① **断言错了**（异常子类被父类 `expect_raises` 吞掉；无出处答案被数字校验误接住，换成不含数字的答案就放行）；② **检查不承重**（某步骤去掉后整套自检仍全绿）。<br>**根因常是量纲不匹配**：判据全写成「至少有 N 个」，而想抓的错误方向是「你多算了」——「至少」型判据测不出多算，那格正对照**从一开始就不可能承重**。**检查的量纲要和它想抓的错误方向对得上。**<br>我方对应防御：`test:privacy` 的出站计数器（不只断言 503）、`test:billing` 的 `range_utc`（不只断言 totals）、`test:egress` 的正对照、T1.5.2/3/4 的配对变异测试。共同点是**不给自己留一条「看起来做了」的退路**。<br>**待办**：把这条写进未来每个 task 的验收设计检查——新增验收命令时问一句「这个断言能不能因为别的原因变绿？它的量纲对得上要抓的错误方向吗？」 | OPEN |
| FU-19 | T4.2 角色枚举统一 PR 评审（M4）| **`fast`/`daily` 的机械改名与规范尺寸定义冲突**：`docs/interfaces/iDoris-Agent24-边界与接口规范.md` §3.12 定义 `fast`=常驻 1-4B 延迟优先、`daily`=7-12B 质量优先，但 `config/catalog.yaml` 里把旧枚举 `core`→`daily` 是**逐字机械改名**（角色语义不变，只换名字），没有按新的尺寸定义重新分类，导致 `nanbeige4.2-3b`（约 4B，标了 daily）、`granite-4.2-3b`（3B，标了 daily）、`qwen3-8b`（8.19B，标了 fast）都不在规范给出的尺寸区间内。<br>**根因**：`roles` 这个字段目前同时承担两个含义——「对外通过 idoris/<role> 暴露成哪个角色」和「是否参与常驻自动推荐（isEligibleForRole 的 daily 分支）」，两者耦合在一起，按尺寸重新分类会同时改变对外角色语义**和**常驻推荐结果（哪个模型被自动选中常驻），不是纯重命名。<br>**待办**：拆出一个独立的 `resident_eligible`（或等价）字段，把「参与常驻自动推荐」和「对外角色」解耦，然后按规范尺寸重新分类 `roles`；这会改变现有 24GB/16GB/32GB 档的常驻推荐结果（`recommend.test.ts` 里的验收基线断言要跟着改），**需要 jason 拍板**再动手。本 PR（T4.2）只做枚举改名，不动这条。 | OPEN |
| FU-20 | T4.1 code review（M5）| **审计接入 server（record_id 可查）**：目前 `packages/router/src/server.ts` 每请求只在 stderr 打一行结构化 JSON（`record_id/provider/served_locality/status/duration_ms`），这是个临时兜底，不是真正的审计落地——没有持久化、没有查询接口，重启进程或日志轮转后就查不到了。`packages/router/src/audit.ts` 已有 `AuditWriter` 之类的审计基础设施但没有接到 server 的请求路径上。**待办**：评估把这行 stderr 日志升级成走 `AuditWriter`（或等价的持久化审计通道），让 `record_id` 真正可查、可追溯，而不是只在进程存活期间的 stderr 里 | OPEN |
| FU-21 | T4.1 code review（L2）| **`CONTRACT_VERSION` 未纳入契约漂移检查**：`packages/contracts/src/version.ts` 的 `CONTRACT_VERSION` 是手写常量，不经过 `scripts/gen-contracts.mjs` 生成管线，`pnpm check:contract-drift` 也就管不到它——`packages/contracts/schema/*.schema.json` 实际发生不兼容变更（哈希变化）时，没有任何机制强制要求同步升 `CONTRACT_VERSION`，容易出现"契约变了、版本号没动"的静默漂移。**待办**：设计一种把 schema 内容哈希（或版本）与 `CONTRACT_VERSION` 绑定校验的机制（例如漂移脚本里加一条"schema 变更但 CONTRACT_VERSION 未变则报警"的检查），并决定这条规则该多严格（是否所有 schema 变更都必须升版本，还是只有破坏性变更才需要） | OPEN |
| FU-22 | R2-G（Rust `idoris` 直连转发路径）| **`LoadMode::Resident` `http_service` 卡的直连转发路径（`idoris-router::proxy::ChatProxy`）完全没有接预算 reserve/settle**（TS 参考实现 `proxy.ts` 同样没有——本来就不是移植缺口，是这条路径从设计上就还没接预算）。v0.x 暂以 `components::load_components` 启动期硬闸挡住：任何 `Resident` + `http_service` 卡若价格不是可证明的 `0`（付费，或价格未知/畸形，如 NaN/Infinity），直接拒绝启动（`LoadError::PaidResidentUnsupported`），不允许起服务后悄悄绕过预算。**待办**：给直连转发路径接上 `idoris-tenancy` 的 reserve/settle（复用 `dispatch::budget` 模块已有的 estimate/reserve/settle/release helper，模式对齐 `dispatch::dispatch_local` 现有的 Supervisor 路径），届时可以放开这道启动期硬闸 | OPEN |
| FU-23 | R2-C 复验遗留（Low）| **改时区后账期窗口漏计**：租户只有一个子维度、且尚未做租户配置时，若这个子维度在没有 active 预留的情况下改了时区，旧时区账期键里已经 settle 的花费不会计入新租户限额。修法：`configure` 改时区前检查当前账期的 `tenant_periods`/`budget_periods`，不为空则拒绝 | OPEN |
| FU-24 | R2-C 复验遗留（Low）| **migration 0004 没有回填 `tenant_period`**：迁移之前就处于 active 的预留，settle/release/extend 时会报 Storage 错误。目前还没有线上老库；要兼容老库时补一句 `UPDATE reservations SET tenant_period = period WHERE tenant_period IS NULL` | OPEN |
| FU-25 | R2-A 复验遗留（Low）| **chat 与 load/unload 共用 `adapter_call_timeout`（默认 30s）**：长生成会被截断。应给 chat 单独设超时，流式场景按 token 空闲超时处理 | OPEN |
| FU-26 | Codex 额度耗尽（2026-09-27 至 10-04）| **Rust 内核整栈要补一轮 Tier 1（Codex）复审**：R2-A/B/C/D/E/G 后段只经过 Tier 2（Opus 本地 + prdaemon）。10/04 额度恢复后，重点复审 supervisor.rs、policy/registry.rs、tenancy/ledger.rs、router/proxy.rs | OPEN |
| FU-27 | #48 第二轮评审遗留（Low）| **reload 时 `AdapterTimedOut` 一律还原旧 Ready 状态**（`supervisor.rs` `resolve_load_failure`）：超时其实是"不确定新 policy 是否已生效"。目前 `PLACEHOLDER_MEMORY_GB=1.0` 影响有限；修法是还原前读一次 status 确认，或在文档里写明 | OPEN |
| FU-28 | #48 第二轮评审遗留（Low，配置驱动加固）| `models.rs` 按配置端点转发的 SSRF 面、`omlx/http.rs` 把 bearer token 发给任意 `base_url`、`remote/client.rs` 的 `base_url` 缺 scheme 校验。都需要 operator 自己配出恶意值才能触发；`RemoteClient` 接进 router 前补上 | OPEN |
| FU-29 | #48 第二轮评审建议 | **预算把关分裂成两处**（`dispatch_local::reserve()` 与 `components.rs` 启动期"确定免费"闸门），`decide()` 自己的预算阶段在生产路径上实际是死代码。付费/远程上游接入前，合并成统一的 reserve/settle 包装（与 FU-22 一起做）| OPEN |
| FU-200 | A 机 v0.1.1 冒烟 | 模型身份：Supervisor 响应 model 使用实际服务模型；不匹配的具体模型名显式 400；HTTP 回归与回显/校验变异验证 | PR_OPEN（feat/openai-compat-01） |
| FU-201 | A 机 v0.1.1 冒烟 | Supervisor 流式：stream=true 及非法类型显式 400，false 保留整块响应；HTTP 回归与绕过校验变异验证 | PR_OPEN（feat/openai-compat-02） |
| FU-202 | A 机 v0.1.1 冒烟 | Supervisor 参数：文本消息之外未支持参数（含 max_tokens）显式 400；参数清单、HTTP 回归与放行 max_tokens 变异验证 | PR_OPEN（feat/openai-compat-03） |
| FU-203 | A 机 v0.1.1 冒烟 | 模型目录鉴权：loopback oMLX 复用 IDORIS_OMLX_API_KEY，401/403 显式 502 并记录安全日志；凭证隔离回归与变异验证 | PR_OPEN（feat/openai-compat-04） |

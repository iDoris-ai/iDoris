# iDoris 统一模型服务 实时状态 — progress

> 「此刻仓库真实发生了什么」。由 `pilot run` 每一步更新。
> 更新时间：2026-09-26

## 当前聚焦
- **Milestone**：M1/M2/M3 的 task 全部 `DONE`（38 个），代码已在集成分支
- **正在开发的 Task**：无
- **分支 / worktree**：`preview`（干净，无 worktree 残留）
- **PR**：**0 open**

## 仓库基线（2026-09-26 合并收尾后）
- `preview` = `79487c3`，**38 个 PR 全部合入**，open PR 归零。
- 7 个包（contracts / adapters / router / tenancy / recommender / growth / federation）；`packages/*/src` 下 61 个 `.ts`，测试 64 个 `.ts`（实测计数，非估算）。
- 全门禁在干净安装且**不先 build** 的条件下绿：`lint` / `typecheck` / `check:contract-drift` / `build` / `test` **375 条**（contracts 105 · router 120 · adapters 54 · recommender 37 · growth 22 · federation 21 · tenancy 16）/ `smoke`。
- 分支已清理（29 个已合并分支 + 3 个 worktree 回收）。遗留两个待人决定：`test/cla-action-check`（PR #1 已 CLOSED 未合并）、`docs/model-capability-design-agent24`（squash 合入 PR #8，`-d` 判不出，删除需人敲 `-D`）。
- ⚠️ **`preview` → `main` 尚未进行**：`main` 仍是 `1be703c`，落后 preview 全部内容。这一步要单独走受控 PR。

## 本轮修掉的三个静默失效（形状相同：边界处的断线，都不崩不报错）
| PR | 断在哪 | 后果 |
|:---|:---|:---|
| #25 | `parseProfile().profile` 丢掉 `.tenantId` | 跨租户幂等缓存泄漏（评审实测 B 拿到 A 的响应正文）|
| 合并 #32 时 | `resolveProfile` 未收到由注入 `env` 算出的 `deployMode` | tenant 模式静默降级成 personal → `X-iDoris-Tenant` 被忽略 → 缓存又串 |
| #23 | 探针函数被调用了，但 `describeTarget` 把实参静默丢弃（Node `_normalizeArgs` 把参数包成数组；`path: null` 被当 unix socket）| 出网探针对 `fetch`/`http.request` 完全失明，而「零出网」断言照样绿 |

## 方法论留档：变异测试在本轮四次抓到「测试不承重」
四次**没有一次**是靠读代码发现的。最险的一次是合并 #32：解完语义冲突后 119/119 全绿，按理就该继续合，是变异证明「删掉 `tenantId` 赋值照样全绿」——**server 层接线从来没有测试**（那四条跨租户测试测的是 `ChatProxy` 单元、显式传参），差点把跨租户泄漏放回生产。
另三次：#32 的故障注入打错了 `embed()` 调用（`detect()` 里有两次用途不同的调用）· #25 的「插入序不变式」测试因时间推进太快而场景没构造出来 · #23 的正对照用了被 patch 的同一个 API。
→ 已并入 FU-8，并新立 FU-15 作为护栏。

## 历史基线（2026-09-07 盘点，已被上方「仓库基线（2026-09-26）」取代，保留作过程留档）
- 集成分支 `preview` 已建立并推送；`main` 有 active ruleset 保护，只由 `preview` 经受控 PR 进入。
- PR #3（十篇规划文档 + U0 spike log）已 squash 合并进 `preview`；本地分支 `docs/idoris-unified-model-plan` 已清理。
- **仓库尚无任何代码**——只有 `docs/`（规划）与 `spike/u0/`（实测日志）。M1 的第一个 task 就是起 pnpm workspace 骨架。
- 遗留分支 `test/cla-action-check`：对应 PR #1 已 CLOSED **未合并**，safe-cleanup 正确地不动它，是否废弃待人决定。

## 进行中 / 待回执的 PR

**无。** open PR 归零，`preview` 上没有在途分支。

> 历史留档：上表原记录的两条已闭合 —— 规划台账经 PR #5 合入；PR #4（iDoris-website 提的 R0–R6）已修正 base 为 `preview` 并合入，R1–R6 的采纳结论见本文下方。

## 阻塞项（BLOCKED）

- **T2.5.2 三路由保真度矩阵**：缺 Anthropic + Gemini API key（用户凭证）；且无真实消费者。解除条件见 [`tasks.md`](tasks.md) 该 task。
- **T3.4.1 DP-FedLoRA**：当前写不出可机器验证的验收命令，保持 BACKLOG 不进 READY，等 F3.3 跑通后细化。

## R0 已拍板（2026-09-07）：多租户属于 iDoris，**iDoris 是组织大脑**

用户原话：「多租户属于 iDoris 的范围，未来为组织提供服务，要提供多租户，iDoris 是组织大脑」。走 **(a)**：

- **定位扩展而非推翻**：`01` 的「个人 AI 网关」是形态之一，不是全部。已在 [`research.md`](research.md) §2.5、[`architecture.md`](architecture.md) 核心判断 7 写明。
- **新增 `deploy_mode: personal | tenant`** 正交维度；`X-iDoris-Tenant` 进控制面。
- **新增 F1.5 多租户基线**（T1.5.1 契约 / T1.5.2 预算终态 / T1.5.3 硬隔离 / T1.5.4 reason）与 **F2.6 计费与账期**（T2.6.1）。
- **原圈定的三个 task 已按多租户改**：T1.3.2（路由顺序 + tenant header）、T1.4.2（`deploy_mode != personal` 一律拒绝注册订阅 provider）、T2.2.3（审计字段白名单 + reason + tenant 作用域）。
- **合规红线不松动**：多租户只作用于能力②③；能力① 在 `tenant` 模式下拒绝注册。
- **对外契约已交付**：[`contract-tenancy.md`](contract-tenancy.md) v1 随本 PR 发布——下游 iDoris-website 已把 `products/gateway/` 降级为消费者并停工等它。
- **移交在途**：对方的 `routing.py`(10 条变异) / `audit.py` / `egress_guard.py`(16 条变异)，Apache-2.0；对方保留一份直到我方跑通（FU-7）。

## 生态职责边界（2026-09-07，经 codex 对抗式评审）

[`ecosystem-boundaries.md`](ecosystem-boundaries.md) 初稿已出，四组件 + 四个补齐的职责。要点：
- **iDoris 按权限定义，不按「智能」定义**：决定「哪个租户在什么预算与隐私策略下可访问哪些推理资源」，不天然拥有每种模型运行时。
- **守的不变式改了**：不是「所有模型请求都过 iDoris」（组织纪律，可绕过），而是「所有远程/计费/受策略约束的访问必须过准入；隐私敏感的纯本地推理可在可信边缘执行，但须取授权并回传无内容的审计」（拓扑与沙箱，绕不过）。
- **语音走控制面/数据面分离** —— 推翻了我最初的倾向，且被 AgentEar 的现实验证（见下）。
- **两个漏洞**：能力①订阅中转会绕过 Agent24 审批门（必须沙箱化，已进 T1.4.1 验收）；多租户需**每个有状态组件各自隔离**，iDoris 只隔离账单（已进 T1.5.3）。
- **四条待拍板**：B1 双 harness 二选一（最迫近）· B2 MemPalace 归属 · B3 AgentEar M3/切换时机 · B4 模型制品层。

## Voice V0：泰国业务的硬阻塞已解除

[`voice-v0-findings.md`](voice-v0-findings.md)：「Voice 已有基础」这个假设**成立，但不在 iDoris/Agent24，在 [`iDoris-ai/AgentEar`](https://github.com/iDoris-ai/AgentEar)**（M1+M2 完成，有发布版）。
- 主链路是 **SenseVoiceSmall q8 + FunASR llamacpp**，不是 faster-whisper —— 对方那条「faster-whisper 停更」的风险**不适用**，但要换成 FunASR runtime + SenseVoice「即将停止维护」的新形状。
- 泰语走**独立 whisper.cpp 引擎 + 显式语言选择**（SenseVoice 语种集里没有 `th`，实测会误标 `en` 并输出乱码）。
- ⚠️ **Apple Silicon 限定**，与对方「Voice 可单独部署在客户机器上」的承诺冲突（上游 FunASR 只发 macos-arm64）。
- **AgentEar 已在消费 `POST /v1/chat/completions` @ `127.0.0.1:8793`，模型正是 Ornith** —— 它是 iDoris 的第二个消费者，接入只需改一个 URL。

## 待人拍板的其余问题（不阻塞 M1 开工）
- PR #4 的 R1–R6 **已全部采纳**（2026-09-07）：
  - **R1** `budget_exceeded` 提升为与 `local_only_unavailable` **同级的拒绝终态**（共性：都不是重试/降级能解决的问题），已写进 [`spec.md`](spec.md) 状态机 + 失败分类；对应的 routing policy 字段等 R0 定了归属再落 task。
  - **R4** T2.2.3 补上字段名黑名单闸门 + 500 字符上限（原设计只有哨兵测试——哨兵证明「这次没漏」，闸门证明「结构上漏不出去」）。实现可从 iDoris-website `products/gateway/audit.py` 移植，同为 Apache-2.0。
  - **R5** 新增 **T1.3.6 出网启动断言**（含正对照）——运行期路由（`test:privacy`）与启动期部署配置是两个不同的洞，我们原本只有前者。
  - **R6** 账期时区记为 FU-5（本仓库当前无计费，暂不落 task）。
  - **R2** 路由执行顺序（隐私判定必须排在意图匹配之前，否则「做 banner → 视觉模型」会先命中）已写进 [`spec.md`](spec.md) 并进 T1.3.2 验收。
  - **R3** 决策 `reason` 可解释 → T1.5.4 + 审计字段。
  - **R0** 见上节。
- `test/cla-action-check` 分支（PR #1 已关闭未合并）是否废弃。
- **PR 合并顺序**：评审建议 #4 → #5 → #6（#4 是需求文档，记录的是提出时的状态，紧接 #5 即答复；反序则 `docs/11` 一落地就过期）。三个 PR 均已 APPROVE，等合并。

## 最近完成
- 2026-09-07 建立集成分支 `preview`；PR #3 retarget 到 `preview` 并 squash 合并（`dd9ae99`）；清理已合并本地分支 `docs/idoris-unified-model-plan`。
- 2026-09-07 落地 `docs/agent/` 规划七件套（research / acceptance / architecture / spec / roadmap / tasks / progress）+ `.pilot.yml`，把 `docs/01~10` 十篇散文规划蒸馏为 M→F→T 三级台账。

## 下一个 READY
- **T1.1.1** pnpm workspace 骨架 + 门禁流水线（无依赖）
- **T1.1.2** 契约的 TS 类型 + zod schema（依赖 T1.1.1）
- **T1.1.3** 组件卡策略校验器（依赖 T1.1.2）
- **T1.5.1** `deploy_mode` + TenantContext 契约（依赖 T1.1.2）—— **契约文档已交付，剩 TS 类型 + schema**；下游在等，优先级最高

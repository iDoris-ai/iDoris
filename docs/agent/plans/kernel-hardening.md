# `feat/kernel-hardening` —— FU-26 Rust 整栈 Tier 1 复审的修复（目标 v0.1.1）

> 复审：工作站 B Codex（2026-10-01，基线 `main@30d43c3` = v0.1.0）。抽查确认：H3（supervisor.rs 测试明文要求确认失败也继续加载）、
> H6（全仓库没有配置 redirect/no_proxy）、H7（凭证源 Err → `Ok(None)` → genai 回退宿主机环境密钥）、H10（只有 paid 请求走 reserve）。
> 优先级高于 B1：涉及已发布版本的隐私出站、凭证边界与预算。协作规则见 [`../COLLAB.md`](../COLLAB.md)。

## 执行约定
- 集成分支 `feat/kernel-hardening`（从 main 拉）。每个任务一个 worktree + 分支 `feat/kernel-hardening-KNN-<短名>`，PR 目标 `feat/kernel-hardening`，≤300 行。
- 每个修复必须带回归测试，并证明"去掉修复时测试失败"。对应原有测试明文要求旧行为的（如 H3 的 `eviction_proceeds_after_confirmation_gives_up`），改写该测试并在 PR 里说明。
- 全部完成后 release PR → main，打 `v0.1.1`。

### K13 / PR #185 范围例外

- 本 PR 明确采用 ≤300 行约定的范围例外：评审基线 `575e2e9` 相对集成分支新增 2062 行、删除 81 行，后续评审修复计入同一 PR。
- 保留在同一 PR 的范围为 singleflight、请求指纹、取消/不确定执行的 fail-closed 保留及其容量边界；缓存/flight 字节预算和并发许可用于约束这些保留状态，相应回归测试与 HTTP 契约同步随实现一起验收。拆开这些路径会使中间增量缺少重复 POST 防护或容量保护，因此不在本轮重写历史拆分。
- 例外仅适用于 K13 / PR #185，不改变其他任务的 ≤300 行要求，也不扩展到 K14/K15 的其他工作。本轮修改仅修复等待者读取成功缓存的竞态、补充受控并发回归测试，并记录本例外；回归测试须验证移除修复后失败，且运行仓库全部门禁。

## 任务（按优先级）

| 任务 | 对应 | 内容 | 验收要点 |
|---|---|---|---|
| K01 | H6 | 所有通往上游的 reqwest 客户端（router 直连转发、oMLX 适配器、models 探测）`redirect(Policy::none())` + `no_proxy()`；genai 远程客户端确认不启用 system-proxy | wiremock 返回 307→外部地址，请求不被跟随（外部 mock 收到 0 次）；设置 HTTPS_PROXY 指向探针时探针收到 0 次 |
| K02 | H7 | `CredentialSource` 失败显式返回错误并映射为 `AuthFailed`，绝不 `Ok(None)` | 环境里设 `OPENAI_API_KEY`、凭证源拒绝 → 返回 AuthFailed，上游 mock 收到 0 次 |
| K03 | H5 | 过渡期：启动时发现多于一个 lifecycle（非 Resident http_service）后端就拒绝启动；分发前核对选中 provider 与 Supervisor 绑定的 provider 一致，不一致 fail-closed | 两张 lifecycle 卡 → 非 0 退出；伪造不一致 → 503 且上游 0 次 |
| K04 | H8 | Rust 版在 B3 之前一律拒绝注册 subscription（订阅中转）卡 | 订阅卡 → 启动非 0 退出，错误信息说人话 |
| K05 | H10 | 所有请求（含零成本）在有租户账本时统一过预算门禁，由 ledger 按 `SpendGate` 决定放行 | `SpendGate::All` + 额度 0 → 免费请求被拒；`PaidOnly` → 免费请求放行（正对照） |
| K06 | H9 | settle 存储错误不再 `.ok()` 吞掉：持久化待结算记录并重试，错误可观测；与 `OverageTooLarge` 分开处理 | 注入 SQLite Busy → 请求成功返回但待结算记录存在，重试后入账；去掉修复时漏账 |
| K07 | M2 | reserve / extend 在取得写事务后再采样时钟、账期、TTL | 注入时钟 + 锁等待 → 返回的预留未过期 |
| K08 | M3 | 金额累加显式整数溢出检查 + 列类型约束，溢出不落 REAL | `i64::MAX` settle → 明确错误，账期仍可查询 |
| K09 | H1 | oMLX 变更类请求（load/pin）超时或连接中断 → `LoadUnconfirmed`，不当作确定拒绝 | wiremock 延迟超过 call_timeout → `load_unconfirmed`，Supervisor 走 best_effort_release |
| K10 | H2 | 对 `Error` 状态的模型重试加载：保存旧状态与旧占用，失败且未确认释放时保持 `Error` + 原占用 | 8GB 卸载失败 → Error；再 load 失败 → used_gb 仍为 8 |
| K11 | H3 | 释放确认失败 → 停止准入并保留占用；确认要校验目标模型确实不在 + 剩余容量足够，而不是"任意下降"；`best_effort_release` 同样需要确认 | 改写 `eviction_proceeds_after_confirmation_gives_up` 为"确认失败则拒绝加载"；多 victim 只释放一部分 → 拒绝 |
| K12 | H4 | Supervisor 启动时读取引擎状态对账，未知驻留作为保守占用；对账失败阻止准入 | 引擎预先有 20GB 驻留 → 启动后 used_gb ≥ 20，超额加载被拒 |
| K13 | M4 | 直连缓存按租户隔离键做 singleflight 并校验请求指纹；只重试确定未执行的失败 | 同 request_id 并发两次 → 上游 1 次；响应头丢失 → 不重试 |
| K14 | M5 | 直连转发：连接/响应头期限、缓冲响应字节上限、流式空闲超时、缓存按字节限额、并发许可 | 上游不回头 → 超时 504；超大 body → 502 不缓存 |
| K15 | M6 | 远程流式与直连 SSE：未见终态的 EOF 产生错误，接上 `ensure_terminated` | 200 + 部分 SSE + EOF → 客户端收到明确错误 |

M1（provider 当模型、固定 1GB）已在 B1 task 15 与 B2 范围内，这里不重复。

---

## 附：复审原文（路径已改为仓库相对路径）

本轮建议 **CHANGES_REQUESTED**。发现涉及真实驻留被账本遗忘、`local_only` 出站、凭证回退和已完成调用漏记账。

全程只读，工作区未修改。现有预编译测试中 backend **81 项**、policy **61 项**通过，但未重新编译当前 HEAD；HTTP 集成验证因沙箱禁止监听端口未完成。以下区分源码确认的问题与待验证的实机行为。

## Critical

未发现证据充分的 Critical 项。

## High

**H1．oMLX 自身超时被当作“未分配”，直接清空首次加载的占用**

位置：[omlx/mod.rs:195](../../../crates/idoris-upstream/src/omlx/mod.rs:195)、[omlx/http.rs:134](../../../crates/idoris-upstream/src/omlx/http.rs:134)、[supervisor.rs:650](../../../crates/idoris-backend/src/supervisor.rs:650)。

- **触发**：引擎收到 load 并开始分配，但响应超时或连接中断。oMLX 默认 10 秒超时先返回普通 `Upstream`，Supervisor 的 30 秒外层超时不会触发；失败被落为 `Stopped`，不执行清理，账本释放占用。
- **结果**：引擎仍可能驻留，账本却认为为空，后续模型按虚假容量准入。
- **修复**：对已发出的变更请求区分“确定拒绝”和“执行结果未知”；后者返回 `LoadUnconfirmed`，直到确认终止加载并释放前保留占用。真实 oMLX 故障注入复现**待验证**。这不是 FU-27 的 Ready reload 问题。

**H2．对 Error 状态重试加载，失败后会遗忘此前真实驻留**

位置：[supervisor.rs:891](../../../crates/idoris-backend/src/supervisor.rs:891)、[supervisor.rs:656](../../../crates/idoris-backend/src/supervisor.rs:656)、[supervisor.rs:981](../../../crates/idoris-backend/src/supervisor.rs:981)。

- **触发**：A 已加载 8GB；卸载失败，进入仍占预算的 `Error`；再次加载 A 被普通错误拒绝。旧槽位只在 `Ready` 时被保存，因而这次落为 `Stopped`。
- **结果**：A 的旧实例仍在，账本占用变为零；再调用 unload 还会直接返回成功。现有 Mock 的加载失败分支也不会移除旧驻留。
- **修复**：保存所有可能占用内存的旧状态及旧占用，不能把 Error 重试当作 fresh load；未确认释放时继续保留 `Error` 和原占用。

**H3．释放确认失败仍继续加载；“任意下降”也不足以证明容量已释放**

位置：[supervisor.rs:436](../../../crates/idoris-backend/src/supervisor.rs:436)、[supervisor.rs:535](../../../crates/idoris-backend/src/supervisor.rs:535)、[supervisor.rs:622](../../../crates/idoris-backend/src/supervisor.rs:622)、[supervisor.rs:1209](../../../crates/idoris-backend/src/supervisor.rs:1209)。

- **触发**：unload 返回成功，但内存没有下降；确认轮询耗尽后照常 load，victim 仍落为 `Stopped`。多 victim 时，只释放少量内存也满足 `used_gb < before_gb`。
- **结果**：未获得准入所需容量就加载新模型。`best_effort_release` 同样仅凭 unload 成功便认定释放。
- **修复**：确认函数返回失败，失败时停止准入并保留占用；校验目标模型确实释放、剩余容量足够，而非仅比较任意下降。
- 现有 [测试:2804](../../../crates/idoris-backend/src/supervisor.rs:2804)明确要求“确认永不成功也继续加载”，因此绿灯覆盖了这个 fail-open。真实 oMLX 返回成功后的释放时序**待验证**。

**H4．Supervisor 启动时默认引擎为空，没有恢复已有驻留**

位置：[supervisor.rs:1021](../../../crates/idoris-backend/src/supervisor.rs:1021)、[supervisor.rs:1080](../../../crates/idoris-backend/src/supervisor.rs:1080)、[supervisor.rs:856](../../../crates/idoris-backend/src/supervisor.rs:856)。

- **触发**：Router 重启，但外部 oMLX 进程及已加载的 A 仍存活。新 Supervisor 创建空 HashMap，status 返回零占用，准入只使用这个空账本。
- **结果**：已有驻留不计入预算，新模型可以超额准入；账本一致性在第一次操作前就已失效。
- **修复**：首次准入前读取并对账引擎状态；无法识别的驻留至少作为保守占用保留。初始化和后续对账失败都应阻止容量准入。

**H5．选中的后端与实际执行后端可能不同，绕过 `local_only`**

位置：[bin/idoris.rs:57](../../../crates/idoris-router/src/bin/idoris.rs:57)、[bin/idoris.rs:67](../../../crates/idoris-router/src/bin/idoris.rs:67)、[dispatch.rs:362](../../../crates/idoris-router/src/dispatch.rs:362)。

- **触发**：注册两个合法 lifecycle 后端，排序首个 A 指向远端，B 指向 loopback；`local_only` 选择 B，但全局唯一 Supervisor 实际绑定 A。若 A 也支持所请求模型，请求便在 A 执行。
- **结果**：提示词送到远端，响应仍根据 B 的卡片标记为 loopback。即便两个端点都本地，也会执行错误后端。
- **修复**：按选中 provider/endpoint 查找对应 Supervisor，并核对实际 locality；未实现多后端前，启动时拒绝多个 lifecycle 后端。
- 启动代码注释声称后续卡会 fail-closed，实际分发没有这项检查。

**H6．合法 loopback URL 经重定向或系统代理出站，仍被标为 loopback**

位置：[lib.rs:203](../../../crates/idoris-router/src/lib.rs:203)、[omlx/mod.rs:101](../../../crates/idoris-upstream/src/omlx/mod.rs:101)、[proxy.rs:251](../../../crates/idoris-router/src/proxy.rs:251)、[lib.rs:639](../../../crates/idoris-router/src/lib.rs:639)。

- **触发**：合法 `127.0.0.1` 上游返回指向外网的 307/308，默认 reqwest 会重发 POST。另一条路径是设置外部系统代理而未排除 loopback。
- **结果**：请求内容出站，但隐私判定、响应及缓存 locality 仍来自初始卡片。
- **修复**：受本地隐私约束的客户端显式使用 `redirect(Policy::none())` 和 `no_proxy()`；如允许跳转，必须逐跳验证。
- 已核对锁定依赖：reqwest 默认跟随重定向；genai 的依赖特性合并重新启用了 `system-proxy`。无需 FU-28 所述的恶意初始 URL。

**H7．CredentialSource 拒绝后，静默回退宿主机环境密钥**

位置：[remote/client.rs:108](../../../crates/idoris-upstream/src/remote/client.rs:108)、[remote/client.rs:116](../../../crates/idoris-upstream/src/remote/client.rs:116)。

- **触发**：凭证缺失、撤销或读取失败，同时宿主机存在 `OPENAI_API_KEY`／`ANTHROPIC_API_KEY`。代码返回 `Ok(None)`，锁定的 genai 0.6.5 随后调用默认环境凭证解析器。
- **结果**：请求没有按预期 `AuthFailed`，而是使用宿主机账户，并将该密钥发往配置的兼容端点。这跨越了指定凭证源的授权边界。
- **修复**：凭证源失败必须显式返回错误并映射为 `AuthFailed`；增加“环境中存在默认密钥，但指定源拒绝”的零出站测试。

**H8．订阅 provider 的部署门禁和禁用开关完全未接入**

位置：[components.rs:145](../../../crates/idoris-router/src/components.rs:145)、[registry.rs:165](../../../crates/idoris-policy/src/registry.rs:165)、[lib.rs:504](../../../crates/idoris-router/src/lib.rs:504)。

- **触发**：配置合法的免费 Resident HTTP subscription 卡，声明 remote／privacy any；使用 tenant 部署、未显式 enable，或设置 `IDORIS_DISABLE_SUBSCRIPTION=1`。
- **结果**：Rust 仍注册该卡并可直接转发。注册校验只拒绝矛盾的 local 声明，没有执行 personal-only、默认禁用或 kill switch。
- **修复**：把部署模式与订阅启动门禁接入注册；相关能力未支持时直接拒绝订阅卡。模块注明“后续实现”不能保证已接受卡片的安全边界。

**H9．结算存储错误被吞掉，已完成调用可能永久漏记**

位置：[dispatch.rs:383](../../../crates/idoris-router/src/dispatch.rs:383)、[dispatch.rs:391](../../../crates/idoris-router/src/dispatch.rs:391)。

- **触发**：付费调用成功，但 settle 遇到 SQLite Busy／Storage 错误并回滚。reservation 已通过 `take()` 从 guard 移走，错误再被 `.ok()` 丢弃。
- **结果**：返回成功，没有结算重试或持久化待办；预留 TTL 到期释放后，这笔真实消耗永久消失，重复故障可绕过额度。
- **修复**：持久化待结算结果并可靠重试，保留 reservation 与实际费用的关联；错误必须可观测。这里特指未提交的存储失败，`OverageTooLarge` 已提交记账，应分别处理。

**H10．FU-29 严重度提升：当前免费请求已经绕过租户冻结**

位置：[dispatch.rs:259](../../../crates/idoris-router/src/dispatch.rs:259)、[dispatch.rs:297](../../../crates/idoris-router/src/dispatch.rs:297)、[ledger.rs:1120](../../../crates/idoris-tenancy/src/budget/ledger.rs:1120)。

- **触发**：租户设置 `SpendGate::All`，额度为零或已耗尽，再请求免费模型。policy 收到 `budget: None`，dispatch 仅对 paid 请求 reserve。
- **结果**：免费请求仍执行；正确调用 ledger reserve 会拒绝。这违反的是现有“冻结全部调用”语义。
- **修复**：所有请求统一经过预算门禁，包括零成本请求；由 ledger 自己决定 `PaidOnly` 是否放行。
- **升级理由**：FU-29 将修复时点放在付费／远程接入前，但此问题在当前免费路径已经可达。

## Medium

**M1．Supervisor 分发把 provider 当模型，并用固定 1GB 准入**

位置：[dispatch.rs:40](../../../crates/idoris-router/src/dispatch.rs:40)、[dispatch.rs:284](../../../crates/idoris-router/src/dispatch.rs:284)、[dispatch.rs:351](../../../crates/idoris-router/src/dispatch.rs:351)。

- **触发**：使用默认 provider `omlx`，请求 `idoris/daily`，引擎实际模型 ID 是目录中的模型名。
- **结果**：代码加载并聊天的模型是字面量 `omlx`，未执行角色到模型的映射，正常引擎会拒绝。即使 provider ID 恰好匹配实际模型，20GB 模型也只登记 1GB。
- **修复**：明确分离 provider ID 与 model ID，解析真实模型及内存估计；映射或内存未知时返回不支持，不能用占位值声称完成容量准入。

**M2．reserve 在等待写锁前取时钟，成功返回的预留可能已经过期**

位置：[ledger.rs:491](../../../crates/idoris-tenancy/src/budget/ledger.rs:491)、[ledger.rs:623](../../../crates/idoris-tenancy/src/budget/ledger.rs:623)。

- **触发**：TTL 配置为 200ms；A 在 t=0 取时钟，等待写锁到 t=300ms，随后插入 `expires_at=200` 并返回成功。
- **结果**：A 刚开始工作，其预留已经不可见；B 可立即再次预留全部额度，两个成功调用合计超过限额。`extend` 也有锁前取时钟的问题。
- **修复**：取得写事务后再采样时钟、账期和 TTL，并确保提交时预留仍有效。Rust 锁竞争端到端复现**待验证**。

**M3．SQLite 金额累加溢出会转成 REAL，破坏账本整数类型**

位置：[ledger.rs:753](../../../crates/idoris-tenancy/src/budget/ledger.rs:753)、[ledger.rs:784](../../../crates/idoris-tenancy/src/budget/ledger.rs:784)。

- **触发**：账期已记账 1，再对另一个预留 settle `actual_cost_minor=i64::MAX`；超额结算路径仍会提交真实费用。
- **结果**：SQLite 的加法结果存为 REAL；后续 `row.get::<_, i64>` 报 Storage，金额失去精度，账期查询及准入异常。已用内存 SQLite 确认类型转换。
- **修复**：事务内执行显式整数溢出检查，增加列类型约束；超出金额表示范围时保留可恢复的费用记录，不能让 SQLite 隐式转浮点。

**M4．相同 request_id 的并发请求及不确定 POST 重试会重复执行**

位置：[proxy.rs:217](../../../crates/idoris-router/src/proxy.rs:217)、[proxy.rs:251](../../../crates/idoris-router/src/proxy.rs:251)、[proxy.rs:316](../../../crates/idoris-router/src/proxy.rs:316)。

- **触发**：同租户、同 request_id 的两个请求同时 cache miss；或者上游已执行 POST，但响应头丢失。
- **结果**：并发 miss 各自发送请求；所有 send 错误都可重试，而 request_id 没有作为上游幂等键发送。重复生成，最终缓存结果互相覆盖。
- **修复**：按现有租户隔离键做 singleflight，校验请求指纹；仅重试确定未执行的失败，或依赖明确支持的上游幂等协议。

**M5．直连转发没有活动请求超时、响应字节上限及并发上限**

位置：[lib.rs:203](../../../crates/idoris-router/src/lib.rs:203)、[proxy.rs:273](../../../crates/idoris-router/src/proxy.rs:273)、[proxy.rs:357](../../../crates/idoris-router/src/proxy.rs:357)。

- **触发**：上游接受连接但不返回响应头，或返回头后持续发送／一直不结束 body。
- **结果**：默认 reqwest 没有请求／读取超时；缓冲路径无限等待或增长内存。缓存只限制条目数，不能限制保留字节；并发请求继续积累连接和任务。
- **修复**：设置连接及响应头期限、缓冲响应大小限制、流式空闲超时；缓存按字节限额，转发持有贯穿 body 生命周期的并发许可。此项不同于 FU-25。

**M6．Remote 流式响应缺少终态时，裸 EOF 被当作正常结束**

位置：[remote/client.rs:206](../../../crates/idoris-upstream/src/remote/client.rs:206)、[remote/client.rs:348](../../../crates/idoris-upstream/src/remote/client.rs:348)、[chat.rs:113](../../../crates/idoris-upstream/src/chat.rs:113)。

- **触发**：上游返回 HTTP 200 和部分 SSE 内容，然后正常结束 HTTP body，但未发送完成事件。
- **结果**：genai 流结束后 `DeadlineStream` 直接返回 `None`，既无 `done: true` 也无错误，违反本仓库明确的流终止契约。已有 `ensure_terminated` 没有接上。
- **修复**：包装返回流，或在未见终态的 EOF 分支产生错误；直连 [lib.rs:628](../../../crates/idoris-router/src/lib.rs:628)也应区分 HTTP 正常 EOF 与 SSE 应用层完整结束。

## Low

未另列证据充分、值得独立修复的 Low 项；FU-22…FU-29 的原样问题未重复报告。

总体结论：**请求修改；v0.1.0 的内存账本、隐私出站和预算结算边界尚未做到 fail-closed。**
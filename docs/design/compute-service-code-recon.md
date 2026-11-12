# 计算服务 S1–S3 代码勘察（现状地图）

> 只读勘察，对照定稿 `docs/design/compute-service-design.md`（定稿 1.0）。本文不改 Rust、不改契约。
> 勘察日期：2026-11-12。行号对应当时工作区 `task/compute-s1-recon`。
> 设计稿点名的三处都还在，没有改名：`DEFAULT_KEY_ID` 在 `crates/idoris-router/src/budget.rs`；`retain_tenants` 的**调用**仍在 `crates/idoris-router/src/storage.rs:224`（实现在账本，不在 `store.rs`）；`admin_scopes` 的拒绝仍在 `crates/idoris-router/src/key_issue.rs:122`。
> 仓库里没有主体、成员关系、付款账户、额度委派、担保这些类型。`Principal` / `Membership` / `Account` 在 `crates/**/*.rs` 中无定义。

权威顺序：设计稿与本文冲突时，以设计稿的目标为准；本文只记录代码现状。脑暴稿和 Codex 评审只作背景，不覆盖定稿。

## 0. 设计稿点名三处

| 设计稿 | 现状 | 出处 |
|---|---|---|
| `budget.rs` 的 `DEFAULT_KEY_ID` | `const DEFAULT_KEY_ID: &str = "default"`。全仓库只有 `scope()` 用它，把每次预留的 `key_id` 写成 `"default"`，并把 `model_id` 写成 `provider_id`。 | `crates/idoris-router/src/budget.rs:48-50`、`:200-203` |
| `storage.rs:224` 的 `retain_tenants`（稿中曾怀疑改名为 `store.rs`） | 调用仍是 `budget.retain_tenants(&seen)`。`store.rs` 是另一张租户记录表，没有这个函数。函数本体在账本。 | 调用 `crates/idoris-router/src/storage.rs:223-225`；定义 `crates/idoris-tenancy/src/budget/ledger.rs:452-483` |
| `key_issue.rs:122` 拒绝 `admin_scopes` | `if !spec.admin_scopes.is_empty() { return Err("M4 virtual key 暂不允许 admin_scopes") }`。持久化时写成空数组。 | `crates/idoris-router/src/key_issue.rs:122-135` |

## 1. 数据模型

持久化都在**同一个 SQLite 文件**。路径：`IDORIS_DB_PATH`，否则 macOS 为 `~/Library/Application Support/iDoris/idoris.sqlite3`（`crates/idoris-router/src/storage.rs:172-177`、`:428-437`）。启动时 `TenantStore`、`BudgetLedger`、`VirtualKeyStore`、`EventLogStore` 依次打开同一路径（`storage.rs:190-198`）。四套迁移表互不共用版本号。账本连接把文件设成 WAL，并设 5 秒 `busy_timeout`（`ledger.rs:296-298`）。其余三个 store 的打开路径没有自己的 `busy_timeout`。进程级互斥靠 `<db>.lock` 的 OS 锁（`storage.rs:101-135`），`bin/idoris.rs:339-372` 在拉起服务前取得。

金额在账本里是无币种的 `i64`，列名 `*_minor`。契约文案是「最小货币单位」（`docs/agent/contract-tenancy.md:49`、`:57`）。没有 `THB` / `USD` / `QUOTA`，没有微单位类型，没有汇率。报价用 `f64` 乘单价再 `ceil` 成 `i64`（`budget.rs:62-83`）。用量汇总把 `cost_minor` 加进 `f64`（`crates/idoris-tenancy/src/billing.rs:31-36`）。

### 1.1 预算账本

迁移由 `BudgetLedger::open` 在一次 `BEGIN IMMEDIATE` 里按版本号执行（`ledger.rs:127-133`、`:1282-1306`）。版本表：

```text
schema_migrations(version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL)
```

（`ledger.rs:1285-1286`）

| 版本 | 文件 | 表 |
|---|---|---|
| 1 | `crates/idoris-tenancy/src/budget/migrations/0001_init.sql` | `budget_config`、`budget_periods`、`reservations` |
| 2 | `.../0002_overage_events.sql` | `budget_overage_events` |
| 3 | `.../0003_tenant_scope.sql` | `tenant_config`、`tenant_periods` |
| 4 | `.../0004_tenant_period_column.sql` | `reservations.tenant_period` |
| 5 | `.../0005_integer_amounts.sql` | 重建 `budget_periods` / `tenant_periods`，强制 `typeof(spent_minor)='integer'`；新增 `budget_amount_overflows` |

列（迁移后的最终形状）：

- `budget_config`：`tenant_id, key_id, provider_id, model_id, limit_minor, billing_timezone`。主键四元组（`0001_init.sql:14-22`）。这是可选的更细子限额，不是设计稿里的父子账户。
- `budget_periods`：同上四元组 + `period TEXT` + `spent_minor INTEGER NOT NULL DEFAULT 0`，且 `spent_minor >= 0`（`0005_integer_amounts.sql:4-9`）。只在 `settle` 时增加。
- `reservations`：`id, tenant_id, key_id, provider_id, model_id, period, reserved_minor, status, created_at_ms, expires_at_ms, actual_cost_minor, tenant_period`。`status ∈ {active, settled, released, expired}`（`0001_init.sql:39-52`，`tenant_period` 见 `0004`）。
- `tenant_config`：`tenant_id PK, limit_minor, billing_timezone, scope ∈ {paid_only, all}`（`0003_tenant_scope.sql:6-12`）。
- `tenant_periods`：`tenant_id, period, spent_minor`（`0005:13-18`）。
- `budget_overage_events`：`id, tenant_id, reservation_id, reserved_minor, actual_cost_minor, overage_minor, created_at_ms`（`0002:8-16`）。
- `budget_amount_overflows`：`reservation_id PK, tenant_id, actual_cost_minor, created_at_ms`（`0005:23-28`）。溢出后该租户不能再预留，直到人工对账（`ledger.rs:874-877`、`reject_amount_overflow` 在 `admit` 开头，`:636`）。

Rust 类型：

- `BudgetScope { tenant_id, key_id, provider_id, model_id }`（`crates/idoris-tenancy/src/budget/scope.rs:14-19`）。
- `Price::Known(i64) | Unknown`（`ledger.rs:145-148`）。`Unknown` 直接拒绝，不当免费。
- `SpendGate::PaidOnly | All`（`ledger.rs:188-193`）。零成本且 `PaidOnly` 时跳过余额检查（`:690-695`）。
- `ReservationId`、`SettleReceipt { reserved_minor, actual_cost_minor, refunded_minor, late, overage_minor }`（`ledger.rs:151-184`）。
- `TenantBudgetReadView`（`ledger.rs:198-210`）：`remaining = limit - spent`，**不含**在途；`available = remaining - reserved`，可以为负。
- 账期键 `YYYY-MM`，按租户 IANA 时区，不用服务器本地时区（`budget/period.rs:29-38`）。时区白名单是写死的 10 个名字，含 `UTC` 和 `Asia/Bangkok`（`crates/idoris-contracts/src/tenant.rs:65-76`）。

谁写：

- 启动：`bootstrap` 对配置里每个租户 `configure_tenant`，然后 `retain_tenants`（`storage.rs:209-225`）。不写 `budget_config` 子限额。
- 请求：`BudgetLedger::reserve` / `settle` / `release` / `extend` / `replay_settlement` / `sweep_expired`。路由胶水是 `crates/idoris-router/src/budget.rs` 的 `reserve`、`check_admission`、`settle`、`settle_replayable`、`release`。

谁读：

- 准入：`admit` 在同一事务里读 `budget_config`、`tenant_config`、已结算额、未过期在途（`ledger.rs:636-739`）。
- 只读：`balance`、`tenant_balance`、`tenant_readview`。HTTP `GET /idoris/tenants/{tenant_id}/budget` 走 `crates/idoris-router/src/queries/budget.rs:38`。
- 决策管道的 `BudgetView` **没有接上**。`dispatch` 构造 `PolicyCtx { budget: None }`（`crates/idoris-router/src/dispatch.rs:442-445`）。于是 `budget_stage` 走 `BudgetNoTenantContext`，原样放行候选（`crates/idoris-policy/src/pipeline/mod.rs:84-90`）。真正的闸门是选完候选之后的账本 `reserve`。

### 1.2 租户记录（用量与旧审计）

迁移：`crates/idoris-tenancy/src/store/migrations/0001_records.sql`，版本表 `record_schema_migrations`（`store.rs:188-201`）。

```text
tenant_records(
  tenant_id, kind CHECK (kind IN ('usage','audit')),
  record_id, request_id, origin_record_id, payload JSON object,
  PRIMARY KEY (tenant_id, kind, record_id)
)
```

`request_id` 故意不唯一（SQL 第 1 行注释）。

Rust：`RecordKind::{Usage, Audit}`、`TenantRecord`（`store.rs:11-45`）。`put`/`list` 必须带租户，否则 `ScopeRequired`（`store.rs:75-76`）。

用量写入：`UsageEntry { ts_utc, cost_minor, tokens_in, tokens_out, request_id }`（`usage.rs:6-13`），由响应结束后的 `finish_usage` 调用 `write_usage_record`（`lib.rs:3744-3778`）。`cost_minor` 未知就拒绝落一条假的 0（`lib.rs:3751-3753`）。读：`billing::aggregate_usage_records`，以及 `GET /idoris/tenants/{id}/usage`。

这不是设计稿的成本/售价/补贴/担保分录。一条用量是一次推理的 `cost_minor` 和 token 计数。

### 1.3 事件日志

迁移：`event_log/migrations/0001_events.sql`、`0002_tenant_type_time.sql`（只加索引）。版本表 `event_log_schema_migrations`（`event_log/mod.rs:502` 一带）。

```text
event_log_events(
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT UNIQUE, tenant_id, record_id, event_type, ts_utc_ms,
  request_id, session_id, trace_id, parent_id, origin_record_id, metadata
)
```

触发器禁止普通 UPDATE/DELETE，插入必须由数据库分配正的 `sequence`（`0001_events.sql:9-12`）。模块注释写明：能跑 DDL 或直接改文件的一方可以拆掉触发器，这不是防篡改账本（`event_log/mod.rs:93-100`）。

事件类型（`event_log/mod.rs:25-29`）：`request.received`、`inspected`、`profiled`、`decided`、`budget.reserved`、`dispatched`、`stream.started`、`stream.chunk`、`stream.completed`、`completed`、`budget.settled`、`audit.finalized`、`feedback.received`。

元数据白名单含 `provider_id, model_id, tokens_in, tokens_out, cost_minor, reserved_minor, settled_minor, price_version` 等，并明确禁止 `prompt` / `messages` / `content` 等正文键（`event_log/mod.rs:10-13`）。

写：请求路径上的 `append_*`。读：`GET /idoris/tenants/{tenant_id}/requests/{record_id}`。

### 1.4 虚拟 key（凭证的现有形态）

迁移：`virtual_key/store/0001_virtual_keys.sql`、`0002_tenant_binding.sql`（给表加 `tenant_id`）。版本表 `virtual_key_schema_migrations`（`virtual_key/store.rs:254-280`）。

```text
virtual_keys(
  key_id PK, key_hash BLOB UNIQUE (32 字节),
  owner, tenant_id NULL,           -- 0002 才有 tenant_id
  allowed_privacy JSON array, allowed_roles JSON array,
  budget_ref TEXT NULL, expires_at_ms, admin_scopes JSON array,
  status CHECK (status IN ('active','revoked')), revoked_at_ms,
  CHECK (active ⇒ revoked_at_ms IS NULL) AND (revoked ⇒ revoked_at_ms IS NOT NULL)
)
```

Rust：`VirtualKeyScope`（`store.rs:11-21`）含 `budget_ref` 和 `admin_scopes`。`AuthenticatedVirtualKey { key_id, scope }`（`:24-27`）。明文只在签发时出现，库存 SHA-256（`virtual_key.rs:1-4`、`:52-54`）。线格式 `idk_` + 64 位小写十六进制（`virtual_key.rs:12-13`）。`key_id` 形如 `vk_` + UUID simple（`virtual_key.rs:115`）。

`budget_ref` 会写入、读出，签发路径固定 `None`（`key_issue.rs:133`）。请求路径不读它。接口规范写明旧的 `budget_ref` 不是当前签发字段（`docs/interfaces/iDoris-Agent24-边界与接口规范.md:76`）。

没有单独的「成员关系」表。`tenant_id` 是 key 上的一个可选天花板，不是组织树。

### 1.5 契约里有、账本里没有的字段

`TenantContext`（`crates/idoris-contracts/src/tenant.rs:44-50`）还有可选 `quota.rpm` / `quota.tpm`。校验只要求 ≥ 1（`:107-113`）。`crates/**/*.rs` 里除了这个校验，没有读取 `quota.rpm` 或 `quota.tpm` 的请求路径。HTTP 用量查询把 `quota` 填成 `None`（`crates/idoris-router/src/queries/usage.rs:53`）。

TS `packages/tenancy/src/budget.ts` 的 `checkBudget` 只比较调用方传入的 `spent_minor >= limit_minor`，没有 SQLite，也没有预留。README 把它标成参考实现，Rust 账本不是逐行移植（`crates/idoris-tenancy/README.md:16-25`）。生产请求走 Rust 账本。

## 2. `POST /v1/chat/completions`：鉴权到预留再到结算

路由：`lib.rs:385-386`，外层中间件 `remote_bind_auth_middleware`（`:400-402`）。

### 2.1 鉴权（key 在这里已经知道）

1. 中间件对非 `/health` 请求取 `Authorization: Bearer idk_…`（`lib.rs:477-518`，解析在 `auth.rs:193-208`）。
2. `VirtualKeyStore::authenticate` 只匹配 `status='active'` 且未过期的哈希（`virtual_key/store.rs:84-117`）。未知、吊销、过期都变成 `None`，HTTP 不区分原因。
3. 个人形态：key 上若绑了租户，必须是内部 id `"personal"`，且不能用别的 `X-iDoris-Tenant` 冒充（`lib.rs:519-527`）。个人形态解析 profile 时**丢掉**租户头，`tenant_id = None`（`profile.rs:273-274`）。
4. `tenant` 形态：数据面路径缺 `X-iDoris-Tenant` 即 400 `tenant_missing`（`lib.rs:489-501`）。有头时必须与 key 上的 `tenant_id` 全等，否则 403 `tenant_scope_forbidden`（`:528-550`）。未绑定租户的旧 key 过不了这道。
5. 中间件把 `AuthenticatedVirtualKey` 放进 extensions（`lib.rs:555`）。**`chat_completions` 的函数签名不取这个 extension**（`lib.rs:2461-2466`）。处理函数若装了 authenticator，会再鉴权一次，只用于 `auth::enforce_scope`（隐私、角色、`next_in_chain`），然后丢掉 `identity`（`lib.rs:2501-2508`，`auth.rs:64-97`）。`enforce_scope` 不看 `key_id`，也不看 `tenant_id`。

因此在进预算之前，真实 `key_id`、key 上的 `tenant_id`、`owner` 都已经在内存里，但没有任何字段传到 `BudgetScope`。

`IDORIS_DEV_NO_KEY=1` 且没有 bearer、又不是 tenant 形态时，中间件直接放行（`lib.rs:503-504`）。非 loopback 绑定禁止这个开关（`bin/idoris.rs:252-256`）。

### 2.2 选路（模型和单价在这里已经知道）

`parse_profile` →（可选）事件 `request.received` → 内容检查 → 意图 → `dispatch::select_with_gate_fallback_and_completion`（`lib.rs:2487-2713`）。

候选报价：

- 常驻 HTTP：`estimate_cost_minor_with_completion`，输出 token 至少 1024，或请求里 `max_tokens` / `max_completion_tokens` 的较大值（`dispatch.rs:184-189`，地板 `budget.rs:18`、`:26-45`）。
- 其它（本机 Supervisor）：固定按 1024 输出 token 估价（`dispatch.rs:191`）。
- 单价来自组件卡 `provider.cost.input_per_m` / `output_per_m`（`f64`，每百万 token，单位与 `minor` 相同）。见 `crates/idoris-contracts/src/provider.rs:27-33`。
- 两边都是 0 视为免费；非有限或负数视为价格未知，不当免费（`budget.rs:62-71`）。

`PolicyCtx.budget` 为 `None`，策略层预算阶段不淘汰、不降级（见 §1.1）。

订阅卡（`provider.id` 为订阅）在 `lib.rs:2779-2853` 直接走订阅运行时，**不经过** `reserve` / `settle`。

### 2.3 两条执行路径

**本机 Supervisor**（`dispatch_local_inner`，`dispatch.rs:904` 起）：

1. `backend_model_id(...)` 在预留**之前**算出真正要加载的模型（`:952`）。
2. `ReservationGuard::reserve(ledger, tenant_id, provider_id, estimated_cost_minor)`（`:955-960`）→ `budget::reserve` → `scope()` 使用 `DEFAULT_KEY_ID` 和 `model_id = provider_id`（`budget.rs:200-216`）。`backend_model_id` 没有传进去。`tenant_id` 为 `None` 时账本租户是 `"personal"`（`budget.rs:55`、`:201`）。
3. 付费时写 `budget.reserved`，然后 `load` + `chat`（`dispatch.rs:976-1114`）。
4. 成功：`estimate_actual_cost_minor` 用**提示和补全文本的估算 token**，不用上游 `usage`（`:1124-1136`）。然后 `budget::settle`。结算失败被 `.ok()` 吃掉，响应仍 200；`actual_cost_minor` 只在付费且结算 `Ok` 时回传。极端 overage（>4×）会 `Err`，但账本已经入账（见 §5）。
5. 失败或 future 被丢弃：`Drop` 调用 `release`（`dispatch.rs:598-605`）。已可能出网时 `retain_on_drop` 会留下预留直到 TTL（`:582-586`）。本机这条路径的成功结算没有走这个「不确定就留着」分支。

**常驻 HTTP 代理**（`chat_via_proxy`，`lib.rs:3021`）：

1. `ProxyBudget::new(..., parsed.tenant_id, provider.id, estimate)`（`lib.rs:3030-3034`）。同样进 `scope()`，真实 key 和 body 里的模型都不进账本。
2. 付费且 `stream=true`：直接 503 `paid_proxy_unavailable`，不预留（`lib.rs:3054-3060`）。免费流式不进幂等缓存（`lib.rs:3078-3080`）。
3. 付费：`new` 里先 `check_admission(..., 0)`，不插行（`proxy_budget.rs:41-47`）；`prepare_for_post` 才按估价 `reserve`（`:66-78`）；`arm` 之后丢弃 future 不再 `release`（`:81-87`）。
4. 2xx：必须能解析 `usage.prompt_tokens` / `completion_tokens`，再用同一张卡的单价换算，`settle_replayable`（`proxy_budget.rs:89-128`，解析在 `budget.rs:120-138`）。缺 usage 或金额换算失败则留下预留到 TTL，不按 0 结算。
5. 缓存命中 / 未执行：`release_unsent`（`proxy_budget.rs:138-142`）。执行不确定：`retain_until_expiry`（`:143-147`）。

响应头 `X-iDoris-Cost-Minor` 只在本机路径结算出付费金额时设置（`lib.rs:2993-2998`）。402 只来自 `BudgetError::Exceeded`（`lib.rs:1943-1964`）。余额不够不会先压低 `max_tokens`，也不会自动换更便宜的模型。

### 2.4 占位符落在哪

`BudgetScope::new(tenant_id, DEFAULT_KEY_ID, provider_id, provider_id)`（`budget.rs:200-203`）是唯一构造点。因此：

- 所有预留、结算、子限额都记在 `key_id="default"`。
- `model_id` 列等于 provider id，不是请求模型，也不是 `backend_model_id`。
- 同一租户、同一 provider 下，不同虚拟 key、不同模型共用一个子范围。租户级 `tenant_config` 仍按租户汇总。
- 子范围配置 `BudgetLedger::configure` 存在（`ledger.rs:327`），启动路径不调用它。现网准入主要靠 `tenant_config`。

已知却没传入预算的值：

| 值 | 已知位置 | 预算实际用的 |
|---|---|---|
| `identity.key_id` | 中间件 `lib.rs:515-555`；处理函数 `lib.rs:2502` 随即丢弃 | `"default"` |
| key 的 `tenant_id` / `owner` | 同上 | 个人形态固定 `"personal"`；tenant 形态用请求头里的租户字符串 |
| 请求 `model`、角色映射后的 `backend_model_id` | `lib.rs:2485`；`dispatch.rs:952`（在 `reserve` 前） | `provider_id` |
| 组件卡单价 | 选路时已用于估价 | 只影响金额，不记供应商币种或价格版本。事件元数据允许 `price_version` 字段，这条调用链没有写入价格版本（未在预算胶水里检索到赋值）。 |

## 3. `retain_tenants` 与部署形态

### 3.1 调用点

生产调用只有一处：`storage::bootstrap` 在写入本次配置之后（`storage.rs:223-228`）。撤销数 > 0 时向 stderr 打「已撤销 N 个不再存在于可信配置中的租户」。

测试调用：`ledger.rs:2033`（`retain_tenants_revokes_stale_config_but_keeps_spend_history`）；启动行为测试 `storage.rs:836` `trusted_tenant_list_revokes_removed_tenant_on_restart`。

没有定时任务，也没有请求路径调用。每次进程启动跑一次。

行为（`ledger.rs:452-482`）：在 `BEGIN IMMEDIATE` 里，对 `tenant_config ∪ budget_config` 中不在 `keep` 集合的 `tenant_id`，`DELETE` 这两张配置表。**不删** `budget_periods`、`tenant_periods`、`reservations`、`budget_overage_events`。测试断言 beta 的已结算 7 仍留在两张 period 表（`ledger.rs:2043-2058`）。配置没了之后，新的 `reserve` 得到 `NotConfigured`。已有预留的迟到 `settle` 仍按行上的租户和范围入账（`settle` 不要求配置还在，见 `ledger.rs:955-969` 的注释）。

触发条件：只要 `bootstrap` 成功走到那一行。个人形态和 tenant 形态都走。`keep` 来自本次加载的 yaml；个人形态若没配文件，`keep` 只有 `"personal"`（下面）。

### 3.2 `deploy_mode` 现在怎么影响启动和存储

枚举只有 `Personal | Tenant`（`crates/idoris-contracts/src/deploy_mode.rs:9-12`）。JSON Schema 同样是 `["personal","tenant"]`（`packages/contracts/schema/deploy-mode.schema.json:5`）。

环境变量 `IDORIS_DEPLOY_MODE`（`bin/idoris.rs:279-280`）：空或 `personal`（忽略大小写、trim）→ `Personal`；**其它任何值，包括 `tenant`、`community`、`city`、拼错的词，全部收成 `Tenant`**（`profile.rs:113-127`）。没有「组织」和「云」两个独立值。文档里的组织大脑就是现在的 `tenant`。

| | `personal` | `tenant`（今天的组织形态） |
|---|---|---|
| 租户配置 | 未设 `IDORIS_TENANTS_CONFIG` 时不读文件，合成一条 `tenant_id=personal`、`limit_minor=0`、`UTC`、`paid_only`（`storage.rs:379-387`）。设了文件则必须包含 `personal`，否则启动失败（`:218-221`）。 | 必须有文件。未设环境变量时路径是**可执行文件目录**下的 `config/tenants.yaml`（`storage.rs:18`、`:68-71`）。仓库里没有这份文件。空文件或读失败即启动失败（`:389`、`:395-397`）。 |
| `retain_tenants` | 跑。默认集合只有 `personal`，库里其它租户的配置会被删掉。 | 跑。yaml 里消失的租户，重启后配置消失、不能再预留（测试 `storage.rs:836-864`）。 |
| 监听 | loopback 始终允许。非 loopback 需要已有有效 key，且不能开 `IDORIS_DEV_NO_KEY`（`bin/idoris.rs:238-262`）。 | 非 loopback **直接拒绝启动**。原因写在代码里：tenant 仍依赖调用方传来的 `X-iDoris-Tenant`（`:247-250`）。这和设计稿「云形态公网」不是同一个开关。 |
| 订阅中转 | 仅 `deploy_mode == "personal"` 才可能注册（`subscription/config.rs:67-70`）。 | 拒绝注册，原因 `subscription is personal-only`。 |
| 数据面租户 | 忽略 `X-iDoris-Tenant`（`profile.rs:274`）。 | 缺头 400，且必须与 key 绑定一致。 |

`limit_minor=0` 的个人默认值意味着：付费候选一预留就会 `Exceeded`（除非 `PaidOnly` 下金额为 0）。零成本本机模型仍可用。这是当前个人形态的账本行为。

### 3.3 若要加「组织」「云」，开关加在哪

今天不存在这两个枚举值。若把新字符串塞进 `IDORIS_DEPLOY_MODE`，`deploy_mode_from_env` 会把它当成现在的 `tenant`：仍 `retain_tenants`、仍拒绝非 loopback、仍要求 `X-iDoris-Tenant`。

建议落点（只列位置，不实现）：

1. `packages/contracts/schema/deploy-mode.schema.json` 与 `crates/idoris-contracts/src/deploy_mode.rs` 的 `DEPLOY_MODE_VALUES`。`contract_drift` 会对照这两边。
2. `profile::deploy_mode_from_env`：停止把未知值折叠成 `tenant`，否则新形态没有独立行为。
3. `storage::bootstrap` / `load_configs`：只在 `personal` 保持「配置清单即权威 + `retain_tenants`」。组织/云改为「文件只用于首次种子，之后以数据库为准」，不要每次启动删除。
4. `bin/idoris.rs` 的 `validate_bind_authority`：云形态的公网监听不能复用今天 tenant 的 loopback 限制；个人 loopback 契约不要放宽。
5. `subscription/config.rs` 的 `deploy_mode != "personal"` 拒绝：新形态应继续拒绝订阅中转，不要只靠字符串碰巧不等于 `personal`。
6. 数据面租户来源（设计稿 S5，不是本片）：今天云所需要的「从凭证推导租户」还不存在；tenant 模式仍是头 + key 绑定双重要求。

## 4. 虚拟 key：签发、校验、吊销；`admin_scopes`

### 4.1 签发

命令：`idoris key issue --spec-stdin`（`cli.rs:145-146`，`bin/idoris.rs:164-166`）。只打开 key 库，不跑租户预算 bootstrap（`storage.rs:76-86`）。

stdin JSON 字段：`owner`、`tenant_id?`、`allowed_privacy`、`allowed_roles`、`expires_at_ms?`、`admin_scopes`（默认空）。`deny_unknown_fields`（`key_issue.rs:15-25`）。

顺序：校验 → `MintedVirtualKey::mint` → **先**把 `{key_id, secret}` 写到 stdout → **再** `insert_active`（`key_issue.rs:57-78`）。stdout 失败则不入库。入库失败则调用方可能看见过明文，但 key 从未激活。

校验失败包括：空/超长/控制符 owner、非法 `tenant_id`、空或重复隐私、未知或重复角色、`expires_at_ms <= now`、**非空 `admin_scopes`**。

### 4.2 校验

见 §2.1。另外：`has_active_key` 供非 loopback 启动使用（`store.rs:122-135`，`bin/idoris.rs:374-384`）。过期在 SQL 和第二次时钟读两边都查（`auth.rs:174-180`）。

吊销后的请求不能再通过鉴权。缓存键含租户、URL、provider、`X-iDoris-Request-Id`（`proxy.rs:120-126`），**不含 key_id**。同一租户同一 request id 的缓冲成功体会在窗口内被重放；吊销不能按 key 作废那条进程内缓存。相关测试名：`revoked_tenant_key_cannot_replay_warmed_inference_caches`（`virtual_key_wiring_tests.rs:903`）。

### 4.3 吊销

`idoris key revoke <key_id>`（`bin/idoris.rs:169`）→ `key_manage::revoke` → `VirtualKeyStore::revoke`：`UPDATE ... SET status='revoked', revoked_at_ms=? WHERE key_id=? AND status='active'`（`store.rs:140-150`）。已吊销返回 `already_revoked: true`，不改时间戳。`list` 不输出哈希（`key_manage.rs:41-64`）。没有 HTTP 吊销接口。Admin v0 只有只读 GET：`/status`、`/backends`、`/models`、`/roles`、`/runtimes`（`admin.rs:178-182`），且数据面路由不挂这些路径（测试 `admin.rs:768` 一带）。

### 4.4 `admin_scopes` 定义在哪、为何被拒绝

定义：

- 列：`virtual_keys.admin_scopes TEXT NOT NULL`，必须是 JSON 数组（`0001_virtual_keys.sql:13-15`）。没有枚举值约束。
- 类型：`VirtualKeyScope.admin_scopes: Vec<String>`（`store.rs:20`）。
- 入库校验只拒绝空白字符串，不解释含义（`store.rs:236-239`）。
- 签发 spec 有这个字段（`key_issue.rs:23-24`）。

拒绝：

- 唯一的产品拒绝在 `key_issue.rs:122-124`。非空即错误 `"M4 virtual key 暂不允许 admin_scopes"`，测试用 `"future"` 当作非法样例（`:176`）。
- 通过校验后写入的 scope 把 `admin_scopes` 设为 `Vec::new()`（`:135`），即使输入是缺省空数组。
- 鉴权 `enforce_scope` 不读 `admin_scopes`。没有「操作 × 资源范围」的检查函数。

原因（代码能证明的部分）：这是 M4 留下的占位。列和类型先存在，签发口子关掉，避免发出还没有授权语义的管理权限。设计稿 H5 / §9 要把这个字段启用成控制面授权；那套授权现在不存在。脑暴稿把管理写接口放在当时的 S4，定稿改到了 S2（`compute-service-design.md:207`）。以定稿为准。

## 5. 预算实现对照设计稿 §4、§5

### 5.1 已经有的

- 两阶段：`reserve` 插 `active` 行，`settle` 把实际金额加进 `budget_periods` 和 `tenant_periods`，`release` 不记账。默认 TTL 5 分钟（`DEFAULT_RESERVATION_TTL_MS = 5 * 60 * 1000`，`ledger.rs:139`）。
- 同一事务里同时看子范围（若配置了）和租户总额（`ledger.rs:697-739`）。任一不够则拒绝，402 `budget_exceeded`。
- 在途：未过期的 `active.reserved_minor` 计入「已占用」。这是**和账期上限共用一个额度**，不是设计稿单独的「在途总额上限」。
- `paid_only`：金额 0 跳过检查；`all`：连 0 也要余额 > 0（`exceeds_limit`，`ledger.rs:1359-1370`）。
- 价格未知拒绝。个人默认未配置文件时限额 0，付费调用失败关闭，而不是当成无限。
- overage：`actual > reserved` 仍全额入账，写 `budget_overage_events`，`SettleReceipt.overage_minor`（`ledger.rs:924-987`）。超过 4 倍仍入账，然后返回 `OverageTooLarge`（`:930`、`:991-1001`）。`replay_settlement` 把这个错误当成已完成（`:1026-1027`）。本机路径用 `settle` 且 `.ok()`，所以 4 倍错误会变成「响应成功、头上没有成本」，账上已经扣了。
- 预留过期：`reserve` 时把**同一子范围、同一 period** 里到期的行改成 `expired`（`sweep_expired_scope`，`ledger.rs:832-853`）。余额求和本来就排除 `expires_at_ms <= now`。没有后台线程。`sweep_expired()` 是可选的全表清扫（`:817-828`）。
- 迟到结算：状态已是 `expired` 或 TTL 已过但仍 `active`，`settle` 照样扣款，`late=true`（`ledger.rs:864-904`）。不因为过期就免单。`release` 把过期行收成 `released`，避免随后再 `settle`（`:1125-1144`）。
- 并发：进程内 `Mutex<Connection>`，加上 SQLite `BEGIN IMMEDIATE` + WAL + 5 秒 busy。注释写明要堵住「先检查后扣款」的丢更新（`ledger.rs:231-239`、`:567-573`）。跨进程测试在 `crates/idoris-tenancy/tests/budget_concurrency.rs`，依赖 feature `idoris-tenancy/test-bins`。另有 `mutation-test-hooks`，专门把事务拆开以证明测试能抓住竞态（`ledger.rs:746-759`）。单写进程还有文件锁。`SQLITE_BUSY` 映射为 `BudgetError::Busy`（`error.rs:176-184`），路由把它变成 500，不是 402。
- 整数溢出：不把损坏的 REAL 四舍五入进新表（`0005` 注释）。结算加总会溢出时，金额进 `budget_amount_overflows`，两张合计表不变，租户被锁住。

### 5.2 对照 §4 的缺口

| 设计稿 §4 | 现状 |
|---|---|
| 金额带 `(单位, 精度, 用途, 有效期, 可否转让=否)`，单位只有 `THB`、`USD`、`QUOTA:<节点>` | 无类型。一个 `i64` 叫 minor。契约只说「最小货币单位」，没有写是分还是微单位。测试注释举过「例如分」的例子（`budget.rs:287-289`），那是测试说明，不是账本约束。 |
| 货币精度为微单位 1e-6，不用分 | 未实现。报价是 `f64` 的「每百万 token」再向上取整到 `i64`（`budget.rs:74-80`）。`f64` 不能当微单位账本。用量汇总再用 `f64` 相加（`billing.rs:31-36`）。 |
| 每笔同时记 `cost`（供应商币种）和 `price`（账户币种），换算要汇率和时间 | 只有一笔 `*_minor`。没有汇率表。 |
| 免费额度是补贴池授权，发放幂等键 `(成员, 月份)`，池和成员各记一笔 | 无补贴池、无发放、无幂等键。 |
| 配额不可转让、不可兑现 | 无转让 API。也没有类型系统去禁止。 |

### 5.3 对照 §5 的缺口

| 设计稿 §5 | 现状 |
|---|---|
| 沿**所有祖先**同时检查五项：余额或授权、授信下限、速率、在途上限、补贴池总上限 | 没有祖先。只有「租户总额 + 可选的四元组子限额」。子限额不是父账户。五项里只有「余额（含共用的在途占用）」和「零成本是否也检查」。 |
| 速率闸门（分钟/小时花费） | 无。`quota.rpm/tpm` 只在契约结构里。Provider breaker 是故障熔断（`IDORIS_PROVIDER_BREAKER`，`bin/idoris.rs:432-436`），不是花费速率。 |
| 单独的在途总额上限 | 在途占用账期余额，没有第二道上限。 |
| 预留 = `max_tokens × 输出单价 + 输入估算`，余额不够先压 `max_tokens` 或降级模型 | 常驻 HTTP 会把显式 cap 抬到估价里，且不低于 1024。不够就 402。不改写 `max_tokens`。策略层降级代码在 `budget_stage`（`pipeline/mod.rs:133-141`），但路由传入的 `budget` 是 `None`，这条不会发生。 |
| 有担保的负值，透支到授信下限 | 无担保人、无授信下限。`settle` 可以让已结算额超过 `limit`（overage）。下一笔 `reserve` 会 402。没有「允许负到某下限」。 |
| 宽限：每成员每周期最多 1 次，金额不超过一次预留，不依赖 overage | 无宽限表、无计数、无通知。 |
| overage 只是事实：担保范围内由担保人承担，超出记平台损失账户，告警并冻结凭证 | overage 记在同一租户的 `spent_minor` 和 `budget_overage_events`。没有损失账户，不冻结 key。 |
| 预留过期后的迟到结算走同一套担保/损失规则 | 迟到结算直接记在原租户，`late=true`。没有损失账户分支。 |
| 体验分级和原因码 `budget_degraded` / `capacity_degraded` / `upstream_failover` / `grace_used` / `budget_exhausted` | 预算拒绝的原因码是 `budget_exceeded`（`lib.rs:5255` 一带的测试所断言的字段）。没有 `budget_exhausted` 这个字符串，也没有 `grace_used`。容量/故障是另一套 503，不在账本。 |

并发安全的边界：账本单连接互斥 + `BEGIN IMMEDIATE` 能保住「检查和插入预留」原子。它保不住设计稿的祖先链，因为祖先链不存在。用量写入、事件日志、key 库是另外三条连接，不在预算事务里。结算成功但 `budget.settled` 事件失败时，代码只打 stderr，不回滚账本（`dispatch.rs:1165-1173`）。

## 6. 代理响应缓存

设计稿 H8「存的是完整 body」与代码一致。

`CacheEntry`（`proxy.rs:104-118`）含：

- `at: Instant`
- `fingerprint: [u8; 32]`（请求体 + 隐私 + served locality）
- `status: u16`
- `body: Bytes`（成功响应的完整 body，`remember` 时 `outcome.body.clone()`，`proxy.rs:1319-1329`）
- `record_id`
- `served_locality`

键：`"{tenant}\0{url}\0{provider_id}\0{request_id}"`（`proxy.rs:120-126`）。无租户时用 `"\0personal"`（`proxy.rs:1210`）。

写入条件（`proxy.rs:1314-1330`）：缓冲转发、带了 `X-iDoris-Request-Id`、成功门通过、状态 2xx、`execution == Executed`、本响应不是缓存命中。没有 request id 的请求不写缓存（`:1200-1207`）。流式不进这个缓存（`lib.rs:3078-3080`）。

窗口默认 60 秒（`proxy.rs:96-97`）。条目上限 1000（`:101`）。字节池 32 MiB（`:806`）。超限从最旧的挤掉（`prune`，`:875` 起）。命中时原样回放 body、`X-iDoris-Cached`、`X-iDoris-Origin-Record-Id` 和当时的 served locality（`lookup_cached`，`:1433-1459`）。`local_only` 不会重放非 loopback 的条目。

开关：**没有**环境变量或租户属性可以关掉它。`ChatProxy::new` 总是带上缓存（`proxy.rs:793-818`，`AppState` 在 `lib.rs:353` 构造）。付费与否都不决定是否存储 body；付费只是额外要求 usage 能解析，否则成功门失败、不写入（`finalize_success`，`proxy.rs:1374-1385`）。进程内 `Mutex<IndexMap>`，不落 SQLite。重启即空。

本机 Supervisor 路径不经过 `ChatProxy` 的这段缓存。

edu 租户「关闭响应缓存」在设计稿 §7 / S6。现状没有租户级开关。

## 7. 测试与门禁

### 7.1 可作回归基线的测试

账本本身（改 `reserve` / `settle` / 迁移时优先保持全绿）：

- `crates/idoris-tenancy/tests/budget_concurrency.rs`：跨线程/跨进程，不超卖。CI 才打开 `test-bins`。
- `crates/idoris-tenancy/tests/budget_mutation_hooks.rs`：拆事务必须被抓住。CI 才打开 `mutation-test-hooks`。
- `crates/idoris-tenancy/tests/budget_ttl.rs`：过期预留在下次 `reserve` 时让出额度。
- `crates/idoris-tenancy/tests/budget_period_boundary.rs`：时区账期边界。
- `crates/idoris-tenancy/tests/budget_public_api.rs`、`budget_readview.rs`、`settlement_replay.rs`。
- `crates/idoris-tenancy/src/budget/ledger.rs` 内的 `retain_tenants_revokes_stale_config_but_keeps_spend_history`（约 `:2012`）、overage、租户级与子范围互相否决。
- `crates/idoris-tenancy/tests/scoped_store.rs`：无租户范围不得扫全表。

启动与 `retain_tenants`：

- `crates/idoris-router/src/storage.rs` 的 `personal_defaults_are_persistent_and_unknown_tenant_is_not_fake_zero`（`:518`）、`trusted_tenant_list_revokes_removed_tenant_on_restart`（`:836`）。
- `crates/idoris-router/tests/portable_startup.rs`：进程启动与 key 库。

请求接线（S1 换掉 `DEFAULT_KEY_ID` 时，这些会锁定「现在记在 default / provider id 上」的行为，改完必须改断言，不能当「一行不动」的黄金）：

- `crates/idoris-router/src/budget.rs` 的 `reserve_settle_release_round_trip`（`:404`）。
- `crates/idoris-router/src/lib.rs`：`chat_completions_paid_candidate_over_budget_is_402_with_a_structured_body`（`:5233`）、`chat_completions_paid_candidate_settles_and_sets_the_cost_header`（`:5262`）、`paid_buffered_proxy_settles_explicit_usage_and_cache_replay_does_not_charge_twice`（`:5386`）。
- `lib.rs` 模块：`budget_concurrency_wiring_tests`、`budget_reserved_wiring_tests`、`budget_settled_wiring_tests`、`budget_completion_reservation_tests`（`:3818`、`:3845`、`:3854`、`:6273`）。
- `proxy_budget.rs` 的单元测试；`paid_replay_admission_tests.rs`。

凭证：

- `key_issue.rs` 的 `issues_once_persists_only_hash_and_reopens_scope`、`rejects_unsafe_metadata_before_minting`（含 `admin_scopes:["future"]`）。
- `key_manage.rs` 的吊销测试；`crates/idoris-router/tests/key_manage_cli.rs`；`crates/idoris-router/src/cli.rs` 的解析测试。
- `auth.rs` 内的鉴权测试；`virtual_key_wiring_tests.rs`。
- `crates/idoris-tenancy/src/bin/conformance_key_seed.rs`：一致性测试的播种，`admin_scopes` 为空。

形态与契约：

- `crates/idoris-contracts/tests/contract_drift.rs` 的 `deploy_mode_values_match_schema`（`:225`）。
- `packages/contracts/tests/contract/tenant.test.ts`。
- `packages/tenancy/tests/budget.test.ts`、`billing.test.ts`、`store.test.ts`：TS 参考实现，不是 Rust 账本。
- `conformance/tests/tenant.test.ts`：`deploy_mode=tenant` 缺头 → 400。

策略层 `crates/idoris-policy` 的 `budget_stage` 测试描述的是「若传入 `BudgetView`」的行为。当前路由不传入。它们不能证明线上会降级。

### 7.2 门禁

本地文档没有一份单独的「必须跑这些」清单。CI 实际命令在 `.github/workflows/ci.yml`：

`gates`（ubuntu）：

- `pnpm install --frozen-lockfile`
- `pnpm lint` → `eslint .`（根 `package.json:10`）
- `pnpm typecheck` → `pnpm -r typecheck`
- `pnpm check:contract-drift`
- `pnpm build`
- `pnpm test` → `pnpm -r test`
- `pnpm conformance`

`rust`（ubuntu）：

- `rustup toolchain install 1.95 --profile minimal --component rustfmt,clippy`
- `cargo fmt --check`
- `cargo clippy --all-targets --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks -- -D warnings`
- `cargo test --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks`
- `bash scripts/conformance-rust.sh`（并设坏的 `IDORIS_CONFORMANCE_CMD` 做正控）
- `bash scripts/check-rust-egress.sh`（先装 strace）
- 用错误 argv 跑 `pnpm conformance`，必须失败且日志含 ENOENT
- `cargo test -p genai --lib 'webc::'`
- `cargo deny`：`check advisories bans licenses sources`

`subscription`：ubuntu 与 macos，`bash scripts/check-subscription.sh`。

`release-helpers`：macos，`python3 -B -m unittest discover -v -s scripts -p 'test_*release*.py'`。

根 `package.json` 里还有不在 CI `gates` 主步骤单独点名、但会被 `pnpm -r test` 或手工调用的脚本：`test:tenancy`、`test:billing`、`test:privacy`、`test:audit`、`test:egress`、`test:golden`、`test:growth`、`test:federation`、`smoke`、`smoke:agent24`、`smoke:external`。包管理器字段是 `pnpm@10.34.5`。CI 注释写明 setup 不要再写 version。

本机不要设 `CARGO_TARGET_DIR`（mbx）。CI 的 cargo 缓存路径含 `target`，那是 GitHub runner，不是本机 mbx。

## 8. 改造点清单（不实现）

S0（单位类型、主体/成员/付款/凭证、状态机、许可表、原因码）在代码里还没有。S1 依赖 S0。下面按「现在就能动的文件」列，避免假装 S0 类型已存在。

### 8.1 S1

要动：

- `crates/idoris-router/src/storage.rs`：`bootstrap` 的 `retain_tenants` 条件；种子与「数据库为准」的分叉。
- `crates/idoris-router/src/budget.rs`：`scope()` / `DEFAULT_KEY_ID`。调用方要能传入真实 `key_id` 和 `model_id`。
- `crates/idoris-router/src/dispatch.rs`：`ReservationGuard::reserve` 与 `dispatch_local_inner` 在 `:952` 已有 `backend_model_id`，这里把模型传下去。
- `crates/idoris-router/src/proxy_budget.rs` 与 `lib.rs` 的 `chat_via_proxy`：同样把 key 和模型传入。`chat_completions` 需要接住中间件已放入的 `AuthenticatedVirtualKey`，不要鉴权两次后丢掉。
- `crates/idoris-router/src/profile.rs` 与 `deploy_mode.rs`：仅当 S1 就要区分组织/云时。若 S1 只把「今天的 tenant」视为组织+云的临时合体，仍须改 `retain` 的条件，否则组织重启继续删在线租户。
- `crates/idoris-tenancy/src/budget/ledger.rs`：若范围键含义变了，迁移要处理旧行。
- 启动测试 `storage.rs`、`portable_startup.rs`，以及 §7.1 里断言 402/结算的路由测试。

建议新增（S1 最小集，不是把 S2 的树一次建完）：

- 一张「租户配置以数据库为准」的标记或种子表，例如 `tenant_bootstrap(tenant_id, seeded_at_ms, source)`。首次从 yaml 写入 `tenant_config`，之后启动不再 `DELETE`。个人形态不要用这张表改变今天的 `retain`。
- 不必在 S1 新建账户树。先把现有 `BudgetScope.key_id` 写成真实 `vk_…`，`model_id` 写成真实模型。`tenant_config` 继续当这一层的额度，直到 S2/S3 的账户表接上。
- 若 S0 的金额类型同时落地，新增类型应包住 `i64`，不要把现有列改成 `REAL`。

迁移怎么保住个人形态：

- `IDORIS_DEPLOY_MODE` 为空或 `personal` 时，继续合成或要求 `tenant_id=personal`，继续 `retain_tenants`，继续 `limit_minor=0` / `UTC` / `paid_only` 的默认（`storage.rs:379-387`）。
- 个人形态的历史行全是 `key_id='default'` 且 `model_id=provider_id`。若个人请求也改记真实 key，旧的 `budget_periods` 不会自动算进新 key，限额会「看起来被重置」。个人形态要么继续用 `default` + provider 当模型，要么做一次显式改写并在测试里锁定改写前后的 `spent_minor`。这是 owner 决定（见下）。
- 不要改 `DeployMode` 的未知值折叠，除非新值有自己的测试。折叠是为了避免误配跳过租户头（`profile.rs:113-119`）。
- 订阅拒绝、loopback、`X-iDoris-Tenant` 在个人形态被忽略，这些回归要留在 `subscription` 与 conformance。

风险：

- 只改 `scope()` 而不改历史行：同一租户限额下，新 key 与旧 `default` 行分账，已花费对不齐。
- 组织形态停止 `retain` 之后，从 yaml 删掉的租户会一直留在库里。需要别的吊销路径，否则「配置里删了」不再等于「不能花」。
- 本机结算用文本估算、代理结算用 `usage`。把模型 id 接上不会自动统一这两套金额。
- `chat_completions` 与中间件双重鉴权。只改一边会让 key 仍传不进预算。
- 事件日志和用量记录今天也不写 `key_id`。只改账本范围，审计仍对不上凭证。

要 owner 定：

- 个人形态是否也取代 `DEFAULT_KEY_ID`，还是只有组织/云取代。设计稿 S1 的句子是取代；§9 第 2 条只把「保持现状」说在 `retain_tenants` 上。两句要一起拍板。
- 今天的 `tenant` 在 S1 算组织、算云，还是暂时两个都算、只先停删除。
- 已有 `key_id='default'` 的已结算额是否归并到某个真实 key。
- minor 在 S1 是否就换成带币种的微单位，还是留到 S0 的类型落地之后再换算。现在没有币种，不能安全地「乘 10000 当成微单位」。

### 8.2 S2

要动：

- 新的控制面模块（不要挂进 Admin v0）。Admin 路由与只读约束在 `admin.rs:170-192` 和 `crates/idoris-contracts/src/admin_v0.rs`。
- `key_issue.rs` 的拒绝、`VirtualKeyScope.admin_scopes`、`0001_virtual_keys.sql` 的无枚举 JSON 数组。启用前要有封闭的操作名和资源范围，不能把任意字符串放行。
- `bin/idoris.rs` / `cli.rs`：今天只有本地 stdin 签发/列表/吊销。控制面若走 HTTP，要新命令或新监听，并写审计事件。
- `auth.rs`：`enforce_scope` 不管管理操作。授权检查要新函数，且「下发权限 ≤ 授权者本人」。
- `virtual_key/store.rs`：吊销已有；缺开户、成员、额度、担保的写 API。`budget_ref` 今天不参与决策，不要直接把它当成账户 id，除非做迁移说明。

建议新增的表和类型：

- `principals(principal_id, issuer, external_ref, created_at_ms)`
- `memberships(principal_id, org_id, role, created_at_ms)`，一个主体多行。
- `accounts(account_id, parent_account_id, currency, purpose, expires_at_ms, transferable)`。S2 可以先只建表和授权上限，祖先检查放到 S3。
- `credentials`：或扩展 `virtual_keys`，但要能指向**一个**付款账户。一次请求不能自选付款人。
- `allocations(parent_account_id, child_account_id, cap_minor, period)`：授权上限，不是余额转移。
- `guarantees(guarantor_account_id, account_id, floor_minor)`：授信下限。谁能担保不能从「是不是成员」推断。
- `admin_grants(credential_id, op, resource_scope)`，替代自由字符串 `admin_scopes`。
- `control_audit`：每次写操作一条，可复用 `event_log` 的新 `event_type`，但正文仍不能进 metadata。

个人形态不变：

- 不开放控制面监听；个人继续 `idoris key issue`，且 `admin_scopes` 仍为空。
- 不给个人形态自动创建组织树。内部租户 `personal` 保持单账户。
- 局域网组织形态默认不互信：新接口不要因为来源地址在 LAN 就跳过凭证。

风险：

- 先放开 `admin_scopes` 再补授权，会把今天故意拒绝的能力发出去。
- key 缓存键不含 `key_id`（§4.2）。控制面吊销后，60 秒内同 request id 仍可能重放 body。S2 若承诺「撤销后调用被拒」，要处理这扇窗口，或明确它只保证新的上游调用被拒。
- 签发「先 stdout 后入库」在控制面批量发证时会留下「看见了但未激活」的窗口。HTTP API 应换成先入库再交付，或可补偿吊销。

要 owner 定：

- 控制面是独立端口，还是同一进程的另一条路由。设计稿只说独立于 Admin v0。
- `admin_scopes` 是改成结构化列，还是新表、旧列永远只许 `[]`。
- 云账号配对、解绑、多设备是否进 S2 的第一刀，还是 S2 只做开户和额度。设计稿 §3 把这些行为放在 S2，但没有状态机细节。
- 老师担保费用与家长同意是两个对象。S2 的担保表是否禁止把「同意」塞进同一个角色字段。

### 8.3 S3

要动：

- `ledger.rs` 的 `admit`：从两层检查改成沿祖先的五项检查，仍须在一个 `BEGIN IMMEDIATE` 里完成。
- `proxy_budget.rs` / `dispatch.rs`：余额不足时的降级或压低 `max_tokens` 必须发生在 `reserve` 之前，并且和最终结算用的模型一致。现在估价和结算模型可以不是同一个 id。
- `budget.rs` 的 `priced_minor`：在金额类型落地前不要继续用 `f64` 做新币种的账。
- `lib.rs` 的 `budget_error_response`：原因码今天是 `budget_exceeded`。设计稿要 `budget_degraded`、`grace_used`、`budget_exhausted` 等，并写到响应头。改码会破坏 `lib.rs:5255` 和 conformance 里对 `budget_exceeded` 的断言，要版本化而不是静默替换。
- 策略层 `PolicyCtx.budget`：要么接上真实只读快照，要么删掉「管道会做预算降级」的假设。两套闸门并存会一个放行、一个 402。

建议新增：

- `account_ancestors` 或可递归的 `parent_account_id`（SQLite 可以 `WITH RECURSIVE`）。预留行要记下本次锁定的祖先链版本。
- `rate_buckets(account_id, window_start_ms, window_kind, spent_minor)`，窗口是分钟和小时。
- `inflight_caps(account_id, cap_minor)`，与账期 `limit` 分开。现有 `reservations` 可继续当在途明细。
- `subsidy_pool(pool_id, cap_minor)` 与 `subsidy_grants(principal_id, period, amount_minor)`，唯一键 `(principal_id, period)`。
- `grace_uses(principal_id, period)` 唯一，金额 ≤ 当次预留，发放不读 overage。
- `ledger_entries(entry_id, account_id, kind, amount_minor, currency, reservation_id, attempt_id)`，`kind` 至少能区分成本、售价、补贴、担保承担、平台损失。在这张表出现之前，不要把 overage 继续只加进租户 `spent_minor` 就当成损失账户。
- 冻结：`credentials.status` 增加冻结，或单独 `frozen_at_ms`。今天只有 `active|revoked`。

个人形态不变：

- 个人不计费（设计稿 §9）。S3 的祖先/授信/速率/补贴默认不装到 `personal`。个人仍是限额 0 + `paid_only`，零成本放行，付费 402。
- 不把个人的 `default` 行解释成补贴或授信。
- 新表的检查函数在 `DeployMode::Personal` 短路上保持今天的 `admit` 结果。用 `budget_concurrency.rs` 和 `chat_completions_paid_candidate_*` 做对照。

风险：

- 祖先检查若拆成多次事务，会重新引入账本注释里写的超卖。必须和现有 `BEGIN IMMEDIATE` 同一事务。
- overage 已经会把 `spent_minor` 打成超过 `limit`，`CHECK (spent_minor >= 0)` 仍允许超过上限。若 S3 引入授信下限，旧的「超限但仍入账」语义要映射到损失账户，否则恢复备份后会重复扣。
- 4 倍 overage 返回错误但已提交。调用方若重试 `settle` 而不是 `replay_settlement`，会得到 `ReservationNotActive`。S3 改错误语义时要保留这条幂等。
- 预留 TTL 5 分钟是全局的。迟到结算今天记在原租户。改成损失账户后，过期清扫与迟到结算的竞态要加测试（现成的 `budget_ttl.rs` 只覆盖「过期后额度释放」）。
- 速率桶和账本抢同一把 SQLite 写锁。设计稿 §10 写明 QPS 未知。S3 不要同时做多写节点。

要 owner 定：

- 宽限的「周期」是账期 `YYYY-MM` 还是自然日。
- 速率闸门按花费（设计稿 §5）还是同时启用契约里的 `rpm/tpm`。两者不是同一个量。
- 降级是「同场景更便宜的模型」还是仅压 `max_tokens`。现有路由没有价格目录，只有组件卡上的一个 `Cost`。
- `budget_exceeded` 是否保留为总原因，细分码放在别的字段。直接改掉会破坏已发布的 402 体。
- 平台损失账户的币种，在 §4 的单位类型定下来之前不能记账。

## 9. 未核实

- 没有为本文做外部网页检索。文中没有引用需要 URL 的外部事实。设计稿 §11 的 GPU 价格、§15 的法律与供应商条款不在本次代码勘察范围内，那些条目在设计稿里已标成待核实。
- `crates/idoris-tenancy/README.md:69-74` 对 token 估算的描述（CJK、`bytes/3`、1.2 系数）与 `estimator.rs:19-38` 的现行规则不一致。现行代码是 ASCII `bytes/3` 向上取整、非 ASCII 每字节 1 token。以 `estimator.rs` 为准。README 的漂移是文档问题，不是账本行为。
- CodeGraph 索引在本 worktree 未初始化，本次用源码和迁移文件核对，没有用 CodeGraph。
- 没有跑 `cargo test` / `pnpm test`。测试清单来自文件名和源码里的 `#[test]` / `#[tokio::test]`，不是一次绿的执行记录。

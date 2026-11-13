# PR #600 pre-pr-check 逐条回应

pre-pr-check rules 1.5.1 (251aa7165e9c)

- 检查器：本机 `~/Dev/tools/PR-daemon/scripts/pre-pr-check.sh --version`，2026-11-13 执行，输出即上一行。规则说明读的是本地 `~/Dev/tools/PR-daemon/.claude/skills/pre-pr-rules/SKILL.md`。
- 发现清单：228 条 review，0 条 block。计数：SZ 4、D1 32、T2 69、E4 5、B1 104、C2 13、R1 1。
- 对照的 diff：`origin/main...HEAD`（本文写作时 HEAD `0c55384e2e703594dc00e4711d52c0343fd8b9c8`，分支 `task/compute-prepr-answers`，跟踪 `origin/feat/compute-service`）。
- 本文不改代码。标了「未找到」的，是读过守卫和测试之后没有一条现有测试会因为去掉该守卫而变红。没有把邻近函数名当成测试名。
- 外部事实：PR 本身见 <https://github.com/iDoris-ai/iDoris/pull/600>（2026-11-13 用 `gh pr view 600` 核对标题、base `main`、head `feat/compute-service`、正文开头）。设计稿正文与附录是仓库内文件，不是外部事实。

## SZ-1

改动 51398 行（检查器口径，不含锁文件、生成物、测试代码），超过 300。

不拆 PR。owner 在本次回应任务里指定：所有切片在一条功能分支集成、充分测试后提一个 PR。这句话不在 `docs/design/compute-service-design.md` 附录里。附录「已拍板（owner，2026-11-13）」（设计稿 268–271 行）只记了两件事：已发送尝试报 0 用量按 0 收费，以及儿童租户安全检查器方案 B。

设计稿 §12 标题（第 200 行）原文仍是「每片一个 PR 进 main，用配置开关隔离」。实现和 [PR #600](https://github.com/iDoris-ai/iDoris/pull/600) 正文写的是另一件事：「九个切片都在 `feat/compute-service` 上集成，逐片验收后合入」。评审按下面的切片边界和合并提交看，不必把整份 diff 当成一块。

`git log --merges`（`origin/main..HEAD`）有 31 个合并提交。S7 的合入 `05baa2d6` 只有一个父提交，不在 `--merges` 里，下面单独列出。短号在本分支上唯一。

| 片 | 边界（与 PR #600 正文、设计稿 §12 对齐） | 合入提交 |
|---|---|---|
| S0 | `crates/idoris-compute-contracts`：金额、账户树、凭证、准入、状态机、许可表、原因码 | `d1e8e1f8` 合入 task/compute-s0 |
| S1 | `idoris-tenancy/src/accounts/` 账户存储；真实预算接线 | `e8c40de5` 合入 task/compute-s1；`a076cc9f` 合入 task/compute-s1-recon |
| S2 | `/control/v1`、配对、bootstrap、操作授权 | `0d01df15` 合入 task/compute-s2；`1f91e2f6` 合入 origin/feat/compute-service（S2） |
| S3 | `accounts/ledger/` 准入、预留、结算、宽限、补贴 | `a874f49d` 合入 task/compute-s3；`ec4dd431` 合入 origin/feat/compute-service（S3，含 S2） |
| S4 | 云上游、尝试记录、许可表硬约束 | `4919beb8` 合入 task/compute-s4；`da58e887` 合入 origin/feat/compute-service（S4）；`ff3aeb5e` 合入正式端点许可表 |
| S5 | `cloud/ingress/` 凭证推导租户、三级上限 | `3655decf` 合入 task/compute-s5 |
| S6 | `cloud/safety/` 儿童安全、正文不落盘 | `45ad48b0` 合入 task/compute-s6；`2d7bb841` 合入 origin/feat/compute-service（S6） |
| S7 | `cloud_tier/`、对账游标。合入不是 merge commit | `05baa2d6` `merge: 合入 task/compute-s7`（父提交 `3655decf`）；`e9143b5d` 记录 s7 历史 |
| S8 | 评测集、端点核验、许可表、合规备忘、Litestream 演练、泰语对比 | `4ce9e87c` evalset；`4aeea416` endpoints；`2f4db0cd` runner；`0fea3a6c` ops-backup；`4d59185b` legal；`d8ee4acc` 泰语对比报告 |
| 排除 AWS | owner 决定不使用任何 AWS 方案。这不是候选 | `6580833c` 合入 task/compute-no-aws |
| 审查修复 | 合入功能分支之后的修复，不是新切片 | r1 `0aaa1fdc`；r2 `3fc7d8df`；r3 `aa20b24f`；r4 `53fbb7ae`；方案 B `2ccef157`；附录 `31f8b7f2`、`ad789012` |
| 测试稳定 | 两个启动集成测试 | `f7a37611`、`8374e392`、`250d9952` |

## SZ-2

改了 243 个文件，超过 8。同一条 owner 指定，不拆。文件落在上表的切片目录里，按片看。

## SZ-3

跨 4 个计入的顶层目录：`config`、`crates`、`eval`、`scripts`（测试和文档目录不计），超过 3。

- `config/`：云端档示例和端点许可表，S4/S7/S8。
- `crates/`：S0–S7 的实现。
- `eval/`：S8 泰语 edu 评测，本机脚本，不是服务进程。
- `scripts/`：出网检查和 Litestream 恢复演练，S8 运维。

四个目录是同一条纵向路径的配置、实现、评测和演练，按上表切片看。

## SZ-4

高风险标签是 `migrations` 和 `money_security`，和其它改动在同一个 PR 里。检查器点名混入的文件：`.gitignore`、`Cargo.toml`、`config/compute/cloud-tier.example.yaml`、`config/compute/endpoint-permit-table.yaml`、`crates/idoris-compute-contracts/Cargo.toml`。

不拆。迁移和账本在 S1/S3，许可表在 S0/S4/S8，工作区清单和根 `Cargo.toml` 是把新 crate 接进工作区。钱和迁移的测试在账本并发测试和准入测试里（见 C2 的 `account_store_concurrency.rs`、`ledger_concurrency.rs`）。评审看 S1 和 S3 那几颗合并提交即可。

## D1

32 处。下面每处说明是「取不到」还是「确实没有」，以及默认值为什么不出网、不多放行、不少收钱。没有一处是「检查失败就放行且不安全」。`holds.rs:562` 的损坏行边界写在该条和文末，未标「⚠ 需修」：查询失败走 `?`，不是失败即放行。

### `crates/idoris-compute-contracts/examples/validate_permit_table.rs:42`

`.unwrap_or(0)`。时钟早于 Unix epoch 时毫秒转换失败，示例工具把「现在」当成 0。这是取不到。示例不在请求路径上，不放行、不记账、不出网。

### `crates/idoris-compute-contracts/src/account.rs:257`

`depth().unwrap_or(0)`。账户已经在树的 map 里。`depth` 只有成环时才是 `None`，构造函数在此之前已经拒绝环。这是取不到的兜底，避免 panic。正常路径走不到。按深度 0 不会额外放行一棵已通过构造的树。

### `crates/idoris-compute-contracts/src/account.rs:341`

`ancestors().unwrap_or(false)`。账户不在树上，就不是任何人的后代。这是确实没有这个账户。结果偏拒绝。

### `crates/idoris-compute-contracts/src/money.rs:314` 和 `:317`

展示舍入的 `unwrap_or(0)`。注释写运算不会溢出，这里只是不 panic。只影响展示用的小数，不写账本，不少收、不多收。取不到（溢出这一支在注释里认为走不到）。

### `crates/idoris-compute-contracts/src/request.rs:1863`

在 `proptest!` 测试 `request_follows_the_reference_model_and_settlement_invariants_hold` 里。进度模型找不到合法边时用码 0。测试代码，不在服务路径上。

### `crates/idoris-router/src/bin/idoris.rs:543`

`tenant_policies().unwrap_or_default()`。没有入口配置就是空的租户策略名单。这是确实没有，不是读文件失败。空名单不会多放行儿童租户。

### `crates/idoris-router/src/cloud/executor.rs:453`

尝试记录里没有模型名时用空串，随后仍然调用输出检查。这是取不到名字。不跳过检查，不出网放行。

### `crates/idoris-router/src/cloud/ingress/abuse.rs:277`

剩余毫秒转 `u64` 失败时用 0，再 `.max(1)`。`Retry-After` 至少 1 秒。这是取不到。不放行，限流回答仍然是拒绝。

### `crates/idoris-router/src/cloud/ingress/bucket.rs:376` 和 `:378`

`|| true` 在测试函数 `a_table_full_of_in_flight_entries_may_exceed_its_cap_rather_than_lose_one` 的闭包里，插入值 `true` 表示 in flight。检查器的 `\|\|\s*true` 把 Rust 闭包当成 shell 的 `|| true`。测试代码。

### `crates/idoris-router/src/cloud/mod.rs:358`

`u128::try_from(ttl_ms).unwrap_or(0) <= longest` 则 `return Err`，拒绝启动。取不到（负数或溢出）当成 0，比较更容易失败。失败即关闭，不出网。

### `crates/idoris-router/src/cloud/safety/config.rs:494` 和 `:497`

`civil_from_days` 里 `try_from` 失败用 0。`days_from_civil` 往返对不上就返回 `None`，非法日期被拒绝。取不到。不把非法确认日当成有效放宽。

### `crates/idoris-router/src/cloud/safety/extract.rs:114`

`decoded_pieces().unwrap_or_default()`。解析失败得到空列表。注释写 rebuild 时已经失败即关闭。`judge_text` 空则回退原文，不把文本丢掉。取不到解析结果，不是跳过检查。

### `crates/idoris-router/src/cloud/safety/extract.rs:805`

响应里没有 `created` 就用 0。这是字段确实没有。只重建元数据，不因此放行。

### `crates/idoris-router/src/cloud_tier/forward.rs:406`、`:836`、`:858`、`:887`、`:930`

五处都是测试回调 `&|| true`。测试代码。检查器把闭包 `|| true` 报成失败即放行。

### `crates/idoris-router/src/cloud_tier/mod.rs:804`

请求体没有 `stream` 就当 `false`。这是字段确实没有。非流式比流式更严，不多放行。

### `crates/idoris-router/src/cloud_tier/mod.rs:1068` 和 `:1098`

`contains_secret().unwrap_or(true)`，以及锁中毒 `Err(_) => true`。判不了就当成「是本地密钥」。宁可拒绝回落、不出网。取不到（或锁不可用）。对出网是失败即关闭。

### `crates/idoris-router/src/control/pairing_codes.rs:66`

码位 `unwrap_or(0)`。索引在位宽内；失败落到字母表第 0 个字符。不是跳过校验，也不是放行一个空码。取不到。

### `crates/idoris-tenancy/src/accounts/ledger/holds.rs:562`

`hold.charged.unwrap_or(0)`，在 `HoldState::Settled` 分支里做幂等比较。查询失败走 `?`，不会进到这里。这是字段确实没有：已结算行的 `charged` 为 `None`。

正常写入路径结算时会写下金额。若有人绕过写入、把已结算行的 `charged` 改成 NULL，再次以 0 结算会得到 `AlreadySettled`，可能少收。这不是「检查失败就放行」。未标「⚠ 需修」。场景留给主会话决定要不要修存储不变量。见文末。

### `crates/idoris-tenancy/src/accounts/ledger/project.rs:177`

`.optional()?.unwrap_or_default()`。无限额行则 `LimitMicros` 全是 `None`，表示不设限。SQL 错误用 `?` 上抛。这是确实没有限额行，不是读失败。

### `crates/idoris-tenancy/src/accounts/ledger/project.rs:194`

补贴池没有行则剩余按 0。注释写「没有设置 = 0（失败即关闭）」。查询错误上抛。确实没有。0 是更严的上限，不多放行。

### `crates/idoris-tenancy/src/cloud_tier/mod.rs:464`

`limit` 转 `i64` 失败时用 `i64::MAX`。对账读取的行数变多，不跳过记录。取不到。不少记、不多放行。

### `crates/idoris-tenancy/src/cloud_tier/mod.rs:477` 和 `:506`

游标没有行就当 0。注释写从未推进是 0。查询错误上抛。从 0 重读，配合幂等推进，不跳记录。确实没有游标行。

### `crates/idoris-upstream/src/cloud/sse.rs:49`

`parser.push().unwrap_or_default()`，在 `SseEvent::from_lines`。容量上限是 `usize::MAX`，正常推不失败。取不到时得到空，不把半截事件当成完整放行数据。

## T2

69 个文件。每条写出一处「去掉会变红」的测试；定位行单独去掉却仍会绿的，改记同文件里真正会变红的那一处，并写明定位行为什么杀不死。S3 回答约束是在执行动作时成立，还是只在入口或启动时成立。

已实测并还原的只有一处：`account.rs` 空树守卫。其余是读断言对照，没有再改代码。`git status` 在写本文前是干净的。

### `crates/idoris-compute-contracts/src/account.rs:177`（30 处）

测试 `invalid_trees_are_rejected_with_the_expected_error_table`（同文件）。空列表断言 `TreeError::Empty`。去掉第 177 行后落到 `TreeError::NoRoot`。

已实测：临时注释 `return Err(TreeError::Empty);` 后，`cargo test -p idoris-compute-contracts invalid_trees_are_rejected_with_the_expected_error_table --lib` 失败，`left: NoRoot`，`right: Empty`（用例 empty）。守卫已还原。

S3：执行处。每次 `AccountTree::build`，写账户时也会建树，不是只在进程启动时建一次。

### `crates/idoris-compute-contracts/src/admission.rs:287`（5 处）

测试 `invalid_inputs_are_errors_not_guesses`（同文件）。`evaluate_admission` 对空链断言 `AdmissionError::EmptyChain`。去掉后循环不跑，返回 `Ok`，`unwrap_err` 失败。

S3：执行处。每次准入评估调用它。

### `crates/idoris-compute-contracts/src/control.rs:470`（6 处）

定位行是成员凭证早退 `OpNotGranted`。成员凭证带不了权限，空权限在后面同样是 `OpNotGranted`，只删第 470 行，`authorization_matrix_for_every_operation` 仍绿。

会变红的是同文件第 498 行负金额 `ControlReason::InvalidAmount`。测试 `grant_allocation_is_authorized_on_the_grantor_side_with_a_per_grant_cap`：`grant("eduai", -1)` 断言 `InvalidAmount`。去掉后 `-1` 被当成盖得住，`authorize` 返回 `Ok`。

S3：执行处。每次控制操作调用 `authorize`。

### `crates/idoris-compute-contracts/src/credential.rs:193`（23 处）

测试 `member_credentials_cannot_carry_admin_scopes`（同文件）。`Credential::new` 断言 `CredentialError::MemberCredentialWithAdminScopes`。去掉后构造成功，`unwrap_err` 失败。

S3：执行处。每次 `Credential::new`，签发路径会走到。

### `crates/idoris-compute-contracts/src/grant.rs:148`（3 处）

测试 `negative_amounts_are_rejected_zero_is_allowed`（同文件）。负微单位断言 `GrantError::Money(MoneyError::Negative)`。去掉后 `Grant::new` 返回 `Ok`。

S3：执行处。构造额度和反序列化都走 `Grant::new`。

### `crates/idoris-compute-contracts/src/ids.rs:72`（9 处）

测试 `invalid_tokens_are_rejected_with_the_right_error`（同文件）。`AccountId::new("")` 断言 `IdError::Empty`。去掉后空串通过，`new` 返回 `Ok`。

S3：执行处。每次解析标识。

### `crates/idoris-compute-contracts/src/money.rs:386`（7 处）

测试 `fx_rate_construction_rules`（同文件）。`FxRate::new` 的非货币端断言 `QuotaNotConvertible`。去掉后汇率为 1 时 `new` 返回 `Ok`。

S3：执行处。构造或反序列化汇率时，不是启动门闩。

### `crates/idoris-compute-contracts/src/permit.rs:274`（14 处）

定位行在 `parse_verified_at`：十个字符里必须全是数字。只删这一处，`"2026-11-0a"` 仍会被后面的 `parse` 拒绝，`timestamps_accept_rfc3339_and_dates_and_print_canonically` 不会因此变红。

会变红的是同文件第 353 行空 `processing_regions` → `PermitProblem::NoRegions`。测试 `entries_must_be_self_consistent`。去掉后加载返回 `Ok`。

S3：第 353 行只在加载许可表时（启动）。选路不重跑自洽检查。第 274 行在每次解析许可表时间时，同样是加载期。

### `crates/idoris-compute-contracts/src/pricing.rs:70`（2 处）

测试 `cost_and_price_reject_negative_amounts_everywhere`（同文件）。`Cost::from_micros(..., -1)` 断言 `MoneyError::Negative`。去掉后构造成功。

S3：执行处。每次把金额收成 `Cost` / `Price`，计价会走到。

### `crates/idoris-compute-contracts/src/request.rs:309`（24 处）

测试 `after_the_first_byte_no_switch_or_retry_is_possible`（同文件）。发布之后 `begin_next_attempt` 断言 `RequestError::PublishBoundary`。去掉后会继续建下一次尝试。

S3：执行处。切换或重试时调用，不是启动检查。

### `crates/idoris-compute-contracts/src/reserve.rs:48`（2 处）

定位行 `max_tokens_hard_cap == 0`。只删它，`policy_validation` 仍会被后面的 `min_useful` 打成 `InvalidPolicy`，不一定变红。

会变红的是同文件第 125 行 `max_tokens == 0` → `ReserveError::ZeroMaxTokens`。测试 `effective_max_tokens_boundaries`。去掉后 0 被当成未超过硬上限，返回 `Ok`。

S3：执行处。每次 `reserve` 调用 `effective_max_tokens`。

### `crates/idoris-router/src/auth.rs:202`（2 处）

定位行是决策时钟早于查找时钟时返回 `Unavailable`。现有测试的时钟两次读到同一个 `now_ms`，没有断言这一支。未找到会因去掉第 202 行而变红的测试。

同函数第 209 行过期返回 `Unauthorized`。测试 `missing_malformed_wrong_revoked_and_expired_all_collapse_to_unauthorized`（同文件）：`expires_at_ms = 100` 且 `now = 100` 必须是 `Unauthorized`。去掉后同一把钥匙认证成功。

S3：执行处。每次 `authenticate_secret_with_clock`。第 202 行也是每次认证，只是没有专测。

### `crates/idoris-router/src/bin/idoris.rs:194`（4 处）

定位行拒绝非 Unicode 的 `IDORIS_FORM`。未找到把非 Unicode 环境变量送进这条路径的测试。

会变红的是同文件第 236 行：`IDORIS_ALLOCATED_BUDGET_BYTES` 解析不成 `u64`。测试 `allocated_budget_env_preserves_absence_zero_and_exact_bytes_and_rejects_invalid_values`。`""`、`"-1"`、`"1.5"` 必须 `unwrap_err()`，且文本含变量名。

S3：仅启动。两处都只在进程读启动环境时跑一次。

### `crates/idoris-router/src/cloud/config.rs:501`（20 处）

测试 `decimal_prices_become_exact_micros`（同文件）。`"-1"`、`".5"`、`"1."` 断言 `parse_micros` 为 `Err`。去掉第 501 行的格式拒绝后，`"-1"` 能解析成负数并返回 `Ok`。

S3：仅启动。加载云上游价目时解析，不在每次聊天请求上重解析配置字符串。

### `crates/idoris-router/src/cloud/executor.rs:378`（15 处）

测试 `a_local_only_call_is_refused_by_the_executor_before_any_upstream_work`（`crates/idoris-router/tests/cloud_r2_fixes.rs`）。错误必须是 `CloudError::LocalOnlyRefused`，上游连接数为 0。

S3：执行处。每次 `execute` 开头，出网上游之前。

### `crates/idoris-router/src/cloud/ingress/abuse.rs:115`（1 处）

测试 `the_global_quota_caps_all_anonymous_traffic_together`（同文件）。第五个来源断言 `Err(1)`。去掉 `return Err(retry)` 后退还令牌，但函数落到 `Ok(())`。

S3：执行处。每个没有资格的数据面请求调用 `AnonymousLimiter::admit`。

### `crates/idoris-router/src/cloud/ingress/bind.rs:45`（4 处）

定位行要求 Admin 监听是 loopback。`AdminBindConfig` 造不出非 loopback，`admin_stays_on_loopback_whatever_the_data_plane_binds` 只走成功路径。只删第 45 行，该测试不一定变红。

会变红的是同文件第 69 行拒绝通配、组播、广播。测试 `the_cloud_bind_host_accepts_any_explicit_unicast_address_but_never_a_wildcard`。`"0.0.0.0"`、`"::"`、`"255.255.255.255"` 断言 `is_err()`。

S3：仅启动。解析数据面监听地址时，不在每个请求上重解析。

### `crates/idoris-router/src/cloud/ingress/bucket.rs:45`（2 处）

测试 `a_rate_limit_needs_positive_numbers`（同文件）。`RateLimit::new(0, 1)` 断言 `is_err()`。去掉后 burst 为 1，返回 `Ok`。`new(1, 0)` 仍由下一行拦住。

S3：仅启动。解析限额时构造 `RateLimit`。请求路径用已经造好的限额。

### `crates/idoris-router/src/cloud/ingress/caller.rs:151`（9 处）

测试 `a_child_declaration_on_an_existing_account_below_the_top_is_refused_at_start_up`（同文件）。`school-a` 的错误必须含 `school-a`、`eduai` 和「顶层客户」。去掉后 `refuse_child_declarations_below_the_top` 返回 `Ok`。

S3：这一处仅启动。`CloudIngress::new` 装配时调用一次。请求期另有付款链判定（同文件测试 `an_account_created_after_start_up_with_a_child_declaration_makes_its_requests_child_requests`），那是另一条守卫。

### `crates/idoris-router/src/cloud/ingress/config.rs:342`（10 处）

定位行是 `from_env`：云形态没设入口配置环境变量就启动失败。未找到调用 `IngressConfig::from_env` 的测试。

会变红的是同文件第 354 行 `schema_version` 不对。测试 `broken_files_are_refused_with_a_reason`。`schema_version: 2` 必须失败且文本含 `schema_version`。去掉后其余字段与版本 1 相同，`parse` 返回 `Ok`。

S3：仅启动。加载入口配置时，不在每个请求上重读 YAML。

### `crates/idoris-router/src/cloud/ingress/cors.rs:59`（2 处）

测试 `only_exact_bare_origins_are_accepted_at_configuration_time`（同文件）。`CorsPolicy::new(&[], MAX_PREFLIGHT_AGE_SECS + 1)` 断言 `is_err()`。去掉年龄上限后空名单直接返回 `Ok`。坏来源仍由 `validate_origin` 拦住。

S3：仅启动。`CorsPolicy::new` 在解析配置时。请求上的 `allows` 只查已经建好的集合。

### `crates/idoris-router/src/cloud/ingress/guard.rs:197`（5 处）

测试 `the_member_header_is_zero_or_one_valid_id_and_never_the_first_of_several`（同文件）。`["kid-1", "kid-2"]` 断言 `Err(())`。去掉后收下第一个 id，返回 `Ok(Some)`。

S3：执行处。每个数据面请求调用 `parse_member_header`，失败是 400。

### `crates/idoris-router/src/cloud/ingress/limits.rs:56`（4 处）

测试 `a_zero_concurrency_or_rate_is_a_configuration_error`（同文件）。`LevelLimits::new(60, 5, 0)` 断言 `is_err()`。去掉后速率和 burst 合法，返回 `Ok`。`new(0, 5, 1)` 仍由 `RateLimit::new` 拦住。

S3：这一处仅启动。请求上的 `admit` 使用已经造好的限额。同文件另有 `concurrent_admissions_never_exceed_the_concurrency_limit`，打的是执行期并发上限，不是第 56 行。

### `crates/idoris-router/src/cloud/ingress/mod.rs:180`（2 处）

测试 `a_child_tenant_declared_in_the_ingress_config_requires_a_safety_section_at_start_up`（`crates/idoris-router/tests/cloud_ingress_safety.rs`）。`CloudIngress::new` 的错误必须含「儿童安全」和 `eduai`。去掉后 `callers` 为空，构造返回 `Ok`。

S3：仅启动。执行器里另有运行期兜底，本条守卫不在请求路径上。

### `crates/idoris-router/src/cloud/ingress/net.rs:102`（3 处）

定位行要求前缀全是数字。只删它，`"10.0.0.0/"` 和 `"10.0.0.0/ 8x"` 仍会被后面的 `u8` 解析拒绝。

会变红的是同文件第 120 行 `CidrError::HostBitsSet`。测试 `malformed_and_ambiguous_cidrs_are_refused`。`"10.0.0.5/8"` 的 `unwrap_err()` 必须是这个变体。去掉后解析返回 `Ok`。

S3：仅启动。解析 `trusted_proxies` 时。请求上的 `contains` 只查已经解析好的网段。

### `crates/idoris-router/src/cloud/mod.rs:359`（4 处）

测试 `from_file_wires_the_ledger_and_refuses_a_reservation_ttl_shorter_than_a_request`（`crates/idoris-router/tests/cloud_ledger_admission.rs`）。`reservation_ttl_ms: 1_000` 时错误文本必须含 `TTL`。去掉后加载返回 `Ok`。

S3：仅启动。`CloudUpstream::from_file` 装配时比较，不在每次预留时重比。

### `crates/idoris-router/src/cloud/request.rs:24`（8 处）

测试 `privacy_is_read_from_the_header_and_the_body_and_the_strictest_wins`（同文件）。`["any", "local_only"]` 断言 `Err(InvalidPrivacy)`。去掉「第二个值」这一支后，第一个值 `any` 会返回 `Ok(CallPrivacy::Any)`。

S3：执行处。每个请求解析隐私声明。

### `crates/idoris-router/src/cloud/run.rs:178`（2 处）

定位行是库接受了事件、内存状态却对不上时的 `CloudError::Internal`。未找到断言这句 `"the stored attempt state and the in-memory state diverged"` 的测试。

会变红的是同文件第 261 行：儿童回答尚未通过输出检查就不能 `publish`。测试 `a_child_answer_cannot_be_published_until_the_output_check_cleared_it`。未放行时必须是 `CloudError::Internal`，且库里的 `published_attempt` 仍是 `None`。

S3：两处都在执行期。`publish` 在写出首字节之前。第 178 行在每次把存储事件套到内存状态时，只是没有专测。

### `crates/idoris-router/src/cloud/safety/config.rs:250`（14 处）

测试 `registered_operator_prompts_are_validated_and_must_pass_the_jailbreak_rules`（同文件）。含 “Disable the filter” 的提示必须 `unwrap_err()`，文本含「越狱类规则」、不含提示正文。去掉后 `build_trusted` 返回 `Ok`。太短的提示是 `TrustedOperatorText::new` 的另一处拒绝。

S3：仅启动。安全配置 `build` 时登记提示，不在每次输入检查时重扫。

### `crates/idoris-router/src/cloud/safety/extract.rs:245`（20 处）

测试 `a_registered_prompt_must_be_long_enough_and_unique`（同文件）。`TrustedOperatorText::new(["short"])` 断言 `is_err()`。去掉长度下限后这条不重复、也不超长，`new` 返回 `Ok`。

S3：仅启动。装配时登记。请求期只用已经登记的提示做开头匹配。

### `crates/idoris-router/src/cloud/safety/ledger_cost.rs:178`（2 处）

定位行是检查端点没有付款单位价目时拒绝。运行期测试在进 `reserve` 之前就由配置加载拒绝这种 YAML。未找到会因去掉第 178 行而变红的测试。

会变红的是同文件第 221 行：账本预留失败返回 `ExtraCostRefusal::Unavailable`。测试 `the_ledger_refusing_the_reservation_is_an_admission_refusal_and_a_double_reservation_is_not_reused`（`crates/idoris-router/tests/cloud_child_safety_ledger.rs`）。第二次 `reserve` 必须是这个 `Err`。

S3：第 221 行在执行处，每次安全检查占额度时。第 178 行也在 `reserve` 里，只是没有专测。

### `crates/idoris-router/src/cloud/safety/llm.rs:336`（3 处）

`LlmJudge::new` 在 `max_output_tokens == 0`、`max_chunk_bytes < 4`、`max_chunks == 0` 时返回错误。这三句错误文本只出现在 `llm.rs`。现有测试传入的都是正的上限。

未找到会变红的测试。

S3：仅启动。`build_judge` 装配时调用，检查请求时不再重判这三个上限。

### `crates/idoris-router/src/cloud/safety/mod.rs:451`（9 处）

测试 `uncheckable_malformed_and_oversized_requests_never_reach_the_checker`（同文件）。65 字节正文、上限 64 时，拒绝必须是 `GateDenial::Unavailable { stage: Input, cause: TooLarge }`，检查器调用次数是 0。

S3：执行处。每次 `check_input`。

### `crates/idoris-router/src/cloud_tier/cli.rs:130`（10 处）

测试 `wrong_expired_and_malformed_codes_are_refused_with_clear_messages_and_leave_nothing_behind`（`crates/idoris-router/tests/cloud_tier_process.rs`）。空 stdin 的进程输出必须含「没有从 stdin 读到配对码」，退出码 1，且不留下设备文件。去掉第 130 行后空串会继续往下走，这条断言对不上。

S3：执行处。每次 `idoris cloud pair` 读 stdin，不是守护进程热路径，也不是只在启动时读一次。

### `crates/idoris-router/src/cloud_tier/config.rs:159`（9 处）

测试 `unknown_fields_a_wrong_version_and_nonsense_limits_are_refused`（同文件）。`schema_version: 2` 断言 `is_err()`。去掉第 159 行后，其余字段合法，`parse` 返回 `Ok`。

S3：仅启动。加载云端档配置时。

### `crates/idoris-router/src/cloud_tier/credential.rs:311`（10 处）

定位行在 `DeviceStore::read`（`credential.rs:298-311`）：`version`、展示 id 或 `origin` 不合法就 `return Err(CredentialError::Malformed)`。不是 `CloudCredential::parse`。

测试 `a_corrupt_or_extended_device_file_fails_closed`（同文件，约 882 行）。`"version": 2` 和 membership 改成 `"has space"` 都断言 `CredentialError::Malformed`。去掉第 311 行后，这两次改写会 `read()` 成功，`unwrap_err()` 失败。`only_a_virtual_key_shaped_secret_is_a_cloud_credential` 打的是另一条 parse，不杀死这一行。

S3：执行处。每次读设备凭证文件。

### `crates/idoris-router/src/cloud_tier/forward.rs:226`（7 处）

测试 `a_request_the_last_guard_refuses_is_not_sent_at_all`（同文件）。最后一道守卫拒绝时断言 `CloudFailure::Withheld`，并且没有发出请求。

S3：执行处。出网前。

### `crates/idoris-router/src/cloud_tier/gate.rs:208`（8 处）

测试 `local_only_is_denied_by_the_privacy_check_whatever_else_is_true`（同文件）。`local_only` 必须是 `Denial::PrivacyNotAny`。去掉后隐私检查不再拒绝。

S3：执行处。每次 `grant`。

### `crates/idoris-router/src/cloud_tier/mod.rs:294`（6 处）

测试 `the_cloud_form_and_legacy_tenant_deployments_refuse_to_enable_a_cloud_tier`（`crates/idoris-router/src/cloud_tier/tests.rs:373`）。云形态错误必须含「cloud 形态」，并且 `/nonexistent/cloud-tier.yaml` 那次不创建 `idoris.sqlite3`。去掉第 294 行后 `unwrap_err()` 对不上。同测的 legacy 分支打的是约 299 行，错误必须含 `IDORIS_FORM=organization`。

S3：仅启动。`resolve_for_form` 在装配时调用，注释写明启动时不发网络请求。

### `crates/idoris-router/src/cloud_tier/pairing.rs:220`（2 处）

定位行在 `read_capped`（`pairing.rs:212-229`）：响应体超过 `MAX_BODY_BYTES` 就 `PairingError::BadResponse`。不是畸形 JSON。

测试 `a_redirect_is_never_followed_and_an_oversized_answer_is_refused`（同文件）会打到超长正文，但断言只要求 `BadResponse`。去掉第 220 行后，超长正文仍会在后面的 JSON 解析里得到同一个错误，这条断言不一定变红。`a_malformed_success_body_is_a_bad_response_and_never_a_half_credential` 打的是后面的解析，不杀死第 220 行。

未找到「只去掉这一行就会变红」的测试。

S3：执行处。每次读配对 HTTP 响应体。

### `crates/idoris-router/src/control/bootstrap.rs:162`（1 处）

空 spec 或超过 64 KiB 返回「bootstrap spec 为空或超过 64 KiB」（`bootstrap.rs:161-162`）。

测试 `bootstrap_rejects_bad_specs_before_touching_the_database_and_stops_at_the_first_failure`（`control/tests.rs:3325`）对 `"empty"` 和 `"too large"` 只断言 `is_err()`，并断言审计行数仍是 0。空串去掉第 162 行后，随后的 JSON 解析仍失败。超长用例是带未知字段 `pad` 的对象，`deny_unknown_fields` 仍会失败。审计行也仍是 0。只去掉第 162 行，这条测试不一定变红。

未找到会变红的测试。

S3：执行处。每次 `idoris control bootstrap` 读 spec，不是守护进程热路径。

### `crates/idoris-router/src/control/mod.rs:130`（1 处）

测试 `without_a_control_plane_none_of_the_routes_exist_and_the_data_plane_answers_exactly_as_before`（`control/tests.rs:428`）。末尾约 477–478 行：`ControlPlane::new(..., DeploymentForm::Personal).unwrap_err()`，错误必须含 `"personal"`。去掉第 130 行后 `unwrap_err()` 会 panic。

S3：仅构造。个人形态在这里就返回错误，不会挂上控制面。

### `crates/idoris-router/src/control/wire.rs:180`（5 处）

`validate_ceilings`（`wire.rs:178-180`）：`allowed_privacy` 为空就返回「allowed_privacy must contain at least one privacy class」。

测试 `bootstrap_rejects_bad_specs_before_touching_the_database_and_stops_at_the_first_failure`（`control/tests.rs:3325`）有 `"no privacy class"`：`allowed_privacy` 为 `[]`。循环之后断言 `audit_rows().len() == before`（约 3351–3354 行，「a rejected spec writes nothing, not even an audit row」）。只看 `is_err()` 不够：`insert_virtual_key`（`accounts/control.rs:1341`）仍会因空隐私返回 `InvalidCredential`。那发生在已认出的 `Who::Operator` 的 `control_write` 里，失败的 body 仍会写审计并提交（`control.rs:466-479`）。去掉第 180 行后，审计行数会大于 `before`。这是读调用链的推论，没有再跑 `cargo test`。

S3：执行处。每次 `validate_ceilings`，在 bootstrap 写库之前。

### `crates/idoris-router/src/dispatch.rs:534`（1 处）

定位行在 `ReservationGuard::reserve`：`ledger` 为 `None` 且金额是付费时，返回 `ledger_unavailable_error`。`free_candidate_with_unconfigured_ledger_fails_closed`（同文件约 1921 行）传入的是已打开但没配置的账本，打的不是这一支。

会变红的是 `chat_completions_paid_candidate_without_a_ledger_is_a_server_error`（`crates/idoris-router/src/lib.rs:5365`）。它断言 `INTERNAL_SERVER_ERROR`。`budget_error_response`（`lib.rs:2075-2079`）把非 402 的账本错误映射成 500。去掉第 534 行后，`None` 分支放行，接着 `dispatch_local_inner`（`dispatch.rs:1012-1020`）因没有 supervisor 返回 `supervisor_unavailable`，`backend_error_response`（`lib.rs:2042-2046`）映射成 503。断言的状态码对不上。这是读调用链的推论，没有再跑 `cargo test`。

S3：执行处。每次 `ReservationGuard::reserve`。

### `crates/idoris-router/src/proxy_budget.rs:43`（1 处）

同样是付费但 `ledger` 为 `None` 时返回 `ledger_unavailable_error`。`budget.rs` 的 `ledger_unavailable_error_matches_a_real_not_configured_rejection` 打的是 helper 的文本，不经过 `ProxyBudget::new`。去掉第 43 行不保证那条测试变红。

未找到专测这一支的测试。

S3：执行处。每次构造 `ProxyBudget`。

### `crates/idoris-router/src/storage.rs:112`（1 处）

测试 `the_operator_command_exists_only_in_the_organization_and_cloud_forms`（`control/tests.rs:3402`）。personal 和 legacy 调 `open_account_store_process`，错误必须含 `IDORIS_FORM`，`never-created.sqlite3` 不存在（约 3429–3433 行）。去掉第 112 行后会继续去开库，这条「不存在」对不上。

S3：执行处。每次离线控制命令打开账户库之前，不是只在守护进程启动时。

### `crates/idoris-tenancy/src/accounts/control.rs:440`（33 处）

测试 `an_unauthenticated_write_does_not_take_the_database_write_lock`（`accounts/control_tests.rs:2122`）。另一连接持 `BEGIN IMMEDIATE` 时，未知密钥必须是 `ControlReason::Unauthenticated`；认得出的调用者才是 `StoreBusy`（约 2141–2149 行）。去掉第 440 行后，未知密钥会去抢写锁，变成 `StoreBusy`，断言对不上。写事务里第 447 行仍会再认一次，但那已经拿了写锁。

S3：执行处。每次 `control_write`，写事务之前。

### `crates/idoris-tenancy/src/accounts/ledger/admit.rs:111`（3 处）

凭证状态不是 `active` / `revoked` / `frozen` 时返回 `LedgerError::Corrupt("unknown credential status ...")`。这句只出现在 `admit.rs`。未找到断言它的测试。

未找到。

S3：执行处。每次准入读凭证状态。正常状态走上面的拒绝码，不进这一支。

### `crates/idoris-tenancy/src/accounts/ledger/components.rs:74`（6 处）

主预留不是 `Open` 时，附加成本预留返回 `LedgerError::Invalid`（「an extra cost can only be reserved while it is open」）。未找到断言这句的测试。

未找到。

S3：执行处。每次给已有尝试加附加成本。

### `crates/idoris-tenancy/src/accounts/ledger/holds.rs:75`（4 处）

预留行的 attempt id 解析失败，或和 request id / index 对不上，返回 `LedgerError::Corrupt`。未找到断言「hold attempt id」或「names request」的测试。

未找到。

S3：执行处。每次把预留行收成 `HoldRow`。绕过 Rust 层的坏数据才会走到。

### `crates/idoris-tenancy/src/accounts/ledger/hook.rs:70`（5 处）

创建尝试时，预留不是 `Open` 就返回 `LedgerError::Invalid`（「not open」）。未找到断言这句的测试。

未找到。

S3：执行处。每次 `bind_attempt`，在创建尝试的事务里。

### `crates/idoris-tenancy/src/accounts/ledger/limits.rs:29`（6 处）

`require_non_negative`：负数返回 `LedgerError::Invalid`（「must not be negative」）。未找到断言这句文本的测试。授信相关测试打的是别的路径。

未找到专测第 29 行的测试。

S3：执行处。每次设置限额、授信、入账前检查金额。

### `crates/idoris-tenancy/src/accounts/ledger/mod.rs:267`（3 处）

测试 `settling_zero_is_a_release_and_settling_is_idempotent_but_never_overwrites`（`crates/idoris-tenancy/tests/ledger_settlement.rs:120`）。约 176–184 行：THB 预留用 USD `Price` 结算，断言 `LedgerError::Invalid(_)`。去掉第 267 行后不再是这个 `Invalid`。

S3：执行处。每次 `settle_attempt`。

### `crates/idoris-tenancy/src/accounts/ledger/subsidy.rs:56`（11 处）

免费额度金额不是正数时返回 `LedgerError::Invalid("a free-tier grant must be a positive amount")`。测试 `free_tier_grants_are_validated`（`crates/idoris-tenancy/tests/ledger_grace_subsidy.rs`）对金额 0 和 -5 断言 `LedgerError::Invalid`。去掉第 56 行后，0 会继续往下，不再是这个 `Invalid`（账户存在且单位一致），`matches!(..., Invalid(_))` 失败。-5 可能仍被后面的单位或金额检查拦住，本条对应的是 0。

S3：执行处。每次 `issue_free_tier_in`。

### `crates/idoris-tenancy/src/accounts/pairing.rs:204`（24 处）

测试 `a_code_is_short_lived_and_one_use_only`（`crates/idoris-tenancy/src/accounts/pairing_tests.rs`）。不存在的码断言 `ControlReason::PairingCodeInvalid`，并且不增加审计行。去掉 `Ok(None)` 这一支后，查无此码不再是这个原因。

S3：执行处。每次兑换。

### `crates/idoris-tenancy/src/accounts/rows.rs:156`（4 处）

祖先链长于 `MAX_CHAIN_WALK`（64）时返回 `AccountStoreError::Corrupt`（`rows.rs:155-158`）。未找到断言「ancestor chain longer」的测试。

这一支按当前 SQL 走不到：递归句是 `WHERE chain.depth < {MAX_CHAIN_WALK}`（`rows.rs:148`），结果最多 64 行，`len() > 64` 不会成立。去掉第 156 行，现有测试也不会变红。

未找到。

S3：执行处。每次 `load_ancestors`。正常树在写入时已经限制深度。按现在的 SQL 上界，这一支是死代码。

### `crates/idoris-tenancy/src/accounts/store.rs:200`（19 处）

测试 `accounts_round_trip_and_the_ancestor_chain_runs_self_to_root`（`accounts/tests.rs:309`）。约 339–345 行：`ancestors(&aid("nope"))` 必须是 `AccountStoreError::NotFound { kind: "account" }`。去掉第 200 行后返回空链 `Ok`，`matches!(..., NotFound)` 失败。

S3：执行处。每次 `ancestors`，准入要用它。

### `crates/idoris-tenancy/src/attempts/mod.rs:545`（24 处）

第一次尝试必须是新请求的 index 0，否则 `AttemptStoreError::Invariant`。这句只出现在 `attempts/mod.rs`。未找到断言它的测试。

未找到。

S3：执行处。每次 `create_request`，在拿锁之前。

### `crates/idoris-tenancy/src/budget/ledger.rs:605`（4 处）

这一行在 `seed_tenant`，空白 `tenant_id` 返回 `BudgetError::InvalidScope { field: "tenant_id" }`。`configure_rejects_blank_scope_field` 打的是 `configure()`，去掉第 605 行不会让它变红。

会变红的是 `invalid_seed_values_fail_and_leave_nothing_behind`（`crates/idoris-tenancy/tests/tenant_seed.rs`）。`seed("  ", ...)` 断言 `InvalidScope { field: "tenant_id" }`，并且库里没有标记行。

S3：执行处。每次 `seed_tenant`，包括启动播种。不是只改内存里的配置。

### `crates/idoris-tenancy/src/cloud_tier/mod.rs:283`（11 处）

测试 `free_text_cannot_be_written_into_any_column`（`crates/idoris-tenancy/src/cloud_tier/tests.rs`）。`""`、`"a b"`、超长、非标识符都断言 `InvalidField("local_request_id")`。去掉第 283 行后这些字符串会进入插入。

S3：执行处。每次 `begin_call`，出网前落 `pending` 行。

### `crates/idoris-tenancy/src/sqlite_conn.rs:83`（2 处）

测试 `standard_refuses_a_connection_that_cannot_be_wal`（同文件）。内存库的 journal mode 不是 wal，必须 `ConnectionSetupError::NotWal`。

S3：执行处。每次 `apply_standard` 打开库时，不只是守护进程启动那一次。CLI 打开同样走这里。

### `crates/idoris-upstream/src/cloud/client.rs:123`（5 处）

测试 `base_url_rules`（同文件）。非 https 且不是 loopback 必须 `ConfigError::InsecureScheme`。

S3：执行处。建客户端时，在出网之前。不是每次请求重解析，也不是进程启动专有。

### `crates/idoris-upstream/src/cloud/sse.rs:157`（1 处）

测试 `an_oversized_event_is_an_error_not_unbounded_memory`（同文件）。超限事件必须是 `SseError::EventTooLarge`。

S3：执行处。每次 `push`。

### `crates/idoris-upstream/src/cloud/wire.rs:124`（10 处）

`require` 在组上游请求体时拒绝缺字段。测试 `invalid_shapes_are_rejected_before_anything_is_sent`（同文件）。去掉 `require` 的拒绝后，缺字段的体会被发出或不再是现在的错误。

S3：执行处。每次组上游请求体，出网前。

### `eval/thai-edu/compare.mjs:33`（9 处）

`mergeItems`：`items` 不是数组就抛「结果缺少 items 数组」。`compare.test.mjs` 有参数缺失的 `assert.throws`，成功路径传入的是数组。没有断言这句。

未找到。去掉第 33 行，现有测试仍绿。

S3：执行处。每次合并评测结果。本机脚本，不是服务进程。

### `eval/thai-edu/lib.mjs:512`（2 处）

目录里没有 jsonl 就抛错。`cases.test.mjs` 的 dry-run 用的是非空的已提交用例目录。未找到「空目录」断言。

未找到。

S3：执行处。每次 `loadCases`。本机评测，不是服务启动。

### `eval/thai-edu/run.mjs:440`（4 处）

非法请求 URL 抛「请求 URL 不合法」。`runner.test.mjs` 没有断言这句。

未找到。

S3：执行处。每个 worker 发请求前。本机评测脚本。

### `scripts/check-rust-egress.sh:244`（5 处）

`grep -q ... || exit 1`。没有针对这个脚本的单元测试。脚本本身就是检查。去掉第 244 行不会让仓库里任何 `cargo test` 或 `node --test` 变红。

未找到会变红的测试。

S3：脚本跑到云端档段时检查 fixture，不是服务运行时。PR #600 正文写本机跑不了新加的第三段，要靠 CI 的 ubuntu。那不是「去掉守卫会变红」的测试。

### `scripts/compute-ops/rehearse-litestream-restore.sh:16`（5 处）

缺 `sqlite3` 时 `exit 1`。没有单元测试。

未找到。

S3：演练脚本开头，在复制之前。运维文档里的本机演练记录不是这条守卫的回归测试。

## C2

13 处。写出真并发测试；没有的写「未找到」。并说明拿锁之后有没有重查。

### `crates/idoris-router/src/cloud/blocking.rs:3`

`worker_threads(1)` 和 `tokio::spawn`。测试 `a_slow_synchronous_section_does_not_starve_the_other_tasks_of_the_runtime`（同文件）：心跳任务和阻塞段同时在一个只有一个 worker 的运行时上跑。没有「检查 → 等锁 → 动作」。阻塞段在 `block_in_place` 里跑完，取消不把它劈开。

### `crates/idoris-router/src/cloud/ingress/abuse.rs:40`

`Mutex` 和 `Semaphore`。`AnonymousLimiter::admit` 在各自的锁里扣令牌。`AuthBudget::admit` 先在一把锁里看前缀是否已知，放锁之后才扣未知桶，两把锁之间不重查。窗口是：刚失败的前缀可能被当成已知，少扣查询预算。方向是多查库，不是出网放行。

真并发：同文件测试多是单线程。`crates/idoris-router/tests/cloud_r2_ingress.rs` 有 `thread::spawn` 打入口。同文件行为测试：`the_in_flight_authentication_slots_are_bounded_and_released_on_drop`、`a_flood_of_unknown_prefixes_is_capped_by_the_budget_and_a_refusal_gives_the_token_back`。未找到只针对「两锁之间不重查」的线程测试。

### `crates/idoris-router/src/cloud/ingress/limits.rs:37`

`acquire_in` 持锁后 `acquire`。`admit` 三级各在自己的锁里扣，外层失败会 `undo`。没有跨锁的「先检查再等锁再放行」而不重查。

真并发测试：`concurrent_admissions_never_exceed_the_concurrency_limit`（同文件，`thread::scope`）。

### `crates/idoris-router/src/cloud/safety/cost.rs:28`

`SimpleExtraCost` 的 `Mutex<SimpleState>`。`reserve` 在锁内做 `next_id` / `open += 1`。没有锁外先检查再等锁。未找到针对 `SimpleExtraCost` 的 `thread::spawn` 测试。生产计量是 `LedgerExtraCost`，这一把锁只给没有账本的测试装配。

### `crates/idoris-router/src/cloud_tier/credential.rs:268`

命中的是注释里的 `flock`。实现是 `DeviceStore::try_lock`。测试 `the_state_lock_excludes_a_second_holder_and_is_released_when_dropped`（同文件）。配对在 `cli.rs` 的 `lock_state_dir` 拿到锁之后才 `store.read()`，拿锁后重查是否已配对。

### `crates/idoris-router/src/cloud_tier/mod.rs:193`

`Semaphore`。`try_acquire_owned` 失败标 `saturated`。拿到名额后再查 `kill_switch_on()`，拿锁后重查了开关。

真并发测试：`the_cloud_tier_limits_how_many_calls_are_in_flight_and_does_not_queue`（`crates/idoris-router/tests/cloud_tier_e2e.rs`）。第二个请求的 `x-idoris-cloud-attempt` 是 `saturated`。

### `crates/idoris-router/src/control/handlers.rs:598`

`redeem_limiter.acquire` 先占名额再查库。重查在 `FailureLimiter::acquire` 持锁之后做 prune 和计数（`pairing_codes.rs`）。锁中毒返回 `None`，失败即关闭。占名额发生在查库之前，这是预算，不是「检查通过再等锁再放行」。

真并发：`parallel_wrong_pairing_codes_cannot_all_get_to_the_database_lookup`（`control/tests.rs`）、`concurrent_http_redemptions_of_one_code_yield_exactly_one_device`（同文件）。存储层 `concurrent_redemptions_of_one_code_produce_exactly_one_device`（`accounts/pairing_tests.rs`）。

### `crates/idoris-router/src/control/pairing_codes.rs:19`

`Mutex`。`acquire` 持锁后 prune 再计数。真并发测试：`parallel_acquires_never_hand_out_more_than_the_limit`（同文件，多线程）。

### `crates/idoris-router/src/proxy.rs:544`

新增 `uncached_tenants: RwLock`。`disable_cache_for_tenants` 持写锁 `extend`。`cache_disabled_for` 读锁失败按关闭缓存。集合只增不减。不是「检查通过 → 等锁 → 放行」而不重查。未找到只打这把 `RwLock` 的 `thread::spawn` 测试。

### `crates/idoris-tenancy/src/accounts/store.rs:32`

`Mutex<Connection>`。写函数持锁后 `BEGIN IMMEDIATE`。控制面写在只读事务里先认人，写事务里再认一次（见 T2 的 `control.rs:440`），拿锁后重查。

真并发：`crates/idoris-tenancy/tests/account_store_concurrency.rs` 的 `racing_identical_subsidy_grants_create_one_allocation_and_replay_the_rest`、`racing_creation_of_the_same_account_id_makes_exactly_one`、`racing_platform_roots_leave_exactly_one_root`、`racing_to_bind_one_payment_account_lets_exactly_one_membership_have_it`。账本另有 `tests/ledger_concurrency.rs`。

### `crates/idoris-tenancy/src/attempts/mod.rs:33`

`Mutex<Connection>`。`create_request` 先做不依赖库的校验（index 0），然后持锁并在 `Immediate` 事务里插入。状态变更在事务里重读。未找到针对 `AttemptStore` 的 `thread::spawn` 测试。

### `crates/idoris-tenancy/src/cloud_tier/mod.rs:38`

同样是 `Mutex<Connection>`。`begin_call` 先校验标识符，再持锁插入。重复开始由数据库约束拒绝。同文件测试 `the_same_attempt_cannot_begin_twice`、`the_cursor_starts_at_zero_and_advancing_is_idempotent_and_monotonic` 是单线程。未找到 `thread::spawn`。

锁中毒时 `into_inner()` 继续用连接。注释写每个方法是单个事务，panic 后随 drop 回滚。这是取回毒化锁，不是跳过检查。

### `eval/thai-edu/run.mjs:432`

`Promise.all` 开 workers。游标 `index = cursor; cursor += 1` 在 `await` 之前同步完成。JavaScript 单线程，没有「检查完再等锁」的窗口。

测试：`eval/thai-edu/runner.test.mjs` 的 `"timeout, one retry, and a failed case still let the others finish"`，`--concurrency 2`，断言 `maxInflight >= 2` 且两条 pass。

## E4

5 处。都没有把上游或数据库的原始错误、内部路径交给 HTTP 客户端。未标「⚠ 需修」。

### `crates/idoris-router/src/cloud/handler.rs:131`

`RequestId::new` 失败返回 `StatusCode::INTERNAL_SERVER_ERROR`，正文是固定的 `"internal_error"` / `"request id is invalid"`。不带 `RequestId` 错误的 `Display`，也不带路径。调用方输入不合法在这之前已经是 4xx。500 是服务端生成的记录号不变量被破坏。

### `crates/idoris-router/src/control/wire.rs:257`

`InternalError => 500`。第 320 行 `message_for` 对这一类是固定的 `"the service cannot complete this request right now"`。`failure_response`（337–348 行）只在驳回类带上 `detail`，`InternalError` 不带。不把内部路径或 stderr 交给客户端。

### `eval/thai-edu/compare.mjs:336`

本机脚本。`throw new Error(\`读结果失败 ${file}: ${message}\`)`。`message` 来自 `JSON.parse`，`file` 是操作者传入的路径。没有 HTTP 客户端。

### `eval/thai-edu/lib.mjs:522`

同样是本机评测。错误文本是 `${name}:${index + 1} JSON 解析失败: ${message}`。给跑评测的人，不是 API 响应。

### `eval/thai-edu/run.mjs:112`

`parseArgs` 失败把 `error.message` 写到 `io.error`，然后 `exit 1`。CLI。不是 HTTP。

## R1

版本号变了，diff 里没有改 `CHANGELOG` 或 `Release.md`，所以规则命中。

`git diff origin/main -- '**/Cargo.toml' 'package.json' '**/package.json'` 里，唯一新增的 `version = ` 行是 `crates/idoris-compute-contracts/Cargo.toml` 的 `version = "0.1.0"`。这是新 crate 的初始版本。没有 `-version` 行。根目录和工作区里其它 crate 的 `package.version` 没有在这个 diff 里改。没有 `package.json` 的版本 diff。

`rusqlite` 的版本约束如果出现在依赖行上，形式是 `rusqlite = { version = "..." }`，检查器只认行首的 `version =`，那一行不是这次 R1 的触发点。

仓库没有根目录 `CHANGELOG` 或 `Release.md`。发版说明在 `docs/releases/v0.3.0.md`、`v0.3.1.md`、`v0.3.2.md`、`v0.4.0.md`。本 PR 的 diff 没有改这些文件，所以检查器仍命中。

产品版本没有从 0.4.0 往上抬。新的内部契约 crate 是 0.1.0，不是对外产品发版。不需要为这次 diff 新增 `docs/releases/v0.5.0.md` 或改 `v0.4.0.md`。若以后把 `idoris-compute-contracts` 当成对外发布的 crate，再单独写它的版本说明。现状就是：产品发版说明停在 `docs/releases/v0.4.0.md`，本分支没有新的产品版本号。

## B1

104 个文件。检查器把含断言词（只在、仅、只有、才会、不会、一定、必然、保证、绝不、永远、总是、never、always、guarantee、only、must）的新增注释各算一条，按文件合并成一条 review。`^\s*#` 也会把 `#[error("... must ...")]` 算进注释。

抽查方法：对 `git diff -U0 origin/main` 的新增行，每个文件取首条、中位、末条断言注释，对照相邻代码在什么时候成立。不要求 104 文件里的每一条注释都写一行。下面每个文件给结论。

有 9 处「⚠ 注释不符」。其余文件在抽查的那几行上相符。后半文件（`cloud/safety` 之后）没有把子代理报告里未再读源码的条目升级成不符。

`request.rs` 测试注释「maybe_executed 的成本永远按预留额挂账、待核销，不会是 0、不会被释放」和同测试里 `Certainty::Maybe` 时 `CostBasis::ReservedPendingReconciliation` 且金额等于预留一致。这和 owner 已拍板的「已确认 0 用量按 0 收费」是两种状态，不是注释写错。

下表「抽查」只摘新增行的行号。成立时刻：启动 = 进程装配或加载配置；执行 = 每次请求、每次写库或每次命令；测试 = 只在 `#[cfg(test)]` 或测试注释里。

| 文件 | 条数 | 抽查 | 结论 |
|---|---|---|---|
| `idoris-compute-contracts/examples/validate_permit_table.rs` | 1 | 7 | 相符。只有示例读系统时钟，库本身不读。执行示例时成立。 |
| `idoris-compute-contracts/src/account.rs` | 12 | 27、172、1029 | 相符。花费沿祖先查是每次准入；检查顺序是每次 `build`；1029 是测试生成器。 |
| `idoris-compute-contracts/src/admission.rs` | 15 | 9、163、1145 | 9 与 1145 是每次准入。163 是 `#[error("... must not be empty")]`，检查器误报，和空链拒绝一致。 |
| `idoris-compute-contracts/src/attempt.rs` | 6 | 30、41、541 | 相符。先记 `maybe_executed` 再写字节，是每次发布。 |
| `idoris-compute-contracts/src/control.rs` | 9 | 3、33、467 | 相符。模块没有存储和 HTTP；检查顺序每次 `authorize`；管理权限只在 service / node。 |
| `idoris-compute-contracts/src/credential.rs` | 18 | 8、320、1017 | 8 与 1017 是类型不变量，每次构造成立。320 是 `#[error]` 误报。 |
| `idoris-compute-contracts/src/form.rs` | 10 | 6、120、329 | 相符。schema 仍只有两个值；329 是测试注释，改 schema 会失败。 |
| `idoris-compute-contracts/src/grant.rs` | 3 | 12、25、55 | 相符。`QUOTA` 不可兑现由 `FxRate` 拒绝；预付必须是货币；不可转让只有 `false`。每次构造。 |
| `idoris-compute-contracts/src/ids.rs` | 2 | 9、45 | 9 是文档，解析时拒绝邮箱形状。45 是 `#[error]` 误报，和空串拒绝一致。 |
| `idoris-compute-contracts/src/lib.rs` | 6 | 4、106、118 | 相符。crate 只有纯函数；状态机额外边在模块文档里写明；空 `allowed_data_classes` 的端点选不中。 |
| `idoris-compute-contracts/src/money.rs` | 12 | 5、140、904 | 5 与 904 每次换算成立。140 是 `#[error]` 误报。 |
| `idoris-compute-contracts/src/permit.rs` | 18 | 9、406、1817 | 9 是许可表文档，加载时只有 schema 1。406 是 `#[error]` 误报。1817 是测试：空列表往返不被补全。 |
| `idoris-compute-contracts/src/pricing.rs` | 1 | 52 | 相符。只有 `Cost` 和 `Price` 实现带侧金额。 |
| `idoris-compute-contracts/src/reason.rs` | 4 | 5、33、106 | 相符。原因码字符串发布后不能改；和第 5 节的码不是同一个字符串。 |
| `idoris-compute-contracts/src/request.rs` | 14 | 12、398、1935 | 相符。只有最新尝试可发布，每次状态转移。1935 见本节开头。 |
| `idoris-compute-contracts/src/reserve.rs` | 4 | 6、157、195 | 相符。纯函数；`fit` 取最大放得下的值，每次预留。 |
| `idoris-compute-contracts/src/safety.rs` | 3 | 8、11、21 | 相符。检查结果只有放行和拒绝，没有不确定但放行。每次检查。 |
| `idoris-router/src/bin/idoris.rs` | 7 | 171、348、588 | ⚠ 注释不符。171–172：cloud 命令「只读写设备凭证、不开数据库、不碰 local-key」。`cloud_tier/cli.rs` 的 `load_from_env` 会取 `process_db_path()`；`unpair` 经 `VirtualKeyFileProbe` 只读打开虚拟 key 库。348–350 前半句「only the cloud form may listen beyond loopback」不符：非云走 `parse_bind_host`（`idoris.rs:260-264`），允许 Tailscale 非 loopback。后半句「其它形态按原样解析」相符。588 抽查相符：账户库只在组织、云形态。 |
| `idoris-router/src/budget.rs` | 6 | 58、96、543 | 相符。虚拟 key 形态、调用方字符串不进每一行、provider 列不是模型。每次记账。 |
| `idoris-router/src/cli.rs` | 2 | 11、18 | 相符。控制面命令只在组织、云形态；建平台根只有这条离线路径。 |
| `idoris-router/src/cloud/admission.rs` | 2 | 21、59 | 相符。收尾有账本幂等；宽限在换候选之后。每次准入。 |
| `idoris-router/src/cloud/answer.rs` | 11 | 11、384、679 | 相符。首字节后不切换；注释不在发布前转发；中途 usage 不是最终用量。每次流。 |
| `idoris-router/src/cloud/blocking.rs` | 3 | 9、10、49 | 相符。`block_in_place` 不改变「事务跑完」；49 是测试注释。 |
| `idoris-router/src/cloud/config.rs` | 17 | 6、683、1661 | 相符。密钥不写在配置里；静态调用方的树只有平台和该组织；摘要不含凭证号。加载时和每次打日志时。 |
| `idoris-router/src/cloud/executor.rs` | 16 | 30、487、1050 | 相符。请求形状的 4xx 不切换；儿童租户不直接走非儿童路径；1050 是测试夹具。每次执行。 |
| `idoris-router/src/cloud/handler.rs` | 6 | 1、77、474 | 相符。处理函数只在云上游打开时挂载；安全拒绝不在四级阶梯上。启动挂载，拒绝在每次检查失败时。 |
| `idoris-router/src/cloud/ingress/abuse.rs` | 7 | 1、19、372 | 相符。只在云形态加载；认证前预算防随机假凭证；372 是测试注释。 |
| `idoris-router/src/cloud/ingress/bind.rs` | 6 | 1、25、76 | ⚠ 注释不符。第 1 行「只有云形态允许非 loopback」。同文件第 7–8 行表写存量 tenant / 组织仍是 loopback 或 Tailscale。25、76 抽查相符：Admin 不跟着上公网；loopback 永远允许。启动绑定。 |
| `idoris-router/src/cloud/ingress/bucket.rs` | 5 | 7、14、117 | 相符。逐毫秒回填；调用方应传单调时钟；退令牌不超过容量。每次扣桶。 |
| `idoris-router/src/cloud/ingress/caller.rs` | 8 | 10、475、921 | 相符。成员 id 每次请求验证不缓存；475 是边界注释；云形态拒绝 QUOTA 树。启动与每次解析。 |
| `idoris-router/src/cloud/ingress/config.rs` | 6 | 5、320、610 | 相符。未知字段启动失败；儿童租户走安全管线；610 是测试注释。 |
| `idoris-router/src/cloud/ingress/cors.rs` | 7 | 3、14、56 | 相符。默认不发 CORS；配了就带 `Vary: Origin`；重复来源是错误。配置时校验，请求时套用。 |
| `idoris-router/src/cloud/ingress/guard.rs` | 6 | 25、237、398 | 相符。读体在认证和限流之后；假凭证两道闸拦不住所以另有预算；398 是收拢阈值说明。每次请求。 |
| `idoris-router/src/cloud/ingress/limits.rs` | 6 | 23、431、503 | 相符。内层拒绝不扣外层；431 与 503 是测试注释，和分桶实现一致。 |
| `idoris-router/src/cloud/ingress/mod.rs` | 19 | 16、89、312 | ⚠ 注释不符。第 16 行模块表「只有云形态允许非 loopback」，与 `bind.rs:7-8` 和 `idoris.rs:260-264` 不符。同一行后半「Admin v0 永远只听 loopback」本轮未推翻。89、312 抽查相符。 |
| `idoris-router/src/cloud/ingress/net.rs` | 6 | 3、10、136 | 相符。对端是代理；伪造的最左地址走不到；空受信名单则忽略 `X-Forwarded-For`。每次取客户端地址。配置在启动时解析。 |
| `idoris-router/src/cloud/ingress/reason.rs` | 3 | 3、34、48 | 相符。三个位置的码相同；匿名限流不拦有效凭证；CORS 拒绝只在配了 CORS 之后。每次拒绝。 |
| `idoris-router/src/cloud/mod.rs` | 17 | 22、129、442 | ⚠ 注释不符。22–24：设了 `IDORIS_CLOUD_UPSTREAM_CONFIG` 且 `deploy_mode=tenant` 才会启用，不设就是 `None`；个人形态永远不会把请求送到云上游。启动入口是 `from_env_for_form`（`mod.rs:388-428`）：云形态没设变量是 `Err`，不是 `None`；非云设了变量也是 `Err`。同文件 375–384 已经写了更正后的规则。旧入口 `from_env`（431 行起）仍按 `DeployMode::Tenant` 启用。129、442 抽查相符。 |
| `idoris-router/src/cloud/request.rs` | 2 | 37、197 | 相符。`metadata` 不转发；输入估算只多不少。每次解析。 |
| `idoris-router/src/cloud/run.rs` | 5 | 12、47、132 | ⚠ 注释不符。第 12 行：`Drop` 兜底，所以不会留下永远「进行中」的请求。`Drop`（402–408）调用 `finalize`，失败只 `eprintln`。`finalized` 只在 `store.finalize_request` 成功之后才设为 true（374–394）。收尾失败时请求在库里仍未关闭，`Drop` 不再试。收尾成功时请求会关。47、132 抽查相符。 |
| `idoris-router/src/cloud/safety/config.rs` | 4 | 35、177、194 | 相符。运营方提示先过越狱规则，加载失败；`rules_only` 确认只在该模式；警告在启动时打印。 |
| `idoris-router/src/cloud/safety/cost.rs` | 2 | 21、284 | 相符。`SimpleExtraCost` 不进生产装配；丢弃按 Unknown 结算。测试装配与执行期结算。 |
| `idoris-router/src/cloud/safety/extract.rs` | 16 | 26、632、1786 | ⚠ 注释不符。632：同一段对话里不同请求的工具调用 id 不会撞，且只含字母数字和 `_`。`tool_call_id`（632–640）只取请求号前 12 个 ASCII 字母数字再加 index。字符集成立。不同请求可以共用这 12 个字符，不会撞不成立。26、1786 抽查相符。 |
| `idoris-router/src/cloud/safety/hardening.rs` | 4 | 3、19、57 | 相符。正文只在内存；panic 报告没有 payload；57 是类型宽度说明，不是运行时断言。启动时收紧 rlimit。 |
| `idoris-router/src/cloud/safety/ledger_cost.rs` | 2 | 14、45 | 相符。重复预留不复用；日志没有内容。每次预留和拒绝。 |
| `idoris-router/src/cloud/safety/llm.rs` | 4 | 14、16、152 | ⚠ 注释不符。15–16 前半：请求体只有 `messages` 和固定的采样参数。`judge_once` 先放 messages 和 temperature 0，`build_chat_body`（`idoris-upstream/src/cloud/wire.rs:185-198`）最后还写入 model、max_tokens、stream。后半「不带能关联到孩子的字段」和密钥不进日志仍相符。14 的「判不了就不继续送」相符。152 抽查相符：配置层保证 max ≥ 4，这一支是兜底。 |
| `idoris-router/src/cloud/safety/mod.rs` | 10 | 17、174、671 | 相符。客户端写的话按孩子的话；失败即关闭；日志没有内容。每次检查。 |
| `idoris-router/src/cloud/safety/rules.rs` | 7 | 16、104、697 | 相符。运营方待遇只给登记提示；否定词只在命中旁边；变形命中在 plain 视图没有时规则仍成立。每次匹配。 |
| `idoris-router/src/cloud/safety/test_support.rs` | 1 | 1 | 相符。只在 `cfg(test)` 编译。 |
| `idoris-router/src/cloud/safety/text.rs` | 11 | 1、469、1192 | 相符。短模式只在 plain 视图；1192 是测试注释，前一句的 never 不算。每次归一化。 |
| `idoris-router/src/cloud_tier/cli.rs` | 4 | 6、433、434 | 相符。status 不显示密钥；解绑时凭证只发往签发它的源；本地密钥不发给云端。每次命令。 |
| `idoris-router/src/cloud_tier/config.rs` | 9 | 4、22、577 | 相符。没配云端档则本机行为不变；decide 不出网；配置里的密钥字段被未知字段拒绝。加载时。 |
| `idoris-router/src/cloud_tier/credential.rs` | 20 | 7、351、1326 | 相符。表是三种密钥的对照，读密钥时成立；半截文件失败即关闭；过大的文件不读。每次读文件。 |
| `idoris-router/src/cloud_tier/forward.rs` | 4 | 10、186、193 | 相符。密钥只在 Authorization；没有出网凭条就发不了；凭条只有 `gate::grant` 能造。每次转发。 |
| `idoris-router/src/cloud_tier/gate.rs` | 13 | 11、70、471 | 相符。`local_only` 只收紧；同意不跨请求记住；471 是测试注释，和单因素翻转拒绝一致。每次 grant。 |
| `idoris-router/src/cloud_tier/mod.rs` | 24 | 3、495、1001 | ⚠ 注释不符。495 前半：库忙就重试几次，仍然保证落下。`write_cancelled`（580–596）最多 3 次，`Sql` 失败只 `eprintln!` 并 return，行保持 pending，下次启动标 interrupted。后半「实在落不下……interrupted」相符。3、1001 抽查相符。 |
| `idoris-router/src/cloud_tier/pairing.rs` | 3 | 12、21、134 | 相符。设备凭证只有数据面权限；解绑先删本机；客户端只有配对的两个调用。每次配对或解绑。 |
| `idoris-router/src/connection.rs` | 2 | 56、82 | 相符。取消令牌新建时不触发；云端档回落必须能被调用方离开取消。每次连接。 |
| `idoris-router/src/control/bootstrap.rs` | 4 | 1、32、34 | 相符。命令只在组织、云形态；已输出的密钥重跑不丢，凭证不幂等；顶层紧急操作没有凭证能做。每次命令。 |
| `idoris-router/src/control/handlers.rs` | 1 | 589 | 相符。只有猜错的尝试占着名额。每次兑换。 |
| `idoris-router/src/control/mod.rs` | 3 | 9、10、127 | 相符。只在组织、云形态挂载；个人形态路径不存在；个人形态构造失败。启动。 |
| `idoris-router/src/control/pairing_codes.rs` | 6 | 5、123、310 | 相符。字母表去掉易混字符；只有猜错占名额；310 是测试注释，过期名额不误删。每次兑换。 |
| `idoris-router/src/control/wire.rs` | 2 | 337、410 | 相符。`detail` 只在驳回类；JSON 视图没有密钥或验证器哈希。每次响应。 |
| `idoris-router/src/lib.rs` | 9 | 111、311、3122 | 相符。控制面和只读 Admin 分开；云端档只在本地失败之后问；402 不是出网理由。每次路由。 |
| `idoris-router/src/proxy.rs` | 1 | 580 | 相符。关闭缓存的租户集合只增不减，进程运行期间不重新打开。每次判断缓存。 |
| `idoris-router/src/startup_form.rs` | 4 | 7、103、119 | 相符。没设形态按改动前启动；只有组织、云用 Standard 连接；云入口只在云形态。启动。 |
| `idoris-router/src/storage.rs` | 13 | 63、402、1433 | 相符。账户库只在组织、云打开；已初始化的库不能丢形态；1433 是测试注释，旧路径不写新账本表。启动。 |
| `idoris-tenancy/src/accounts/control.rs` | 18 | 23、917、1682 | 相符。审计没有密钥；未登记主体只有运维能动；同 id 重发不复活已撤销额度。每次控制写。 |
| `idoris-tenancy/src/accounts/control_reads.rs` | 2 | 9、60 | 相符。读接口没有密钥、哈希、配对码。每次读。 |
| `idoris-tenancy/src/accounts/error.rs` | 2 | 26、47 | 相符。Corrupt 是绕过 Rust 层的最后一道门；已撤销额度重发不复活。每次读或重发。 |
| `idoris-tenancy/src/accounts/ledger/admit.rs` | 9 | 10、228、437 | 相符。同一棵树第二个写者看得到第一个预留；决定阶段只扫过期和释放失败候选；只有放行才写预留。每次准入。 |
| `idoris-tenancy/src/accounts/ledger/error.rs` | 3 | 3、34、82 | 相符。和账户错误分开；结算冲突不覆盖；约束拒绝是绕过 Rust 层。每次结算或写。 |
| `idoris-tenancy/src/accounts/ledger/holds.rs` | 7 | 529、545、840 | 相符。已释放再结算不写；余量不依赖扫描顺序；失败候选腾出的额度在同一事务里可见。每次结算。 |
| `idoris-tenancy/src/accounts/ledger/hook.rs` | 2 | 12、20 | 相符。发布的那次才有 `charged`；重放不重复扣。每次收尾。 |
| `idoris-tenancy/src/accounts/ledger/hooks.rs` | 3 | 1、7、15 | 相符。只在 `mutation-test-hooks` 编译；屏障让超发确定出现。测试特性，默认关闭。 |
| `idoris-tenancy/src/accounts/ledger/limits.rs` | 2 | 5、52 | 相符。限额只追加；关掉授信不改已经发生的透支。每次设置。 |
| `idoris-tenancy/src/accounts/ledger/mod.rs` | 8 | 18、75、302 | 相符。不超发的理由在模块文档，由立即事务保证；候选失败不烧掉宽限；302 是过期扫描说明。每次准入或扫描。 |
| `idoris-tenancy/src/accounts/ledger/period.rs` | 1 | 20 | 相符。不认识的时区是错误，不当成 UTC。每次算账期。 |
| `idoris-tenancy/src/accounts/ledger/project.rs` | 2 | 10、12 | 相符。速率是滑动窗口；池剩余没有上限行则按 0。每次投影。和 D1 的 0 一致。 |
| `idoris-tenancy/src/accounts/ledger/reconcile.rs` | 2 | 39、152 | 相符。守恒检查总是做；责任合计不超过实际消费。每次对账。 |
| `idoris-tenancy/src/accounts/ledger/subsidy.rs` | 3 | 16、30、81 | 相符。函数不负责定时；花钱时再卡池上限；撤销的额度重跑不复活。每次发放和准入。 |
| `idoris-tenancy/src/accounts/ledger/types.rs` | 2 | 85、103 | 相符。已收尾的预留不再变；只有宽限预留有下限字段。每次读预留。 |
| `idoris-tenancy/src/accounts/mod.rs` | 2 | 3、9 | 相符。存储层没有 HTTP；表只在组织、云形态创建。打开库时。 |
| `idoris-tenancy/src/accounts/pairing.rs` | 6 | 16、270、639 | 相符。兑换出的凭证只有数据面权限；过期凭证不占名额；设备只能撤销自己。每次兑换或解绑。 |
| `idoris-tenancy/src/accounts/rows.rs` | 1 | 7 | 相符。绕过 Rust 层的坏数据读出来是错误，不静默放行。每次读行。 |
| `idoris-tenancy/src/accounts/store.rs` | 9 | 20、281、721 | 相符。约束只在绕过本文件的函数时触发；新增凭证没有密钥；721 是只读状态列，撤销仍可进行。每次写或撤销。 |
| `idoris-tenancy/src/attempts/mod.rs` | 11 | 16、423、990 | 相符。分录幂等键重放不重复记账；账本侧重放不重复扣；迟到 usage 只有一个入口。每次收尾。 |
| `idoris-tenancy/src/budget/ledger.rs` | 5 | 233、596、693 | 相符。旧启动留下的行只保留不改写；seed 不删除；693 是测试注释，旧路径不写新表。每次 seed 或读。 |
| `idoris-tenancy/src/cloud_tier/mod.rs` | 6 | 12、21、268 | 相符。只记 token 当量；重放不记两遍；毒化锁注释和 `into_inner` 一致。每次 begin 或 finish。 |
| `idoris-tenancy/src/lib.rs` | 3 | 37、43、56 | 相符。标准连接只用于组织、云；账户类型按冻结的契约存；云端档库默认不打开。打开时。 |
| `idoris-tenancy/src/sqlite_conn.rs` | 4 | 7、66、180 | 7 与 180 相符：过去只有账本显式 WAL；内存库必须报错。66 是 `#[error]` 误报，和 `NotWal` 一致。每次 `apply_standard`。 |
| `idoris-tenancy/src/test_support.rs` | 1 | 1 | 相符。只在 `cfg(test)` 编译。 |
| `idoris-tenancy/src/virtual_key.rs` | 1 | 93 | 相符。验证器不是密钥，仍然不能日志或返回。每次展示。 |
| `idoris-tenancy/src/virtual_key/store.rs` | 4 | 132、147、149 | 相符。本地数据密钥不能发走；只读已有文件；打不开是错误。每次探测。 |
| `idoris-upstream/src/cloud/chunk.rs` | 1 | 13 | 相符。过长的供应商请求号当成没有。每次读响应。 |
| `idoris-upstream/src/cloud/client.rs` | 6 | 10、59、146 | 10 与 146 相符：连接失败不回到 `not_sent`；Debug 不含密钥。59 是 `#[error]` 误报，和 `InsecureScheme` 一致。建客户端时。 |
| `idoris-upstream/src/cloud/credentials.rs` | 2 | 2、4 | 相符。文件密钥由部署方限制权限；Debug 只写路径。每次读文件。 |
| `idoris-upstream/src/cloud/failure.rs` | 2 | 5、34 | 相符。这里不下「是不是 4xx 拒绝」的结论；流总时长超限是失败。每次读上游结果。 |
| `idoris-upstream/src/cloud/mod.rs` | 1 | 20 | 相符。这条云客户端路径只有 role / content，没有工具调用。每次组请求。 |
| `idoris-upstream/src/cloud/sse.rs` | 4 | 8、119、298 | 相符。行结束符归一；BOM 只在流开头；只有 event 没有 data 就没有数据。每次解析。 |
| `idoris-upstream/src/cloud/usage.rs` | 1 | 19 | 相符。`raw` 只含数字字段。每次记账。 |
| `idoris-upstream/src/cloud/wire.rs` | 8 | 5、185、400 | 相符。只转发明确支持的字段；协议字段最后写；模型、max_tokens、stream 永远是准入时的值。每次组体。 |
| `eval/thai-edu/lib.mjs` | 4 | 354、386、387 | 相符。覆盖率下限只在跑完整默认目录时；`--only` 与 `--ids` 取交集。每次筛选用例。387 是 JSDoc 返回类型，断言词来自 `only` 字段名，检查器误报。 |
| `scripts/check-rust-egress.sh` | 4 | 217、225、274 | 相符。启动和 health 之后零出网；免密钥开发请求不出网，所以探针不能用 `IDORIS_DEV_NO_KEY`。脚本运行时。 |

## 需要主会话处理

没有「⚠ 需修」。有 9 处「⚠ 注释不符」，都在注释，本文不改代码。下面这些由主会话决定要不要另开修复。

### ⚠ 注释不符

1. `bin/idoris.rs:171-172`：cloud 命令会解析数据库路径，并在识别本地密钥时只读打开虚拟 key 库。注释写不开数据库、不碰 local-key。
2. `bin/idoris.rs:348-350` 前半句：非云仍可绑 Tailscale，不是「只有云形态可以听非 loopback」。后半句「按原样解析」相符。
3. `cloud/ingress/bind.rs:1`：同文件第 7–8 行允许存量 tenant / 组织听 Tailscale。
4. `cloud/ingress/mod.rs:16`：模块表同一句，和 `bind.rs` 的表不一致。
5. `cloud/mod.rs:22-24`：启动入口 `from_env_for_form` 在云形态缺配置、或非云设了配置时都是 `Err`，不是「不设就是 None、设了且 tenant 就启用」。同文件 375–384 已更正。
6. `cloud/run.rs:12`：`Drop` 里 `finalize` 失败只打印，请求可以留在进行中。收尾成功会关。
7. `cloud/safety/extract.rs:632`：「不会撞」不成立。实现只用请求号前 12 个字母数字。字符集那半句成立。
8. `cloud/safety/llm.rs:15-16`：发出去的请求体还有 model、max_tokens、stream，不是只有 messages 和采样参数。不带可关联到孩子的字段、密钥不进日志，这两句仍相符。
9. `cloud_tier/mod.rs:495` 前半：「仍然保证落下」不成立。三次 `Sql` 失败就返回，行留 pending。后半句下次启动标 interrupted 相符。

### 其它

1. `holds.rs:562`：已结算且 `charged` 为 NULL 时按 0 做幂等比较。查询失败不会走到这里。正常结算会写金额。绕过写入把 `charged` 改成 NULL 后，再以 0 结算会当成 `AlreadySettled`，可能少收。未标「⚠ 需修」。
2. `abuse.rs` 的 `AuthBudget`：已知前缀的判断和未知桶扣减不在同一把锁里，中间不重查。窗口是少扣查询预算、多查库，不是出网放行。没有专测这个窗口。
3. 未找到真并发测试：`SimpleExtraCost`、`AttemptStore`、tenancy 的 `cloud_tier` 存储、`proxy.rs` 新增的 `RwLock`。单线程测试覆盖了状态机或只增不减。见 C2。
4. T2 未找到「去掉该守卫会变红」的测试，或定位行单独去掉仍绿、同文件另有会变红的守卫：
   - `auth.rs:202` 时钟倒退 `Unavailable`（同函数过期有测试）
   - `bin/idoris.rs:194` 非 Unicode 的 `IDORIS_FORM`（预算环境变量有测试）
   - `ingress/bind.rs:45` Admin 必须 loopback（通配地址有测试）
   - `ingress/config.rs:342` `from_env` 缺环境变量（错误 schema 有测试）
   - `ingress/net.rs:102` 前缀必须是数字（主机位有测试）
   - `cloud/run.rs:178` 内存与库状态不一致（儿童发布有测试）
   - `ledger_cost.rs:178` 没有价目（重复预留有测试）
   - `llm.rs:336` `max_output_tokens == 0`，三处构造拒绝都没有测试
   - `cloud_tier/pairing.rs:220` 响应体体积上限（超长正文后面的 JSON 解析仍是 `BadResponse`）
   - `control/bootstrap.rs:162` 空 spec 或超过 64 KiB（空串和超长对象后面仍会解析失败）
   - `proxy_budget.rs:43` 付费但没有账本
   - `ledger/admit.rs:111` 未知凭证状态
   - `ledger/components.rs:74` 预留不是 Open 不能加附加成本
   - `ledger/holds.rs:75` 预留行 attempt id 损坏
   - `ledger/hook.rs:70` 绑定尝试时预留不是 Open
   - `ledger/limits.rs:29` 负数金额（SQL CHECK 也会拒，现有测试只 `is_err`）
   - `accounts/rows.rs:156` 祖先链超过步行上限（SQL `depth < 64` 使这一支走不到）
   - `attempts/mod.rs:545` 第一次尝试必须是 index 0
   - `eval/thai-edu/compare.mjs:33`、`lib.mjs:512`、`run.mjs:440`
   - `scripts/check-rust-egress.sh:244`、`scripts/compute-ops/rehearse-litestream-restore.sh:16`
5. 设计稿 §12 第 200 行仍写「每片一个 PR 进 main」。owner「一条功能分支、充分测试后一个 PR」的原句不在附录「已拍板」里。出处是本次任务和 PR #600 正文。要不要改设计稿，由主会话决定。本文不改设计稿正文。
6. 已实测的守卫只有 `account.rs` 的空树 `TreeError::Empty`：临时去掉 `return Err(TreeError::Empty)` 后，`invalid_trees_are_rejected_with_the_expected_error_table` 失败（left `NoRoot`，right `Empty`），随后还原。其余 T2 是读断言或读调用链，没有再跑 `cargo test`。`control/wire.rs:180` 和 `dispatch.rs:534` 标成会变红，依据是审计提交和 500/503 映射，未 cargo 实测。

# Mini：#485 独立防御测试迁移验收

2026-10-15。本报告只验收 Mini 的 #485 迁移。测试提交 `e630eabdefc097760eaeadc3422dce286836d816` 已普通推送到 `ops/travel-20261015-library`。该提交只在 `crates/idoris-router/src/virtual_key_wiring_tests.rs` 新增 236 行测试，不修改生产认证规则，不更新历史 PR。

## 精确来源与基线

开工时 `git status --porcelain=v1` 为空，当前分支为 main。确认任务远程引用后创建并跟踪本人分支，没有 reset 或 stash。

| 项目 | 精确 SHA |
| --- | --- |
| 当前 main，GitHub 只读查询确认 | `959337d444a9b0df371e43a7e0a2854ae894f8e3` |
| 本人任务包与起点 | `9054a5d0f74e6468adc6207336077801d838c36d` |
| #485 原始 base，#484 | `afe3a6eb543ac5d35b8e499b3b4d3b2b8db72a59` |
| #485 原始 head | `27237cf2ae1ad310d1c66dd95d67785d491b1b7b` |
| #513 比较 head | `a3cc0c77336766457faff430c829c235ed0c323c` |
| 已实测并推送的测试提交 | `e630eabdefc097760eaeadc3422dce286836d816` |
| 新测试文件 Git blob | `489fbc49027805fa5bfd008a762f635a7be08791` |

实际执行 `gh pr view 485 --json number,title,baseRefName,baseRefOid,headRefName,headRefOid,body,url,files`、`gh pr diff 485` 和同字段的 #513 查询。原始响应、diff 与 #513 文件列表保存在 [pr485](pr485/)；原始 diff 的 SHA-256 为 `9ca06da7ba0801834e7341fd627ffa665517f0235d7cc7f17902703ed8035113`。另外读取上述精确 Git 对象间的 diff，避免根据 PR 描述推断覆盖。

#513 相对 main 的八个文件没有 `virtual_key_wiring_tests.rs`。其中 #483 的持久化/签发断言、#484 的显式租户凭据 helper、协议缺失租户头与正向 attribution 测试，已由 [独立报告](idoris-m4-storage-paid-independent-review.md) 列明。本次没有复制这些测试、helper 或生产实现，也不宣称 #513 已合并或本轮重新实测了它。

## 原始断言到现有测试的映射

原 #485 测试增量为 `virtual_key_wiring_tests.rs` 的两个测试，以及 `feedback_wiring_tests.rs` 中无效租户标识响应的两处断言调整。下表覆盖全部独立测试义务；其余 `auth.rs` 和 `lib.rs` 改动属于生产实现，本次不迁移。

| 原始独立断言 | 当前承接与实际结果 |
| --- | --- |
| `tenant_keys_reject_cross_tenant_unbound_and_anonymous_requests_without_side_effects` 的九端点、四种身份拒绝 | 同名新增测试，位于测试文件 1041 行；九端点各验证四种身份，36 次请求全通过，逐项矩阵见下表。 |
| 每个拒绝响应都有 Record-Id；tenant-a/b 都没有对应事件 | 同名测试保留逐响应事件查询断言。 |
| 403/401 的 reason_code；错误正文不泄漏 tenant-a | 401 原值保留；403 映射到当前协议 `tenant_scope_forbidden`。全部正文不包含 tenant-a。 |
| 九端点重复租户头拒绝 | 同名测试保留九次拒绝，按当前 main 映射到 400；新增 reason_code、无泄漏及双租户零事件断言。 |
| 全部请求后两租户记录均为空；上游 POST 为零 | 同名测试保留两租户 `TenantStore::list` 空集合和 wiremock `expect(0)`、`verify()`。 |
| `tenant_library_without_authenticator_fails_closed_without_egress_or_audit`：DEV_NO_KEY 关闭/开启 × 无/有 Bearer | 同名新增测试，位于测试文件 1215 行；四次 chat 均 401、`VIRTUAL_KEY_UNAUTHORIZED`，逐次检查 tenant-a 零事件、零记录；上游 POST 为零。 |
| 缺少 authenticator 时 health 仍可用 | 同名测试对两种 DEV_NO_KEY 配置各检查一次 `/health` 200。 |
| 无效租户标识的 feedback 拒绝 | 当前 main 已有 `feedback_wiring_tests::tenant_feedback_rejects_invalid_tenant_identifier_before_event_log`，216 行；实测 400 / `invalid_tenant_scope`。原 #485 的 403 / `policy_violation` 不再是当前协议，不恢复旧实现。 |
| 生产增量涉及的 role scope 在 RequestReceived 前拒绝 | 当前已有 `tenant_authority_regressions::rejected_key_role_never_writes_request_received`，786 行；实测 403 / `VIRTUAL_KEY_ROLE_FORBIDDEN`，没有 RequestReceived，上游为零，认证后的拒绝可以有本租户 AuditFinalized。 |
| 可信租户 attribution，拒绝不得污染受害者记录 | 当前已有 `feedback_wiring_tests::rejected_tenant_requests_do_not_write_victim_audit_rows`，371 行，及 `authenticated_tenant_request_writes_its_own_terminal_audit`，447 行；本轮均通过。 |

下表各行均使用有效请求体、真实铸造并存入 VirtualKeyStore 的绑定 key 与未绑定 key。绑定 key 允许所有当前角色、LocalOnly；组件支持 Chat/Embedding/Rerank；DEV_NO_KEY 开启。拒绝状态不是由无效 body 或缺失角色权限代替租户授权检查。

| 方法与端点 | tenant-a key 请求 tenant-b | 未绑定 key 请求 tenant-a | 匿名请求 tenant-b | malformed Bearer | 重复 tenant-a/b 头 |
| --- | --- | --- | --- | --- | --- |
| POST `/v1/chat/completions` | 403 | 403 | 401 | 401 | 400 tenant_missing |
| POST `/v1/embeddings` | 403 | 403 | 401 | 401 | 400 tenant_missing |
| POST `/v1/rerank` | 403 | 403 | 401 | 401 | 400 tenant_missing |
| POST `/v1/messages` | 403 | 403 | 401 | 401 | 400 tenant_missing |
| POST `/v1/feedback` | 403 | 403 | 401 | 401 | 400 invalid_tenant_scope |
| GET `/idoris/tenants/tenant-b/requests/record-1` | 403 | 403 | 401 | 401 | 400 invalid_tenant_scope |
| GET `/idoris/tenants/tenant-b/audit` | 403 | 403 | 401 | 401 | 400 invalid_tenant_scope |
| GET `/idoris/tenants/tenant-b/usage` | 403 | 403 | 401 | 401 | 400 invalid_tenant_scope |
| GET `/idoris/tenants/tenant-b/budget` | 403 | 403 | 401 | 401 | 400 invalid_tenant_scope |

四个推理端点的重复头由 `remote_bind_auth_middleware` 最前面的租户头唯一性检查返回 tenant_missing；其余端点由该中间件稍后的唯一性检查返回 invalid_tenant_scope。跨租户与未绑定 key 均在同一中间件返回 tenant_scope_forbidden。这些是 main959 的现有协议，403、401 与 400 都保持拒绝和零副作用。

负向断言有现成的正向对照。本轮实测 `bound_tenant_key_serves_only_its_tenant_and_records_the_authenticated_chain` 获得 200、一次真实本机 mock POST、本租户 RequestReceived/AuditFinalized，且外租户无事件；`valid_key_reaches_execution_boundary_and_none_authenticator_preserves_legacy_behavior` 验证 Personal 兼容路径仍可获得 200；反馈模块也验证认证后受害者能写自己的审计记录。因此不依靠一个始终拒绝或从不写入的空实现获得绿色结果。

## 本轮真实执行

命令均在仓库根目录执行，使用 Rust/Cargo 1.95.0、`CARGO_BUILD_JOBS=2`、locked/offline。测试仅请求 wiremock 本机回环，不下载模型、不调用付费 API。完整命令及实际退出码见 [commands.json](pr485/commands.json)。

| 检查 | 结果 | 原始日志 |
| --- | --- | --- |
| 未修改基线认证模块 | 17 passed | [baseline-auth.log](pr485/baseline-auth.log) |
| 第一轮迁移后的 `virtual_key_wiring_tests::tenant_` | 9 passed，含新增两项及既有正向对照 | [migration.log](pr485/migration.log) |
| CI features 下认证模块 | 19 passed，0 failed | [auth-final.log](pr485/auth-final.log) |
| 同 features 下反馈审计模块 | 8 passed，0 failed | [feedback-final.log](pr485/feedback-final.log) |
| `cargo fmt --all -- --check` | exit 0，无输出 | [fmt.log](pr485/fmt.log) |
| 受影响包 `cargo clippy -p idoris-router --all-targets --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks -- -D warnings` | exit 0 | [clippy.log](pr485/clippy.log) |
| `git diff --check` | exit 0 | 提交前实际执行 |

各过滤器结果存在重叠，不累计为唯一测试数。最终认证 19 项和反馈 8 项没有重叠。首次沙箱基线执行因 wiremock 无权监听端口而失败，5 passed / 12 failed；确认错误是 bind 的 Operation not permitted 后，使用获准的本机回环执行权限，未改源码即得到基线 17 passed。没有把该环境失败算作产品回归或隐去失败后声称首次全绿。

## 推送、边界与停止条件

第一轮只修改并验证一个测试文件，提交后普通 push 输出 `9054a5d..e630eab HEAD -> ops/travel-20261015-library`。第二轮只保存本报告、GitHub 原始来源和日志，源码 blob 保持不变，故不重测相同产物。没有为扩展范围新建项目或再派工，没有需丢弃的源码尝试。

本报告是迁移作者的实测证据，不是外部批准。未运行全仓测试、发布验收或跨分支组合验收；不据此关闭 #485、合并或宣称其已落 main。未修改或 checkout Grok Bot 车道，未审 #507/#479/#510，未进入 Agent24，未启动任何本地 PR Review Watcher。没有模型子代理调用；订阅 usage 未知。

Mini 的最终完成判据是本测试提交与本报告均已推送、以上独立断言有映射且目标测试通过。推送最终提交与远程引用的核对结果记入 `~/.pi/agent/baton/decisions.tsv`；满足后停止，不等候或代验 Grok Bot 的目标。

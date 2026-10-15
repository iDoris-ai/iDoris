# Grok Bot 独立复核：#507 / #479 / #510 已保存修复增量（2026-10-15）

车道：Grok Bot（云电脑独立 checkout，Linux x86_64）。任务包基线 `ops/travel-20261015-library@9054a5d0f74e6468adc6207336077801d838c36d`。
范围：只读复核 HANDOFF.md 列出的三个保存 head。未改生产代码，未更新三个 PR，未 merge/force，未下载模型，未调用付费模型 API，未启动 PR Review Watcher，未触碰 Mini 分支、#485 迁移或 Agent24。
日志在 `logs/`（检出路径已替换为占位符），哈希见文末。

## 总结论

| PR | 原 reviewed head（=当前 PR head） | 保存 head | 结论 |
|---|---|---|---|
| #507 | a232f8391107263408589a5c846dda524ebd0421 | a69f954293c0dcddc4195dd08debdefe9fbd8d57 | 未发现阻断；late-backend 行为在同源 Linux 构建上实测复现 |
| #479 | b8f3214d4bc79a319f5b722015d68b4ead71c61c | e3426f657328d21f7afd8901206de7593f02de1a | 未发现阻断；null 修复正确；n 未乘风险实测确认仍在（非本增量引入，已文档化） |
| #510 | bbf99ec34950867814ebb57ba33763dc8d96d536 | f4651f3c00aa6ba64f8564925ddbcdfcce9464f1 | 未发现阻断；conformance 与 check-subscription 实测通过，subscription 仍用固定 runtime model |

共同核对（实测）：
- 三个保存远程分支 `handoff/20261015-pr507-fix`、`-pr479-null-fix`、`-pr510-ci-fix` 当前 tip 与上表保存 head 完全一致。
- `gh pr view` 显示三个 PR 当前 head 仍等于原 reviewed head（保存修复尚未推进 PR，符合 HANDOFF）。
- 三个保存 head 均以原 reviewed head 为祖先。#507 合并提交 6d6bb215 与 #510 合并提交 aa70c7d9 的父为 (reviewed head, main 959337d)；用 `git merge-tree --write-tree` 重做合并，树哈希与实际提交完全一致（03256b99… / 0b1ca7ca…），即合并无手工改动。#479 父为 b8f3214，未混入 main。
- 507 与 510 两个保存 head 互相 `merge-tree` 无冲突。
- 测试均在本次云端 checkout 新跑；作者报告中的 /tmp 日志在云端不可读，凡未在此重跑的只作“仅源码/仅引用”结论。

## #507 onboarding（a232f83 → a69f954）

增量：合并 main959（#508 auth，3 文件）+ 自有提交 a69f954（README、v0.3.0 安装指南、`scripts/test_onboarding_smoke.py`，无 Rust 生产改动）。

实测：
- 以不可变 tag `v0.3.0`（03c079c70611331a8c85a1666e32a75df6359d09）源码在 Linux 上 `cargo build --release --locked -p idoris-router`，按 release.yml 打包布局（idoris + 4 个 config 文件）组装。注意：这是同源 Linux 构建，不是正式 macOS arm64 归档（sha256 ce5406…），归档本身无法在本机运行。
- `python3 -u scripts/test_onboarding_smoke.py <pkg>/idoris`：exit 0，四组 PASS，fixture 收到 model 字段 `['fixture-model', 'idoris/daily']`。
- `... --late-backend-only`：exit 0。实测行为：后端不可达时启动 → `/v1/models` 恢复后返回 200，但 chat 持续 503 且 remediation 与首次完全一致、后端 chat 计数不变 → 只重启自有子进程后 chat 200 / `fixture-ok`。即“晚启动必须 restart”是真实行为，不是文档推测。Linux 上不可达端口的报错文案落在脚本允许的两种之一。
- `git diff --check`、`py_compile` 通过。

仅源码结论：
- README 新表述（付费 resident buffered 走预算预留/结算、付费 streaming 在上游前拒绝、无全局启动拒绝）与 main959 源码一致：`lib.rs` 中 `if is_paid && stream_requested` 分支及 `paid_streaming_proxy_remains_fail_closed_before_forwarding` 测试（503、0 次上游 POST）。
- `startup_error` 锁存逻辑在 `crates/idoris-backend/src/supervisor.rs`；tag→main959 间该文件未变，因此该恢复指引对当前 main 同样适用。
- 指南把“准备后端”移到“启动服务”之前，`/capabilities` 示例改为就绪后且用 stdin Bearer，没有把 key 放进 argv。

发现：
- 非阻断 N507-1：smoke 脚本刻意锁定 v0.3.0 行为（`SEEN[1]['model']=='idoris/daily'`、`model not found: omlx`）。#510 落地后 resident 别名会被解析为具体 model，这些断言将反转；脚本与指南已声明“验证 v0.3.1 前须先更新预期”，且未接入 CI，故不阻断，但发布 0.3.1 前必须改。
- 非阻断 N507-2：本复核未在 macOS 正式二进制上重跑；macOS 实测仍只来自作者报告。

## #479 completion-cap null 修复（b8f3214 → e3426f6）

增量：生产仅 `budget.rs` 一行行为改动 `body.get(field).filter(|v| !v.is_null())` + 注释；测试与计划文档。

实测：
- `cargo test -p idoris-router --test supervisor_parameters --locked`：4 passed。
- `cargo test -p idoris-router --lib budget --locked -- --test-threads=1`：41 passed，467 filtered。
- `cargo clippy -p idoris-router --lib --test supervisor_parameters --locked -- -D warnings`：exit 0；`cargo fmt --all -- --check`：exit 0。
- 独立探针（临时测试，已撤销、未提交）：`{"max_tokens":2048,"n":4}` 在预算 3000（每 completion token 1 minor）下被接受 200；`{"max_tokens":null,"n":8}` 在预算 1024 下被接受 200。1 passed，见 `logs/r479-n-probe.log`。

结论：
- null 修复正确：null 与缺省同义只作用于报价；非 null 仍严格 u64 ≤ 2^53；null 兄弟字段不能掩盖非法字段（新增 `"4096"` 字符串 + null/8 兄弟组合实测 400）。请求体原样保留 null。
- 既有风险确认仍在（非阻断，非本增量引入）N479-1：报价不乘 `n`。探针证明 n=4 时只按单份 2048 报价准入，理论最大输出 8192 tokens。这依赖结算按真实 usage 收费；报价阶段的预算上限保护对 n>1 不成立。计划文档已明确列为后续：报价乘 n 或对付费 resident 拒绝 n>1。
- 非阻断 N479-2：f64 费率计算仍非精确货币运算，注释已改为如实表述，2^53 只保证整数 cap 转换无损。
- 非阻断 N479-3：显式 null 原样转发上游，上游对 null 的解释（通常等同无上限）与缺省一致，报价仍为 1024 底线；与缺省行为同等风险，不比缺省更差。
- 传播：e342 尚未传播到 480/489/490/492/497 栈，需按依赖图顺序推进（HANDOFF 已记录），本复核未做。

## #510 role 绑定 CI 修复（bbf99ec → f4651f3）

增量：合并 main959 + 自有提交 f4651f3（dispatch.rs、lib.rs 生产改动；conformance fixture/测试；role 文档；egress 脚本 fixture）。

实测：
- `cargo test -p idoris-router --lib --locked role_binding_`：8 passed（含新增 `role_binding_protocols_send_exact_target_and_preserve_concrete_payload`）。
- `bash scripts/conformance-rust.sh`（真实脚本，release 构建 idoris + conformance_key_seed）：exit 0；Test Files 16 passed / 1 skipped；Tests 136 passed / 8 todo（144）。
- `bash scripts/check-subscription.sh`：exit 0；其中 TS 与 Rust 两轮共享 conformance 均为 136 passed / 8 todo；其余 Rust 过滤与集成测试全部 ok。
- `cargo clippy -p idoris-router --all-targets --locked -- -D warnings`：exit 0；`cargo fmt --all -- --check`：exit 0。
- Linux strace 类检查未单独运行（作者也未在 macOS 跑）；check-rust-egress.sh 本次未单独执行。

仅源码结论（重点：订阅固定 relay 与协议 model 绑定）：
- subscription 执行仍用 `subscription/dispatch.rs:64` 的 `handle.model_id()`，本增量未改；请求 model 与映射目标都不会进入 subscription 沙箱。
- `fixed_subscription_roles` 仅在 `classify_subscription_card` 判定为真正 subscription 卡且 extensions 中不存在 `idoris_role_models` 时恢复原角色资格；存在映射（含空对象）仍在启动与程序化选择处失败。单测覆盖 daily/auto/fast 可选、无隐私头时不可选。
- `bound_http_body` 现统一用于 chat resident、embeddings、rerank、messages 四处；messages 在选择前已过滤为 `is_resident_http_service`，因此不会对 subscription 卡调用解析。具体 model 覆盖原样保留；解析内部错误统一脱敏。
- conformance fixture 仅给共享 HTTP fixture 声明 `daily: fake-model-1`（与 fake upstream 默认 model 一致），未给生产 stock card 编造 model ID；`check-rust-egress.sh` 只绑定其本地 Python fixture 的 `fixture-model`。

发现：
- 非阻断 N510-1：`fixed_subscription_roles` 对所有 catalog 角色放行（含 embed/rerank/vision/decide），依赖能力门再过滤；单测只覆盖 daily/auto/fast，建议补一条 embed/rerank 走 subscription 卡被能力门拒绝的断言。
- 非阻断 N510-2：与 #507 smoke 的 v0.3.0 别名断言存在语义交互（见 N507-1），不冲突于 CI。

## 阻断项

无。三项均未发现需要阻止推进的问题。以上不代表远程 CI、外部审核或合并已完成；保存分支仍需协调者按 HANDOFF 第 25 行流程推进。

## 日志 sha256（logs/ 内，已替换检出路径）

见同目录 `logs/SHA256SUMS`。

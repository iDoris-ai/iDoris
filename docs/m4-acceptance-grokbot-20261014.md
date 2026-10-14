# M4 发布候选独立验收报告（Grok Bot / Linux）

- **任务**: `m4-remote-audit-20261014` / attempt `f1b7203a-18d8-482f-9014-dfad0ec831f2`
- **候选 source SHA**: `357bfd92cc2fe58169016154560fc8639ff4b530`
- **分支**: `baton/grokbot-20261014`
- **平台**: **仅 Linux x86_64**（本 box 无 macOS；**未做 macOS 验收**）
- **执行方**: grokbot
- **时间窗口**: 2026-10-14T12:48Z ~ 2026-10-14T13:26Z（UTC）；本地 Asia/Bangkok = UTC+7
- **说明**: 云端交付完成 ≠ 本报告的验收通过结论；本报告只覆盖上述单一候选 SHA 在 Linux 上的门禁与租户/审计证据。

## 1. 工具链

| 组件 | 版本 / 备注 |
|------|-------------|
| rustc / cargo | 1.95.0（rustup；`rust-toolchain.toml`）+ rustfmt + clippy |
| Node | v22.20.0（用户目录安装；系统原为 v20） |
| pnpm | 10.34.5（`packageManager` 字段；`COREPACK_HOME` / `PNPM_HOME` 指向 `$HOME`） |
| 并行 | `CARGO_BUILD_JOBS=2`（约 15Gi RAM，避免 OOM） |
| strace | apt 安装后用于 egress 脚本 |

## 2. 命令与退出码（如实记录）

| # | 命令 | exit |
|---|------|------|
| 1 | `pnpm install --frozen-lockfile` | 0 |
| 2 | `pnpm lint` | 0 |
| 3 | `pnpm typecheck` | 0 |
| 4 | `pnpm check:contract-drift` | 0 |
| 5 | `pnpm build` | 0 |
| 6 | `pnpm test` | 0 |
| 7 | `pnpm conformance` | 0 |
| 8 | `pnpm test:tenancy` | 0 |
| 9 | `pnpm test:audit` | 0 |
| 10 | `cargo fmt --check` | 0 |
| 11 | `cargo clippy --all-targets --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks -- -D warnings` | 0 |
| 12 | `cargo test --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks` | **101** |
| 13 | `cargo test --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks -- --skip fixture_basic_modes_are_real --skip fixture_exposes_hang_ignore_term_hold_pipe_and_pid_marker_modes` | 0 |
| 14 | `cargo test --test query_acceptance --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks` | 0 |
| 15 | `cargo test -p idoris-tenancy --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks` | 0 |
| 16 | `cargo test -p genai --lib webc::` | 0 |
| 17 | `IDORIS_CONFORMANCE_CMD=/idoris-conformance-missing IDORIS_CONFORMANCE_ARGV='["/idoris-conformance-missing"]' bash scripts/conformance-rust.sh` | 0 |
| 18 | `bash scripts/check-rust-egress.sh` | 0 |

### 关于 exit 101（命令 12）

全量 `cargo test` 在 `idoris-upstream` 的 `subscription_cli_fixture` 卡死：

- `fixture_basic_modes_are_real`：`Command::status()` 继承本环境 stdin（socket），假 CLI 的 `nonzero`/`empty` 模式执行 `cat`，读不到 EOF 而挂起。
- `fixture_exposes_hang_ignore_term_hold_pipe_and_pid_marker_modes`：挂起子进程长时间未退出（本环境 process group / 回收表现异常）。

约 24 分钟无进展后对卡住的测试进程发送 SIGKILL，cargo 以 **101** 结束。随后用 `--skip` 跳过上述 2 个用例重跑（命令 13）**通过（0）**。  
**结论**：这 2 个 fixture 用例在本 Linux box **未得到干净通过**；其余 cargo 套件在 skip 重跑中全部通过。

### 未跑

- `IDORIS_SUBSCRIPTION_REAL_CLI` 真实订阅 CLI 选择加入测试：**未跑**（本 box 无真实登录）。
- **macOS** 全部门禁 / subscription matrix：**不可用**。
- `cargo-deny` advisories/bans/licenses/sources：CI 有对应 action；本手测未单独跑 deny（非本次 predicate 强制项，已记录缺口）。

## 3. 通过/失败计数（按套件）

| 套件 | 通过 | 失败 | 备注 |
|------|------|------|------|
| pnpm test（聚合） | **574** | 0 | contracts 144 + tenancy 16 + growth 22 + recommender 60 + adapters 102 + federation 21 + router 209 |
| pnpm conformance | **134** | 0 | 另 8 todo / 1 file skipped |
| pnpm test:tenancy | **16** | 0 | |
| pnpm test:audit | **30** | 0 | |
| cargo fmt / clippy | n/a | 0 | clippy `-D warnings` 通过 |
| cargo test（全量，命令 12） | 部分通过后中断 | **101** | 卡在 2 个 subscription fixture |
| cargo test（skip 2 fixture，命令 13） | **1210** | 0 | 跨 93 个 test result 行合计；2 fixture 被 skip |
| cargo query_acceptance | **3** | 0 | |
| cargo -p idoris-tenancy | **170** | 0 | |
| cargo -p genai --lib webc:: | **14** | 0 | |
| scripts/conformance-rust.sh | **134** | 0 | 与 TS conformance 同量级；正控坏路径仍选 release 产物 |
| scripts/check-rust-egress.sh | PASS | 0 | loopback 本地路径 + 正控捕获远端 connect |

**门禁级汇总（Linux）**：TS CI gates（install/lint/typecheck/contract-drift/build/test/conformance）全绿；Rust fmt/clippy 全绿；cargo test 在跳过 2 个 fixture 后全绿；egress PASS。  
**残留红点**：全量 cargo test 因 2 fixture 环境挂起记为 101；无 macOS；无真实订阅 CLI。

## 4. 租户隔离（tenant-isolation）发现

证据来源：`pnpm test:tenancy`、`cargo test -p idoris-tenancy`、`query_acceptance`、conformance `tests/tenant.test.ts`、audit tenant 作用域用例。

1. **同维度跨租户隔离**：`same_scope_dimensions_remain_isolated_between_tenants` 通过 — 相同 scope 维度下租户预算互不可见。
2. **Scoped store**：`same_ids_are_isolated_and_list_filters_kind`、`mismatch_is_rejected_without_changing_either_tenant` 通过 — 同 ID 跨租户隔离；租户不匹配拒绝且不改写任一侧。
3. **Event log**：`tenant_mismatch_is_rejected_and_queries_are_isolated` 通过。
4. **未配置 / 空白租户**：`blank_and_unconfigured_tenants_return_errors` 通过 — 不静默回落默认租户。
5. **Audit TS**：`A 的审计 B 一条都看不到`；缺 tenant 上下文抛 `TenantScopeError`；`tenant_id` 与上下文不一致抛错（防跨租户伪造），且写入失败后 A 侧列表仍为空。
6. **Conformance**：`tests/tenant.test.ts` 2 例通过。

**发现摘要**：在本候选 SHA 上，Linux 单测与 conformance 显示租户边界（读写隔离、mismatch 拒绝、无默认回落）行为符合预期；**未发现**本轮手测可复现的跨租户泄漏。

## 5. 审计（audit）发现：record-id 查询 / 投影 / 跨租户

证据来源：`pnpm test:audit`（30）、`cargo test --test query_acceptance`（3）。

1. **record-id 可查询且持久化**：`request_record_is_queryable_and_survives_reopen` 通过 — 请求产生 `record_id` 后，经 `/idoris/tenants/{PERSONAL_TENANT_ID}/audit?record_id=...` 可查到同一 ID；进程/库 reopen 后仍可查。
2. **字段白名单 / 内容黑名单 / 长度上限 / reason 四类**：audit.test.ts 对应 describe 共 30 例全过（含投影字段约束与非法内容拒绝）。
3. **跨租户审计**：见上节 — A/B 互不可见；伪造 `tenant_id` 拒绝；缺上下文拒绝。
4. **时区子进程固定月度答案**：`timezone_child_probe`、`three_timezone_child_processes_return_fixed_monthly_answers` 通过（与租户用量/周期相关的稳定性证据）。

**发现摘要**：record-id 查询路径与审计投影约束在 Linux 上通过；跨租户审计隔离用例通过。**未跑**真实多进程长期审计存储压测或 macOS 路径。

## 6. 对协调器的总体结论

| 项 | 判断 |
|----|------|
| 候选 SHA | `357bfd92cc2fe58169016154560fc8639ff4b530` |
| Linux CI 等价门禁（TS + fmt/clippy + conformance-rust + egress） | **通过** |
| 全量 `cargo test`（含 2 个 subscription fixture） | **未干净通过**（环境挂起 → 101；skip 后通过） |
| 租户隔离 / 审计 record-id | **Linux 证据通过** |
| macOS | **未测** |
| 真实订阅 CLI | **未测** |
| **是否可把“云端交付”当成验收通过** | **否** — 本报告仅为 Linux 独立验证；残留 fixture 环境问题 + 无 macOS，**不能**单独签发完整 M4 验收通过 |

**建议下一步（协调器）**：在 macOS runner 或本地 Mac 补跑 subscription matrix 与全量 cargo test；用非 socket stdin / 正常 TTY 环境复验 `subscription_cli_fixture` 两例；按需补 `cargo deny`。

## 7. 证据索引

- 原始命令日志目录（执行机本地，不入库）：`/workspace/m4-gate-logs/`（`summary.tsv` + 各 gate `.log`）
- 本文件：`docs/m4-acceptance-grokbot-20261014.md`
- Baton：`baton-result.json`、`baton-decisions.tsv`

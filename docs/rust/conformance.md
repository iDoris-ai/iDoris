# 用 conformance 套件测 Rust 版 `idoris`

`conformance/`（R0，PR #49，2026-10-01 已合并进 `main`）是一套语言无关的黑盒
HTTP 契约测试：它只通过 HTTP 访问被测服务，不 `import` 任何 `@idoris/*` 代码，
被测服务是 TS 参考实现（`packages/router`）还是 Rust 版，套件本身完全不关心——
接线方式见套件自己的 `conformance/README.md`「接入新实现」一节。本文档只覆盖
Rust 这一侧：现在跑会看到什么、以后怎么跑 TS/Rust 两版对照。

## 现状（2026-10-01，#48 合并前实跑）

R2-D/E/G 已经把 `idoris-policy`（决策管道）、`idoris-tenancy`（预算账本）、
组件卡加载（`IDORIS_COMPONENTS_DIR`）、路由策略（`IDORIS_ROUTING_POLICY`）、
`/v1/models`、`/v1/chat/completions`（本地 Supervisor 路径 + 直连转发 + SSE）
接进了 `idoris-router`。在合并了 #49、#153 的 #48 分支上跑
`bash scripts/conformance-rust.sh`：

```
Test Files  7 passed | 1 skipped (8)
     Tests  47 passed | 7 todo (54)
```

47 条非 todo 用例全部通过；7 条 `it.todo` 是 `known-spec-conflicts.test.ts`
里记录的已知规范落差，TS 与 Rust 两侧一致。此前唯一的失败（客户端断开时取消
上游请求）是 TS 参考实现的 bug，#153 修复后两侧都通过。

`scripts/conformance-rust.sh` 目前还**没有**接进 CI 的 `rust` job（该 job 不装
Node/pnpm）。通过率已经追平，下一步可以把它接成必跑项。

## 怎么跑 Rust 版

```bash
bash scripts/conformance-rust.sh
```

这一步做了三件事：

1. `cargo build --release --locked -p idoris-router`，产出
   `target/release/idoris`；
2. 设置 `IDORIS_CONFORMANCE_CMD=$(pwd)/target/release/idoris`（不带任何参数
   ——`idoris-router` 目前不解析 argv，只读环境变量，见
   `crates/idoris-router/src/bin/idoris.rs`；conformance harness
   [`conformance/src/harness.ts`] 按空白切分 `IDORIS_CONFORMANCE_CMD` 得到
   `bin` + `args` 来 spawn 子进程，所以这里给纯路径即可，不是漏写了
   `serve`）；
3. `pnpm conformance`（等价于 `pnpm --filter @idoris/router... build &&
   pnpm --filter @idoris/conformance test:conformance`——前半句会顺带 build
   一遍 TS router，这一步对跑 Rust 版是多余的，但无害，就是慢几秒；套件本身
   跑的是 `IDORIS_CONFORMANCE_CMD` 指向的进程，不是刚 build 出来的 TS
   `dist/`）。

`IDORIS_PORT`/`IDORIS_COMPONENTS_DIR`/`IDORIS_ROUTING_POLICY` 不需要手动
设置——harness 会在每组测试里自己算好、注入给子进程的环境变量（见
`conformance/src/harness.ts` 的 `spawnConformanceServer`）。

## TS 与 Rust 双跑对照

### 已批准的 Rust 安全差异（B1）

B1 不以“字节级复刻 TS”为目标来削弱 Rust 已有的安全行为。下面这些差异已经
被显式锁定；共同 HTTP conformance 不应为了制造表面一致而反向改掉它们：

- **候选排序**：Rust 保留 `admission → cost → provider id` 的确定性排序；TS
  参考实现仍取注册顺序第一张。B1 task08 用双候选负对照锁定这条差异。
- **Supervisor 冲突**：同一 Supervisor 内，完全相同的并发 load 会
  singleflight；不同 model、不同 policy 或不同 `memory_gb` 的冲突请求立即
  `supervisor_busy`，不实现 TS `evict-lock` 工具类设想的等待队列。Busy 只在
  单个 runtime Supervisor 内生效，不跨独立 backend。
- **取消**：Rust 请求 future 被丢弃时会取消传给 adapter 的 token，并释放预算
  reservation；这条由 `dropping_the_future_mid_chat_releases_the_reservation_and_cancels_the_token`
  承重，不能为了追平 TS 旧的取消传播缺口而移除。

task38 只锁定这些已经存在的行为，不新增等待队列或新的驱逐算法；若未来产品
决定需要排队/超时等待，应另开独立实现 PR，并重新定义 Busy 契约。

想知道 TS 参考实现和 Rust 版在同一套用例上的差异，分别跑一遍，diff 两次的
输出（或者更直接地看各自失败在哪些用例上）：

```bash
# TS 参考实现（套件缺省值，不用设 IDORIS_CONFORMANCE_CMD）
pnpm conformance | tee /tmp/conformance-ts.log

# Rust 版
bash scripts/conformance-rust.sh | tee /tmp/conformance-rust.log
```

现阶段两份日志应该完全一致：47 条通过，7 条 todo（同一份「已知规范落差」清单）。
以后任何一侧出现另一侧没有的失败用例，就是一处行为分叉，需要先判断哪一侧偏离了规范。

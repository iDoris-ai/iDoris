# 用 conformance 套件测 Rust 版 `idoris`

`conformance/`（R0，`origin/feat/r0-conformance`，PR #49，本文写作时还没合并进
`main`）是一套语言无关的黑盒 HTTP 契约测试：它只通过 HTTP 访问被测服务，不
`import` 任何 `@idoris/*` 代码，被测服务是 TS 参考实现（`packages/router`）
还是 Rust 版，套件本身完全不关心——接线方式见套件自己的 `conformance/README.md`
「接入新实现」一节。本文档只覆盖 Rust 这一侧：现在跑会看到什么、以后怎么跑
TS/Rust 两版对照。

## 现状：大部分用例会失败，这是预期的

Rust 侧目前只有 `idoris-router` 这一个 R1 骨架（`crates/idoris-router/src/lib.rs`）：

- 只实现 `GET /health`（服务身份字段、`X-iDoris-Record-Id`）；
- 没有接 `idoris-policy`（隐私/角色/预算/准入决策管道，R2-B 已经在做纯逻辑，
  但还没接进 router）；
- 没有接 `idoris-tenancy`（预算账本、`deploy_mode=tenant` 校验）；
- 没有读取 `IDORIS_COMPONENTS_DIR` / `IDORIS_ROUTING_POLICY`；
- 除 `/health` 外的一切路由（`/v1/models`、`/v1/chat/completions`……）一律
  回 `501`。

所以拿 conformance 套件测这个二进制，除了 `/health` 相关的用例之外，**其余
绝大多数用例都会失败（或直接因为 501 不匹配预期状态码而报错）**——套件测的
是规范 v1.0.1 的完整行为面，Rust 侧要等 R2-D 把 `idoris-policy`/
`idoris-tenancy`/组件卡加载/路由决策都接进 `idoris-router` 之后，覆盖率才会
追上 TS 参考实现。在那之前看到一屏红是正常的，不代表 Rust 骨架本身有问题，
也不代表套件写错了——两边都没错，只是 Rust 还没实现那些行为。

这也是为什么本仓库**暂时不**把 `scripts/conformance-rust.sh` 接进 Rust 的
CI 必跑项（`.github/workflows/ci.yml` 的 `rust` job）：接一个已知会大面积
失败的门禁，只会训练大家忽略红色 CI，起不到门禁的作用。等 R2-D 把行为接线
完成、Rust 侧的通过率追上 TS 参考实现之后，再把它接进 CI 作为一个真正会拦
东西的必跑项。

## 怎么跑 Rust 版

前提：`conformance/` 目录（以及根 `package.json` 里的 `pnpm conformance`
脚本）来自 `feat/r0-conformance`，需要先合并/cherry-pick 到你正在跑的分支，
否则下面的脚本第一步就会报错退出并提示你去合并那条分支。

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

想知道 TS 参考实现和 Rust 版在同一套用例上的差异，分别跑一遍，diff 两次的
输出（或者更直接地看各自失败在哪些用例上）：

```bash
# TS 参考实现（套件缺省值，不用设 IDORIS_CONFORMANCE_CMD）
pnpm conformance | tee /tmp/conformance-ts.log

# Rust 版
bash scripts/conformance-rust.sh | tee /tmp/conformance-rust.log
```

现阶段预期看到的差异：TS 侧只有 `conformance/tests/known-spec-conflicts.test.ts`
里记录的那些已知规范落差（`it.todo`，见套件自己的 README）会不通过；Rust 侧
除了 `/health` 相关用例之外基本全灭——两份日志的失败用例集合本身就是「Rust
距离追平 TS 参考实现还差多少」的进度表，R2-D 每接进一块行为，重新跑一次这两
条命令，Rust 侧失败用例集合应该单调缩小，直到理论上能跟 TS 侧收敛到同一份
「已知规范落差」清单。

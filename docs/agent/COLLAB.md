# 双机并行开发协作约定

> 自 2026-10-01 起生效。两台机器各自用自己的 Claude Code / Codex 并行开发，靠**功能分支隔离 + PR 协作**来避免互相干扰。
> 本文是常设规则；某一次交接的快照见 `HANDOFF-*.md`。

## 1. 两台机器的分工

| | **工作站 A：MacBook Pro M1 Max 64GB** | **工作站 B：Mac mini 16GB** |
|---|---|---|
| 负责 | **本地模型相关**：oMLX/MLX 推理、模型加载/驻留/淘汰、量化与内存评估、本地模型评测、`idoris-backend` Supervisor 和 `idoris-upstream` oMLX 适配器的实机验证 | **不需要大内存的业务开发**：路由/策略/预算/租户、接口与契约、TS 参考实现、文档、CI/发版、Agent24 对接 |
| 长期集成分支 | `feat/local-model` | `feat/<业务名>`（每个业务功能开一条，例如 `feat/remote-upstream`） |
| 评审主力（Tier 1） | 优先请 **B 的 Codex** 评审（A 的 Codex 额度 2026-10-04 才恢复） | 本机 Codex；也可以请 A 侧交叉评审 |
| `tasks.md` FU 编号段 | **FU-100 ~ FU-199** | **FU-200 ~ FU-299** |

判断某个任务放在哪台机器上做：**需要真的把模型加载进内存跑（7B 以上、多模型并存、内存压力测试）→ A；其他 → B。** 拿不准就放 B，需要实机验证时再给 A 的分支提 PR，或者请 A 跑一遍。

## 2. 分支模型（两边一样）

```
main ──────────────────────────────────────────────●── release PR 合入后打 tag
  │                                                ↑
  └─ feat/<功能>（长期集成分支）──●──●──●──●──────────┘  release PR（feat/<功能> → main）
        ↑        ↑        ↑
     task PR  task PR  task PR      （task 分支 → feat/<功能>）
        │
   feat/<功能>-<序号>-<任务>（各自一个 git worktree）
```

1. **开功能**：从最新的 `main` 拉出 `feat/<功能>`，推到远端。
2. **做任务**：每个任务从 `feat/<功能>` 拉一个 `git worktree` + 任务分支 `feat/<功能>-NN-<短名>`，PR **目标是 `feat/<功能>`，不是 `main`**。单个 PR 不超过 300 行，需单独评审通过才能合并。
3. **发版**：功能完成、门禁全绿后，开一个 **release PR：`feat/<功能>` → `main`**，PR 正文列出包含的 task PR 和验收数据。批准后合并，需要发版的就在 `main` 上打 tag（`vX.Y.Z`，release workflow 自动构建）。
4. **release PR 要小而勤**：功能完成一个可交付的切片就合进 main，**不要攒到上百个子 PR 再合**（#48 一次汇总了约 115 个子 PR，评审只能做整体复核）。
5. **合并方式**：统一用 merge commit（`gh pr merge --merge`），保留 task PR 的合并历史。

## 3. 双方怎么协作而不互相干扰

- **各改各的分支**：A 不直接 push B 的分支，反之亦然。
- **需要对方改东西**：给对方的 `feat/<功能>` 分支提 PR（例如 B 在 A 的 `feat/local-model` 上提一个接口调整），由分支所有者评审合并。
- **需要对方的成果**：等对方的 release PR 进了 `main`，再把 `main` 同步进自己的功能分支；不要直接 merge 对方未发布的功能分支。急用时可以 cherry-pick，并在 PR 正文说明。
- **共享热点文件**（容易冲突，改之前先同步 main）：
  - `docs/agent/tasks.md`：只用自己的 FU 号段（见 §1）；新增条目追加在表格末尾。
  - `Cargo.toml` / `Cargo.lock` / `pnpm-lock.yaml`：加依赖单独开一个 task PR，尽快合进 main。
  - `.github/workflows/*.yml`：归 B 维护；A 需要改就给 B 提 PR。
  - `crates/idoris-backend/src/adapter.rs`（`RuntimeAdapter` trait）、`crates/idoris-backend/src/error.rs`（`BackendError`）：两边都依赖，改了要在 PR 里 @ 对方。

## 4. 保持同步的节奏

- **每天开工先同步**：`git fetch && git merge origin/main` 进自己的功能分支（对方有 release PR 合进 main 时立刻同步）。
- **推送会让批准失效**：PR 获批后再推新提交（包括同步 main），会清掉已有的批准，需要重新请求。所以**先同步 main、跑完门禁，再请求最终批准**。
- **状态公示**：每个 release PR 合并后，在 `docs/agent/progress.md` 记一笔（谁、哪个功能、哪个 tag），对方同步 main 就能看到。

## 5. 评审与合并

- 触发 prdaemon 评审：在 PR 下留言 `@clestons 请审阅` / `@clestons 请复审`，附上修了什么、怎么验证的。
- 评审分档沿用全局约定：Tier 1 = Codex；Codex 不可用时 Tier 2 = 本地模型 + prdaemon，并在 PR 里注明是哪一档。**不使用 Copilot。**
- **批准 + CI 全绿 → 直接合并**；release PR 合并后需要发版就直接打 tag。已合并、内容已在远端的分支和 worktree 直接清理。
- 门禁（本地先跑一遍再推）：
  - Rust：`cargo fmt --check`、`cargo clippy --all-targets --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks -- -D warnings`、同 features 的 `cargo test`、`cargo deny check advisories bans licenses sources`
  - TS：`pnpm lint && pnpm typecheck && pnpm check:contract-drift && pnpm build && pnpm test && pnpm conformance`
  - 改到路由行为时，加跑 `bash scripts/conformance-rust.sh`（当前基线 47 passed / 7 todo）

## 6. 机器本地、不进 git 的东西

- 根目录 `CLAUDE.md` 被 gitignore，各机器自己维护（它用绝对路径 `@` 引入 `../Brood` 仓库的 4 个文件）。
- 模型权重、oMLX 配置、`IDORIS_OMLX_API_KEY` 等凭证只放在 A 本机。B 上需要跑 oMLX 相关测试时，用 wiremock 单测，不连真实实例（`IDORIS_OMLX_IT=1` 的集成测试只在 A 上跑）。
- `pnpm` 统一，不用 npm。

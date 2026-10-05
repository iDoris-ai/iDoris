# iDoris Prime Checkpoint

> Rolling execution checkpoint for the Prime Agent. Update this file after key task/PR/milestone transitions.
> This file is a recovery anchor, not the sole source of truth. On resume, always reconcile it with live Git/GitHub state before acting.

## Resume protocol

If the chat disconnects, compacts, refreshes, or moves to a new conversation:

1. Read this file first.
2. Read `docs/agent/HANDOFF-2026-10-03.md` for historical context and constraints.
3. Fetch live refs and inspect GitHub before changing code:
   - `git fetch origin`
   - `git rev-parse origin/main origin/feat/rust-parity`
   - `gh pr list --state open`
   - `git worktree list`
4. Treat live Git/GitHub as authoritative when it is newer than this checkpoint.
5. Continue the Prime execution loop from the first unfinished item below. Do not restart old completed tasks merely because an older handoff says they were unfinished.
6. PR loop:
   - APPROVED + CI green + mergeable => merge.
   - REQUEST_CHANGES => fix immediately, validate, push, request re-review.
   - CI failure => diagnose; fix real regressions, rerun genuine flakes only with evidence.
   - Review pending => continue independent work and come back later.
7. Update this checkpoint again after significant progress.

## Long-running Prime contract

Target: continue autonomously from M4 through M8 unless a real product/architecture decision requires user input.

Prime owns architecture, task decomposition, acceptance, challenge review, TPR handling, merge decisions, and milestone tracking. Prefer cheaper workers for implementation when available; Prime continues useful work while workers/reviews/CI run.

Production-code budget per PR: target <=300 changed production lines; exceptional <=500 with justification; >500 must split. Tests, fixtures, design docs and plan docs are excluded from this production-code budget.

Core product invariants:

- iDoris is a model-management and decision kernel, not merely an API proxy.
- Decision layering: privacy -> intent/capability -> budget -> admission/capacity -> model selection/fallback -> model lifecycle -> audit/feedback.
- Security-sensitive boundaries default fail-closed.
- macOS / Apple Silicon / oMLX is the current priority platform.
- Core policy must stay cross-platform; Windows/Linux runtime/process/hardware differences belong behind backend/runtime adapters.
- Router/runtime/fallback/health/capacity designs should actively learn from mature open-source model routers/runtimes while preserving iDoris invariants.
- Do not regress Rust safety improvements merely to mimic TS behavior; approved differences must be explicit and tested.

## Verified snapshot — 2026-10-04 evening

### Milestone position

- Overall roadmap: M1-M8.
- M1-M3: complete.
- Current milestone: **M4**.
- TS is now reference/PoC only; new product behavior is maintained in Rust.
- M4 top-level workstreams are B1-B9 + A1-A6.
- Rust convergence status:
  - **B1 Rust parity: 38/39 merged; task33 implemented as PR #298.**
  - **B2 recommender-rs: 21/21 complete; release PR #294 merged into main.**
  - **B3 subscription relay: tasks 01-10 + 13 merged; task11/12/14 open; follow-up drain fix #299 open.**
- Critical path: close B1 #298 -> stabilize/sync B3 onto current main -> finish B3 -> R6/v0.2.0 -> B4-B9/A lanes -> M4 close.

### Live refs at checkpoint write

- `origin/main = def5b3d3ffe01185d57afe2f6371c11b849846dd`
  - includes B2 Rust recommender release #294.
- `origin/feat/rust-parity = ad67fe618a6d3390f3c7754b55e1132421974cae`
  - includes B2/main sync #297; B1 task33 is PR #298 on top.
- `origin/feat/recommender-rs = 1f61d06073673d3a2d76e9b5aacfc3a010faf594`
  - B2 task21 complete; feature is closed 21/21.
- `origin/feat/subscription-relay = ebf0d2d608a245cc31297e817a642767ff95b0d6`
  - includes tasks 01-10 +13 and fixture stabilization #296.

### Open PRs at checkpoint write

- **#292** task11 secure output file, latest head `c161a8b`; CI green; exact-head re-review requested after base stabilization.
- **#293** task12 stateless relay API, latest head `4f36371`; CI green; exact-head re-review requested.
- **#295** task14 graceful shutdown, latest head `edb240b`; CI green; exact-head re-review requested.
- **#298** B1 task33 live capacities, exact head `4a0f376`; CI/TPR running. Production Rust source delta ~262 lines; full idoris-router test suite green locally.
- **#299** subscription terminal/drain reason fix, exact head `4343430`; CI/TPR running.

Always refresh these PRs before acting.

### Important integration state

- B3 is **224 main commits behind** at this snapshot. Do not continue Router-facing B3 task15+ on the stale long branch.
- A dry-run integration branch/worktree exists:
  - branch `integration/b3-main-sync`
  - worktree `/Users/jason/Dev/auraai/iDoris-b3-main-sync`
  - merge commit `aa5cb05` from #295 head + current main
  - only textual merge conflict was `crates/idoris-router/src/lib.rs`; resolution keeps both B3 `subscription` module and main's sse/supervisor/write-timeout modules.
- Router tests pass on the integration merge.
- Upstream full-suite exposed a real terminal/drain reporting race: Cancelled could become CleanupFailed when bounded stdout/stderr drain scheduling exceeded the test grace. This is fixed independently in PR #299; after #299 lands, update the B3 stack/main-sync before declaring integration green.

### Immediate next execution order

1. Merge #299 when APPROVED + CI green; sync the fix through #292 -> #293 -> #295 and the B3 main-sync integration branch.
2. Merge #292, then safely repoint/merge #293, then safely repoint/merge #295; never delete a stacked base before repointing dependents.
3. Merge #298 when APPROVED + CI green. That closes B1 39/39.
4. Merge current main into `feat/subscription-relay` using the already-proven one-conflict integration recipe; run Router + Upstream full suites/clippy/fmt/diff-check.
5. Only after that sync, implement B3 task15 runtime-handle, then task16 dispatch, 17-19 HTTP lifetime/result chain, 20-28 enable/conformance/release evidence.
6. Close R6/v0.2.0, then execute M4 B4 -> B5/B6 -> B7/B8 -> B9 and A1-A6 acceptance.
7. Continue M5 -> M6 -> M7 -> M8 under the same plan/implement/accept/challenge-review/TPR/merge loop.

## Known review lessons that must not be forgotten

- Task15: arbitrary concrete model IDs must be catalog-validated before Supervisor `load()`; otherwise unknown IDs can trigger eviction of a warm model before backend rejection.
- Task18: trusted tenant YAML is a complete active-tenant set. Removing a tenant must revoke its active budget config while preserving spend/history; stale config must not retain spending authority.
- Startup gate precedence matters: existing component/policy/runtime hard gates (including subscription/K04 rejection) must not be masked by later storage/bootstrap errors.
- JS parity length limits must consider UTF-16 code units where the TS contract uses `string.length` (important for emoji boundaries).
- Config-root behavior is already release-established: bundled defaults are executable-relative; explicit env paths remain explicit. Do not regress to cwd-dependent defaults.
- Stacked PR/base-branch deletion caused lost/auto-closed PRs in the past. Before merging/deleting a base branch, identify dependent PRs and repoint/safely integrate them first.

## Working-tree caution

The main worktree also had an unrelated untracked file:

- `docs/research/RESEARCH-MIGRATION-NOTICE.md`

Do not overwrite/delete/commit it as part of unrelated Prime work unless explicitly handling that research migration task.

## Checkpoint maintenance rule

Update this file whenever any of the following happens:

- a key PR is created or merged;
- a task changes from local-only to published/merged;
- an architectural blocker/fix changes the next-step order;
- a milestone/sub-milestone closes;
- before intentionally ending a long Prime work session.

When updating, preserve useful historical review lessons but replace stale SHA/PR/task-status facts with current live truth.

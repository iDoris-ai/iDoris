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

## Verified snapshot — 2026-10-04

### Milestone position

- Overall roadmap: M1-M8.
- M1-M3: complete.
- Current milestone: **M4**.
- Current main lane: **B1 Rust parity**, now in late convergence.
- B2 recommender and B3 subscription relay have both started in parallel.
- Critical path toward v0.2.0/R6 remains: finish required B1 work + B2 + B3 + release/conformance evidence.

### Live refs at checkpoint write

- `origin/main = b3fc03072fc676331600840188513fc3ea040304`
  - includes B1 release tasks 01-07 on main.
- `origin/feat/rust-parity = f20fa3d30a5929f97141d9e57d89fa70f32c2508`
  - latest merge at checkpoint: PR #252 task27, after PR #250 task20.

### B1 tasks already merged into `feat/rust-parity` / released where applicable

Known completed/merged B1 work includes:

- 01-07
- 08
- 09
- 10
- 11
- 12
- 13
- 14
- 15
- 16
- 17
- 18
- 19
- 20
- 23
- 24
- 25
- 26
- 27
- 30
- 31
- 32
- 34
- 35
- 36
- 38
- 39

Important recent PRs already merged:

- #237 task15 concrete model dispatch — fixed review blocker by catalog-preflighting concrete model IDs before Supervisor load/eviction.
- #238 task34 CLI.
- #239 task17 scoped store.
- #240 task08 selection contract.
- #241 task31 capacity fixtures.
- #242 task39 contract/version drift.
- #243 task32 capabilities surface.
- #244 task18 persistent tenancy storage — later review added stale-tenant revocation (`retain_tenants`) and preserved startup gate precedence.
- #245 task19 audit validation.
- #246 task24 billing aggregation.
- #247 task35 config-root abstraction.
- #248 task36 startup-egress assertion.
- #249 task38 eviction contract.
- #250 task20 buffered audit — merged into `feat/rust-parity`.
- #251 task26 usage query — merged into `feat/rust-parity`.
- #252 task27 budget query — merged into `feat/rust-parity`.

### Open PRs at checkpoint write

Only two GitHub PRs were open immediately after merging #250/#252:

- #253 `chore(recommender): add B2 Rust dependencies`
  - head `9e0e36ecea4b2f38b00534fed80976ab97390423`
  - base `feat/recommender-rs`
  - CI green at last check; review pending.
- #254 `chore(subscription): add safe process-group dependencies`
  - head `646e1b935e791834ead64cdec2671a26e1f7fdc5`
  - base `feat/subscription-relay`
  - CI green at last check; review pending.

Always re-check before acting: either PR may already be approved/merged after this checkpoint.

### Local B1 worktrees with unpublished / follow-up work

The following local worktrees existed at checkpoint write and are important recovery anchors:

- `iDoris-b1-21` — `feat/rust-parity-21-audit-streaming` @ `ee01ec7`
- `iDoris-b1-22` — `feat/rust-parity-22-usage-write` @ `8101518`
- `iDoris-b1-28` — `feat/rust-parity-28-audit-http` @ `36ac561`
- `iDoris-b1-29` — `feat/rust-parity-29-query-acceptance` @ `ffb0cce`

Before opening any of these as PRs, rebase is NOT allowed by project convention; instead inspect ancestry/current base, merge/sync safely if needed, validate exact diff, production-line budget, tests, fmt/clippy/diff-check, then Prime challenge-review before push/PR.

### B2/B3 local worktrees

- B2:
  - `iDoris-b2-01` — `feat/recommender-rs-01-deps` @ `9e0e36e` (PR #253)
  - `iDoris-b2-02` — `feat/recommender-rs-02-facts-vectors` @ `02de838`
- B3:
  - `iDoris-b3-01` — `feat/subscription-relay-01-deps` @ `646e1b9` (PR #254)
  - `iDoris-b3-02` — `feat/subscription-relay-02-gate-policy` @ `f5e1547`

### Immediate next execution order

1. Reconcile PR #253/#254 status; merge immediately if APPROVED + CI green + mergeable. Fix immediately if review requests changes.
2. Validate and publish B1 task21, then task22, preserving dependency order.
3. Validate and publish B1 task28, then task29, preserving dependency order after task27.
4. Re-evaluate remaining B1 tasks against `docs/agent/plans/B1-rust-parity.md`; do not rely on old completion counts from the 2026-10-03 handoff.
5. Continue B2/B3 in parallel while B1 reviews run.
6. When B1 release requirements are satisfied, prepare the next main release slice and then converge B1+B2+B3 toward R6/v0.2.0.
7. Continue M4 remaining B4-B9 as specified by the current roadmap, then M5 -> M6 -> M7 -> M8 using the same plan/implement/accept/review/TPR/merge loop.

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

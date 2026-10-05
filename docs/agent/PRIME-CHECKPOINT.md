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

## Verified snapshot — 2026-10-05 afternoon

### Milestone position

- Overall roadmap: M1-M8.
- M1-M3: complete.
- Current milestone: **M4**.
- TS is reference/PoC only; new product behavior is maintained in Rust.
- M4 top-level workstreams are B1-B9 + A1-A6.
- Rust convergence status:
  - **B1 Rust parity: 39/39 complete; release PR #303 merged to main.**
  - **B2 recommender-rs: 21/21 complete; release PR #294 merged to main.**
  - **B3 subscription relay: tasks 01-25 merged; task26-28 are the only remaining B3 closure work.**
- Critical path: finish B3 tail -> B3 release to main -> R6/default Rust + v0.2.0 -> B4-B9/A lanes -> M4 close -> M5-M8.

### Live refs at checkpoint write

- `origin/main = 0017c2ba14e37d7e904b7eb8784303477d77920a`
  - includes B1 final Rust parity release #303 and B2 release #294.
- `origin/feat/subscription-relay = eb22298a41c2a481cbb4cd307b096e3127219f4b`
  - includes B3 tasks 01-25 and merged EPERM cleanup fix #315.

### B3 tail PRs / exact recovery order

- **#317** `test(subscription): stabilize reaper cancellation fixture`
  - head `c398cc18c02a464cd53c3eb797114c5daa04cad0`; base `feat/subscription-relay`.
  - CI green; external reviewer `clestons` requested; TPR verdict pending at this snapshot.
  - fixes the second macOS test race: install TERM trap before readiness marker, treat only ESRCH as group gone.
  - local evidence: targeted cancellation 20/20 consecutive passes; full reaper/cancel suites, clippy/fmt/diff-check green.
- **#314** `ci(subscription): require B3 Unix safety matrix`
  - remote approved old head `178e42b...`; local repaired head is **`615ccabf29fdbe8136b36a849e3f4fb835e90903`** in `/Users/jason/Dev/auraai/iDoris-b3-26`.
  - local head already contains #315 + #317 and the task26 Unix matrix. Rust portion of `scripts/check-subscription.sh` is green; local harness lacks Node runtime so pnpm cannot run locally.
  - do not push this repaired head before #317 is approved/merged.
- **#316** `test(subscription): add B3 real CLI smoke`
  - approved old head `3f0fa7279b364dd62d1f23a1f6eb84bcb3c0553d`; base is #314 branch.
  - real Mac smoke recorded PASS against Codex CLI 0.156.1 + Claude Code 2.1.289. Do not re-run real account smoke automatically.
  - local combined/repaired head is **`dd801c0d20c3ab57c7b4c379f27742ca70242afe`** in `/Users/jason/Dev/auraai/iDoris-b3-27`; default no-op smoke/clippy/fmt/diff-check green.
- **#318** `test(subscription): harden real CLI smoke assertions`
  - head `9d1dd1c64dae509876fa4f9eb2703f9b1480a5cc`; stacked on #316.
  - exact-token CLI flag matching, generic fixed-reply failure text, and acceptance wording narrowed to actual evidence; production delta 0.
  - one macOS run failed on the old base in `subscription_disconnect`; another passed. Repaired task26 base runs the concurrent disconnect case 20/20 locally, so do not duplicate the reaper fix in #318.
- **task28 release evidence**
  - worktree `/Users/jason/Dev/auraai/iDoris-b3-28-current`; local final head **`fdeda7013de4869625ca9b52ebb16dad0770b6b3`**.
  - docs are already synchronized with repaired task27 and honest tested-vs-not-tested wording. Do not open the task28 PR until #314/#316/#317/#318 close; then refresh final PR numbers/status and publish.

### Safe stacked merge sequence

1. Merge #317 only after APPROVED + CI green.
2. Push local task26 repaired head to #314 and require fresh Linux/macOS subscription jobs green; re-review exact head if approval is dismissed.
3. Before merging #314, repoint #316 to `feat/subscription-relay` so source-branch deletion cannot auto-close it.
4. Merge #314 after repaired exact-head gates pass.
5. Validate/re-run #316 on the repaired base; merge when APPROVED + green. Before deleting its source branch, repoint #318 to `feat/subscription-relay`.
6. Merge #318 when its final-base CI/TPR are green.
7. Refresh task28 evidence, open/TPR/merge task28. B3 is then 28/28.

### Local final-preview evidence

- B3 release preview combines #315 + #317 + repaired task26 + task27 hardening + task28 and is conflict-free.
- Full Rust subscription matrix is green locally.
- `idoris-upstream`: 110/110.
- `idoris-router --lib`: 251/251.
- clippy/fmt/diff-check green.
- local `scripts/check-subscription.sh` reaches pnpm only after all Rust subscription tests pass; local harness has **no Node runtime**, so pnpm/conformance is not locally claimed as passed. GitHub CI is authoritative for Node/cross-platform gates.

### R6/v0.2.0 preview evidence

- Worktree `/Users/jason/Dev/auraai/iDoris-r6-preview-current`; current local head **`a941693c37de3f7588a90af1e9a6b326e71a9e0b`** (main + repaired B3 tail preview).
- `cargo test --workspace --locked`: pass.
- `cargo deny check advisories bans licenses sources`: pass (existing duplicate/yanked warnings remain warnings).
- `cargo build --release --locked -p idoris-router`: pass.
- Release archive contains exactly `idoris` + three runtime config files; `scripts/release-smoke.py` passes from unrelated cwd with clean IDORIS environment.
- README already states Rust is production and TS is reference; CI rust job already runs Rust release conformance. R6 should not delete TS tooling because shared conformance/reference tests still use Node.
- Crates remain `0.1.3`; after B3 release/R6 closure, create a dedicated version PR bumping the seven Rust crates + Cargo.lock to `0.2.0`, update progress, then tag `v0.2.0` only after release workflow gates pass. Contract version `1.0.1` is independent and should not be bumped just for crate release version.

### Immediate next execution order

1. Close #317 -> repaired #314 -> #316 -> #318 -> task28 using the safe stacked order above.
2. B3 release integration PR to current main; run full CI/shared conformance/FU-26 Tier-1 review.
3. R6 default-Rust closure + v0.2.0 version/tag/release smoke.
4. Begin M4 B4 direct-proxy budget reserve/settle, then B5/B6, B7/B8, B9 with A-lane acceptance interleaved.
5. Continue M5 -> M6 -> M7 -> M8 with the same Prime loop.

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

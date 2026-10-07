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

## Live delta — 2026-10-07 morning

Live Git/GitHub has moved well beyond the 2026-10-06 snapshot below. Treat this
section as the first recovery anchor, then reconcile again with `git fetch origin`
and `gh pr list --state open`.

- `origin/main = fed7571585f8fbd15e97c083db07ba730555d71b`.
- B8 process-owner **#376 merged** at exact head
  `6bf7ae9d336943fcfa3c9b072722aa8042a10447`; merge commit
  `2267a5fa78ceb2ced4187c960ddb83370687d906`.
- B8 HTTP runtime **#377** was retargeted to `main` after #376 landed; exact
  head remains `002f0adbc6a1a9f67b7d5fc77f4ffc1d52102585`. Its repaired cancellation
  and MLX model-identity behavior is locally green; current-base review is pending.
- B8 global-capacity **#382** is at repaired exact head
  `31741528a2d183d921a59b65464584e54f054beb`, APPROVED/CLEAN on its stacked
  base. Internal accounting is deterministic fixed-point while the public GB API
  remains `f64`; production delta is <=300.
- B7 rerank **#370** exact head
  `28abf4d33d89b8908057667fe429fa59efd3c961` passed a fresh read-only
  challenge review for pre-egress SpendGate admission plus audit/UsageFact, but
  GitHub still requires a current-base approval.
- B9 Admin v0 has advanced through **#383 status**, **#384 session token**, and
  **#385 loopback bind** merged to main; published **#386 backends snapshot**
  and **#387 models snapshot** remain stacked. #387 exact head is
  `e90347c51c52d5f59fd6d0ee67e0cf4c841285bf`; its fail-closed Admin parsing
  and data-plane compatibility received a fresh read-only FINAL PASS before
  publication.
- Do **not** invent an Admin session-token HTTP header/wire format. The docs
  specify a loopback management port and session-token concept, but not the
  transport spelling. Continue transport-free/read-only slices or other lanes
  until that public-contract boundary is explicit.
- B5 #346, B6 #335, B7 #370, B8 #377, and B9 #386 are current root landing
  gates waiting on current-base review. Keep advancing independent work rather
  than waiting.

## Verified snapshot — 2026-10-06 morning

### Milestone position

- Overall roadmap: M1-M8.
- M1-M3: complete.
- Current milestone: **M4**.
- TS is reference/PoC only; new product behavior is maintained in Rust.
- **B1 Rust parity: complete.**
- **B2 recommender-rs: complete.**
- **B3 subscription relay: complete.**
- **R6/default Rust: complete; v0.2.0 released.**
- Current frontier: **B4/B5 closeout + B6 Event Log**, then B7/B8 -> B9 -> M5-M8.

### Live refs at checkpoint write

- `origin/main = 6678649d0568961281afe3e6cdcf05893e8b36aa`.
- Main worktree is intentionally not the development base; it is stale/diverged and contains the user's unrelated untracked `docs/research/RESEARCH-MIGRATION-NOTICE.md`. Do not reset/clean it.
- Local Node runtime is absent. Do not claim local JS conformance; GitHub CI is the authoritative Node/cross-platform gate.

### Open PR frontier

- **#326** B4 paid Resident startup gate
  - head `15ce77c8c625445a401f155b48add0e45c8d9c65`, base `main`.
  - CI green; current-head approval still required.
- **#328** B5 virtual-key SQLite scopes
  - head `43c3e436cf47b0cad01634818b88006d5aed6e7a`, base `main`.
  - CI green; current-head approval still required.
- **#334** B5 chat virtual-key enforcement
  - head `d90ce7526b1acef298930e5612678bd5686ce1f4`, base #328 branch.
  - Current-head CI green/CLEAN.
  - Previous review blocker fixed twice: no shared hard-coded bearer; each Rust conformance server now owns a fresh DB + freshly minted one-time secret; Authorization is origin-scoped.
  - Local JS conformance not run because Node is absent.
- **#333** B6 Event Log core
  - head `964f64db7979381decc2d4f74522ad753f6a473c`, base `main`.
  - Production diff = **358 changed lines** (301-500 security-hardening exception; double review required).
  - Static sqlite_master substring validation and predictable behavioral sentinels were both found spoofable during Prime/worker challenge review.
  - Current design: generate canonical schema in an in-memory SQLite DB from the same migration + migration-table DDL, require exact schema object equality, then run a randomized savepoint behavior probe as defense-in-depth.
  - Full idoris-tenancy 122/122 plus integration groups, fmt/clippy/diff-check green locally; remote current-head CI/review pending at checkpoint write.
- **#335** B6 EventLogStore router bootstrap
  - head `b52dc82c11bc402af70378b72abf1342f29e661e`, base #333 branch.
- **#336** B6 correlation context
  - head `7d0ccc4a609f9e82004ea0c87ade993baf12d970`, base #335 branch.
- **#337** B6 request.received
  - head `30be822234adc436008587ec3733b5647e1e3abd`, base #336 branch.
  - CI green after evidence-backed Ubuntu rerun.
- **#339** B6 profiled event
  - head `77ea47c08362c4a6ceeef63f3c4fe412a223f09d`, base #337 branch.
  - Production diff = 80 changed lines.
  - Emits one `profiled` event after local intent/profile resolution and before routing/budget/upstream; metadata only `intent` + `privacy`; deliberately does not synthesize future M5 `inspected`.
- **#338** subscription-startup readiness test hardening
  - head `6a1a061f9e6d163be81e843ee0b39e3beb6735c8`, base `main`.
  - The target startup-reset regression itself is green, but duplicate push CI exposed unrelated pre-existing flakes. Do not blindly rerun.
- **#340** load-fence lifetime fix
  - head `73fdeec27a77782b72aaae99026cf2d51443954f`, base `main`.
  - Fixes a real Linux `flock` fork/dup lifetime race by explicitly unlocking when the owning `LoadFence` drops; production +11, test +24.
  - Ownership-focused local repeat 100/100 + full idoris-backend/fmt/clippy/diff-check green; CI/review pending.

### B6 sequencing and invariants

Implemented/published sequence:

1. Event Log core (#333)
2. router storage bootstrap (#335)
3. validated correlation context (#336)
4. `request.received` (#337)
5. `profiled` (#339)

Next B6 work must preserve these invariants:

- Event Log is the truth source and append failures fail closed.
- Synchronous SQLite on async request paths goes through `spawn_blocking`.
- tenant scope comes from trusted parsed/internal authority; caller metadata cannot select another tenant.
- `record_id` is server authority.
- no prompt/messages/content/body/corrected_output in Event Log metadata.
- M5 owns real `inspected` privacy scanning; M4 must not synthesize a fake `inspected` event.
- Before persisting `decided`, preserve the complete routing decision fact. Current `policy_cards()` discards matched-rule metadata and the Supervisor path calls pure `decide()` twice; do not record a partial or duplicated decision.
- `/v1/feedback` remains required for B6, but `corrected_output` cannot be smuggled through metadata; content storage needs an explicit boundary.
- M4 acceptance still requires record-id lookup of the decision chain and audit projection from Event Log.

### Safe stacked merge procedure

B5 stack:

1. #328 -> main.
2. Before merging/deleting #328 source branch, repoint #334 base to main.
3. Merge #334 only after its current head is APPROVED + green + mergeable.

B6 stack:

1. #333 -> main.
2. Before merging/deleting #333 source branch, repoint #335 base to main.
3. Before each subsequent parent branch is deleted, repoint the next child to main:
   #335 -> #336 -> #337 -> #339 (and later B6 children).
4. No rebase, no force-push. Temporary base retargeting may widen a child diff until its parent lands; that is expected.
5. Never treat a dismissed/stale approval as current-head approval.

### CI flake state

- #337's earlier Ubuntu `subscription_startup` ECONNRESET was a readiness-probe panic. #338 makes connect/write/read reset a retryable readiness miss instead of an unwrap panic.
- #338's later push-run rerun failed in `load_fence::ownership_is_exclusive_even_after_marker_clear`; #340 addresses the underlying fork/dup lock lifetime race.
- The original #338 push run also ended one `Conformance bad-command negative control` step with exit 143. That is under separate investigation; do not conflate it with load-fence and do not rerun repeatedly without evidence.

### Immediate next execution order

1. Finish current-head CI + TPR on #333/#334/#339/#340 and older #326/#328/#335-#338.
2. Merge only when APPROVED + all required checks green + mergeable; retarget stacked child bases before deleting parent branches.
3. After #340 lands, sync #338 to the new main with a normal merge (no rebase) so it gets a fresh SHA and clean CI.
4. Continue B6 with a small decision-fact seam, then `decided`, record-id query/audit projection, completion/budget events as needed, and `/v1/feedback`.
5. Close B5/B6, then B7/B8, then B9 to finish M4.
6. Continue M5 -> M6 -> M7 -> M8 under the same Prime loop.

## Latest override snapshot — 2026-10-06 09:30 GMT+7

This section is newer than the earlier morning snapshot above and overrides stale PR/base facts there. Live Git/GitHub still wins over both.

### B5 landing stack

- Old aggregate PRs **#328 and #334 are CLOSED as superseded**, not merged. Superseded #343 is also closed.
- The size-safe main landing stack is now:
  1. **#342** `5fff5c9330725fbd66f214dd732c7cfdb729d8d8` -> `main` — virtual-key SQLite store; original production slice ~220 lines.
  2. **#346** `d67e339c222ff268e6f9fd7b4f14d2a5814eba3b` -> #342 — authenticator; 198 total additions, production <100.
  3. **#347** `5a5d4d63fe10afcded5218d1607b302584abf47c` -> #346 — scope ceilings + store bootstrap; 166 file-level additions.
  4. **#344** `d90ce7526b1acef298930e5612678bd5686ce1f4` -> #347 — chat wiring; production 142 changed lines.
- All four reuse the original exact commit lineage; no rebase/force/rewrite. Retarget the immediate child to `main` before each parent branch can be deleted.
- #344 retains both conformance challenge fixes: fresh one-time bearer per Rust server, origin-scoped injection, and a fresh harness-owned DB per server. Local JS conformance is still not claimed because this host has no Node.

### B6 current heads

- **#333 Event Log core** current head: `bcfaed2a4b89772dd3fa27544274c19b938ee394`, base `main`.
  - Production delta remains 373 lines (301-500 security exception; double review required).
  - Exact canonical `main` schema comparison + relevant TEMP rejection + `main.*` runtime/migration/probe qualification + randomized behavioral probe.
  - ATTACH bearing regression proves a same-name attached schema cannot capture first migration or runtime writes.
  - Full `idoris-tenancy` 124/124 plus integration groups, fmt/clippy/diff-check green locally.
- #335 `b52dc82c11bc402af70378b72abf1342f29e661e` -> #333.
- #336 `7d0ccc4a609f9e82004ea0c87ade993baf12d970` -> #335.
- #337 `30be822234adc436008587ec3733b5647e1e3abd` -> #336 — `request.received`.
- #339 `77ea47c08362c4a6ceeef63f3c4fe412a223f09d` -> #337 — `profiled`, production 80.
- **#345** `fdb1bedd5b903fc97b031f0daef7d65cd4e18462` -> #339 — typed route-decision fact seam, production 22; no `decided` event yet.
- **B6-07 `decided` is in active worker implementation** on exact #345 head. It must emit exactly one truthful pre-budget/pre-upstream decision fact on success and pre-execution rejection/no-eligible paths, never duplicate the historical Supervisor double-decide, and use only whitelisted non-content metadata.

### Other gates

- #326 paid Resident startup gate has green CI and Prime re-challenge found no new budget/settlement blocker; external current-head approval still required.
- #340 load-fence lifetime fix is green and approval-pending. After it lands, merge current `main` normally into #338 (no rebase) for a fresh SHA/clean CI.
- The #338 bad-command exit 143 was independently diagnosed as GitHub-hosted runner preemption; do not add code for it. Its real `flock` race is #340.
- #341 is this docs-only rolling checkpoint PR; keep updating it instead of creating another checkpoint PR.

### Immediate execution override

1. Finish CI + current-head TPR on #326/#333/#340/#342/#346/#347/#344/#345 and the older B6 stack.
2. Merge only `APPROVED + required CI green + mergeable`, with stacked child base retargeting before parent branch deletion.
3. Finish B6-07 `decided`, then prioritize record-id decision-chain query/audit projection.
4. Add budget/dispatched/completed/settled events in <=300 production slices as needed for the B6 decision chain.
5. Implement `/v1/feedback` only with an explicit safe content-storage boundary for `corrected_output`; never bypass Event Log metadata content rules.
6. Close B5/B6, then B7/B8 -> B9 -> M5 -> M6 -> M7 -> M8.

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

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

## Live delta — 2026-10-11 Codex takeover (newest; read first)

- Goal mode is active; work continues on workstation B. Live main remains
  `792ed904dfbfda984fff81ea5dd930e12bfe6a30`.
- GitHub authentication is valid on this machine. All 40 queued PRs have been
  created as **#422–#461**, preserving the queue's base/head pairs; #423 is
  the draft M4 integration vehicle. No queued PR has been approved or merged.
- #374's current review blocker is repaired at `2d1507616cfa67b15d3d705e5b6347a71d8391de`:
  endpoint-valid bodies, eligible WireMock components, explicit zero-request
  expectations and verification. Full feature-enabled Rust tests, strict
  clippy, cargo deny and all TS gates passed locally (conformance 134/8).
  Current-head independent approval and fresh CI are still required.
- All open PRs currently lack APPROVED review status; do not bypass the
  repository's last-push approval rule or mark M4 complete prematurely.
- Owner decisions received in chat: accept `/v1/inspect` `{input: string}`
  and UTF-16 spans; bump wire contract on endpoint publication. The new
  version slice chooses additive minor **1.1.0**. REDACT consent and Admin v1
  public write endpoints remain pending owner review; continue safety work.
- Tenant key repair is published in independent <=300-line slices:
  #463 `164444c` storage/issuance (172 lines) →
  #465 `4c62d18` explicit conformance key binding (22 lines) →
  #467 `29aff5d` HTTP tenant authority / events / audit (296 lines) →
  #468 `1a5008c` (239-line PR delta,
  including ordinary merge of #374's repaired test). The last slice also
  denies missing authenticators in tenant library mode; real bound-key
  cfg(test) fixtures preserve authorized routing tests. Three hand mutations
  (binding bypass, untrusted audit scope, missing authenticator bypass) fail
  by assertions; restored final heads pass full Rust/TS gates and release
  conformance (134 passed / 8 todo). None is approved or merged yet.
- #464 `aba5ecf` publishes additive wire contract **1.1.0** with Rust/TS/health
  expectations synchronized. Full Rust/TS gates and release Rust conformance
  (134 passed / 8 todo) pass locally. It ships with M5, not early with M4.
- #466 `c8a3ced`, base `feat/subscription-test-hardening`, repairs the two
  historical macOS fixture races: readiness marker after TERM trap, bounded
  ESRCH-only descendant reaping, distinct port reservations and bounded
  EADDRINUSE-only startup retries. Both affected suites passed 30 repetitions
  each; all local gates pass (full Rust run serially). The old failed runs
  were rerun once after logs were diagnosed; keep observing remote results.
- Parallel full-suite execution hit portable_startup's existing 5s copied-
  binary launch deadline on two different negative tests. Serial 8/8 and
  final full serial gates pass. No production change or relaxed assertion.
- Remote checks are advancing: #374/#463/#467 have green Linux Rust/TS/
  subscription checks; macOS subscription remains queued on these runs.
  No current-head approval exists. Do not conflate queued with failed.
- 2026-10-12 after midnight: buffered/Event Log repair is published as
  #469 `7d328e7` transport finalizer → #470 `493ba53` protocol cache ordering →
  #471 `21c545d` protocol attempt terminals → #472 `f330a05` free-chat ordering →
  #473 `3f13aab` chat attempt terminals → #474 `5be697f` paid pre-send release.
  Each PR is <=300 total changed lines. All final heads pass feature-enabled
  serial Rust tests, strict clippy/fmt, cargo deny and all pnpm gates;
  protocol/chat behavior slices also pass release Rust conformance (134/8).
- Failed completion never publishes a free/protocol 200 cache/replay or a
  duplicate same-key POST in the retention window. Pending-hook cancellation
  retains the existing generic failure marker. Recorded connect attempts
  terminate with completed/failure without changing execution classification.
  Direct preflight tests cover guards that an HTTP selection refusal masked;
  hand mutations fail by assertions. Older tests expecting unfinished stream
  chains were revised explicitly, not deleted.
- Preserve the existing paid-chat rule: committed settlement keeps its success
  response when completion logging fails. Do not move that append ahead of
  paid cache publication without reviewing charged-response cancellation.
- #474 additionally reproduces and fixes a paid pre-send error retaining 2051
  budget units despite zero POSTs. With/without request ID and retries, restored
  balances remain unchanged; cancellation/uncertain execution still retain.
- #475 `9a715db` (127 lines) adds the HTTP → authorized runtime → repository
  CLI fixture flow. Three real fixture failures open OAuth breaker; fourth
  spawn stops. Removing handler feedback fails an assertion. Full local gates
  pass (router 499/499). Real vendor CLI validation remains owner decision #6.
- Draft #476 `7fea357` combines that stack with ordinary merges of #464 and
  #466. The exact combined head passes every B gate and release Rust conformance
  (134/8), at synchronized wire version **1.1.0**. Main and v0.3.0 tag refs
  were checked again: main remains 792ed904; the tag does not exist.
- Additional budget regressions are real: max_tokens=4096 under a 3000-unit
  limit still reached the paid upstream because completion quote was fixed at
  1024; a same-ID paid waiter returned 402 from a duplicate full reservation.
- #477 `8a76839` (257 lines, cfg(test) coordinator) → #478 `f48f92f`
  (243-line production wiring) moves priced reservation to the singleflight
  leader. Cache/replay/waiters use zero-cost SpendGate; paid candidates still
  require a ledger. Pre-send cancellation releases; armed cancellation retains.
  All gates pass, and #478 release conformance is 134/8. One POST charges 2201
  once and returns 200 to waiter/cache with no leftover reservation.
- #479 `f8e7728` (188 changed lines) completes the explicit max_tokens
  resident-HTTP quote fix, with #477/#478 underneath. Final feature-enabled
  serial Rust suite passes (router **504/504**), strict clippy/fmt, cargo deny,
  every pnpm gate, and release Rust conformance **134 passed / 8 todo**.
  The 4096-cap/3000-budget case is 402 with zero POST/debit/reservation;
  ignoring the cap fails an assertion. Small caps preserve their payload.
- This closes the explicit cap case; other backends, missing/invalid caps and
  prompt estimates retain legacy assumptions. No claim of arbitrary-provider
  physical billing protection. REDACT consent and public Admin writes stay
  pending, as do the other owner decisions described in the original handoff.
- Remote CI: #476 and #479 complete push/PR checks, including both macOS
  jobs, are SUCCESS. APPROVED search is empty. All M4 tail PR authors were
  checked (#361/#362/#364–#367/#374/#398/#408/#416–#418/#423): jhfnetboy,
  the current account. Do not self-approve or switch identities.
- #480 `ea5c70e` (218 changed lines) adds gate-two buffered response inspection
  core: this call's credential (including normalized Unicode escapes), L0
  inspection of body/header names/header values, aggregate size and UTF-8
  fail closed. Ten credential tests and both removed-check negative controls
  pass; full serial feature-enabled Rust (policy 135/router 504), fmt/clippy/
  deny and all pnpm gates pass (134/8). No runtime wiring, streaming, arbitrary
  encoded payload handling or public/storage contract was added. C-3 is not
  production-enforced yet. All remote push/PR CI is SUCCESS; FU-215 remains PR_OPEN.
- #481 `81c2e75` adds warmed-cache revocation acceptance for all four buffered
  inference endpoints: real cache hit before revoke, then 401/no cached payload,
  one upstream POST and no added tenant events/audit rows. Removing SQL's active
  predicate fails with 200 versus 401. Restored full gates pass (router 505,
  shared conformance 134/8). This is test-only evidence, not a new behavior fix.
- M4 #423 did not contain #374's latest repaired head. New #482 `2abf0c2`
  branches from #424 and ordinarily merges `2d15076`; 34 changed test lines.
  Full feature-enabled serial Rust (router 435), clippy/fmt/deny, every pnpm
  gate and whitespace checks pass. Real paid-chat chain lookup, Event Log-only
  audit and frozen exact-ready snapshot tests execute. Wire remains 1.0.1.
- M4 tenant-key backport candidate is now complete in review slices:
  #483 `98e7203` (155 lines) storage/issuance → #484 `afe3a6e` (138 lines)
  real bound-key routing/conformance fixtures → #485 `27237cf` (294 lines)
  admission/default-deny/trusted audit → #486 `687c6ce` (245 test lines)
  final acceptance. Legacy keys stay unbound; reissue explicitly for tenant mode.
- M4 baseline reproduced tenant-a key + valid tenant-b chat returning 200.
  Final nine-endpoint rejection matrix uses valid bodies and eligible cards,
  covers cross-tenant/unbound/anonymous/malformed/duplicate headers and missing
  authenticators under both dev settings, with zero unauthorized egress/writes.
  Binding, trusted audit and default-deny removals independently fail assertions.
- #486 proves authorized tenant chain, four warmed-cache revocation paths and
  no request.received before role scope. Late scope mutation fails; restored
  15 key HTTP tests pass. Full serial feature-enabled Rust (router 441), strict
  clippy/fmt/deny and all pnpm gates pass. Rebuilt release Rust/seeder and shared
  conformance pass **134/8**, wire **1.0.1**, on source head `687c6ce`.
- #483–#486 complete remote push/PR checks are SUCCESS. Independent
  approval/main landing/v0.3.0 remain absent. Do not treat candidate verification
  as real-main acceptance or publish early.
- #487 `f64f7a0` (81 changed lines) fixes the reproduced release health
  0.2.0-versus-tag-0.3.0 mismatch: all seven Rust crates/lock entries follow
  the prior release convention at 0.3.0; wire remains 1.0.1. Operator migration
  guide is `docs/releases/v0.3.0.md`. Full gates (router 441), release Rust
  conformance 134/8 and actual five-file archive smoke at 0.3.0 pass locally.
- Release smoke now isolates the child test user's default home/data/state
  paths and still supplies no IDORIS_* settings. An invalid caller DB sentinel
  stays untouched; removing isolation makes the real smoke fail. The initial
  old-script baseline used workstation default DB. Existing history remains
  (390 records/150 reservations), but previous budget configuration was not
  captured and cannot be assumed unchanged. Owner question is pending; no
  further writes or guessed restoration. Private impact counts are in
  `night-evidence-2026-10-12/m4-release01-default-db-impact.json`.
- Release dry run **38173125316** is verified IN_PROGRESS on `f64f7a0`:
  workflow_dispatch build/test/package only, release job disabled for this
  event. Poll that run; do not retrigger merely because observation times out.
  Local candidate archive SHA-256 starts `294d4987b4b39b6c`; full checksum/logs
  live beside the archive in `night-evidence-2026-10-12/`.
- All new code is committed and pushed. Latest M5 validated stack head is #481;
  latest M4 version/migration candidate is #487;
  #476 remains the historical combined integration snapshot, not a branch to
  merge its descendants into (that would collapse the dependent PR diffs).
- Resume: inspect run 38173125316 and #487 CI/reviews; diagnose any actual
  failure, then follow the M4 release procedure. Keep the default-DB owner
  question pending until answered; do not assume permission to restore data.
  Follow independent approval/merge gates and re-run M4 acceptance on actual
  main before tag. REDACT consent and Admin v1 public writes remain pending.
  Do not recreate completed tests or collapse open parent/child review diffs.

- Active worktrees live beside the main checkout under `/Users/jason/Dev/iDoris/`.
  Current debug caches: `target-identity32-night` built M4 authority #485;
  `target-version-night` and `target-inspect22-release` now hold #487's
  crate 0.3.0 / wire 1.0.1 debug and release builds (older M5 wire 1.1.0 artifacts
  were cleaned). Clean workspace packages before reuse, keep dependency caches,
  and run full suites serially. Logs/bodies in `night-evidence-2026-10-12/`
  use `m4-security02..04` and `m4-release01` prefixes; `run-night-gates.py` records the full gate loop.


## Live delta — 2026-10-11 Claude handoff (historical snapshot)

Full handoff: `docs/agent/HANDOFF-2026-10-11-NIGHT.md` on branch `docs/handoff-2026-10-11-night` (`git fetch origin && git switch docs/handoff-2026-10-11-night`).

- `origin/main = 792ed904dfbfda984fff81ea5dd930e12bfe6a30` (unchanged tonight; nothing merged).
- M4 landing tail (#361 #362 #364–#367 #416–#418, #398 #408, #374) still waits for a current-head approval from someone other than the last pusher.
- `integration/m4-landing-tail` merges all of them onto main without conflicts; the full final-main acceptance set is green on it (see `HANDOFF-2026-10-11-NIGHT.md`). Acceptance evidence gaps for claims 1/3/4 are closed in `test/m4-acceptance-gaps`.
- M5 gate one is implemented as a stacked chain `feat/inspect-01-contract-freeze` → … → `feat/resilience-07-subscription-breaker` → review fixes → `fix/review-5-hardening` (stack top; see the handoff for every branch) (contract, pure L0 + rule packs, `/v1/inspect`, INSPECTED on chat and protocol endpoints, kill switch core, egress counter, evasion folding, one-layer decoding, REDACT engine, budget race acceptance test, three-grain breaker + proxy wiring + selection steering + Supervisor model lockout + subscription OAuth breaker). **No PRs opened yet**: `gh` auth is invalid on this Mac; bodies + opener committed in `docs/agent/pr-queue-2026-10-11/` on the `docs/handoff-2026-10-11-night` branch.
- Pushed as **clestons** over SSH (`~/.ssh/cleston-ed25519`), so another account can approve.
- Owner decisions pending: redact-consent wire format; Admin v1 write surface (kill switch toggle, egress/breaker view); `/v1/inspect` request shape, kind list and wire-version bump; `credential_path` S2 vs S3; `provider_circuit_open` error type.
- Four independent agent reviews ran tonight; their fail-open and false-positive findings are fixed with regression tests. Provider breakers are opt-in in the binary (`IDORIS_PROVIDER_BREAKER=1`) because on-by-default broke shared conformance.
- Owner working rule (2026-10-11): when blocked on review, keep going by stacking new PRs on the open ones; the owner reviews later.

## Live delta — 2026-10-07 morning

Live Git/GitHub has moved well beyond the 2026-10-06 snapshot below. Treat this
section as the first recovery anchor, then reconcile again with `git fetch origin`
and `gh pr list --state open`.

- `origin/main = 7680efa95acd7543332ec04a14fdead9004858f1`.
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
- B9 Admin v0 has advanced through **#383 status**, **#384 session token**,
  **#385 loopback bind**, **#386 backends snapshot**, and **#387 models
  snapshot** merged to main. A transport-free **roles snapshot** is the next
  safe slice; Admin HTTP token/header spelling is still intentionally
  unresolved.
- B8 exact global-capacity resize/adoption CAS is published as **#389**, head
  `5585d149f4132f9520f46fcb04c123cd7542be47`, stacked on #382. Net production
  delta is +92 lines with concurrent-CAS and large-magnitude regressions.
- Do **not** invent an Admin session-token HTTP header/wire format. The docs
  specify a loopback management port and session-token concept, but not the
  transport spelling. Continue transport-free/read-only slices or other lanes
  until that public-contract boundary is explicit.
- B5 #346, B6 #335, B7 #370, and B8 #377 are current root landing
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

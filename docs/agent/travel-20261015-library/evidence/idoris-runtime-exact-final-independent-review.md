# Independent final acceptance: exact runtime budget

## Verdict and identity

**ACCEPT for the explicit-byte estimate-charge contract; no blocking finding.** Reviewed immutable commit `a078b033bb503216fbc0f2e2bcc419d3a4118a15` against `03c079c70611331a8c85a1666e32a75df6359d09`, independently of author branch positions. Exact candidate tree: `35567b6b8a7bb3de3caaba42d65e86c946c734f7`.

The former `27f3e21` rejection is resolved end to end: both unchanged public-dispatch strict-growth cases now reject before the tiny model reaches the adapter, after the full model actually loads. Acceptance is not based solely on integer-ledger tests. The author report and the previous independent rejection were read; this review did not author production code.

The proposed backend/router partition is outside this immutable-head verdict. Before publication, the final combined child must have this exact tree (including tests and routing); changed intermediate/combined trees need their own assessment. No claim is made here that unpublished children already match.

## Source audit

- `global_capacity.rs:17–47`: typed `CapacityCharge(u128)` converts each individual accepted estimate upward before addition. Checked per-model and ledger additions fail closed. IEEE-754 decoding scales the actual finite input in integer arithmetic, including positive subnormal values; no aggregate f64-to-byte conversion is used for fresh named slots.
- `supervisor.rs:80–99,180–184,1449–1480`: slots and PreviousModel retain exact charges. Exact planning sums surviving occupying slots, conservative unknown-residency remainder, and the target. Fresh target estimates cannot disappear through aggregate equality. Inherited targets consume existing observed coverage rather than being represented as entirely new growth.
- `global_capacity.rs:69–115,265–332`: CAS compares expected integer charges under the allocation mutex, independently of equal GB displays. `claim_charge` checks other actors plus the prospective plan against the exact ceiling even when this actor shrinks; it holds `max(current, planned)` until settlement. A rejected prospective claim does not alter allocations.
- `supervisor.rs:1480–1535`: admission occurs before slot/victim state mutation and before adapter work; rejection clears the acquired durable fence and returns. The existing float eviction planner may propose an insufficient plan, but the exact gate denies it before eviction/load.
- `supervisor.rs:1054–1086,2060–2105`: confirmed settlement uses exact slot charges without epsilon. Previous-state restoration restores charge together with estimate/policy; uncertain previous release or load retains their maxima. Any unconfirmed load prevents lowering the runtime hold. Uncertain observed growth may be recorded truthfully above the cap; unexpected confirmed excess poisons the actor instead of approving its load.
- Cancellation drops a reply waiter, not the independently spawned engine flow or its ownership. Confirmed unload/failure removes occupying charges through settlement. Float release-evidence checks still require named target absence and a valid status measurement; this existing observation contract is not strengthened into physical-memory measurement.
- Trusted startup observations, including over-budget residency, are adopted without pretending admission rejected reality. Unknown startup residency blocks operations through startup error and installs a conservative sentinel. Coverage credits use checked integer sums, cap each credit at the slot charge, and cannot subtract away fresh named charges.
- `runtime.rs:37–85,96–145,275–315` and `bin/idoris.rs:177,246–250`: explicit Some(bytes) selects `new_bytes` and shares one ledger among lifecycle runtimes; per-runtime decimal GB settings receive the allocation. None and legacy constructors retain the original default/epsilon aggregation path. Resident/subscription providers return before adapter construction. No OS reserve is subtracted again.

## Audit of the author's eight focused tests

All eight were read in the exact archive; these are actual mock-adapter/Supervisor or public-dispatch actions, not synthetic ledger arithmetic alone.

| Test | Action and observable assertions | Independent execution here |
| --- | --- | --- |
| Full allocation positive growth | Ordinary and huge full model loads once; tiny distinct model denied; no full-model unload; repeated full request stays ready | Equivalent unchanged prior public proof passed |
| Per-model/cross-runtime one-byte headroom | A full plus tiny both load; B tiny denied before adapter; repeated A tiny does not reload | Source audited |
| Separate sub-byte models | One quarter-byte model loads; second distinct quarter-byte model denied under one byte | Source audited |
| None compatibility | Full 24GB plus positive tiny both reach adapter | Source audited |
| Valid huge models | Under u64::MAX, large and extra load; replacement loads after confirmed large-model unload | Source audited |
| Canceled tiny load | Wait for adapter load start, abort caller, deny B during and after completion, unload A tiny, admit B, deny A, then unload all and assert zero reservation; ordinary and huge cases | Passed unchanged test independently |
| Inherited observation/release | Actually preload adapter, observe 4GB even above cap, deny tiny, unload inherited, load/unload tiny to zero; fitting inherited adoption included | Source audited |
| Shrinking actor/global excess | A loads 8GB, B brings observed 4GB under 8GB cap; 7GB replacement denied with zero load and zero old unload; 3GB replacement unloads old, loads once, settles global 7GB | Passed unchanged test independently |

Exact CAS/overflow unit additions were source audited, as was the expanded strict/legacy unconfirmed cleanup test. Their assertions reject stale expected charges hidden by identical GB snapshots, reject overflow without allocation mutation, and deny another adapter load while release remains unconfirmed. They were not independently rerun here.

## Independent execution and preservation

Fresh archive: `/tmp/idoris-runtime-exact-final-independent-src-ctx_2d9a2a58a5d5`.
Fresh target: `/tmp/idoris-runtime-exact-final-independent-target-ctx_2d9a2a58a5d5`.
The target never built baseline/mutant/author production source. All commands ran locked/offline with `CARGO_NET_OFFLINE=true` and that CARGO_TARGET_DIR.

1. `cargo test -p idoris-router --test independent_strict_growth --locked --offline -- --nocapture`: exit 0, **3 passed**. Log: `/tmp/idoris-runtime-exact-final-independent-proof.log`. Covers original Some(1)/500-byte denial, large exact reserve and CAS boundary controls, and both formerly failing ordinary/huge full-cap positive-growth dispatches. Full loads each equal 1; tiny loads each equal 0 and return Oom.
2. `cargo test -p idoris-backend --test runtime_exact_charge cancelled_tiny_load_keeps_exact_claim_until_confirmed_unload --locked --offline -- --exact --nocapture`: exit 0, **1 passed**, two other tests filtered. Log: `/tmp/idoris-runtime-exact-final-independent-cancel.log`.
3. `cargo test -p idoris-backend --test runtime_exact_charge shrinking_replacement_must_fit_alongside_other_observed_runtime_truth --locked --offline -- --exact --nocapture`: exit 0, **1 passed**, two other tests filtered. Log: `/tmp/idoris-runtime-exact-final-independent-shrink.log`.

Only the three prior proofs plus these two targeted lifecycle risks ran, not a whole suite. Author full-suite/clippy results remain reported evidence, not independently repeated checks.

The copied prior proof was unchanged: SHA256 `0a84f3235d2be221f74533c310fd0bb93b75bd780f1815c1b4b954b121bc01a7`. After testing, all **758** exact-candidate archived regular source files were byte-compared against a newly read git archive and remained identical; the proof was the only added source file in that temporary archive. `git diff --check 03c079c a078b033` passed.

The onboarding worktree stayed at `a232f8391107263408589a5c846dda524ebd0421`; its sole status entry remains the pre-existing untracked `idoris-v0.3.0-aarch64-apple-darwin.tar.gz.sha256`, hash `1a4ee40448ea2d67f19c87267cdb88b4aecfef71da702184e6c64ada641ef936`. No source edit, branch switch, commit, GitHub write, watcher, model download/call, paid call, or Fast/service-mode selection occurred. Orca orchestration was used solely for dispatched-task communication.

## Limits

This is a hard admission cap on checked conservative **estimate charges**, not RSS, actual engine allocation, or OS available RAM. Original catalog estimates and engine aggregate observations remain f64 and cannot regain precision they never supplied. Fresh positive named estimates charge at least one byte; separate rounding can conservatively reject fractional sums.

Inherited/unknown observation is different from newly authorized growth: existing truth can exceed the budget and release/shrink can proceed; newly requested prospective plans must fit with other actors. Temporary held claims may remain above budget while confirmed release is pending, without authorizing a prospective over-cap replacement. Display snapshots may hide individual bytes at large magnitudes.

Float eviction selection and release-evidence tolerance remain inherited constraints. Exact gating may deny a feasible plan instead of searching for additional victims at a sub-byte boundary. No real-engine/RSS/CLI-server validation, broad release acceptance, or final partition-tree verification is claimed.

# PR #479 null completion-cap review fix

## Exact candidate

- Branch: `jhfnetboy/m5-pr479-null-fix-20261015`.
- Head: `e3426f657328d21f7afd8901206de7593f02de1a`.
- Parent: reviewed head `b8f3214d4bc79a319f5b722015d68b4ead71c61c`.
- Author/committer: `jhfnetboy <jhfnetboy@gmail.com>`.
- Diff: four files, +76/-24 total; only production source is `budget.rs`, +5/-3 including comments (one behavioral line). Under 300 production changed lines.
- Exact patch: `/tmp/idoris-m5-pr479-null-fix.patch`.

Started clean and created the task branch from b8f3214 without merging main or changing the original #478 base/history. Paid branch `jhfnetboy/m4-budget-event-acceptance-20261015` remains at `0e5fee082bf5863c5e8c33a2046bbb7b82d76379`. Budget foundation `e1187aa888d84e0fee1ef35257fe3b8a1dd43e67`, wiring `168502be577a8cc6fe7567cda509ff32c165d506`, and original budget `a078b033bb503216fbc0f2e2bcc419d3a4118a15` refs remain unchanged. Final task worktree is clean. No remote publication or propagation was performed.

## Review resolution

Read cached review `/tmp/idoris-postrelease-coordination/pr479-review-new.json` for exact b8f3214. The completion validator now skips explicit null for either `max_tokens` or `max_completion_tokens`. Missing/null caps contribute no explicit quote, so quotation remains `max(1024, valid non-null sibling caps)`. Request payloads remain unchanged, including explicit null fields. This restores null quotation compatibility without relaxing strict non-null validation.

Null was removed from the invalid matrix and added to the valid matrix. Added cases cover each field alone, both null, each null paired with a smaller valid sibling, each null paired with a 2048 sibling, and each null paired with an unaffordable 4096 sibling. Existing test helper checks exact presence/value of both forwarded fields, one POST on accepted cases, no retained reservation, and expected settled spend. Every valid/default case also runs with budget 1023 to prove the 1024 floor. The 4096 sibling cases at budget 3000 prove a null cannot hide the valid larger cap.

Invalid non-null values remain 400 `invalid_completion_cap`: integral/fractional/exponential floats, strings, negative values, booleans, arrays, objects, integers above 2^53 and overflowing integer representations. Numeric string invalid values are also paired with both valid and null siblings in each field direction; sibling values cannot mask the error. These public request cases assert zero upstream POSTs and unchanged spent/reserved balances.

The actual Supervisor integration fixture retains four tests. Its unsupported-field matrix now includes null for both fields and checks the existing 400 `unsupported_field` / `unsupported_parameter`, locality header and remediation contract. Its stream-precedence test additionally checks invalid non-null caps with stream false and true: global preflight intentionally takes precedence, returns `invalid_completion_cap`, and omits selected backend locality. No Supervisor capability support was added: null is absent for quotation, but the unchanged Supervisor execution layer still rejects these supplied fields.

Updated `docs/agent/plans/M5-completion-cap-reservation.md` to cover both cap fields, null/missing semantics, strict invalid 400 behavior, and explicit global preflight placement after profile/authentication and before correlation/selection/backend validation. Corrected the previous claim that Supervisor checks were unconditionally unchanged: valid/null values still reach those checks, while invalid non-null values take global-preflight precedence even for free/no-ledger/Supervisor paths. Source precision comments now distinguish exact integer-cap conversion from floating-point rate arithmetic.

## Actual gates

Fresh final target `/tmp/idoris-m5-pr479-null-final-target-20261015` was verified absent before first build. It contains only this task's fixed source; no mutant, baseline or previous budget target was reused. Commands used two build jobs, offline mode and standard/default service.

```sh
CARGO_BUILD_JOBS=2 CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/idoris-m5-pr479-null-final-target-20261015 cargo test -p idoris-router --test supervisor_parameters --locked --offline
CARGO_BUILD_JOBS=2 CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/idoris-m5-pr479-null-final-target-20261015 cargo test -p idoris-router --lib budget --locked --offline -- --test-threads=1
CARGO_BUILD_JOBS=2 CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/idoris-m5-pr479-null-final-target-20261015 cargo clippy -p idoris-router --lib --test supervisor_parameters --locked --offline -- -D warnings
cargo fmt --all -- --check
git diff --check
```

All passed, exit 0:

- Actual `tests/supervisor_parameters.rs` integration binary: **4 passed, 0 failed, 0 filtered**; log `/tmp/idoris-m5-pr479-null-supervisor.log`.
- Existing budget-filtered library group including added matrix cases: **41 passed, 0 failed, 467 filtered**; log `/tmp/idoris-m5-pr479-null-budget.log`. All six completion-reservation tests ran, alongside existing budget tests.
- Strict clippy passed; log `/tmp/idoris-m5-pr479-null-clippy.log`.
- Formatting and whitespace checks passed.

No whole-stack/full-suite test or mutation run was performed. The review's original b8f3214 failure proof is cached review evidence; this task reports actual fixed-source runs and does not claim a newly rerun negative-control baseline.

## Known limits and next ownership

The `n` output multiplier is still not included in reservation quotation. It remains a tracked follow-up to quote cap times n or reject unsupported multiplicity for paid resident requests; no claim of complete upstream billing protection is made. Pricing still uses f64 rate arithmetic and addition: the 2^53 bound makes the accepted integer cap itself exactly convertible, not arbitrary-rate currency calculations exact. Legacy prompt estimates, nonresident fixed quotes, backend enforcement and upstream usage trust remain outside this incremental fix.

This candidate fixes the null blocker and documents/tests the existing global non-null error precedence rather than narrowing validation to resident paths. Independent acceptance, the coordinator's one-batch publication, and downstream propagation remain pending. No push, PR/GitHub write, watcher, Fast service, real paid call, model call or model download occurred.

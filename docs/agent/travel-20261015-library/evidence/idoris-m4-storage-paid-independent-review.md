# Independent review: historical storage/fixtures and paid reservation event acceptance

Review date 2026-10-15. Reviewer scope is read-only fixed Git sources plus isolated targeted verification; no production changes, auth bypass, branch switches, GitHub operations, watcher, model/download or actual paid service call.

## Exact authority and disposition

- Reviewed storage `a3cc0c77336766457faff430c829c235ed0c323c` (parents ca66c2d + 959337d), paid-event `0e5fee082bf5863c5e8c33a2046bbb7b82d76379` (parents bb1f121 + 959337d).
- Approved main/local `origin/main` observed as `959337d444a9b0df371e43a7e0a2854ae894f8e3`. Both candidates merge this main into their branches; neither candidate is part of that observed main. “Merged latest main” must not be read as “replacement has landed on main.” No network refresh or live PR-state assertion.
- Historical exact heads: #483 `98e72035af961098eff307229a95cf399d51c8ab`; #484 `afe3a6eb543ac5d35b8e499b3b4d3b2b8db72a59`; #496 `ae3db80292e3c5368cfbe08b15792736e2e71367`.
- Preserved checkout: clean `jhfnetboy/agent24-contract-v12-20261015` at `a1a08c003decda0f6a710dfc2f13c162429a8bd2`. No tracked/untracked additions there. All review artifacts live in /tmp.

Read both author reports `/tmp/idoris-m4-storage-acceptance-port.md` and `/tmp/idoris-m4-budget-event-acceptance-port.md`, historical slice patches under `/tmp/idoris-cleanup-evidence/prs/{483,484,496}/slice.patch`, and corresponding actual Git source. Reports concern earlier local heads; the requested review heads incorporate auth-main. Exact owned acceptance files remain identical to their earlier author-tested versions; merge-base and actual diffs were checked independently.

## #483 storage / issuance coverage

No lost unique behavior. Production implementation is already on 959337d via released #499, and is not reintroduced by this acceptance patch.

| Historical obligation | Actual replacement proof |
|---|---|
| Optional scope tenant binding, serde default, persistence INSERT/SELECT, no inferred authority | `crates/idoris-tenancy/src/virtual_key/store.rs` already has optional `tenant_id`; field is written and reconstructed explicitly. Fixture `tenant_id:None` additions in auth/storage/portable startup already exist on main. |
| CLI explicit binding persists and emitted secret authenticates | `crates/idoris-router/src/key_issue.rs::issues_once_persists_only_hash_and_reopens_scope` issues through the real issuance function and authenticates emitted secret, checks owner/key ID/binding. Despite its name this unit test uses an in-memory connection, not a physical reopen; real reopen is separately covered below. |
| Reject empty/whitespace/newline binding and emit nothing | Added `local_issue_rejects_invalid_tenant_binding_without_output_or_active_key` covers all old invalid cases plus NUL/129 ASCII/130-byte multibyte; after every failure output empty and no active key. |
| Version-2 migration idempotence, legacy unbound, bound identity survives reopen | `migrates_legacy_keys_without_granting_tenant_authority` creates an actual file with v1 schema and legacy row, initializes twice, writes bound row, drops connection, opens file again, compares both entire authenticated identities. This strengthens the historical test, which moved the same in-memory connection into a new wrapper. |
| Metadata rejection and no key activation | `tenant_binding_rejects_unusable_identifiers_without_activating_key` checks InvalidMetadata, authenticate None and no active key for each case; valid 64×é positive authenticates exact full scope. |
| Historical plan preserved | Exact historical `docs/agent/plans/M4-tenant-key-backport.md` remains in 98e72035, blob f12653ca6248d312c69235b0b9ebf4bfed23e922; author report archives it at `/tmp/idoris-m4-storage-historical-plan.md`. Its broad future runtime/release phases are not silently declared complete by this acceptance slice. |

Length semantics are intentionally the current **128 UTF-8 bytes**, not the historical store's `encode_utf16().count()`. Both production CLI and store use Rust `str::len()`. Tests reject 65×é (130 UTF-8 bytes, 65 UTF-16 units) and accept 64×é (128 bytes). SQL's `length(TEXT)` constraint counts characters, not bytes; the stricter application-store/CLI validation is the reviewed boundary. This is unchanged production behavior, not a claim that direct SQL insertion enforces a byte limit. Author's isolated UTF-16 mutation failed; this review does not reuse its target or mutant artifacts.

## #484 authentic protocol fixtures / attribution coverage

- `tenant_test_support.rs` is cfg(test) only, inserts a freshly minted real scope into VirtualKeyStore and constructs VirtualKeyAuthenticator. Returned key is explicitly attached by `tenant_test_request`; an assertion rejects preexisting Authorization instead of overwriting it. No automatic credential injection middleware or authentication bypass was imported from the historical helper.
- Chat, embeddings, rerank and three-endpoint protocol missing-tenant tests preserve 400/tenant_missing and zero observed upstream requests. Protocol matrix retains unified envelope and reason_code assertions. Authentication succeeds with the bound bearer, so these are tenant-parser tests rather than accidental auth failures.
- Feedback's existing explicit bound-key test remains unchanged on main, checking `invalid_tenant_scope` for oversized tenant header before a configured event store; historical `tenant_missing` expectation is obsolete, not coverage worth restoring.
- Added `tenant_mode_uses_bound_fixture_tenant_not_untrusted_metadata` gets HTTP 200/body ID tenant-positive, exactly one observed local mock POST, reads EventLogStore by the response Record-Id, asserts first RequestReceived/Profiled/Decided rows and every tenant=acme, no evil rows. Not vacuous: status/body/positive POST and indexed event assertions require successful execution.
- Existing `tenant_mode_rejects_unauthenticated_request_before_event_write` remains direct build_app without helper, asserts 401 and no acme/evil events. Existing main auth-negative tests remain unchanged.
- Main conformance already passes `keyTenantId` to feature-gated real `conformance_key_seed`, which mints a fresh credential, validates extra arguments, persists binding and prints secret only for fixture use. Tenant conformance starts an actual Rust server and local fake upstream, tests missing header and successful 200. Historical rustTenantId naming is replaced equivalently, not dropped. Author reported 2 Rust conformance tests passed; this review inspects that path without claiming to have independently rerun Vitest.

## #496 paid proxy non-vacuity and ordering

All historical assertions retained in `budget_reserved_wiring_tests::paid_proxy_budget_event_failure_releases_before_upstream`: exact 503 reason EVENT_LOG_APPEND_UNAVAILABLE, no committed BudgetReserved event, exactly zero mock POSTs, spent/reserved/available unchanged.

Stronger prerequisite: event and budget fixtures share only their temporary SQLite file. BEFORE INSERT trigger aborts budget.reserved only while a reservation exists with matching event tenant, provider and exact positive reserved_minor, status=active. Afterward the test reads exactly one persisted reservation with personal tenant, paid-proxy provider, positive amount and released status. Therefore unchanged totals cannot pass merely because no reservation was made; an unrelated event-log error would not produce that positive released row. No actual paid network or model service is used.

Positive companion `paid_proxy_records_budget_reservation_before_dispatch` returns 200, observes exactly one POST, expects RequestReceived/Profiled/Decided/BudgetReserved/Dispatched/Completed, exactly one reservation event, increases spent/decreases available and restores reserved total. Event order alone is not a timestamped observation at the POST; the negative zero-POST trigger case plus actual `lib.rs::chat_via_proxy` control flow prove the before-send barrier: reserve → append_budget_reserved → return on append error → buffered/stream dispatch. ReservationGuard releases on early return. This is adequate for this historical slice; streaming payment support, absent-event-log policy, unrelated settlement paths are outside it.

## Scope, close eligibility and limits

Storage diff against 959337d: eight files +232/-33, confined to tests, cfg(test) module/imports and helper. Paid-event diff: one test file +78/-0. No production changes, no relaxed auth, no SQL/contract/schema edits. Diff checks pass. Source review finds no blocking defect or unique historical assertion weakened in these exact replacements.

#483/#484/#496 are **coverage-complete replacement candidates**, not eligible to claim already superseded on observed main959337d. After these exact reviewed replacements are accepted and landed, #483/#484 may close as superseded only with preserved historical plan/refs and reconciliation of stack dependents (#484→#483 and historical #485→#484); #496 may close as superseded after its replacement lands. Do not merge old stacked production branches solely to collect these tests. No closure/retarget was performed here; current live PR approvals, dependent status and eventual merge remain coordinator authority.

Independent verification results follow below. Targeted package checks only; no full suites or release/combined-stack readiness claimed. Future role-binding #510 fixture composition is not present in these heads and is not covered by this review.

## Independent targeted execution — all passed

Fresh, separate Git archives and initially absent build targets, with `CARGO_BUILD_JOBS=2`, `--locked --offline`; no author/mutant target reused:

- Storage source `/tmp/idoris-storage-independent-ctx72bc`, target `/tmp/idoris-storage-independent-target-ctx72bc`, exact a3cc0c.
- Paid source `/tmp/idoris-paid-independent-ctx72bc`, target `/tmp/idoris-paid-independent-target-ctx72bc`, exact 0e5fee0.
- Byte-for-byte archive check against fixed Git objects: storage 726 tracked regular files, paid 725; both exact. No source mutations. Historical plan archive independently equals its Git blob.

| Gate | Result | Log |
|---|---|---|
| `cargo test -p idoris-tenancy --lib virtual_key::store::tests` | 6 passed | `/tmp/idoris-storage-independent-store-ctx72bc.log` |
| `cargo test -p idoris-router --lib tenant_` | 32 passed | `/tmp/idoris-storage-independent-router-ctx72bc.log` |
| Router lib `key_issue::tests` | 4 passed | `/tmp/idoris-storage-independent-issue-ctx72bc.log` |
| Router lib `protocol_error_matrix_tests` | 3 passed | `/tmp/idoris-storage-independent-protocol-ctx72bc.log` |
| Router lib exact bound-tenant positive attribution test | 1 passed (overlaps tenant_ filter) | `/tmp/idoris-storage-independent-positive-ctx72bc.log` |
| Paid router lib `budget_reserved_wiring_tests -- --test-threads=1` | 5 passed | `/tmp/idoris-paid-independent-tests-ctx72bc.log` |
| Storage `cargo build -p idoris-router --bin idoris` | passed | `/tmp/idoris-storage-independent-build-cli-ctx72bc.log` |
| Actual CLI subprocess persistence / byte boundary | passed | `/tmp/idoris-storage-independent-real-cli-ctx72bc.log` |

All Cargo calls use the corresponding archive `--manifest-path`, dedicated target and flags above. Counts overlap and are not summed as unique tests. All exit codes zero.

CLI subprocess proof script `/tmp/idoris-storage-cli-independent-ctx72bc.py` invokes the freshly built exact-head `idoris key issue --spec-stdin` with an isolated temporary DB/HOME/XDG and stripped IDORIS environment. It issues 64×é (128 UTF-8 bytes), opens SQLite independently, checks key ID/hash (SHA-256 of emitted secret per real VirtualKeySecret implementation), exact binding and active status; six invalid cases exit nonzero with empty stdout and no new row; a later successful invocation reopens the same DB, preserves the full first row and writes the second identity; migration rows remain exactly versions 1 and 2. Secrets remain in process buffers, never in logs. This supplements the actual store authentication/reopen and HTTP positive tests; it does not pretend a SQL hash assertion alone proves request authentication.

**Verdict:** no blocking findings; approve these exact heads for their narrow acceptance-test replacement scopes. Unique #483/#484/#496 assertions are represented or strengthened. Event trigger/positive reservation proof is non-vacuous, tenant fixtures use real explicit credentials, and UTF-8 byte semantics are preserved. Closure remains conditional as above; this is not a release, whole-M4 or future role-binding approval.

Final preservation check: checkout still clean at a1a08c003decda0f6a710dfc2f13c162429a8bd2 on the canonical docs branch. No repository files changed. Follow-ups checked immediately before completion.

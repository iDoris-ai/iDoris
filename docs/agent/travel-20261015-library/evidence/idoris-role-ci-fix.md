# Role CI repair for #510

Task `task_4ee4cf6aa62f`, dispatch `ctx_7af6deb3363f`, sole writer, 2026-10-15.

## Ownership and exact ancestry

- Started clean on storage branch `jhfnetboy/m4-storage-acceptance-20261015` at `a3cc0c77336766457faff430c829c235ed0c323c`. That ref is preserved.
- Switched to own role branch `jhfnetboy/postrelease-role-binding-20261015` at `bbf99ec34950867814ebb57ba33763dc8d96d536`.
- Merged exact main `959337d444a9b0df371e43a7e0a2854ae894f8e3` before any fixes. Merge commit `aa70c7d96b8174d34729c9e1920dd8ceb8dba7a4` incorporates approved/merged #508 and its tenant key ceiling and early authorization changes. The merge had no conflicts.
- Final repair head: `f4651f3c00aa6ba64f8564925ddbcdfcce9464f1`, author `jhfnetboy <jhfnetboy@gmail.com>`; clean worktree after commit.
- No push or GitHub write. The coordinator owns publication while PRD review runs. No Fast mode, watcher, paid call, model download, authentication bypass, deployment-mode change, or data-access change.

## Causes proved from actual CI logs

Inputs `/tmp/idoris-postrelease-coordination/510-rust-failed.log` and `/tmp/idoris-postrelease-coordination/510-subscription-failed.log` show native Rust tests passing followed by shared HTTP conformance failures: expected upstream 200/status/body or call counts, received 503 local_only_unavailable and zero upstream calls. The latter means the fixture never reached the behavior it claimed to test.

1. `conformance/src/fixtures.ts` generated ordinary HTTP cards without any `idoris_role_models`. Most shared HTTP tests request `idoris/daily`. Under the new production contract those cards correctly have no role eligibility, so upstream status, delay, cancellation, headers, and tenant-positive assertions became unreachable. The shared fixture now declares only `daily: fake-model-1`, matching the default model advertised by `startFakeUpstream`. Explicit extension overrides, including malformed/empty negative fixtures, remain unchanged. No production stock card gains a fabricated model ID. The slow-upstream test now also inspects the real received model, distinguishing the Rust resolved concrete target from the unchanged TS reference behavior.
2. The new role filter also excluded subscription aliases. Subscription cannot carry `idoris_role_models` because its authorized relay executes a fixed model and would ignore arbitrary mapping targets. Restored the previous role eligibility only when `classify_subscription_card` validates a genuine fixed subscription card and the mapping is absent. Present mappings, including an empty object, still fail startup and programmatic selection. Existing privacy, capabilities, policy, source, authorization, and runtime dispatch gates remain unchanged.
3. Protocol trace found that chat completions resolved role bodies but embeddings, rerank, and native messages selected using role mappings then forwarded the alias unchanged. A single `bound_http_body` now applies the same exact backend ID resolution to each resident HTTP handler. Concrete overrides retain their payload, and internal resolution errors remain redacted through a shared error response. No arbitrary model target enters subscription dispatch.

The Python-backed fixture in `scripts/check-rust-egress.sh` advertises `fixture-model` from its local HTTP server. Its local synthetic card now declares exactly `daily: fixture-model`; the deliberately unreachable remote trace control has no observed model and remains unbound. Its request/trace assertions still use /v1/models and were not weakened or repurposed. This Linux strace gate was not run on this macOS host and is not claimed as validated here.

## Shared boundary and protocol source proof

| Path | Selection and backend identity | Actual payload checks |
|---|---|---|
| Lifecycle chat | Existing backend_model_id is shared by readiness, catalog/footprint checks, load, and chat | Existing role lifecycle test checks chosen ID, concrete override, zero OCR/provider fallback loads, and missing-model/capacity refusal |
| Resident chat, buffered and SSE | bound_http_body before chat_via_proxy | Existing buffered/SSE test captures concrete target and override; new protocol matrix also checks full buffered body |
| /v1/embeddings | Same role-aware selection and bound_http_body before EMBEDDINGS forwarding | Actual mock request URL and complete body preserve input; resolved embed target or concrete override; no-binding role refuses with zero extra requests |
| /v1/rerank | Same helper before RERANK forwarding | Complete body preserves query/documents; resolved rerank target or concrete override; no-binding refusal |
| /v1/messages buffered | Same helper before native MESSAGES forwarding | Complete max_tokens/messages/stream body and target observed at mock HTTP server; no-binding refusal |
| /v1/messages SSE | Same resolved body passed to messages_stream | Complete native stream body and concrete target observed; stream collected through message_stop; no-binding refusal |
| Subscription fixed relay | Selection restores legacy roles without a mapping; subscription::dispatch::dispatch_selected still constructs ChatRequest.model from authorized runtime handle.model_id() | Unit controls verify daily/auto/fast selection with any privacy, private refusal, and mapping rejection; black-box daily and auto assert Rust fixed claude-subscription response, exact TS alias metadata, and one actual fixture CLI spawn |

The shared helper is called only after normal profile/auth scope enforcement and selection. Paid protocol guards, budget reservations, privacy, policy filtering, request/event authorization, and fixed subscription runtime logic are retained. Production role bindings remain an operator assertion, not automatic model availability discovery.

Source anchors in the final tree:

- `crates/idoris-router/src/dispatch.rs:130` rejects subscription mappings; `:160` defines the valid-card/no-mapping exception.
- `crates/idoris-router/src/lib.rs:577` resolves HTTP bodies and `:594` redacts internal resolution failures. Its four call sites cover embeddings, rerank, messages, and resident chat.
- `crates/idoris-router/src/subscription/dispatch.rs:64` still uses `handle.model_id()` and is unchanged by this repair.
- `conformance/src/fixtures.ts:105` and `:123` declare verified HTTP test bindings; `conformance/tests/subscription.test.ts:134` verifies fixed-model aliases through actual fixture CLI processes.
- `scripts/check-rust-egress.sh:86` binds only the observed local Python fixture. Production `config/components`, workflows, and both required CI command scripts are unchanged.
- New Rust production diff against the pre-fix merge is 66 insertions and 21 deletions across dispatch.rs and lib.rs, below the 300-line limit. Tests and contract documentation are separate from that count.

## Reproduction and focused controls

- Added subscription role-selection assertions failed before the production repair: a valid shipped subscription card without mapping could not select daily/auto/fast.
- New HTTP protocol capture test failed before the production repair at embeddings: observed `model=idoris/embed`, expected the configured `verified-protocol-model`. Initial test setup was corrected to explicitly include the transport's existing `stream:false` normalization for chat and buffered messages, rather than changing transport behavior.
- `cargo test -p idoris-router --lib --locked role_binding_`: all 8 pass. Matrix covers exact upstream paths/bodies, concrete overrides, missing mappings and zero extra egress, lifecycle footprint failure, no first-model fallback, fixed subscription exception, and forbidden mapping startup/runtime controls.
- The first native-script attempt exposed an incorrect assumption in the two newly added shared tests: TS echoes request model metadata, while Rust reports the fixed runtime ID. `packages/adapters/subscription/relay.ts` builds CLI arguments from prompt/outputFile only, then echoes model in `openAIChatCompletion`; the tests now assert each implementation's exact existing metadata contract and retain the 200/remote/one-spawn checks. No pre-existing test assertion was weakened.
- `cargo fmt --all -- --check`, strict affected-package all-target clippy, conformance TypeScript typecheck, and git diff --check pass. Clippy led to keeping DispatchError as the helper error type rather than returning a large Response inside Result; there is no lint suppression.

## Actual CI commands

Every Cargo command uses `CARGO_TARGET_DIR=/tmp/idoris-role-binding-target-20261015` and `CARGO_BUILD_JOBS=2`. The script runs the actual commands below, not substitute debug binaries or narrowed conformance filters.

- `bash scripts/conformance-rust.sh`: exit 0. The script builds actual release `idoris` and test-bins seeder, sets its own binary/argv/config path, and runs the shared suite. 136 passed, 8 pre-existing TODOs (144 total), 16 files passed and one baseline skipped file. See `/tmp/idoris-role-ci-rust-conformance.log`.
- `bash scripts/check-subscription.sh`: final exit 0. All 10 upstream integration gates, upstream subscription library filter, router connection/subscription filters, and 7 router integration gates pass. The script builds the TS reference and actual Rust release artifacts. Both the TS and Rust shared conformance phases report 136 passed and the same 8 TODOs. See `/tmp/idoris-role-ci-subscription.log`.
- The script's existing pnpm/Vitest argument handling runs the whole shared suite, despite naming the two subscription files. Its invocation and assertions were retained. The required full runs are why no optional broader suite was added.
- `cargo test -p idoris-router --lib --locked role_binding_`: 8 passed. Strict `cargo clippy -p idoris-router --all-targets --locked -- -D warnings`, fmt check, conformance typecheck, shell syntax check for the changed egress fixture script, and git diff --check pass.
- No pre-existing test was disabled, deleted, or weakened. The 8 TODOs remain the original known contract gaps. Two new shared subscription aliases account for 136 passes instead of the old 134.


Logs: `/tmp/idoris-role-ci-focused.log`, `/tmp/idoris-role-ci-protocol-red.log`, `/tmp/idoris-role-ci-red.log`, `/tmp/idoris-role-ci-clippy.log`, `/tmp/idoris-role-ci-typecheck.log`, `/tmp/idoris-role-ci-fmt.log`, `/tmp/idoris-role-ci-rust-conformance.log`, `/tmp/idoris-role-ci-subscription.log`.

## Publication boundary

Repair is local and requires the coordinator's existing publication/review flow. No current remote CI result, new independent approval, merge, or release is claimed. The storage acceptance branch and its work remain preserved; it was not merged into the role branch.

## Evidence integrity

- `/tmp/idoris-postrelease-coordination/510-rust-failed.log` SHA-256 `a5b0cd1b846e8d78a7fa90723f63321b8d8b198d95e5e9844828282bff8c09e7`.
- `/tmp/idoris-postrelease-coordination/510-subscription-failed.log` SHA-256 `a01c3764db840c43f4187ce37b811dad230bf469b516fdc97c2092163781ae0e`.
- `/tmp/idoris-role-ci-rust-conformance.log` SHA-256 `0340c6d3529bd133e880636ec7dece3b7176629aee835c2c3078c2c83fbfa1dd`.
- `/tmp/idoris-role-ci-subscription.log` SHA-256 `f89486dc59932af7ae86d9d738062c09fd90054159a93948b3c84d87b8560bba`.

The failed first native attempt is preserved at `/tmp/idoris-role-ci-subscription-attempt1.log`; it contains only the two new TS response-metadata expectation failures described above.

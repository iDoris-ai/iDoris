# iDoris Handoff — 2026-10-11 night — M4 integration + M5 gate one + resilience

Read after `HANDOFF-2026-10-11-M4-LANDING-TAIL.md` (#421). This file only adds what happened between 18:10 and the stop time on 2026-10-11 (+07).

## Owner instruction for this session

> "GO straight tonight, no reviewer, just submit pr based on old pr without review, stack it, i review tmr"
> "stop your jobs before 9:40 PM, before that time, commit all, push"

So nothing was merged and nothing was approved. Every branch below is pushed and stacked on the previous one; the owner reviews tomorrow. No approval was fabricated and the "approval from someone other than the last pusher" rule was not touched.

## Environment facts found tonight

- `gh` token for `jhfnetboy` in `~/.config/gh/hosts.yml` is **invalid** (HTTP 401). **No PR could be opened.** All PR titles/bodies plus an idempotent opener are in `/Volumes/UltraDisk/Dev2/iDoris/iDoris-wt/pr-queue/` — after `gh auth login -h github.com`, run `bash /Volumes/UltraDisk/Dev2/iDoris/iDoris-wt/pr-queue/create-prs.sh` (opens them in stack order with the right bases; skips heads that already have an open PR).
- HTTPS git has no credential. Pushes went over SSH with `~/.ssh/cleston-ed25519`, which authenticates as **clestons** (the repo's configured git user). `~/.ssh/id_ed25519` authenticates as jhfnetboy.
- Consequence for the approval rule: **clestons is the last pusher on every branch below, so jhfnetboy (or anyone but clestons) can approve them.**
- This Mac had no stale iDoris worktrees or local branches; #421's cleanup checklist had nothing to delete here. Tonight's worktrees live in `/Volumes/UltraDisk/Dev2/iDoris/iDoris-wt/` (one per branch, all clean and pushed at stop time); once a branch's PR is merged its worktree can be removed. Cargo target dirs `iDoris-wt/target`, `target-gaps`, `target-review` are scratch (safe to delete).
- CI runs on every push (`on: push`), so each branch already has GitHub CI evidence even without a PR.

## Stack (base ← head), in landing order

| # | base | head | title |
|---|---|---|---|
| 1 | `main` | `integration/m4-landing-tail` @ `e87b6c77` | chore(m4): integrate open M4 landing tail for acceptance |
| 2 | `integration/m4-landing-tail` | `test/m4-acceptance-gaps` @ `8e82e246` | test(router): close M4 acceptance evidence gaps |
| 3 | `integration/m4-landing-tail` | `feat/inspect-01-contract-freeze` @ `3ee1649e` | feat(contracts): freeze M5-01 /v1/inspect contract |
| 4 | `feat/inspect-01-contract-freeze` | `feat/inspect-02-l0-inspector` @ `e929c419` | feat(policy): add pure L0 inspector for gate one |
| 5 | `feat/inspect-02-l0-inspector` | `feat/inspect-03-cn-identifiers` @ `368e9ada` | feat(policy): add CN/HK identifier and email L0 rules |
| 6 | `feat/inspect-03-cn-identifiers` | `feat/inspect-04-secret-rules` @ `ad107662` | feat(policy): add credential L0 rules with two independent sets |
| 7 | `feat/inspect-04-secret-rules` | `feat/inspect-05-endpoint` @ `ed21b611` | feat(router): serve POST /v1/inspect from the L0 inspector |
| 8 | `feat/inspect-05-endpoint` | `feat/inspect-06-route-stage` @ `e9481f9f` | feat(router): add INSPECTED stage to the chat path |
| 9 | `feat/inspect-06-route-stage` | `feat/inspect-07-protocol-endpoints` @ `5af318cd` | feat(router): inspect embeddings, rerank and messages bodies |
| 10 | `feat/inspect-07-protocol-endpoints` | `feat/inspect-08-remote-kill-switch` @ `261dd3f1` | feat(router): add remote kill switch to gate one |
| 11 | `feat/inspect-08-remote-kill-switch` | `feat/inspect-09-egress-counter` @ `9bb2f31a` | feat(router): count off-host proxy egress bytes |
| 12 | `feat/inspect-09-egress-counter` | `feat/inspect-10-evasion-normalization` @ `9e60d68a` | feat(policy): fold L0 evasions before scanning |
| 13 | `feat/inspect-10-evasion-normalization` | `feat/inspect-11-encoded-content` @ `6c2623ed` | feat(policy): decode one layer of encoding before L0 |
| 14 | `feat/inspect-11-encoded-content` | `test/m5-budget-concurrency` @ `363f3b85` | test(router): prove 100 concurrent paid chats cannot overrun budget |
| 15 | `test/m5-budget-concurrency` | `feat/inspect-12-redact-core` @ `309d9fac` | feat(policy): add reversible redaction engine for gate one |
| 16 | `feat/inspect-12-redact-core` | `feat/resilience-01-breaker-core` @ `71b0fc9f` | feat(policy): add pure circuit breaker for three-grain resilience |
| 17 | `feat/resilience-01-breaker-core` | `feat/resilience-02-proxy-breaker` @ `0511927c` | feat(router): gate buffered proxy sends with provider breakers |
| 18 | `feat/resilience-02-proxy-breaker` | `feat/resilience-03-stream-breaker` @ `63985979` | feat(router): put streaming proxy sends behind provider breakers |
| 19 | `feat/resilience-03-stream-breaker` | `feat/resilience-04-steer-selection` @ `0d9d0d13` | feat(router): steer chat selection away from open provider breakers |
| 20 | `feat/resilience-04-steer-selection` | `feat/resilience-05-protocol-steering` @ `7d08e00f` | feat(router): steer protocol endpoint selection away from open breakers |
| 21 | `feat/resilience-05-protocol-steering` | `feat/resilience-06-local-model-lockout` @ `fc2e0ed5` | feat(router): lock out failing Supervisor models |
| 22 | `feat/resilience-06-local-model-lockout` | `feat/resilience-07-subscription-breaker` @ `e8dbb3ab` | feat(router): feed subscription relay faults to the OAuth breaker |
| 23 | `feat/resilience-07-subscription-breaker` | `fix/inspect-13-review-hardening` @ `eeb695d9` | fix(inspect): close gate-one fail-open paths from adversarial review |
| 24 | `fix/inspect-13-review-hardening` | `feat/inspect-14-off-worker-scan` @ `5881ac3a` | feat(router): scan large bodies off the async worker |
| 25 | `feat/inspect-14-off-worker-scan` | `feat/inspect-15-decided-tightening-reason` @ `1e44c557` | feat(router): record content tightening in the decided reasons |
| 26 | `feat/inspect-15-decided-tightening-reason` | `feat/inspect-16-homoglyph-folding` @ `e8118bf4` | feat(policy): fold digit scripts and Latin homoglyphs before L0 |
| 27 | `feat/inspect-16-homoglyph-folding` | `docs/m5-gate-one-runtime` @ `02867749` | docs(interfaces): describe gate-one runtime behaviour and M5 switches |
| 28 | `docs/m5-gate-one-runtime` | `fix/inspect-17-window-segment-floor` @ `22b5096b` | fix(policy): stop joining short digit runs into one entity |
| 29 | `fix/inspect-17-window-segment-floor` | `fix/inspect-18-second-review` @ `50caf46a` | fix(inspect): address second review of the M5 hardening |
| 30 | `fix/inspect-18-second-review` | `fix/inspect-19-third-review` @ `aced94db` | fix(inspect): judge digit groups whole or fully decomposed |
| 31 | `fix/inspect-19-third-review` | `fix/inspect-20-grouping-recall` @ `b498db6a` | fix(inspect): recover grouped entities next to short numbers |
| 32 | `fix/inspect-20-grouping-recall` | `feat/inspect-21-opaque-in-inspector` @ `228bb1f0` | feat(policy): treat embedded base64 payloads as uninspectable |
| 33 | `feat/inspect-21-opaque-in-inspector` | `feat/resilience-08-connection-cooldown` @ `eae2ac6e` | feat(router): cool whole connections on Retry-After |
| 34 | `feat/resilience-08-connection-cooldown` | `feat/event-log-28-protocol-chain` @ `d1cc81f9` | feat(router): record the decision chain on protocol endpoints |
| 35 | `feat/event-log-28-protocol-chain` | `feat/event-log-29-protocol-dispatch` @ `6df741cd` | feat(router): record dispatch and completion on protocol endpoints |
| 36 | `feat/event-log-29-protocol-dispatch` | `feat/event-log-30-messages-stream` @ `c6906b05` | feat(router): complete streamed /v1/messages in the Event Log |
| 37 | `feat/event-log-30-messages-stream` | `feat/gate-two-01-egress-decorrelation` @ `8a1422e5` | feat(router): de-correlate request bodies sent off host |

Rules for landing (same as PRIME-CHECKPOINT "Safe stacked merge procedure"): merge parent first; before a parent branch is deleted, retarget its child to `main`; no rebase, no force-push; never treat a stale approval as current.

`integration/m4-landing-tail` is a draft vehicle only: it merges #418 (contains #361→#417), #408 (contains #398) and #374 onto main without conflicts. Prefer landing those PRs individually; once they are in `main`, this branch's own diff collapses to merge commits.

## How to review this stack (suggested)

1. **Land M4 first, independently of everything below.** Approve and merge the original tail PRs (#361 → … → #418, #398 → #408, #374) in parent-first order, retargeting children before deleting parents. `integration/m4-landing-tail` only exists to prove they compose and to carry the acceptance run; after the tail is in `main` its diff collapses to merge commits. Then merge `test/m4-acceptance-gaps` (tests only), re-run the acceptance set on `main`, tag `v0.3.0`.
2. **Contract first:** `feat/inspect-01-contract-freeze` is the only public-wire decision point (request shape, kind list, UTF-16 spans). Settle decision 3 here before reviewing the rest.
3. **Pure code next, low risk:** inspect-02/03/04/10/11/12/16 and resilience-01 are IO-free (`idoris-policy`); each commit message lists the hand-run mutations that turn its tests red.
4. **Wiring, highest attention:** inspect-05 (endpoint + auth split), inspect-06/07 (INSPECTED placement — the privacy-critical change), inspect-08 (kill switch), inspect-09 (egress counter), resilience-02..08 (breakers; off by default in the binary).
5. **Read the review-fix PRs as a sequence** (`fix/inspect-13` → `-17` → `-18` → `-19` → `-20`): they record four rounds of trade-offs on the digit-group rule. The final rule is in `-20`; the earlier ones explain why simpler rules were rejected. If you prefer, squash-review `-13..-20` as one diff against `feat/inspect-16-homoglyph-folding`.
6. Everything is stacked, so a change requested low in the stack means merging that branch forward into its children (no rebase, no force-push), the way the breaker opt-in fix was propagated tonight.

## M4 status

- Landing tail unchanged: #361 #362 #364–#367 #416–#418 (B6), #398 #408 (B8), #374 (B7) still need a current-head approval from someone other than the last pusher.
- **Final-main acceptance set was run on `integration/m4-landing-tail` (all tail PRs merged onto main `792ed90`)**: fmt, router lib 431, tenancy, backend, upstream, contracts parity/drift, provider_binding, model_identity, portable_startup, request_event_query (3), audit (20), admin (27+1+5), strict clippy, `git diff --check v0.2.0...HEAD` — all green; GitHub CI on that head green.
- Evidence gaps for acceptance claims 1/3/4 are closed by tests in `test/m4-acceptance-gaps` (real paid chat → full chain over HTTP; `/audit` never reads the legacy store; exact-ready target actually serves with one readiness snapshot). Claims 2 and 5 were already proven.
- Observation for #408: readiness is model-id only, not load-policy aware; a target warmed with a different `idle_ttl_s` is still preferred but gets `load` again. Not a test failure; cost on a real backend unverified.
- M4 can be declared DONE once the tail lands and the same set is re-run on real `main`; then tag `v0.3.0` per #421.

## M5 status (gate one, egress, budget, resilience)

The #421 M5 plan (contract freeze → pure L0 → rule packs → endpoint → INSPECTED stage → kill switch → egress counter) is implemented, plus hardening:

| slice | what |
|---|---|
| M5-01 | `/v1/inspect` contract: `{input}` → `{sensitivity S0..S3, findings[{kind, span?}], action}`; closed kind enum; span = `{start, length}` in UTF-16 units; no free-form string in the response (schema-walk test) |
| M5-02..04 | pure L0 in `idoris_policy::inspect`: CN id/mobile/bank card; USCC/passport/HK permit/HKID/plate/email; credentials with two independent sets (prefixes + entropy/switch-rate), PEM keys, credential paths; L2 default BLOCK |
| M5-05 | `POST /v1/inspect` (privacy-ceiling-only key scope; never echoes input) |
| M5-06/07 | INSPECTED on chat (before profiling, routing policy and the remote intent detector) and on embeddings/rerank/messages: anything short of ALLOW pins to `local_only`; `inspected` Event Log fact |
| M5-08 | remote kill switch core (`IDORIS_REMOTE_KILL_SWITCH=1`, shared `AtomicBool`) |
| M5-09 | off-host egress byte counter in `ChatProxy` (by dialed host) |
| M5-10/11 | evasion folding (full-width, zero-width, dash variants) and one layer of base64/base64url/percent/hex decoding |
| test | 100 concurrent paid chats never overrun the budget (end to end) |
| M5-12 | reversible REDACT engine (stable placeholders, rescan fail-closed, R-1/R-2 refusals, streaming restore) — **not wired** |
| M5-13..19 | three-grain breaker state machine (B-8 params); provider breaker on buffered and streaming proxy sends; chat and protocol selection steer away from open providers; Supervisor single-model lockout; subscription relay → OAuth breaker |
| review fixes | first review: six fail-open paths closed (unreadable content → `local_only`, grouped numbers, invisible separators, encodings); breaker made opt-in after CI caught a conformance break |
| M5-21..23 | large bodies scanned off the async worker (`inspect_failed` fail-closed); decided reasons carry `privacy_tightened_by_content`; all BMP digit scripts + Latin homoglyphs folded |
| review fixes 2–4 | false-positive explosion from tab/newline joining removed; digit groups judged as runs, whole group, or full decomposition into entities; `\uXXXX` and math digits folded in place; report-only breakers re-lock after the window |
| M5-24 | public `/v1/inspect` answers BLOCK/S3 for embedded `data:…;base64,` payloads (the route stage already pinned them local) |
| M5-25 | connection-grain cooldown: `Retry-After` cools every card on the same scheme/host/port; selection steers away (opt-in breakers) |
| M5-26 | decision chain (received → inspected → profiled → decided) on embeddings/rerank/messages; dispatched/completed added in M5-27 |
| M5-27 | `dispatched`/`completed` on buffered protocol requests (replays record nothing); `/v1/messages` streaming completed in M5-28 |
| M5-28 | streamed `/v1/messages` records `dispatched` and completes when the stream ends (`message_stop`); every inference endpoint now has a full decision chain |
| M5-29 | egress de-correlation: off-host bodies drop `user` / `safety_identifier` / `prompt_cache_key` / `metadata`; loopback unchanged |

M5 acceptance shape is tested: an id/phone/card injected anywhere in a declared-`any` body (plain content, array parts, tool arguments, JSON numbers, Anthropic `system`, embeddings input, rerank documents) yields zero upstream requests on a non-local card; kill switch stops the next request; 100-way budget race never overspends.

Every rule/decoder/stage was mutation-checked by hand (removing it turns its tests red); notes are in each commit message.

## Decisions waiting for the owner

1. **Redact consent wire format.** Without it `/v1/inspect` and the route stage never answer REDACT (personal data → BLOCK). The engine (M5-12) is ready to wire once decided.
2. **Admin v1 write surface** for the kill switch toggle (docs/15: management port/UI only; Admin v0 is read-only by decision) and for showing the egress counter / breaker state.
3. `/v1/inspect` request shape (`{input: string}` vs messages), the finding-kind list, and whether to bump wire `contract_version` (still `1.0.1`) when the endpoint ships.
4. `credential_path` sensitivity S2 (vs S3).
5. **Provider breakers are off by default in the binary** (`IDORIS_PROVIDER_BREAKER=1` turns them on). CI showed that on-by-default breaks the shared conformance contract ("persistent 5xx is passed through verbatim"; `conformance/tests/upstream-behavior.test.ts` sends 500 then 503 to one local upstream and expects later cases to reach it). Turning them on by default needs an owner decision plus a conformance update (e.g. an env-gated expectation like `IDORIS_CONFORMANCE_POST_RETRY`). Also new public error type when on: 503 `provider_circuit_open`.
6. Breaker scope still open: the subscription wiring is unit-tested only (no CLI fixture run). (Per-connection Retry-After is done in M5-25.)

## Known CI note

`test/m5-budget-concurrency` CI run 38137840647 failed in `subscription (macos-latest)` → "Subscription Unix safety + shared conformance" (exit 101). The branch only adds one router test file; the same commit's 20 subscription test groups pass locally on this Mac, and every branch stacked on it is green. The same step failed the same way once more on `feat/inspect-15-decided-tightening-reason` (run 38140879392, job 114474328065, exit 101), while its child `feat/inspect-16-homoglyph-folding` — which contains the identical change — passed on macOS. `bash scripts/check-subscription.sh` on that exact commit passes in full on this Mac (20 Rust groups + shared conformance 134 passed / 8 todo). Two occurrences of one intermittent macOS-runner-only panic in the subscription safety tests: worth a look at the job log once `gh` works (logs need auth), then re-run; do not add code for it blindly.

## Independent review (no human reviewer tonight)

A separate agent adversarially reviewed `integration/m4-landing-tail..feat/resilience-05-protocol-steering` and **reproduced six privacy fail-open paths with throwaway router tests** (declared `any`, identifier present, 200 and upstream hit on an `any`-only card): token-id arrays and images/files treated as clean (high), neighbouring grouped numbers, ids split across parts/lines, NBSP/bidi/Arabic-Indic digits, and three encoding gaps. All six were fixed with regression tests in `fix/inspect-13-review-hardening`; the second and third reviews later revised two of those fixes (split-across-lines/parts was withdrawn, the digit-group rule was settled in `fix/inspect-19-third-review`). It also found the breaker fallback re-decide inconsistency, already fixed in M5-18.

Reviewer-confirmed sound: INSPECTED placement (after auth, before profile / routing policy / intent detection / subscription / proxy) on all four endpoints; privacy never loosened later; idempotency replay respects `local_only`; the egressed body is the inspected `Value`; scanner panics fail the request; no span/UTF-16 arithmetic bug found (reading only, no fuzz); no quadratic scans.

Review lows fixed later tonight: homoglyphs/digit scripts (M5-23), decided reasons now carry `privacy_tightened_by_content` (M5-22), large bodies scanned off the async worker (M5-21).

Still open from the review (low): REDACT has no "swallowed placeholder" count check (R-1 is ambiguous about omitted entities; a strict count would reject legitimate summaries — owner call); `IDORIS_REMOTE_KILL_SWITCH` only accepts `1` (same `env_flag` convention as every other switch); `/v1/inspect` treats remote as allowed whenever the key admits `any` (reviewer unsure whether tenant routing policy should also count).

## Second independent review

A second review of everything after the first one found that the first-review hardening itself caused a **false-positive explosion** (tab/newline joining + arbitrary segment windows flagged flattened `logit_bias`, TSV tables, port and year lists; most random numeric tables), that "ids split across text parts" never worked through the router, an `\uXXXX` decoder bypass, report-only breakers that never re-locked, and a few low items. All fixed in `fix/inspect-18-second-review` with regression tests (see its PR body). Consequence worth knowing: **an identifier split across lines or JSON parts is a known gap** — joining on line breaks cost more availability than it bought.

## Third independent review

A narrow third review of the second fix measured a recall regression (one canonical grouping per entity missed `138-12345678`, `110101 1949 1231 0023`, …) and still-high false positives (22% of random 8-number rows). `fix/inspect-19-third-review` changed the digit-group rule to: each run alone, the whole group in any grouping (original M5-02 behaviour), and several entities only when the whole group decomposes into neighbouring entity windows. Measured on 5000 random rows each: 8×4-digit rows 0.36% flagged, 20-number rows 0%. (superseded by the fourth-review fix below).

## Fourth independent review — final digit-group rule

A fourth review found the third fix's DP correct and linear but measured a recall regression (card + expiry/CVV in one group, a phone after a time, two phones + a digit — all missed) and 1.8% false positives on rows of small integers. `fix/inspect-20-grouping-recall` (stack top) settles it: multi-run windows must be a real written form (mobile 3-4-4 / 3-8 / 7-4 / 4-3-4 + 86 variants; id 6-8-4 / 6-12 / 6-4-4-4; card 4-4-4-4[-1..3] / 6-13), and a group is one decomposition into such entities with at most two ≤ 3-digit numbers at each end. Measured on 5000 random rows each: small-integer rows 0%, 5–6 four-digit numbers 0%, 8 four-digit numbers 0.26%; remaining false positives are groups written exactly like a phone (3-4-4) or card (4-4-4-4 + issuer prefix + Luhn), as since M5-02. A final timeboxed check (400k fuzzed groups) found no crash and no entity-free decomposition; its one realistic miss, the `0086` prefix, is fixed on the same branch. **Owner trade-offs:** 4-digit neighbours (`Room 1203 138 1234 5678`, an id followed by a year) and ids split across lines/parts are not found; allowing 4-digit neighbours flagged ~10% of 5-number rows.

## CI caught a real regression tonight

From M5-14 on, CI failed "Rust release conformance" and the subscription job's shared conformance on Linux and macOS: the shared suite expects persistent 5xx to pass through and later cases to reach the same upstream, but the new provider breaker opened after two failures. Fixed on `feat/resilience-02-proxy-breaker` by making breakers opt-in in the binary (decision 5 above), then merged forward (no rebase) through 03→07; Rust conformance re-run locally on the fixed branch and on the stack top: 134 passed / 8 todo.


## Next

1. `gh auth login`, run `pr-queue/create-prs.sh`, re-run the flaky macOS job.
2. Owner review of the stack; land M4 tail → re-run acceptance on main → `v0.3.0`.
3. Owner decisions above; then wire REDACT, Admin v1 kill switch, `inspected` events on protocol endpoints, gate two (Keychain).

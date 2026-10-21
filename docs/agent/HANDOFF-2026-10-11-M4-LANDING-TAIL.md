# iDoris Handoff — 2026-10-11 — M4 landing tail

## Current milestone

- M1–M3: complete.
- Current milestone: **M4**.
- Do not mark M4 complete until the landing tail below is merged to `main` and the final-main acceptance set passes.
- After M4 is complete: create and push the release tag, publish the release, then immediately enter M5.

## Current main

At handoff creation, remote `main` is:

- `792ed904dfbfda984fff81ea5dd930e12bfe6a30`
- latest merge: PR #420 `test(ci): align egress assertion with loopback auth`

## M4 landing tail

### B6 Event Log / audit chain

Parent-first landing order:

1. #361 `feat(router): accept B6 metadata feedback events`
   - head: `c802fda9b9348e865c6f0079ec35d083dcd9c26f`
   - open, mergeable, CI green.
   - latest clestons review is COMMENT only because Opus weekly quota prevented the required later review rounds. It explicitly reports no new code blocker.
   - GitHub repository rule currently rejects merge without an approval from someone other than the last pusher.
2. #362 `feat(event-log): accept B6 binary feedback rubric`
   - head: `1fd024bc09955e2aca1a8df34c491f6d32a04f84`
3. #364 `feat(event-log): query B6 tenant event ranges`
   - head: `67f460e73ca75c7b3acb656cf07b8a87a521b3f6`
   - current-head clestons review text says APPROVE, but GitHub records it dismissed after stack/base movement.
4. #365 `feat(router): project B6 audit records from Event Log`
   - head: `037ab59ba84bb0087fb6511cc245f5e51179abe6`
   - current-head clestons review text says APPROVE, but GitHub records it dismissed after stack/base movement.
5. #366 `feat(router): persist B6 request ids in Event Log`
   - head: `67e1b54278f1410dd0e3abd9caa75adf92ff70ce`
   - latest review is COMMENT because Opus quota prevented completion of later rounds; no precise code blocker reported.
6. #367 `feat(router): record B6 local completion token facts`
   - head: `2c5ba35637a40be0ee56d04f4bbc7461bb4c4361`
   - latest review is COMMENT because Opus quota prevented completion of later rounds; no precise code blocker reported.
7. #416 `feat(router): project terminal audit facts into Event Log`
   - head: `2933d20e056504a595967aea6fa4f3bfd5aeae7b`
8. #417 `feat(router): serve tenant audit from event log`
   - head: `5affb837e2f87c6b5ee96e6991c8bf76ee950850`
9. #418 `fix(router): align tenant ids with event log bounds`
   - head: `ffe3e6e5f8cf717ddbb566df51f2d9903698ba91`

GitHub hard rule observed during a real merge attempt on #361:

> New changes require approval from someone other than the last pusher.

Do not bypass or fabricate approval. A different reviewer must submit APPROVE on the current head.

### B8 runtime tail

- #398 `docs(runtime): add B8 multi-backend launch example`
  - head: `d352b9f1afc030762f7161ed56fea04eea6aab5a`
  - open, mergeable, current CI green.
- #408 `feat(router): prefer exact ready session target`
  - head: `e267624a68c11c27809c610fe31659a09f3bffe0`
  - open, mergeable, current CI green.

Earlier B8 chain #391→#397 is already merged.

### B5 / Admin status

- #415 is merged.
- Agent24 PR #789 (M4 iDoris Admin status card expansion) is merged.
- #419 Admin load/unload was intentionally closed unmerged after the owner decision that M4 Admin v0 remains read-only. It is **not an M4 blocker**.

## M4 acceptance before release

After all landing-tail PRs are merged to real `main`, run the final-main acceptance set from `docs/agent/PRIME-CHECKPOINT.md`:

- `cargo test -p idoris-router --lib`
- `cargo test -p idoris-tenancy`
- `cargo test -p idoris-backend`
- `cargo test -p idoris-upstream`
- `cargo test -p idoris-contracts --test parity`
- `cargo test -p idoris-contracts --test contract_drift`
- `cargo test -p idoris-router --test provider_binding`
- `cargo test -p idoris-router --test model_identity`
- `cargo test -p idoris-router --test portable_startup -- --test-threads=1`
- `cargo test -p idoris-router request_event_query`
- `cargo test -p idoris-router audit`
- `cargo test -p idoris-router admin`
- `cargo fmt --check`
- strict clippy for contracts/tenancy/backend/upstream/router with `-D warnings`
- `git diff --check <pre-M4-main>...HEAD`

Acceptance evidence must explicitly show:

1. exact-ready provider/model transparency with one frozen readiness snapshot;
2. non-loopback bind refuses before bind without an active key and never permits `DEV_NO_KEY`;
3. record-id lookup returns the complete Event Log decision chain;
4. `/audit` reads Event Log only;
5. Admin API v0 + CLI + minimal console + Agent24 status card are all landed.

## Release after M4 DONE

Expected next release tag from the current checkpoint is `v0.3.0`.

Only after final-main M4 acceptance:

1. update release/version notes if required by the repository;
2. commit and push any release metadata;
3. create annotated `v0.3.0` tag on the accepted main commit;
4. push the tag;
5. publish the GitHub release;
6. record the exact release SHA/tag in `docs/agent/PRIME-CHECKPOINT.md`.

## M5 immediate next work

Enter M5 immediately after the M4 release.

First planned slice:

**M5-01 — `/v1/inspect` contract freeze**

- strict JSON Schema;
- Rust/TS parity;
- sensitivity `S0..S3`;
- findings with bounded kind + optional numeric span only;
- action `BLOCK | REDACT | ALLOW`;
- `additionalProperties:false`;
- Rust `#[serde(deny_unknown_fields)]`;
- no matched content/secret substring in responses or logs;
- define span indexing explicitly, preferably UTF-16 code units;
- no router wiring in M5-01.

Then continue with the pure L0 inspector, rule packs, endpoint wiring, INSPECTED route stage, kill switch and remote egress byte counter.

## Local cleanup requested by owner

Owner explicitly requested:

- commit and push all work that should be committed;
- write and push this handoff;
- remove local worktrees and local branches once their PR is merged;
- also remove local worktrees/branches for PRs that have already been submitted and no longer need local continued development;
- preserve active local work only when it is still needed for an open PR or immediate validation.

### Safe cleanup candidates

Any local worktree/branch corresponding only to these already-merged PRs is a cleanup candidate:

- #420, #415, #414, #413, #412, #411, #410, #409
- #407 through #399
- #397 through #391
- #390 through #375, except open #374
- #373 through #369
- #368
- #363
- #360 through #353
- #352 through #344 where merged
- other older merged/superseded M1–M3/M4 branches no longer used by an open PR

Also remove closed-unmerged superseded worktrees such as #419 if no later work depends on their local state.

### Keep until no longer needed

Keep local worktrees/branches for the current open landing tail until their PR is merged or no local work is needed:

- #361, #362, #364, #365, #366, #367
- #374
- #398, #408
- #416, #417, #418

Before deleting any worktree:

1. verify `git status --short`;
2. commit/push authorized changes if they belong to the task;
3. do not delete untracked or unrelated user files;
4. verify the branch exists remotely / PR contains the work;
5. remove the worktree normally;
6. delete the local branch only after the worktree is gone and the remote/PR is safe.

## Local execution caveat at handoff creation

The Chat On Steroids local execution connector was returning repeated internal errors while this handoff was created. Remote GitHub operations remained available. Therefore the remote handoff commit is authoritative for current planning, but the local worktree cleanup still needs to be executed on the Mac once the local execution connector is available.

Do not interpret the connector failure as read-only permission or as a Git problem.

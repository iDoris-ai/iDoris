# PR #507 consolidated review fixes

Exact reviewed head: `a232f8391107263408589a5c846dda524ebd0421`.
Merged requested `origin/main` `959337d444a9b0df371e43a7e0a2854ae894f8e3` first, in merge commit `6d6bb215ab11931857b671c95dd206e6256b1706`.
Final implementation head: `a69f954293c0dcddc4195dd08debdefe9fbd8d57` on `jhfnetboy/postrelease-onboarding-20261015`, authored by `jhfnetboy <jhfnetboy@gmail.com>`.
No push, PR/GitHub writes, watcher, Fast/priority selection, downloads, paid calls, real engine/model execution, or unrelated production edits. Root owns pre-pr-check, current-label checks and publication of the single batch.

## Finding-by-finding

- B1: backend/model preparation now precedes `idoris serve`. The guide explicitly describes released 0.3.0's latched `startup_error`, discovery 200 versus persistent chat 503, and restarting only the owned `IDORIS_PID`, waiting, then repeating startup/readiness. New loopback fixture starts iDoris while backend unavailable, captures startup failure, starts oMLX-shaped fixture, asserts models 200 plus unchanged chat 503 and no upstream chat, then restarts only the owned subprocess and asserts chat 200 / fixture-ok.
- F1: README now states paid resident buffered requests use budget reservation/settlement; paid streaming is rejected before upstream execution. Removed obsolete global startup rejection claim.
- F2: developer fixture instructions explicitly require the source checkout root containing scripts/, not the extracted binary package directory.
- F3: v0.3.0 snapshot fixture now asserts resident forwarded model exactly `idoris/daily` and lifecycle alias remediation exactly `model not found: omlx`.
- F4: capabilities instructions occur after server readiness and show the existing stdin curl config Bearer pattern using the virtual key.
- F5: health polling sleeps on every unsuccessful iteration, including non-200 responses. Existing harmless XDG_STATE_HOME setting in release smoke was retained.
- F6/info: no CI hook added. Guide and script mark this as a 0.3.0 behavior snapshot requiring expectation adaptation before 0.3.1.

## Verification

Archive: `/tmp/idoris-v030-onboarding-assets/idoris-v0.3.0-aarch64-apple-darwin.tar.gz`.
SHA256 independently checked: `ce54063f78b2d092c0ba820be4729635a5fe1df2ec91debeec10ef5dd582dfad`.
Extracted `/tmp/idoris-v030-onboarding-assets/package/idoris` bytes were compared directly to archive member `idoris`: match.

1. `python3 -u scripts/test_onboarding_smoke.py /tmp/idoris-v030-onboarding-assets/package/idoris` — exit 0, all four PASS groups: startup/version/auth/Admin; late-backend recovery; persisted key/discovery/resident/role denial/Admin CLI; packaged oMLX lifecycle/concrete model/known alias failure. Received resident model fields exactly `['fixture-model', 'idoris/daily']`.
2. `python3 -u scripts/test_onboarding_smoke.py /tmp/idoris-v030-onboarding-assets/package/idoris --late-backend-only` — exit 0, baseline startup plus dedicated late-backend sequence PASS, ending owned restart/chat 200.
3. `git diff --check` — exit 0.

The first development trial exposed Darwin's bound-but-not-listening socket reporting `oMLX GET /api/status timed out (transport error)` rather than connection-refused wording. The test accepts these two transport failures but pins the identical captured startup remediation before/after backend start, then requires actual recovery after owned restart; final full and dedicated runs pass.

Release smoke database guard remains byte-for-byte unchanged from a232f83. Existing bearing negative proof preserved at `/tmp/idoris-onboarding-review-negative.log` and `/tmp/idoris-onboarding-review-negative.py`: matching health without an isolated database is rejected with exit 1 and `default database was not created in isolated user state`; historical evidence, not rerun in this task. Broader independent context remains `/tmp/idoris-onboarding-independent-review.md`.

The preexisting untracked `idoris-v0.3.0-aarch64-apple-darwin.tar.gz.sha256` was preserved and not staged. Final tracked worktree is clean. Remaining work belongs to root: pre-pr-check/current labels and publication when review ownership permits.

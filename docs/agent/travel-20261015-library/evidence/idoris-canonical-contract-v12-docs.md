# iDoris canonical contract revision 1.2 — worker result

Completed 2026-10-15, local documentation only. New branch `jhfnetboy/agent24-contract-v12-20261015`, HEAD `11aba3fa858714b3fc083cf54ff54f48b3d25f65`, author/committer `jhfnetboy <jhfnetboy@gmail.com>`. Parent is exactly `03c079c70611331a8c85a1666e32a75df6359d09`; no reset, overwrite, push or GitHub write.

## Preservation and scope

Before switching, `git status --short --branch` showed clean original auth branch `jhfnetboy/postrelease-key-ceiling-20261015` at `1c9c592ffb8f8226e873cfbbc242abf2fe24e747`. That ref remains unchanged. Only the newly created docs branch was edited; Agent24, other iDoris branches/worktrees, jobs and worker ownership were untouched. No watcher, Fast/priority selection, paid/model invocation, download or software test run.

Exactly three files; diff from 03c079c is 136 insertions / 267 deletions, zero production/schema/test changes:

1. `docs/interfaces/iDoris-Agent24-边界与接口规范.md`: one coherent authority, consolidated R0–R8, preserved G1–G8/action authorization/roles/local_only/four credentials/read-only Admin/FD boundaries; document revision 1.2 explicitly separate from wire 1.0.1. Actual I0/A0, unmerged candidates and future v0.3.1 are separated, with artifact/source/test mapping and actual 15-schema inventory. Host facts/whole-machine allocation reverse-link fixed Agent24 HW-TIERS/ADR-036 proposals; #964/#966 are OPEN snapshot state, not main API. Explicit operator HTTP role map including auto and fail-closed, no first-model guess or cloud default. M5 console-user single-use confirmation stays; duration caller-consent remains deferred.
2. `docs/interfaces/negotiation/R9-v1.2-协商提案.md`: OWNER REVIEW PENDING decision record for launcher host, install execution flow and role labels. Local-only data plane independent of launcher; no unilateral downloader/write API/FD framing/labels. Future max_tokens, timeout, capabilities and readiness remain targets.
3. `docs/iDoris-总体规划.md`: exactly two row corrections (four changed diff lines): loopback default key required with explicit restricted dev exception; Admin v0 read-only with no model writes. Future M5 and historical links untouched.

Historical iDoris 09/17/handoff/R1 and Agent24 old entry-router/integration documents retain traceable links; Agent24's short consumer pointer is explicitly the other repository owner's work and is not edited here.

## Evidence and verification

Read `/tmp/idoris-agent24-canonical-design-opus.md`, `/tmp/agent24-idoris-v030-integration-audit.md`, `/tmp/idoris-v030-onboarding-audit.md`; checked real fixed git sources rather than treating audits as implementation truth. Baselines: iDoris 03c079c; Agent24 6072fd373142eb3dbf1a6a4a3222e6eec82c74f6; ADR/HW proposal 961a64085576eec365d0442ed27d2a7422168200; HW-1 f857ecfa4493b181d17a1d8e9323efde36ffe425. Unmerged candidate evidence explicitly names #507 a232f83, #508 1c9c592, #510 bbf99ec and budget a078b033bb503216fbc0f2e2bcc419d3a4118a15.

`git diff --check` and staged check passed. Local checker `/tmp/validate-contract-v12.py` verified 14 local link targets, 38 fixed source/artifact paths, original auth-ref preservation, exact new-branch base and 15 actual schema files; final commit parent/scope/status checked directly. Named test functions were verified in source, not executed. Validation caught and corrected stale audit-derived artifact paths (`scripts/test_onboarding_smoke.py`, Agent24 `rust/apps/agent24d` and `rust/apps/agent24-documents`, role binding `src/role_binding_tests.rs`) and avoided importing the audit's proposed runtime-version bump or treating draft ready output as actual. No full software tests were appropriate for this prose-only change.

## Remaining decisions / coordinator handoff

- Owner review OD-1 launcher host/lifecycle, OD-2 manual install guidance vs future local provision tool, OD-3 fast/daily catalog relabeling; all pending, none silently accepted.
- Agent24 owner creates consumer pointer linked to this exact contract commit and reverse hardware authority; reviews proposed local-only subset. This is needed to finish the user’s BOTH-repositories request and is outside this worker's authorized files.
- Code candidates remain unmerged; future provider/key bridge, runtime allocation enforcement, field/timeout compatibility, readiness and combined fixture/clean-machine acceptance are not delivered by this commit. Full M4 readiness and M5 start remain unclaimed.
- No live GitHub refresh was performed; OPEN/unmerged wording is explicitly the dispatch/audit snapshot, not a claim that main or PR state was mutated.

Draft integration vehicle for the M4 landing tail, opened so the stack below has a reviewable base and so final-main acceptance can be evidenced before the individual PRs land.

Merges (no rebase, no force): #418 head `ffe3e6e` (contains #361→#417), #408 head `e267624` (contains #398), #374 head `8937f84` onto main `792ed90`. All three merged without conflicts.

## M4 acceptance set on this branch (local, 2026-10-11)
- `cargo fmt --check` ✅
- `cargo test -p idoris-router --lib` 431 passed
- `cargo test -p idoris-tenancy` / `-p idoris-backend` / `-p idoris-upstream` ✅
- `idoris-contracts --test parity` / `--test contract_drift` ✅
- `idoris-router --test provider_binding` / `model_identity` / `portable_startup --test-threads=1` ✅
- `request_event_query` 3, `audit` 20, `admin` 27+1+5 passed
- strict clippy (contracts/tenancy/backend/upstream/router, `-D warnings`) ✅
- `git diff --check v0.2.0...HEAD` ✅
- GitHub CI on this head: success

Evidence gaps for claims 1/3/4 are closed in the stacked test PR (`test/m4-acceptance-gaps`).

Landing: prefer landing the individual tail PRs; once they are in main this PR's diff collapses to merge commits and can be closed or merged.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

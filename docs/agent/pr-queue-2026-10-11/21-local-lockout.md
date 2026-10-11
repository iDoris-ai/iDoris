M5-18 — single-model lockout for Supervisor-run cards, stacked on M5-17.

- Supervisor outcomes go into the same breaker book (local class: 2 faults / 15s). Model faults count (ModelUnavailable, Oom, probe/adapter timeouts, adapter panic, load unconfirmed/postcondition, Upstream, EvictionFailed); success resets; config/loading/capacity/cancel/Busy/budget/internal outcomes are ignored.
- The chat open set covers every policy candidate, so selection steers away from a locked model like from an open proxy.
- `DispatchInput::with_blocked_providers` feeds the same set into the legacy fallback re-decide. (In that branch `supervisor` is always `None`, so it never dispatched to a locked model; but it answered `local_only_unavailable` while Decided said `no_eligible_candidate`. Now they agree.)

Tests: a model whose loads keep failing is locked after two faults and the third chat goes to the other model (Decided names it); with the only model locked, response type == decided rejection reason. Dropping the fallback set or the fault reporting turns them red. Router lib 460.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

M5-16 — chat selection steers away from open provider breakers, stacked on M5-15 (before this, an open provider made the request fail fast with 503 even when another candidate could serve).

- `Breaker::would_admit` / `BreakerBook::would_admit`: read-only, never claims the half-open probe.
- `ChatProxy::open_providers`: resident candidates whose breaker would refuse now (poisoned lock → all open).
- Chat path → `dispatch::select_avoiding_open_breakers` marks them `AdmissionStatus::Blocked`; `decide()` already drops Blocked at its admission stage, so another candidate wins and Decided names it. All open → `no_eligible_candidate` before any send.

Test: two equal resident cards; after the first one's breaker opens (two 500s) the third chat is served by the second, Decided says so; empty open set turns it red.

**Not covered — Supervisor path:** the fallback after a failed selection (`dispatch_local` re-decides without this set) would bypass a lock on a Supervisor card, so single-model lockout needs `DispatchInput` to carry the blocked set first.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

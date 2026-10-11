M5-08 — remote kill switch core, stacked on M5-07.

- `AppState.remote_kill_switch: Arc<AtomicBool>`, read per request by gate one (chat, protocol endpoints, `/v1/inspect`): while engaged remote is not allowed, so `any` → `local_only` and `/v1/inspect` answers BLOCK.
- Chat `inspected` event records `reason=remote_kill_switch` when the switch (not content) pinned it.
- Startup: `IDORIS_REMOTE_KILL_SWITCH=1` (same `env_flag` convention as B3's `IDORIS_DISABLE_SUBSCRIPTION`), with a stderr warning.

**Not in this PR — owner decision:** the runtime trigger. docs/15 says it may only be flipped from the management port/UI; Admin v0 is read-only by decision (#419). A test flips the shared Arc between two requests on one app and egress stops on the next request, which is what an Admin v1 write route would do.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

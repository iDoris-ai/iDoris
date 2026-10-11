M5-14 — provider-grain breaker on the resident proxy's **buffered** sends, stacked on M5-13.

> **Opt-in.** The `idoris` binary keeps breakers off unless `IDORIS_PROVIDER_BREAKER=1` (fix commit on this branch). CI showed on-by-default breaks the shared conformance contract — `upstream-behavior.test.ts` sends 500 then 503 to one local upstream and expects later cases to still reach it ("persistent 5xx passed through verbatim"). Enabling by default = owner decision + conformance update. Library-constructed proxies (tests) keep them on.

- `ChatProxy` owns a `BreakerBook` keyed by provider id. Before the dispatched observation an open provider is refused: 503 `provider_circuit_open` / NotExecuted — nothing sent, nothing recorded as dispatched, reservation released as usual.
- Buffered loop moved into `send_buffered` (behaviour unchanged) so its single outcome is reported: 408/429/5xx (incl. the proxy's own transport 502/504) → failure, anything else → success; 429/503 `Retry-After` keeps it open at least that long. Cache hits and local preflight exits never reach the breaker.
- Class from dialed URL: loopback → Local (2 failures/15s), else ApiKey (5/30s). Poisoned lock fails closed.
- Breaker fix: a half-open probe that never reports is replaced after one window (otherwise a cancelled probe pins the provider shut forever).

Not yet: Supervisor and subscription paths; steering selection away from an open provider (today: fail fast 503). One retention test opts out explicitly (it sends eight 503s on purpose).

🤖 Generated with [Claude Code](https://claude.com/claude-code)

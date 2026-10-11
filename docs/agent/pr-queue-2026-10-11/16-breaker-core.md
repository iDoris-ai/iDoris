M5-13 — pure circuit breaker for the plan's §4.4 three-grain resilience; **no wiring, no clock** (callers pass `now_ms`).

- CLOSED → OPEN after B-8 thresholds (OAuth 3 / API key 5 / local 2 consecutive failures); OPEN rejects with remaining wait; after 60s/30s/15s HALF_OPEN admits exactly one probe; probe success closes, failure reopens for a full window; late failures don't extend an open window.
- `on_retry_after` opens at least until the upstream's Retry-After, never shortens; `parse_retry_after_ms` = integer seconds only (HTTP-date falls back to the class window).
- `BreakerBook<K>`: provider / connection / (provider, model) are the same machine under different keys.

Every hand-run mutation turns the suite red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

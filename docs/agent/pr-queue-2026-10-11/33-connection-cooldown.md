M5-25 — connection-grain cooldown (plan §4.4 "connection cooldown honouring Retry-After"), stacked on M5-24.

- A 429/503 `Retry-After` now cools the **connection** (`connection:` + scheme/host/port origin of the dialed URL), so every card behind that server waits at least that long; failure counting stays per provider.
- Sends check the connection read-only before claiming a provider's half-open probe.
- `open_providers` takes cards and reports one open if its provider or its connection is → chat and protocol selection steer away from every card on a cooling host.

Tests: card-b on the same host refused after card-a's 429 RA, other host served; three resident cards (two on a cooling host) → next chat served by the third. Negative controls for both. Opt-in like every breaker.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

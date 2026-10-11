M5-09 — off-host egress byte counter for the M5 "remote egress is 0 bytes" check, stacked on M5-08.

- Every `ChatProxy` upstream POST goes through one `outbound` helper: serialize once, record, send exactly those bytes.
- `EgressCounter` counts requests + body bytes **per attempt, before the send**, when the dialed URL's host is not `localhost`/a loopback literal. Classified by the URL actually dialed, not the card's claimed locality. Over-counting a failed attempt is the safe direction.
- `ChatProxy::egress()` snapshot only; no wire surface yet (Admin v0 and `/capabilities` contracts are frozen).

Scope: ChatProxy is the content-carrying HTTP path. Local runtimes and `/v1/models` discovery are not content egress; the subscription CLI relay is a separate path, not counted here.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

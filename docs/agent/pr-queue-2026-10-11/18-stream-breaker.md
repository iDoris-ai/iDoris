M5-15 — streaming chat/messages sends behind the same provider breaker, stacked on M5-14.

Stream APIs take the card's provider id; open → buffered 503 NotExecuted before the dispatched observation. Only the initial response counts (status, Retry-After, header timeout, transport error); a started 2xx stream's later fate does not trip the breaker. Test-facing `forward_stream(endpoint, body)` keys by endpoint.

Test: 503, 500, then refused with only two upstream requests; removing the gate turns it red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

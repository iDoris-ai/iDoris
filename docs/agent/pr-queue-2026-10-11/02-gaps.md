Tests only. Closes three gaps in the M4 acceptance evidence found while mapping the handoff's five claims to tests.

- **Claim 3, record-id → full decision chain:** a real paid-local chat, then `GET /idoris/tenants/{t}/requests/{record_id}` over HTTP returns the ordered chain request.received → profiled → decided → budget.reserved → dispatched → completed → budget.settled → audit.finalized, tenant-scoped, with a prompt sentinel absent from the response (positive control: present in the chat response).
- **Claim 4, /audit reads Event Log only:** legacy-record-store-only config → 503 `audit_unavailable`; with both stores a legacy-only row never surfaces.
- **Claim 1, exact-ready transparency:** two equal-cost local targets, session pinned, only one warmed; in both orientations the ready one serves (body `model`, Decided/Dispatched `provider_id`), the other upstream gets zero loads, and exactly one readiness snapshot is taken.

Reverse checks run by hand: empty readiness snapshot → claim-1 test fails; /audit reading the legacy store → both claim-4 tests fail.

Observation for #408 review (not a test failure): readiness is model-id only, not load-policy aware. Warming with `idle_ttl_s: 60` while dispatch defaults to 300 still prefers the ready target but calls `load` again (policy re-apply). Whether that second load is expensive on a real backend is unverified.

`cargo test -p idoris-router --lib`: 435 passed.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

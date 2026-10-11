Tests only — M5 acceptance item "并发 100 个请求不会击穿预算", end to end through the router (ledger-level race tests already live in `idoris-tenancy/tests/budget_concurrency.rs`).

Budget = exactly ten of the router's own pre-execution reservations; 100 paid chats race on a multi-thread runtime. Asserted: only 200/402, both occur, settled spend ≤ limit, ledger == sum of `X-iDoris-Cost-Minor`, no leaked reservation. Stable over repeated runs.

Negative control (by hand): ledger configured 1000× larger, same asserted limit → fails "the budget must actually be contended".

🤖 Generated with [Claude Code](https://claude.com/claude-code)

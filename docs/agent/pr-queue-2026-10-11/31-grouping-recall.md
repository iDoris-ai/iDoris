Fix from a **fourth** independent review, stacked on the third-review fix. Final digit-group rule for tonight.

- Fourth review: decomposition DP correct and linear, but recall regressed — card + expiry/CVV in one group (`4111 1111 1111 1111 12/28`), phone after a time, two phones + a digit were missed; and any-grouping whole groups flagged 1.8% of small-integer rows.
- Now: multi-run windows must be a real written form (mobile 3-4-4/3-8/7-4/4-3-4 + 86 variants; id 6-8-4/6-12/6-4-4-4 + X forms; legacy 6-6-3/6-9; card 4-4-4-4[-1..3]/6-13); the group is one decomposition into such entities, optionally with ≤ 2 short numbers of ≤ 3 digits at each end (expiry, CVV, hour).
- Measured (5000 random rows each): small integers 0% (was 1.8%), 5–6 four-digit numbers 0%, 8 four-digit 0.26%, 20 numbers 0%. Remaining: groups written exactly like a phone (3-4-4) or card (4-4-4-4 + IIN + Luhn), as since M5-02.
- **Owner trade-off:** 4-digit neighbours (year, room: `Room 1203 138 1234 5678`) are not allowed — allowing them flagged ~10% of 5-number rows.

Second commit: a timeboxed check (400k fuzzed groups: no crash, no entity-free decomposition, all common forms caught) found one realistic miss, the `0086` prefix — now accepted (`0086 138 1234 5678`, `0086-13812345678`, `008613812345678`).

Router suites green; Rust conformance 134 passed / 8 todo locally; worst-case 256K ~100 ms.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Fix from a **third** independent review, stacked on the second-review fix.

- (med) the second fix's single-canonical-grouping rule missed ordinary writings (`138-12345678`, `1381 234 5678`, `110101 1949 1231 0023`); (med) and still flagged 22% of random 8-number rows / 53% of 20-number rows as cards.
- Now: each run alone + the **whole group in any grouping** (original M5-02 behaviour) + several entities **only when the whole group decomposes** into neighbouring entity windows (`110101 19900307 7379 138 1234 5678` → id + phone).
- Measured (5000 random rows each): 8×4-digit 0.36% (was 22%), 20 numbers 0% (was 53%). Remaining FPs = a whole group that is itself 11/16 digits, as in M5-02.
- **Trade-off for the owner:** an entity glued to an unrelated number in one group (`Room 1203 138 1234 5678`) is not found.
- Also: `ModelUnavailable` counts toward lockout again (also follows unconfirmed loads / failed ready checks); escaped math-digit surrogate pairs fold.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

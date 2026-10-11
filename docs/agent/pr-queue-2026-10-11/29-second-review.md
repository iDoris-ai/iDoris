Fixes from a **second** independent review (of everything after the first review), stacked on the window-floor fix.

| sev | finding | fix |
|---|---|---|
| high | tab/newline joining + arbitrary windows → logit_bias, TSV tables, ports, years flagged (most random numeric tables) and pinned local | tabs/newlines no longer join; multi-run windows only in real groupings (id 6-8-4 / 6-8-3+X, legacy 6-6-3, mobile 3-4-4 / 86-3-4-4, card 4-4-4-4[-1..4]); cards need a real issuer prefix (3–6, 2221–2720, 81) |
| med | "split across parts" never worked through the router (keys interleave) | claim withdrawn; documented as a known gap in the test |
| med | `\uXXXX`: lone surrogate voided decode; escapes not joined with context | escapes decoded in place in normalization (spans cover whole escapes); math digits U+1D7CE.. fold |
| med | report-only breakers (Supervisor, subscription) never re-locked after the window | failure after the window reopens for a full window |
| low | lockout counted ModelUnavailable (incl. concurrent unload) and EvictionFailed (other model) | no longer counted |
| low–med | short token arrays readable; 8+ int enums in tool schemas pinned local | integer arrays under `input` are unreadable at any length; elsewhere readable; part `type` case-insensitive |
| low | decided said "content" for uninspectable/failed scans | only for findings |

Negative controls for the token rule, decided guard and breaker reopen each go red. Router lib + integration green; Rust conformance 134 passed / 8 todo locally.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

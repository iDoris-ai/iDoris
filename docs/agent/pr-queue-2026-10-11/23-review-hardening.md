Fixes from an independent adversarial review of the whole M5 stack, stacked on M5-19. The reviewer reproduced six fail-open paths (declared-`any` body with an identifier → 200 and upstream hit on an `any`-only card); each now has a regression test.

| # | sev | fix |
|---|---|---|
| F1/F2 | high | unreadable content (token-id arrays ≥ 8 ints, `data:…;base64,`, image/audio/file/document parts) pins `local_only`; inspected `reason=uninspectable_content` |
| F3 | med | digit groups judged on every run of neighbouring segments (≤ 19 digits): `110101 19900307 7379 138 1234 5678` → id + phone |
| F4 | med | newline/CR/tab join digit groups, ≤ 3 separators: ids split across text parts / lines |
| F5 | med | NBSP & other spaces fold; LRM/RLM/ALM + bidi controls dropped; Arabic-Indic digits fold |
| F6 | med | `0x` hex; base64 glued to an alphabet prefix; `\uXXXX` escape runs |

Removing the opaque rule turns the router regressions red. Rust conformance passes locally (134 passed / 8 todo). Worst-case 256K adversarial scan ≈ 150 ms (release), linear.

Review items **not** fixed (in the handoff): homoglyphs; decided reason says `privacy_loopback_only` (tightening is only in `inspected`); scanning on the async worker; REDACT R-1 "swallowed placeholder" check; `/v1/inspect` `remote_allowed` ignores tenant routing policy (reviewer unsure). Reviewer found no issue with INSPECTED placement, privacy never being loosened later, idempotency replay, or span/UTF-16 arithmetic.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

M5-30 — gate-two credential broker **core** (docs/15 §2 approach A), storage-free: strict `secret://` refs, grants (exact HTTPS host, methods, segment-bounded path prefix, header/scheme), `authorize()` before any reveal (C-2), `inject()` overwriting every caller copy of the header (C-1), value-free audit, redacted `Secret`. Keychain store / grant config / `/v1/broker/http` wire / HMAC fingerprint need owner decisions. Stacked on M5-29.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

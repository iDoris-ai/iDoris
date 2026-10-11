M5-06 — `INSPECTED` stage on `/v1/chat/completions` (docs/15 §1.5), stacked on M5-05.

- Right after the key scope admits the declared privacy, the **whole body** (every key, string and number — content parts, tool definitions/arguments, metadata) is inspected. Anything short of ALLOW pins the request to `local_only` through `effective_privacy` (tighten-only).
- Placement matters: it runs **before** profiling, the YAML routing policy, and the remote intent detector (which only runs for `any`), all of which now see the effective privacy.
- New `inspected` event between `request.received` and `profiled`: `sensitivity`, effective `privacy`, distinct finding kinds as `labels`, `reason=privacy_tightened_by_content` only when `any` was tightened. No spans, no content. Append failure → generic 503 before profiling/egress.

**M5 acceptance shape, tested:** an id number in plain content, a phone number in an array content part, a card number in tool-call arguments, or a phone number as a JSON number → `local_only_unavailable` with **zero requests** on a non-local card, with or without an Event Log. Removing the privacy overwrite turns these red.

17 existing chain tests updated for the extra event (no assertion weakened; several now assert full type lists). Router lib 443, all integration suites green.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

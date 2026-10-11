M5-21 — scan large bodies off the async worker (review low item), stacked on the review hardening.

- `inspect_body_on_request_path`: bodies ≥ 32 KiB are scanned on the blocking pool; smaller ones inline.
- Blocking-task failure → `local_only`, S3/BLOCK, inspected `reason=inspect_failed` (docs/15 §1.5): a scanner failure never lets content out.
- Used by chat INSPECTED and the three protocol endpoints.

Tests: same verdict off-worker vs inline for a 32K+ body; failure fallback semantics.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

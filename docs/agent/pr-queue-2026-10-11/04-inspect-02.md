M5-02 — pure L0 inspector in `idoris_policy::inspect` (no IO, no model, no regex backtracking). Returns the frozen M5-01 `InspectResponse`. No router wiring.

- L0: CN resident id (GB 11643 province + date structure; MOD 11-2 check digit decides S3 vs S2 per X-2; 15-digit legacy ids), CN mobile (incl. 86 prefix), bank card (16-19 digits, Luhn required).
- Scans UTF-16 code units directly, so spans need no conversion. Digit groups joined by single space/hyphen are judged whole and per segment (adjacent phone + card both found).
- L2 (docs/15 §1.3): default BLOCK. ALLOW only for zero hits + declared `any` + remote allowed; REDACT only for S1/S2 with consent. Oversize input BLOCKed unscanned.
- Tests: proptest bounds + recall property (every generated valid id in arbitrary non-digit text is found as S3); all 10 hand-run mutations of the rules turn the suite red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

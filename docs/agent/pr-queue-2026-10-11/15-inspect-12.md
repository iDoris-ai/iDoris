M5-12 — reversible redaction engine (docs/15 §1.4), **pure, not wired**: when REDACT may be used depends on the caller-consent wire format (owner decision, see M5-05).

- `redact(input, verdict, ctx)` accepts only a REDACT verdict; overlapping hits merge; stable `[KIND_N]` per distinct value, numbered per kind.
- Redacted text is re-inspected with the full L0; any residual hit fails closed.
- `restore()`: R-2 unknown placeholder → refuse; R-1 mangled known placeholder (no brackets, `_`→space/hyphen) → refuse; ordinary `[1]` passes.
- `stream()`: chunk-wise restore; holds an unclosed `[` for at most one placeholder length; mangling checked across chunk boundaries.
- R-4: table only in memory; `Debug` prints counts, never originals.
- contracts: `FindingKind::as_str()` (asserted equal to serde values).

Every hand-run mutation (rescan, stable mapping, R-1, R-2, cross-chunk tail, hold bound, overlap merge) turns the suite red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

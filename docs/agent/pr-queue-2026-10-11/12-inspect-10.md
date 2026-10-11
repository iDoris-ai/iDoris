M5-10 — fold L0 evasions before scanning (docs/15 §1.6 "invisible" class), stacked on M5-09.

Drops zero-width/format chars (ZWSP/ZWNJ/ZWJ, WJ, BOM, soft hyphen); folds full-width ASCII and the ideographic space; folds hyphen/dash/minus variants to `-`. Normalization only drops or maps 1:1, so spans map back to the caller's original text (a dropped char inside a hit stays inside the span).

Tests: full-width phone/id/email, dash variants, ideographic-space card groups, zero-width inside an id and an API key prefix, and a proptest that random zero-width noise never hides a valid id. Each folding rule and the span mapping, removed alone, turns the suite red. Still open: homoglyphs and encodings (base64/hex/URL).

🤖 Generated with [Claude Code](https://claude.com/claude-code)

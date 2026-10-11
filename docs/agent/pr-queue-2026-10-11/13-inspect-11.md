M5-11 — decode one layer of encoding before L0 (docs/15 §1.6 "encoding" class), stacked on M5-10.

- Candidates cut twice: base64/hex runs (standard or URL-safe alphabet + up to two `=`; `payload=<token>` splits at the `=`), and percent-encoding runs (≥ 3 `%XX`).
- Each candidate decoded once; valid UTF-8 is normalized and scanned with the same plain L0; hits are reported with their kind/sensitivity over the whole encoded token in the original text.
- One layer only (nested decoding is unbounded; long opaque blobs are already caught by set B). `scan_plain` keeps decoding non-recursive.

Tests: base64, unpadded base64url (with `-`), percent and hex forms of an id, phone and card; harmless encoded text stays clean. Removing hex, base64, the URL-safe alphabet or the percent path turns the suite red. No new dependency.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

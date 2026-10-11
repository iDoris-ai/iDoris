M5-23 — fold digit scripts and Latin homoglyphs before L0 (review gap), stacked on M5-22.

- All 35 BMP Unicode `Nd` digit blocks (Thai, Devanagari, Khmer, Myanmar, …; verified against `unicodedata`) fold to ASCII digits.
- Cyrillic/Greek letters that render as Latin fold to Latin, so `ѕk-…` / `АКIА…` still hit key prefixes.
- 1:1 per UTF-16 unit → spans still point at the original text.

Tests: Thai-digit phone, Devanagari id (span = original), Cyrillic-spoofed OpenAI/AWS prefixes; Russian/Greek sentences stay clean. Removing Thai digits, Cyrillic DZE or Cyrillic A turns it red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

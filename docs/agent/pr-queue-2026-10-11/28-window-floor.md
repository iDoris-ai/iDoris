Follow-up to the review hardening, stacked on the docs PR. The new "every run of neighbouring digit segments" rule also joined single digits: `sort: 3 1 4 1 5 9 2 6 5 3 5 8 9 7 9 3 2 3 8 4 6` produced two "mobile numbers" and a Luhn-valid "card" and pinned an ordinary request local.

Multi-run windows now require every run ≥ 3 digits (true for 3-4-4, 4-4-4-4, 6-8-4, ids split across lines); single runs unaffected. Test: number lists stay clean, real groupings still hit; removing the floor reproduces the false positives.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Fixes from a **fifth** independent review (M5-24..30), stacked on M5-30: uninspectable reason for embedded data URLs; breaker keys split into Provider/Connection (no id collision); single-probe after a connection cooldown; credentials refuse `..;`, `%25`, invalid grants, control-char values, and drop every caller credential/billing header; de-correlation also drops `messages[].name`. Each fix has a regression test that goes red when reverted.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

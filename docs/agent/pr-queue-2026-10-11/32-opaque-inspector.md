M5-24 — the public `/v1/inspect` now treats an embedded `data:<type>;base64,` payload (markdown image, HTML src, …) as uninspectable: BLOCK at S3, like an unscanned oversize input. The route stage already pinned such requests local; the pure inspector still said ALLOW because decoded binary fires no rule. Plain `data:` text, `metadata:` and non-base64 data URLs unaffected. Stacked on the final digit-group fix.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

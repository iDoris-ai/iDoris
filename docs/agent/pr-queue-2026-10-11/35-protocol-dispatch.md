M5-27 — `dispatched` (pre-send observer) and `completed` (only for executed upstream calls) on buffered /v1/embeddings, /v1/rerank, /v1/messages, completing the chain started in M5-26. Replays/cache hits record nothing. /v1/messages streaming still stops at `decided`. Stacked on M5-26.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

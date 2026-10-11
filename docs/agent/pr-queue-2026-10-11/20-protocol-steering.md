M5-17 — `/v1/embeddings`, `/v1/rerank`, `/v1/messages` select through `select_protocol_card` (same Blocked marking), so a healthy candidate serves instead of failing fast at the send gate. Stacked on M5-16.

Test: two embedding cards; after the first opens on two 503s, the third request goes to the second; empty open set turns it red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

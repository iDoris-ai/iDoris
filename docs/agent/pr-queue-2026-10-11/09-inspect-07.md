M5-07 — same gate one on `/v1/embeddings`, `/v1/rerank`, `/v1/messages`, stacked on M5-06.

After the key scope and before the routing policy, the whole body is inspected and anything short of ALLOW pins it to `local_only`. These endpoints have no Event Log chain yet, so the verdict only tightens routing; recording `inspected` there belongs with their chain.

Tests: clean `any` bodies still reach an `any`-only card; an id/phone in an embeddings input, a rerank document, or an Anthropic `system` block → `local_only_unavailable`, zero upstream requests. Bypassing the helper turns the zero-egress test red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

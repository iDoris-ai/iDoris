M5-05 — `POST /v1/inspect`, stacked on M5-04. Classifies only; nothing rewritten, dispatched or stored.

- Body parsed with the strict contract; any failure is a fixed 400 `invalid_body` that never echoes serde's message (it can quote the value).
- Privacy from `X-iDoris-Privacy` (absent = local_only); tenant scope + auth via the existing profile/virtual-key path.
- **Only the key's privacy ceiling applies.** `enforce_protocol_key_scope` is split into `enforce_key_scope_with` (same auth, injected scope check). Without this, `enforce_scope` with no role demands full role authority, so a role-limited key (Agent24's) would get 403. Negative control: using the role-shaped check turns the role test red. Protocol endpoints unchanged.
- **Redact consent has no wire spelling yet**, so it is always false and personal data answers BLOCK, never REDACT. Owner decision needed on the consent header/field.

router lib 438 passed; all router integration suites green.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

M5-01 — `/v1/inspect` contract freeze. Schema + Rust/TS parity only, **no router wiring**.

- `inspect-request.schema.json`: `{input: string}`, closed.
- `inspect-response.schema.json`: `sensitivity` S0..S3, `findings: [{kind, span?}]`, `action` BLOCK|REDACT|ALLOW. Every object `additionalProperties:false`; Rust `#[serde(deny_unknown_fields)]`.
- `kind` is a closed enum (14 values incl. `input_limit_exceeded` for X-1 oversize). A new kind is a contract change.
- `span = {start, length}` in **UTF-16 code units**, half-open, `length ≥ 1`. `start+length` rather than `start+end` so every rule is expressible in the schema itself and Rust/TS cannot disagree on an inverted range. Explicit `span: null` rejected on both sides.
- No free-form string anywhere in the response; a drift test walks the schema to enforce it (negative control: injecting `{"type":"string"}` turns it red). `InspectRequest`'s `Debug` never prints the input.

Decisions for the owner:
1. Request shape `{input: string}` (one string; callers inspect each part). Alternative was OpenAI-style `messages`.
2. Finding-kind list (see schema).
3. Wire version stays `1.0.1`; bump when the endpoint ships?

Gates: contracts parity/drift, `pnpm --filter @idoris/contracts test` (161), `pnpm check:contract-drift`, typecheck, lint — all green; CI green.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

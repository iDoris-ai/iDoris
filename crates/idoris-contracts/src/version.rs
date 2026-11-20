/// Contract version reported by `GET /health`'s `contract_version` field
/// (see `docs/interfaces/iDoris-Agent24-边界与接口规范.md` §3.12 / R1 task
/// brief for PR #46). Mirrors `packages/contracts/src/version.ts`'s
/// `CONTRACT_VERSION`. `pnpm check:contract-drift` now checks this constant,
/// the TS constant, and conformance's `/health` expectation together, so a
/// one-sided bump fails in CI.
///
/// `1.0.1` (R2-G): matches the TS side's bump (PR #46 复审后的加法修订 —
/// response-header/error-body detail fixes, no breaking field-semantics
/// change) — `conformance/tests/response-headers.test.ts` locks this exact
/// string, not just "some non-empty version".
///
/// `1.1.0`: additive only. `GET /capabilities` entries gain optional choice
/// fields (`degraded_alternative`, `degraded_alternatives`,
/// `headroom_breach_option`), and `POST /v1/chat/completions` accepts the
/// per-request `X-iDoris-Headroom-Breach: confirmed` header.
///
/// `1.2.0`: additive only (interface spec §2.9). New read-only endpoints
/// `GET /v1/recommendation?role=chat` and `GET /v1/models/local`; every 1.1.x
/// field and behavior is unchanged.
///
/// `1.3.0` (the target of iDoris v0.6.0, **not yet in force**): additive only (interface spec
/// revision 1.6 §2.11, IDORIS-CORE). `GET /health` gains `phase`, `ready`, `started_at_ms`,
/// `registered` and `upstreams[]` (each with `engine_kind`); `status` stays `"ok"`, every 1.2.x
/// field and behavior is unchanged. The launcher-hosting additions of the same revision (the
/// `--launcher-stdin` frame, dynamic ports, the discovery file) are launcher contract (§2.7),
/// not HTTP wire.
///
/// **The new `/health` fields have been present since IDORIS-CORE slice S1, but this constant — and
/// therefore `/health.contract_version` and the discovery file's `contract_version` — only becomes
/// `1.3.0` when v0.6.0 is released (slice S11 bumps it everywhere at once).** Until then a build
/// reports `1.2.0` even though it already carries the new `/health` fields (spec §2.11: before the
/// release `/health.contract_version` stays `1.2.0`): the other `1.3.0` features (warmup, the
/// embeddings companion card, the Admin `upstreams` routes) are not all there yet, and a caller must
/// not conclude from the version number that they are. **Callers detect the new fields by whether
/// they are present in the response, never by comparing the version number.**
pub const CONTRACT_VERSION: &str = "1.2.0";

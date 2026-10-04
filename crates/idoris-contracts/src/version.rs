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
pub const CONTRACT_VERSION: &str = "1.0.1";

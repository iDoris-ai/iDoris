/// Contract version reported by `GET /health`'s `contract_version` field
/// (see `docs/interfaces/iDoris-Agent24-边界与接口规范.md` §3.12 / R1 task
/// brief for PR #46). Mirrors `packages/contracts/src/version.ts`'s
/// `CONTRACT_VERSION`. `pnpm check:contract-drift` now checks this constant,
/// the TS constant, and conformance's `/health` expectation together, so a
/// one-sided bump fails in CI.
///
/// `1.1.0` (M5): adds `/v1/inspect` with `{input: string}` and UTF-16
/// finding spans. Existing field semantics remain unchanged.
pub const CONTRACT_VERSION: &str = "1.1.0";

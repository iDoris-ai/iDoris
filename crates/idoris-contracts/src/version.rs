/// Contract version reported by `GET /health`'s `contract_version` field
/// (see `docs/interfaces/iDoris-Agent24-边界与接口规范.md` §3.12 / R1 task
/// brief for PR #46). Mirrors `packages/contracts/src/version.ts`'s
/// `CONTRACT_VERSION` — keep the two in sync by hand until a shared source
/// exists; a mismatch here would fail `idoris-router`'s `/health` contract
/// test if one is ever added downstream.
pub const CONTRACT_VERSION: &str = "1.0.0";

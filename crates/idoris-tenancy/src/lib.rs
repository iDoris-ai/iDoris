//! See `README.md` in this directory for scope and the TS package this
//! crate corresponds to. R1 staked out the module layout; R2-C (this and
//! follow-up changes) fills in `budget` with a SQLite-backed, atomic
//! reserve/settle budget ledger. `billing` resolves monthly UTC ranges;
//! `store` has an initial record-schema migration; record operations
//! remain future work.

/// Tenant-scoped record store: `TenantStore`, `TenantScopeError`
/// (`packages/tenancy/src/store.ts`). A query without tenant context must
/// error, never silently return the full unscoped table.
pub mod store;

/// SQLite-backed budget ledger (R2-C): atomic two-phase `reserve`/`settle`
/// (plus `release` for failed/fallback calls), scoped to
/// `(tenant, key, provider, model)`, bucketed by billing period. See
/// `budget` module docs for how this relates to
/// `packages/tenancy/src/budget.ts`.
pub mod budget;

/// Monthly usage aggregation with an explicit per-tenant billing timezone
/// (`packages/tenancy/src/billing.ts`, T2.6.1). Month boundaries are always
/// resolved in the tenant's configured `billing_timezone`, never the
/// server's local time or a caller-supplied override.
pub mod billing;

/// Tenant-scoped usage records consumed by monthly billing aggregation.
pub mod usage;

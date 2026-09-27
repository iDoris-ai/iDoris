//! Skeleton crate — see `README.md` in this directory for scope and the TS
//! package it corresponds to. R1 only stakes out the module layout; no
//! business logic is ported here yet.

/// Tenant-scoped record store: `TenantStore`, `TenantScopeError`
/// (`packages/tenancy/src/store.ts`). A query without tenant context must
/// error, never silently return the full unscoped table.
pub mod store {}

/// Per-tenant budget checks and charging (`packages/tenancy/src/budget.ts`):
/// `checkBudget`, `charge`, `BudgetDecision`.
pub mod budget {}

/// Monthly usage aggregation with an explicit per-tenant billing timezone
/// (`packages/tenancy/src/billing.ts`, T2.6.1). Month boundaries are always
/// resolved in the tenant's configured `billing_timezone`, never the
/// server's local time or a caller-supplied override.
pub mod billing {}

# idoris-tenancy

Skeleton only — module layout staked out for a later task to fill in. Ported
from (and must stay behaviorally equivalent to) **`packages/tenancy`** (TS,
kept as the reference implementation; see the root `README.md`).

## Scope (mirrors `packages/tenancy/src/*`)

| Rust module | TS source | Responsibility |
|---|---|---|
| `store` | `src/store.ts` | `TenantStore`, `TenantScopeError` — every read/write is tenant-scoped; missing tenant context is a hard error, never a fallback to the full table. |
| `budget` | `src/budget.ts` | `checkBudget`, `charge`, `BudgetDecision` — reject with `402` when a priced candidate would exceed the tenant's budget; the billing counter must stay untouched on rejection. |
| `billing` | `src/billing.ts` (T2.6.1) | Monthly usage aggregation. Month boundaries (`period=YYYY-MM`) are resolved in the tenant's own `billing_timezone` (from `TenantContext`, see `idoris-contracts::tenant`) — never the server's local timezone, never a caller-supplied override. Records store UTC epoch millis; timezone conversion happens only at aggregation time. |

## Explicitly not done in R1

No business logic is ported. This crate exists so `crates/idoris-router` (or
whatever needs tenancy) has a stable dependency to point at once a follow-up
task moves the logic over. Depends on `idoris-contracts` for `TenantContext`.

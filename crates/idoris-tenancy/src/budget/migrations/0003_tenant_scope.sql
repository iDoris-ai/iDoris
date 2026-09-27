-- R2-C-fix migration 0003: tenant-level scope (Opus Tier-2 acceptance H2).
-- contract-tenancy §3's tenant-granularity limit; 0001's `budget_config`/
-- `budget_periods` (the full tenant/key/provider/model tuple) become an
-- *optional* finer sub-limit layered on top — see `ledger.rs` `reserve`.

CREATE TABLE IF NOT EXISTS tenant_config (
    tenant_id        TEXT PRIMARY KEY,
    limit_minor      INTEGER NOT NULL,
    billing_timezone TEXT NOT NULL,
    -- paid_only (default): cost=0 bypasses the check. all: always checked.
    scope            TEXT NOT NULL CHECK (scope IN ('paid_only', 'all'))
);

-- Tenant-level settled spend per period, summed across every sub-scope.
CREATE TABLE IF NOT EXISTS tenant_periods (
    tenant_id   TEXT NOT NULL,
    period      TEXT NOT NULL,
    spent_minor INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, period)
);

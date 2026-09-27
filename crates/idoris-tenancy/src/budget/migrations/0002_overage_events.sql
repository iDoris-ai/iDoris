-- R2-C-fix migration 0003: overage audit trail (Opus Tier-2 acceptance M1).
--
-- `settle()` inserts a row here whenever actual_cost_minor > reserved_minor
-- ("this call cost more than we reserved for it") so the overage is
-- independently auditable via SQL, not only visible transiently in the
-- `SettleReceipt` returned to the one caller of that `settle()` call.

CREATE TABLE IF NOT EXISTS budget_overage_events (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL,
    reservation_id    TEXT NOT NULL,
    reserved_minor    INTEGER NOT NULL,
    actual_cost_minor INTEGER NOT NULL,
    overage_minor     INTEGER NOT NULL,
    created_at_ms     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_budget_overage_events_tenant
    ON budget_overage_events (tenant_id, created_at_ms);

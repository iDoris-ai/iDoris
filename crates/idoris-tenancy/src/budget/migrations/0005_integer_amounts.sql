-- M3: rebuild both accumulated-amount columns with integer type constraints.
-- Copying an already-corrupt REAL fails the migration atomically; never round it.
ALTER TABLE budget_periods RENAME TO budget_periods_old;
CREATE TABLE budget_periods (
    tenant_id TEXT NOT NULL, key_id TEXT NOT NULL,
    provider_id TEXT NOT NULL, model_id TEXT NOT NULL, period TEXT NOT NULL,
    spent_minor INTEGER NOT NULL DEFAULT 0 CHECK (typeof(spent_minor) = 'integer' AND spent_minor >= 0),
    PRIMARY KEY (tenant_id, key_id, provider_id, model_id, period)
);
INSERT INTO budget_periods SELECT * FROM budget_periods_old;
DROP TABLE budget_periods_old;
ALTER TABLE tenant_periods RENAME TO tenant_periods_old;
CREATE TABLE tenant_periods (
    tenant_id TEXT NOT NULL, period TEXT NOT NULL,
    spent_minor INTEGER NOT NULL DEFAULT 0 CHECK (typeof(spent_minor) = 'integer' AND spent_minor >= 0),
    PRIMARY KEY (tenant_id, period)
);
INSERT INTO tenant_periods SELECT * FROM tenant_periods_old;
DROP TABLE tenant_periods_old;

-- The reservation retains scope/period; this immutable cost blocks new spending
-- independently of reservation TTL until an operator reconciles the overflow.
CREATE TABLE budget_amount_overflows (
    reservation_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    actual_cost_minor INTEGER NOT NULL CHECK (typeof(actual_cost_minor) = 'integer' AND actual_cost_minor >= 0),
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_budget_amount_overflows_tenant ON budget_amount_overflows (tenant_id);

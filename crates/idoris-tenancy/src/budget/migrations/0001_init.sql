-- R2-C budget ledger schema, migration 0001.
-- Applied by `BudgetLedger::open` inside a single `BEGIN IMMEDIATE`
-- transaction (see `ledger.rs::run_migrations`), tracked by version in
-- `schema_migrations` so a later task can add `0002_*.sql` etc. without
-- re-running this one. Every statement here is idempotent
-- (`IF NOT EXISTS`) so re-applying it — e.g. two processes racing on first
-- open of a brand-new file — is harmless even before the version check
-- would normally skip it.

-- One row per (tenant, key, provider, model): the account's period budget
-- and the explicit IANA time zone its periods are resolved in
-- (contract-tenancy §3: never the server's local time, never a caller
-- override).
CREATE TABLE IF NOT EXISTS budget_config (
    tenant_id        TEXT NOT NULL,
    key_id           TEXT NOT NULL,
    provider_id      TEXT NOT NULL,
    model_id         TEXT NOT NULL,
    limit_minor      INTEGER NOT NULL,
    billing_timezone TEXT NOT NULL,
    PRIMARY KEY (tenant_id, key_id, provider_id, model_id)
);

-- Settled spend per scope per billing period. `spent_minor` only moves on
-- `settle`, never on `reserve` — reservations are tracked separately below
-- so they can expire/release without ever having touched settled spend.
CREATE TABLE IF NOT EXISTS budget_periods (
    tenant_id    TEXT NOT NULL,
    key_id       TEXT NOT NULL,
    provider_id  TEXT NOT NULL,
    model_id     TEXT NOT NULL,
    period       TEXT NOT NULL,
    spent_minor  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, key_id, provider_id, model_id, period)
);

-- One row per in-flight or finalized reservation. `status` only moves
-- forward: active -> settled | released | expired (all terminal).
CREATE TABLE IF NOT EXISTS reservations (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL,
    key_id            TEXT NOT NULL,
    provider_id       TEXT NOT NULL,
    model_id          TEXT NOT NULL,
    period            TEXT NOT NULL,
    reserved_minor    INTEGER NOT NULL,
    -- Defense-in-depth: a typo'd status would otherwise silently vanish
    -- from the `status='active'` filters below, undercounting reservations.
    status            TEXT NOT NULL CHECK (status IN ('active', 'settled', 'released', 'expired')),
    created_at_ms     INTEGER NOT NULL,
    expires_at_ms     INTEGER NOT NULL,
    actual_cost_minor INTEGER
);

CREATE INDEX IF NOT EXISTS idx_reservations_scope_period_status
    ON reservations (tenant_id, key_id, provider_id, model_id, period, status);

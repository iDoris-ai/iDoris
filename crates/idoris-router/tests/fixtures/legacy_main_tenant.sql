-- 旧库夹具（只读，不要手工编辑）：tenant 模式（acme / beta / gamma 三个租户）。
--
-- 由 origin/feat/compute-service@d1e8e1f8ada9ee7eb2c6ec4b86b2468aebb2a58f（计算服务 S1 之前的基线代码）真实跑出来：
--   1. 用基线的 `storage::bootstrap` 建库（四个存储依次打开、跑各自的迁移）；
--   2. 用基线现有的写入函数写数据：BudgetLedger（注入时钟 2026-11-01T12:00:00Z，覆盖
--      已结算 / 已释放 / 已过期 / 在途 / 超额 / 4 倍超额 / 子范围限额）、TenantStore（用量与审计）、
--      VirtualKeyStore（真实铸造的 key）、EventLogStore；
--   3. 用 sqlite 的 dump（Python iterdump）导成文本。
-- 表结构、触发器、索引和数据都是旧代码写下的原样。
-- 对应测试：crates/idoris-router/tests/legacy_db_upgrade.rs（用新代码打开它，数据不丢、读数不变）。
--
BEGIN TRANSACTION;
CREATE TABLE budget_amount_overflows (
    reservation_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    actual_cost_minor INTEGER NOT NULL CHECK (typeof(actual_cost_minor) = 'integer' AND actual_cost_minor >= 0),
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE budget_config (
    tenant_id        TEXT NOT NULL,
    key_id           TEXT NOT NULL,
    provider_id      TEXT NOT NULL,
    model_id         TEXT NOT NULL,
    limit_minor      INTEGER NOT NULL,
    billing_timezone TEXT NOT NULL,
    PRIMARY KEY (tenant_id, key_id, provider_id, model_id)
);
INSERT INTO "budget_config" VALUES('gamma','sub-key','omlx','m1',800,'UTC');
CREATE TABLE budget_overage_events (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL,
    reservation_id    TEXT NOT NULL,
    reserved_minor    INTEGER NOT NULL,
    actual_cost_minor INTEGER NOT NULL,
    overage_minor     INTEGER NOT NULL,
    created_at_ms     INTEGER NOT NULL
);
INSERT INTO "budget_overage_events" VALUES('f7054bb5-a5ba-4685-829c-1f2867e508e7','acme','da548f2f-6e22-4929-9869-5bba2503d260',100,350,250,1793534400000);
INSERT INTO "budget_overage_events" VALUES('877d9892-c442-4a93-ac2b-97aebe14243b','acme','31a8450a-3fa7-4a04-87b0-d30add426a6f',10,100,90,1793534400000);
INSERT INTO "budget_overage_events" VALUES('467d0fa6-6a84-4d79-b7da-640358dc90e5','beta','1efd7fea-581d-427d-9be0-d7525a856253',100,350,250,1793534800000);
INSERT INTO "budget_overage_events" VALUES('0c1c1f47-104e-441c-ae4e-f5233a54cf9e','beta','1e96f29a-cfc6-4405-9e1c-9f2cdd0791f1',10,100,90,1793534800000);
CREATE TABLE budget_periods (
    tenant_id TEXT NOT NULL, key_id TEXT NOT NULL,
    provider_id TEXT NOT NULL, model_id TEXT NOT NULL, period TEXT NOT NULL,
    spent_minor INTEGER NOT NULL DEFAULT 0 CHECK (typeof(spent_minor) = 'integer' AND spent_minor >= 0),
    PRIMARY KEY (tenant_id, key_id, provider_id, model_id, period)
);
INSERT INTO "budget_periods" VALUES('acme','default','omlx','omlx','2026-11',2250);
INSERT INTO "budget_periods" VALUES('acme','vk-legacy','paid-1','paid-model','2026-11',350);
INSERT INTO "budget_periods" VALUES('beta','default','omlx','omlx','2026-11',2250);
INSERT INTO "budget_periods" VALUES('beta','vk-legacy','paid-1','paid-model','2026-11',350);
INSERT INTO "budget_periods" VALUES('gamma','sub-key','omlx','m1','2026-11',277);
CREATE TABLE event_log_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE, tenant_id TEXT NOT NULL, record_id TEXT NOT NULL,
    event_type TEXT NOT NULL, ts_utc_ms INTEGER NOT NULL, request_id TEXT, session_id TEXT,
    trace_id TEXT, parent_id TEXT, origin_record_id TEXT, metadata TEXT NOT NULL
);
INSERT INTO "event_log_events" VALUES(1,'00000000-0000-4000-8000-000000000100','acme','rec-acme','request.received',1793534400000,'req-rec-acme',NULL,NULL,NULL,NULL,'{}');
INSERT INTO "event_log_events" VALUES(2,'00000000-0000-4000-8000-000000000101','acme','rec-acme','budget.reserved',1793534400001,'req-rec-acme',NULL,NULL,NULL,NULL,'{}');
INSERT INTO "event_log_events" VALUES(3,'00000000-0000-4000-8000-000000000102','acme','rec-acme','completed',1793534400002,'req-rec-acme',NULL,NULL,NULL,NULL,'{}');
INSERT INTO "event_log_events" VALUES(4,'00000000-0000-4000-8000-000000000200','beta','rec-beta','request.received',1793534400000,'req-rec-beta',NULL,NULL,NULL,NULL,'{}');
INSERT INTO "event_log_events" VALUES(5,'00000000-0000-4000-8000-000000000201','beta','rec-beta','budget.reserved',1793534400001,'req-rec-beta',NULL,NULL,NULL,NULL,'{}');
INSERT INTO "event_log_events" VALUES(6,'00000000-0000-4000-8000-000000000202','beta','rec-beta','completed',1793534400002,'req-rec-beta',NULL,NULL,NULL,NULL,'{}');
CREATE TABLE event_log_schema_migrations (version INTEGER PRIMARY KEY);
INSERT INTO "event_log_schema_migrations" VALUES(1);
INSERT INTO "event_log_schema_migrations" VALUES(2);
CREATE TABLE record_schema_migrations (version INTEGER PRIMARY KEY);
INSERT INTO "record_schema_migrations" VALUES(1);
CREATE TABLE reservations (
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
, tenant_period TEXT);
INSERT INTO "reservations" VALUES('8800d240-2efd-4216-ab28-8b116af9f5fd','acme','default','omlx','omlx','2026-11',1000,'settled',1793534400000,1793534700000,400,'2026-11');
INSERT INTO "reservations" VALUES('75a80031-e885-451f-8713-2575a95a66c0','acme','default','omlx','omlx','2026-11',2000,'settled',1793534400000,1793534700000,1500,'2026-11');
INSERT INTO "reservations" VALUES('43ee4514-4dfd-4e1b-af95-01a2004316ca','acme','vk-legacy','paid-1','paid-model','2026-11',500,'settled',1793534400000,1793534700000,250,'2026-11');
INSERT INTO "reservations" VALUES('bf4a582b-6b04-4a77-bbb7-e275d817a7ef','acme','default','omlx','omlx','2026-11',800,'released',1793534400000,1793534700000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('da548f2f-6e22-4929-9869-5bba2503d260','acme','default','omlx','omlx','2026-11',100,'settled',1793534400000,1793534700000,350,'2026-11');
INSERT INTO "reservations" VALUES('31a8450a-3fa7-4a04-87b0-d30add426a6f','acme','vk-legacy','paid-1','paid-model','2026-11',10,'settled',1793534400000,1793534700000,100,'2026-11');
INSERT INTO "reservations" VALUES('f89c0890-5f22-4dce-bd09-a4e6e61859e6','acme','default','omlx','omlx','2026-11',600,'expired',1793534400000,1793534700000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('83cb5469-03e9-4415-a972-94fad15f812f','acme','default','omlx','omlx','2026-11',700,'active',1793534800000,2108894800000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('7a0f9eb4-3225-411f-a3e1-901b910091b7','beta','default','omlx','omlx','2026-11',1000,'settled',1793534800000,1793535100000,400,'2026-11');
INSERT INTO "reservations" VALUES('384ea22e-b3b3-4768-a202-57f86f854e2b','beta','default','omlx','omlx','2026-11',2000,'settled',1793534800000,1793535100000,1500,'2026-11');
INSERT INTO "reservations" VALUES('ef3a88a1-bbe0-4b2c-baf1-849d9c290ffb','beta','vk-legacy','paid-1','paid-model','2026-11',500,'settled',1793534800000,1793535100000,250,'2026-11');
INSERT INTO "reservations" VALUES('d07bc4cd-2206-42ed-8ff7-e7b7ae3be735','beta','default','omlx','omlx','2026-11',800,'released',1793534800000,1793535100000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('1efd7fea-581d-427d-9be0-d7525a856253','beta','default','omlx','omlx','2026-11',100,'settled',1793534800000,1793535100000,350,'2026-11');
INSERT INTO "reservations" VALUES('1e96f29a-cfc6-4405-9e1c-9f2cdd0791f1','beta','vk-legacy','paid-1','paid-model','2026-11',10,'settled',1793534800000,1793535100000,100,'2026-11');
INSERT INTO "reservations" VALUES('d9eceb6c-cdf7-47bd-81f3-c7106a6bd9f5','beta','default','omlx','omlx','2026-11',600,'expired',1793534800000,1793535100000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('21e7ecfb-4bf2-4278-bf63-14266ff0ee7e','beta','default','omlx','omlx','2026-11',700,'active',1793535200000,2108895200000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('3c7181a5-bc98-423b-a214-0fbdbf3f5763','gamma','sub-key','omlx','m1','2026-11',300,'settled',1793535200000,1793535500000,277,'2026-11');
CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL);
INSERT INTO "schema_migrations" VALUES(1,1794498679498);
INSERT INTO "schema_migrations" VALUES(2,1794498679498);
INSERT INTO "schema_migrations" VALUES(3,1794498679498);
INSERT INTO "schema_migrations" VALUES(4,1794498679499);
INSERT INTO "schema_migrations" VALUES(5,1794498679500);
CREATE TABLE tenant_config (
    tenant_id        TEXT PRIMARY KEY,
    limit_minor      INTEGER NOT NULL,
    billing_timezone TEXT NOT NULL,
    -- paid_only (default): cost=0 bypasses the check. all: always checked.
    scope            TEXT NOT NULL CHECK (scope IN ('paid_only', 'all'))
);
INSERT INTO "tenant_config" VALUES('acme',100000,'Asia/Bangkok','paid_only');
INSERT INTO "tenant_config" VALUES('beta',5000,'UTC','all');
INSERT INTO "tenant_config" VALUES('gamma',1000,'UTC','paid_only');
CREATE TABLE tenant_periods (
    tenant_id TEXT NOT NULL, period TEXT NOT NULL,
    spent_minor INTEGER NOT NULL DEFAULT 0 CHECK (typeof(spent_minor) = 'integer' AND spent_minor >= 0),
    PRIMARY KEY (tenant_id, period)
);
INSERT INTO "tenant_periods" VALUES('acme','2026-11',2600);
INSERT INTO "tenant_periods" VALUES('beta','2026-11',2600);
INSERT INTO "tenant_periods" VALUES('gamma','2026-11',277);
CREATE TABLE tenant_records (
    tenant_id        TEXT NOT NULL CHECK (trim(tenant_id) <> ''),
    kind             TEXT NOT NULL CHECK (kind IN ('usage', 'audit')),
    record_id        TEXT NOT NULL CHECK (trim(record_id) <> ''),
    request_id       TEXT NOT NULL CHECK (trim(request_id) <> ''),
    origin_record_id TEXT,
    payload          TEXT NOT NULL CHECK (
        json_valid(payload) AND json_type(payload) = 'object'
    ),
    PRIMARY KEY (tenant_id, kind, record_id)
);
INSERT INTO "tenant_records" VALUES('acme','usage','usage-acme-0','req-usage-acme-0',NULL,'{"cost_minor":400,"request_id":"req-usage-acme-0","tokens_in":11,"tokens_out":22,"ts_utc":1793534401000}');
INSERT INTO "tenant_records" VALUES('acme','usage','usage-acme-1','req-usage-acme-1',NULL,'{"cost_minor":1500,"request_id":"req-usage-acme-1","tokens_in":33,"tokens_out":44,"ts_utc":1793534402000}');
INSERT INTO "tenant_records" VALUES('acme','usage','usage-acme-2','req-usage-acme-2',NULL,'{"cost_minor":250,"request_id":"req-usage-acme-2","tokens_in":5,"tokens_out":6,"ts_utc":1793534403000}');
INSERT INTO "tenant_records" VALUES('acme','usage','usage-acme-3','req-usage-acme-3',NULL,'{"cost_minor":999,"request_id":"req-usage-acme-3","tokens_in":1,"tokens_out":1,"ts_utc":1790078400000}');
INSERT INTO "tenant_records" VALUES('acme','audit','audit-acme-0','req-audit-acme',NULL,'{"cost_minor":7,"reason":"intent_match","status":200,"ts_utc":1793534400005}');
INSERT INTO "tenant_records" VALUES('beta','usage','usage-beta-0','req-usage-beta-0',NULL,'{"cost_minor":100,"request_id":"req-usage-beta-0","tokens_in":7,"tokens_out":8,"ts_utc":1793534404000}');
INSERT INTO "tenant_records" VALUES('beta','audit','audit-beta-0','req-audit-beta',NULL,'{"cost_minor":7,"reason":"intent_match","status":200,"ts_utc":1793534400005}');
CREATE TABLE virtual_key_schema_migrations (
            version INTEGER PRIMARY KEY
        );
INSERT INTO "virtual_key_schema_migrations" VALUES(1);
INSERT INTO "virtual_key_schema_migrations" VALUES(2);
CREATE TABLE virtual_keys (
    key_id          TEXT PRIMARY KEY CHECK (trim(key_id) <> ''),
    key_hash        BLOB NOT NULL UNIQUE CHECK (length(key_hash) = 32),
    owner           TEXT NOT NULL CHECK (trim(owner) <> ''),
    allowed_privacy TEXT NOT NULL CHECK (
        json_valid(allowed_privacy) AND json_type(allowed_privacy) = 'array'
    ),
    allowed_roles   TEXT NOT NULL CHECK (
        json_valid(allowed_roles) AND json_type(allowed_roles) = 'array'
    ),
    budget_ref      TEXT CHECK (budget_ref IS NULL OR trim(budget_ref) <> ''),
    expires_at_ms   INTEGER CHECK (expires_at_ms IS NULL OR expires_at_ms > 0),
    admin_scopes    TEXT NOT NULL CHECK (
        json_valid(admin_scopes) AND json_type(admin_scopes) = 'array'
    ),
    status          TEXT NOT NULL CHECK (status IN ('active', 'revoked')),
    revoked_at_ms   INTEGER, tenant_id TEXT
    CHECK (tenant_id IS NULL OR (length(tenant_id) BETWEEN 1 AND 128 AND trim(tenant_id) = tenant_id)),
    CHECK (
        (status = 'active' AND revoked_at_ms IS NULL)
        OR (status = 'revoked' AND revoked_at_ms IS NOT NULL)
    )
);
INSERT INTO "virtual_keys" VALUES('vk_dc69b96c63bf40c7a8de47d0305aaead',X'4080246ED295F13673EDA90602776975CD3C8B5C345360A53E48BF3CFF5E11F9','agent24-instance','["local_only"]','["fast","daily"]',NULL,NULL,'[]','active',NULL,'acme');
INSERT INTO "virtual_keys" VALUES('vk_7094690725664ba998cb7bb1a16beeab',X'409D951372DC87B57EE6822293F085B5DA639C134E2736FE1FB1E0AF63B6DE92','agent24-instance','["local_only"]','["fast","daily"]',NULL,NULL,'[]','active',NULL,NULL);
CREATE INDEX idx_reservations_scope_period_status
    ON reservations (tenant_id, key_id, provider_id, model_id, period, status);
CREATE INDEX idx_budget_overage_events_tenant
    ON budget_overage_events (tenant_id, created_at_ms);
CREATE INDEX idx_budget_amount_overflows_tenant ON budget_amount_overflows (tenant_id);
CREATE INDEX event_log_tenant_record_sequence
    ON event_log_events(tenant_id, record_id, sequence);
CREATE TRIGGER event_log_guard_insert BEFORE INSERT ON event_log_events WHEN NEW.sequence != -1 OR EXISTS (SELECT 1 FROM event_log_events WHERE event_id = NEW.event_id) BEGIN SELECT RAISE(ABORT, 'event insert violates append-only invariants'); END;
CREATE TRIGGER event_log_sequence_positive AFTER INSERT ON event_log_events WHEN NEW.sequence <= 0 BEGIN SELECT RAISE(ABORT, 'event sequence must be database-assigned'); END;
CREATE TRIGGER event_log_no_update BEFORE UPDATE ON event_log_events BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;
CREATE TRIGGER event_log_no_delete BEFORE DELETE ON event_log_events BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;
CREATE INDEX event_log_tenant_type_time
    ON event_log_events(tenant_id, event_type, ts_utc_ms);
DELETE FROM "sqlite_sequence";
INSERT INTO "sqlite_sequence" VALUES('event_log_events',6);
COMMIT;

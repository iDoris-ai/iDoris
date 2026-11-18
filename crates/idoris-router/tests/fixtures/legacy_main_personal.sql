-- 旧库夹具（只读，不要手工编辑）：personal 模式（固定内部租户 personal）。
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
CREATE TABLE budget_overage_events (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL,
    reservation_id    TEXT NOT NULL,
    reserved_minor    INTEGER NOT NULL,
    actual_cost_minor INTEGER NOT NULL,
    overage_minor     INTEGER NOT NULL,
    created_at_ms     INTEGER NOT NULL
);
INSERT INTO "budget_overage_events" VALUES('69bfbc50-8046-43a2-ba7e-62d88bd7ed76','personal','c767e562-929d-47e4-983a-62ae97b39757',100,350,250,1793534400000);
INSERT INTO "budget_overage_events" VALUES('0b379fe3-0ec2-4b5b-9153-00c01505df25','personal','f088b9b4-5d87-4308-94dd-c7218257fb3a',10,100,90,1793534400000);
CREATE TABLE budget_periods (
    tenant_id TEXT NOT NULL, key_id TEXT NOT NULL,
    provider_id TEXT NOT NULL, model_id TEXT NOT NULL, period TEXT NOT NULL,
    spent_minor INTEGER NOT NULL DEFAULT 0 CHECK (typeof(spent_minor) = 'integer' AND spent_minor >= 0),
    PRIMARY KEY (tenant_id, key_id, provider_id, model_id, period)
);
INSERT INTO "budget_periods" VALUES('personal','default','omlx','omlx','2026-11',2250);
INSERT INTO "budget_periods" VALUES('personal','vk-legacy','paid-1','paid-model','2026-11',350);
CREATE TABLE event_log_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE, tenant_id TEXT NOT NULL, record_id TEXT NOT NULL,
    event_type TEXT NOT NULL, ts_utc_ms INTEGER NOT NULL, request_id TEXT, session_id TEXT,
    trace_id TEXT, parent_id TEXT, origin_record_id TEXT, metadata TEXT NOT NULL
);
INSERT INTO "event_log_events" VALUES(1,'00000000-0000-4000-8000-000000000300','personal','rec-personal','request.received',1793534400000,'req-rec-personal',NULL,NULL,NULL,NULL,'{}');
INSERT INTO "event_log_events" VALUES(2,'00000000-0000-4000-8000-000000000301','personal','rec-personal','budget.reserved',1793534400001,'req-rec-personal',NULL,NULL,NULL,NULL,'{}');
INSERT INTO "event_log_events" VALUES(3,'00000000-0000-4000-8000-000000000302','personal','rec-personal','completed',1793534400002,'req-rec-personal',NULL,NULL,NULL,NULL,'{}');
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
INSERT INTO "reservations" VALUES('3a4b0c6e-7767-4b1b-94b5-7ea53b50aeb9','personal','default','omlx','omlx','2026-11',1000,'settled',1793534400000,1793534700000,400,'2026-11');
INSERT INTO "reservations" VALUES('2a776283-ade8-464e-afe4-b171bc28f688','personal','default','omlx','omlx','2026-11',2000,'settled',1793534400000,1793534700000,1500,'2026-11');
INSERT INTO "reservations" VALUES('97398727-ba7a-4026-bd4a-03dbe9da5719','personal','vk-legacy','paid-1','paid-model','2026-11',500,'settled',1793534400000,1793534700000,250,'2026-11');
INSERT INTO "reservations" VALUES('362fb493-af24-4b19-aa56-f449c01244ea','personal','default','omlx','omlx','2026-11',800,'released',1793534400000,1793534700000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('c767e562-929d-47e4-983a-62ae97b39757','personal','default','omlx','omlx','2026-11',100,'settled',1793534400000,1793534700000,350,'2026-11');
INSERT INTO "reservations" VALUES('f088b9b4-5d87-4308-94dd-c7218257fb3a','personal','vk-legacy','paid-1','paid-model','2026-11',10,'settled',1793534400000,1793534700000,100,'2026-11');
INSERT INTO "reservations" VALUES('3270c6dd-60a1-440b-8204-a1c7e32e9877','personal','default','omlx','omlx','2026-11',600,'expired',1793534400000,1793534700000,NULL,'2026-11');
INSERT INTO "reservations" VALUES('a5fbe9a9-0124-4a1c-8c4f-0db8d13608a8','personal','default','omlx','omlx','2026-11',700,'active',1793534800000,2108894800000,NULL,'2026-11');
CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL);
INSERT INTO "schema_migrations" VALUES(1,1794498679521);
INSERT INTO "schema_migrations" VALUES(2,1794498679521);
INSERT INTO "schema_migrations" VALUES(3,1794498679521);
INSERT INTO "schema_migrations" VALUES(4,1794498679522);
INSERT INTO "schema_migrations" VALUES(5,1794498679523);
CREATE TABLE tenant_config (
    tenant_id        TEXT PRIMARY KEY,
    limit_minor      INTEGER NOT NULL,
    billing_timezone TEXT NOT NULL,
    -- paid_only (default): cost=0 bypasses the check. all: always checked.
    scope            TEXT NOT NULL CHECK (scope IN ('paid_only', 'all'))
);
INSERT INTO "tenant_config" VALUES('personal',50000,'UTC','paid_only');
CREATE TABLE tenant_periods (
    tenant_id TEXT NOT NULL, period TEXT NOT NULL,
    spent_minor INTEGER NOT NULL DEFAULT 0 CHECK (typeof(spent_minor) = 'integer' AND spent_minor >= 0),
    PRIMARY KEY (tenant_id, period)
);
INSERT INTO "tenant_periods" VALUES('personal','2026-11',2600);
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
INSERT INTO "tenant_records" VALUES('personal','usage','usage-personal-0','req-usage-personal-0',NULL,'{"cost_minor":1200,"request_id":"req-usage-personal-0","tokens_in":100,"tokens_out":200,"ts_utc":1793534401000}');
INSERT INTO "tenant_records" VALUES('personal','usage','usage-personal-1','req-usage-personal-1',NULL,'{"cost_minor":3400,"request_id":"req-usage-personal-1","tokens_in":300,"tokens_out":400,"ts_utc":1793534402000}');
INSERT INTO "tenant_records" VALUES('personal','audit','audit-personal-0','req-audit-personal',NULL,'{"cost_minor":7,"reason":"intent_match","status":200,"ts_utc":1793534400005}');
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
INSERT INTO "virtual_keys" VALUES('vk_1179c4419a814f0baeecce7a007aa6fb',X'80AC18EC4F1FC5BE6BDCB045353B21524D303AE87ADE2EBB602453573C59F35E','agent24-instance','["local_only"]','["fast","daily"]',NULL,NULL,'[]','active',NULL,NULL);
INSERT INTO "virtual_keys" VALUES('vk_a23b29158ca84d6a8d382b98f59c079f',X'AE98E3040DDDF39D8B3CC699D8441A0924E96CC34886ECA13B5321306ADEBDDE','agent24-instance','["local_only"]','["fast","daily"]',NULL,NULL,'[]','active',NULL,'personal');
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
INSERT INTO "sqlite_sequence" VALUES('event_log_events',3);
COMMIT;

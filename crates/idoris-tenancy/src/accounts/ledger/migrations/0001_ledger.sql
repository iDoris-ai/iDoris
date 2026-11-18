-- S3 准入账本，迁移 0001（设计稿 §4、§5；契约见 crates/idoris-compute-contracts 的 admission / reserve / request）。
--
-- 这些表和 S1 的账户表住在同一个库文件里，由 `AccountStore::open` 在账户迁移之后、同一条连接上应用，版本记在
-- `ledger_schema_migrations`（和账户迁移 `account_schema_migrations` 互不共用，所以 S2 往账户迁移里加东西不会和这里撞版本号）。
-- 只有组织、云形态会打开账户存储；个人形态和存量 tenant 部署从不创建这些表。
--
-- # 记账原则
--
-- * **账户可用额是分录的投影，不是可改的余额字段。** `ledger_entries` 只追加：触发器禁止 UPDATE 和 DELETE，
--   冲正是一条新分录。可用额、在途、已花、速率窗口都是对分录求和（见 `ledger/project.rs`）。
-- * 一次预留（hold）对付款账户链上的**每一个账户**各记一条分录（沿所有祖先），所以每个祖先的在途、已花、速率窗口、
--   补贴池消耗都可以只看自己那一列分录。预留、结算、释放、过期都是分录，不是对某个计数器的修改。
-- * `ledger_holds` 记预留本身（身份不可改，只有 `state` 沿 open -> settled / released / expired -> settled / released 走），
--   像 S4 的 `compute_attempts` 一样由触发器守着合法的边；钱的真相永远在分录里。
-- * 每条分录有幂等键（唯一），重放同一次结算不会记两次。
-- * 金额一律是「单位 + 微单位整数」，没有浮点，STRICT 表。
--
-- # 和 SQLite 行为有关的几条（S1 迁移里踩过，这里同样用测试守着）
--
-- 1. CHECK 对 NULL 放行：凡是「必须非空」都显式写 IS NOT NULL。
-- 2. INSERT OR REPLACE 在主键 / 唯一约束冲突时先删旧行再插新行，而 `recursive_triggers` 默认关闭，DELETE 触发器不会触发，
--    所以只靠「禁止 DELETE」拦不住它改写历史。每张表的每个主键和唯一索引都配一条 BEFORE INSERT 的「不许冲突」触发器。
-- 3. 触发器里显式检查被引用的行存在，不靠外键（外键可以被 `defer_foreign_keys` 推迟，或在没开外键的连接上完全不检查）。
--    这些表不声明外键：S4 的尝试记录存储也会在它自己的连接上往这里写（同一个事务里结算），不能依赖连接的外键开关。

-- 账期时区与预留 TTL。库为准（和租户配置同一思路）：S4 的结算钩子在另一条连接上跑，也读同一份。
CREATE TABLE ledger_config (
    key   TEXT NOT NULL PRIMARY KEY CHECK (key IN ('billing_timezone', 'reservation_ttl_ms')),
    value TEXT NOT NULL CHECK (length(value) BETWEEN 1 AND 64)
) STRICT;

INSERT INTO ledger_config (key, value) VALUES ('billing_timezone', 'UTC'), ('reservation_ttl_ms', '1800000');

CREATE TRIGGER ledger_config_never_deleted BEFORE DELETE ON ledger_config
BEGIN
    SELECT RAISE(ABORT, 'ledger configuration rows are never deleted');
END;

-- 平台损失账户是分录里的一个保留账户 id（不在账户树里）。树里不许再建一个同名账户，否则两边的分录会混在一起。
CREATE TRIGGER ledger_reserved_account_id BEFORE INSERT ON accounts
WHEN NEW.account_id = 'platform-loss'
BEGIN
    SELECT RAISE(ABORT, 'account id platform-loss is reserved for the ledger loss account');
END;

-- ---------------------------------------------------------------------------------------------
-- 账户的速率闸门与在途上限（版本化，最新一行生效；只追加）。单位就是账户的单位。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_account_limits (
    version             INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id          TEXT NOT NULL,
    inflight_cap_micros INTEGER CHECK (inflight_cap_micros IS NULL OR inflight_cap_micros >= 0),
    minute_cap_micros   INTEGER CHECK (minute_cap_micros IS NULL OR minute_cap_micros >= 0),
    hour_cap_micros     INTEGER CHECK (hour_cap_micros IS NULL OR hour_cap_micros >= 0),
    set_at_ms           INTEGER NOT NULL CHECK (set_at_ms >= 0)
) STRICT;

CREATE INDEX ledger_account_limits_by_account ON ledger_account_limits (account_id, version);

CREATE TRIGGER ledger_account_limits_insert_rules BEFORE INSERT ON ledger_account_limits
BEGIN
    SELECT RAISE(ABORT, 'limits account must exist')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id);
    SELECT RAISE(ABORT, 'limit versions cannot be inserted over an existing version')
     WHERE NEW.version IS NOT NULL AND EXISTS (SELECT 1 FROM ledger_account_limits WHERE version = NEW.version);
END;
CREATE TRIGGER ledger_account_limits_append_only_update BEFORE UPDATE ON ledger_account_limits
BEGIN
    SELECT RAISE(ABORT, 'ledger_account_limits is append-only');
END;
CREATE TRIGGER ledger_account_limits_append_only_delete BEFORE DELETE ON ledger_account_limits
BEGIN
    SELECT RAISE(ABORT, 'ledger_account_limits is append-only');
END;

-- ---------------------------------------------------------------------------------------------
-- 授信（担保人允许下级透支到授信下限）。版本化，最新一行生效；`enabled = 0` 就是把授信关掉。只追加。
-- 担保人 = 这个账户在树里的父账户（写入时由 Rust 层取，不由调用方声明）。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_credit_lines (
    version        INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id     TEXT NOT NULL,
    guarantor_id   TEXT NOT NULL,
    enabled        INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    limit_micros   INTEGER NOT NULL CHECK (limit_micros >= 0),
    valid_until_ms INTEGER CHECK (valid_until_ms IS NULL OR valid_until_ms > 0),
    set_at_ms      INTEGER NOT NULL CHECK (set_at_ms >= 0),
    CHECK (enabled = 1 OR limit_micros = 0)
) STRICT;

CREATE INDEX ledger_credit_lines_by_account ON ledger_credit_lines (account_id, version);

CREATE TRIGGER ledger_credit_lines_insert_rules BEFORE INSERT ON ledger_credit_lines
BEGIN
    SELECT RAISE(ABORT, 'credit line account must exist')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id);
    SELECT RAISE(ABORT, 'the guarantor must be the parent of the account')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id AND parent_id = NEW.guarantor_id);
    SELECT RAISE(ABORT, 'credit line versions cannot be inserted over an existing version')
     WHERE NEW.version IS NOT NULL AND EXISTS (SELECT 1 FROM ledger_credit_lines WHERE version = NEW.version);
END;
CREATE TRIGGER ledger_credit_lines_append_only_update BEFORE UPDATE ON ledger_credit_lines
BEGIN
    SELECT RAISE(ABORT, 'ledger_credit_lines is append-only');
END;
CREATE TRIGGER ledger_credit_lines_append_only_delete BEFORE DELETE ON ledger_credit_lines
BEGIN
    SELECT RAISE(ABORT, 'ledger_credit_lines is append-only');
END;

-- ---------------------------------------------------------------------------------------------
-- 补贴池每个账期的总上限（版本化，最新一行生效；只追加）。没有这个账期的行 = 上限 0（失败即关闭）。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_pool_caps (
    version   INTEGER PRIMARY KEY AUTOINCREMENT,
    pool_id   TEXT NOT NULL,
    period    TEXT NOT NULL CHECK (period GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]'),
    unit      TEXT NOT NULL CHECK (unit IN ('THB', 'USD') OR unit GLOB 'QUOTA:*'),
    cap_micros INTEGER NOT NULL CHECK (cap_micros >= 0),
    set_at_ms INTEGER NOT NULL CHECK (set_at_ms >= 0)
) STRICT;

CREATE INDEX ledger_pool_caps_by_pool ON ledger_pool_caps (pool_id, period, version);

CREATE TRIGGER ledger_pool_caps_insert_rules BEFORE INSERT ON ledger_pool_caps
BEGIN
    SELECT RAISE(ABORT, 'pool cap must be set on an existing subsidy pool account of the same unit')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.pool_id AND kind = 'subsidy_pool' AND unit = NEW.unit);
    SELECT RAISE(ABORT, 'pool cap versions cannot be inserted over an existing version')
     WHERE NEW.version IS NOT NULL AND EXISTS (SELECT 1 FROM ledger_pool_caps WHERE version = NEW.version);
END;
CREATE TRIGGER ledger_pool_caps_append_only_update BEFORE UPDATE ON ledger_pool_caps
BEGIN
    SELECT RAISE(ABORT, 'ledger_pool_caps is append-only');
END;
CREATE TRIGGER ledger_pool_caps_append_only_delete BEFORE DELETE ON ledger_pool_caps
BEGIN
    SELECT RAISE(ABORT, 'ledger_pool_caps is append-only');
END;

-- ---------------------------------------------------------------------------------------------
-- 预留（hold）。`hold_id` 是尝试 id（`<请求 id>#<序号>`）；附加成本（比如安全检查）是同一次尝试下另一个 `component`，
-- hold_id = `<尝试 id>~<component>`。同一个尝试同一个 component 只有一个预留。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_holds (
    hold_id            TEXT NOT NULL PRIMARY KEY CHECK (length(hold_id) BETWEEN 1 AND 192),
    attempt_id         TEXT NOT NULL CHECK (length(attempt_id) BETWEEN 1 AND 160),
    component          TEXT NOT NULL CHECK (component = 'model' OR (length(component) BETWEEN 1 AND 32 AND component NOT GLOB '*[^a-z0-9_]*')),
    request_id         TEXT NOT NULL CHECK (length(request_id) BETWEEN 1 AND 128),
    attempt_index      INTEGER NOT NULL CHECK (attempt_index BETWEEN 0 AND 255),
    payer_account      TEXT NOT NULL,
    credential_id      TEXT NOT NULL,
    unit               TEXT NOT NULL CHECK (unit IN ('THB', 'USD') OR unit GLOB 'QUOTA:*'),
    reserved_micros    INTEGER NOT NULL CHECK (reserved_micros >= 0),
    -- 创建预留时所在的账期（账本时区的 YYYY-MM）。宽限按它计数。
    period             TEXT NOT NULL CHECK (period GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]'),
    created_at_ms      INTEGER NOT NULL CHECK (created_at_ms >= 0),
    expires_at_ms      INTEGER NOT NULL,
    -- 只有宽限放行的预留有值：放行时「可用额度 + 授信」。超出它的部分（至多一次预留）由平台承担。
    grace_floor_micros INTEGER CHECK (grace_floor_micros IS NULL OR grace_floor_micros >= 0),
    state              TEXT NOT NULL CHECK (state IN ('open', 'settled', 'released', 'expired')),
    expired_at_ms      INTEGER,
    closed_at_ms       INTEGER,
    charged_micros     INTEGER,
    -- 1 = 预留过期之后才来的结算（迟到结算）。
    late               INTEGER NOT NULL DEFAULT 0 CHECK (late IN (0, 1)),
    UNIQUE (attempt_id, component),
    CHECK (expires_at_ms > created_at_ms),
    CHECK (late = 0 OR expired_at_ms IS NOT NULL),
    CHECK (
           (state = 'open'     AND expired_at_ms IS NULL AND closed_at_ms IS NULL AND charged_micros IS NULL)
        OR (state = 'expired'  AND expired_at_ms IS NOT NULL AND closed_at_ms IS NULL AND charged_micros IS NULL)
        OR (state = 'settled'  AND closed_at_ms IS NOT NULL AND charged_micros IS NOT NULL AND charged_micros >= 0)
        OR (state = 'released' AND closed_at_ms IS NOT NULL AND charged_micros IS NOT NULL AND charged_micros = 0)
    )
) STRICT;

CREATE INDEX ledger_holds_open_expiry ON ledger_holds (expires_at_ms) WHERE state = 'open';
CREATE INDEX ledger_holds_by_request ON ledger_holds (request_id);
CREATE INDEX ledger_holds_by_payer ON ledger_holds (payer_account);

CREATE TRIGGER ledger_holds_insert_no_conflict BEFORE INSERT ON ledger_holds
WHEN EXISTS (SELECT 1 FROM ledger_holds WHERE hold_id = NEW.hold_id)
  OR EXISTS (SELECT 1 FROM ledger_holds WHERE attempt_id = NEW.attempt_id AND component = NEW.component)
BEGIN
    SELECT RAISE(ABORT, 'hold already exists');
END;

CREATE TRIGGER ledger_holds_insert_rules BEFORE INSERT ON ledger_holds
BEGIN
    SELECT RAISE(ABORT, 'a new hold must start in the open state')
     WHERE NEW.state <> 'open';
    SELECT RAISE(ABORT, 'hold payer account must exist and carry the hold unit')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.payer_account AND unit = NEW.unit);
    SELECT RAISE(ABORT, 'hold id must be the attempt id, or the attempt id followed by ~component')
     WHERE NEW.hold_id <> (CASE WHEN NEW.component = 'model' THEN NEW.attempt_id ELSE NEW.attempt_id || '~' || NEW.component END);
    SELECT RAISE(ABORT, 'attempt id must be <request id>#<index>')
     WHERE NEW.attempt_id <> NEW.request_id || '#' || NEW.attempt_index;
END;

CREATE TRIGGER ledger_holds_never_deleted BEFORE DELETE ON ledger_holds
BEGIN
    SELECT RAISE(ABORT, 'holds are never deleted');
END;

-- 身份与金额一经写下不能改；只有 state 和收尾列可以动。
CREATE TRIGGER ledger_holds_identity_immutable
BEFORE UPDATE OF hold_id, attempt_id, component, request_id, attempt_index, payer_account, credential_id,
                 unit, reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros
ON ledger_holds
BEGIN
    SELECT RAISE(ABORT, 'hold identity and amounts are immutable');
END;

-- 合法的边：open -> settled / released / expired；expired -> settled / released（迟到的结算）。
CREATE TRIGGER ledger_holds_state_edges BEFORE UPDATE OF state ON ledger_holds
WHEN NEW.state <> OLD.state AND NOT (
       (OLD.state = 'open'    AND NEW.state IN ('settled', 'released', 'expired'))
    OR (OLD.state = 'expired' AND NEW.state IN ('settled', 'released')))
BEGIN
    SELECT RAISE(ABORT, 'illegal hold state edge');
END;

CREATE TRIGGER ledger_holds_terminal_frozen BEFORE UPDATE ON ledger_holds
WHEN OLD.state IN ('settled', 'released')
BEGIN
    SELECT RAISE(ABORT, 'settled and released holds are frozen');
END;

CREATE TRIGGER ledger_holds_expiry_write_once BEFORE UPDATE OF expired_at_ms ON ledger_holds
WHEN OLD.expired_at_ms IS NOT NULL AND NEW.expired_at_ms IS NOT OLD.expired_at_ms
BEGIN
    SELECT RAISE(ABORT, 'the expiry time is write-once');
END;

-- ---------------------------------------------------------------------------------------------
-- 分录：只追加。每条分录有四列有符号的微单位，哪一列可以非零由 kind 决定（CHECK）：
--   held      在途（预留占着的额度）
--   spent     已花（结算记下的）
--   funds     自有资金（充值）
--   liability 责任（担保人承担 / 平台损失），不参与任何额度检查，只给对账和报表
-- `window_at_ms` 是这笔钱归属的速率窗口与额度窗口的时刻：预留、结算、释放、过期都记在**预留创建的时刻**上
-- （这样一个请求不管多久之后才结算，对速率窗口的净贡献都只是它最终的金额）；迟到结算记在结算的时刻。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_entries (
    entry_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id       TEXT NOT NULL,
    hold_id          TEXT,
    kind             TEXT NOT NULL CHECK (kind IN (
        'reserve', 'settle', 'release', 'expire', 'late_settle', 'fund',
        'sponsor_covered', 'platform_loss', 'grace_absorbed')),
    unit             TEXT NOT NULL CHECK (unit IN ('THB', 'USD') OR unit GLOB 'QUOTA:*'),
    held_micros      INTEGER NOT NULL DEFAULT 0,
    spent_micros     INTEGER NOT NULL DEFAULT 0,
    funds_micros     INTEGER NOT NULL DEFAULT 0,
    liability_micros INTEGER NOT NULL DEFAULT 0,
    window_at_ms     INTEGER NOT NULL CHECK (window_at_ms >= 0),
    at_ms            INTEGER NOT NULL CHECK (at_ms >= 0),
    idempotency_key  TEXT NOT NULL UNIQUE CHECK (length(idempotency_key) BETWEEN 1 AND 400),
    CHECK ((kind = 'fund') = (hold_id IS NULL)),
    CHECK (
           (kind = 'reserve'     AND held_micros >= 0 AND spent_micros = 0 AND funds_micros = 0 AND liability_micros = 0)
        OR (kind = 'settle'      AND held_micros <= 0 AND spent_micros >= 0 AND funds_micros = 0 AND liability_micros = 0)
        OR (kind IN ('release', 'expire')
                                 AND held_micros <= 0 AND spent_micros = 0 AND funds_micros = 0 AND liability_micros = 0)
        OR (kind = 'late_settle' AND held_micros = 0 AND spent_micros >= 0 AND funds_micros = 0 AND liability_micros = 0)
        OR (kind = 'fund'        AND held_micros = 0 AND spent_micros = 0 AND funds_micros > 0 AND liability_micros = 0)
        OR (kind IN ('sponsor_covered', 'platform_loss', 'grace_absorbed')
                                 AND held_micros = 0 AND spent_micros = 0 AND funds_micros = 0 AND liability_micros >= 0)
    )
) STRICT;

CREATE INDEX ledger_entries_by_account_window ON ledger_entries (account_id, window_at_ms);
CREATE INDEX ledger_entries_by_hold ON ledger_entries (hold_id);

CREATE TRIGGER ledger_entries_insert_no_conflict BEFORE INSERT ON ledger_entries
WHEN EXISTS (SELECT 1 FROM ledger_entries WHERE idempotency_key = NEW.idempotency_key)
  OR (NEW.entry_id IS NOT NULL AND EXISTS (SELECT 1 FROM ledger_entries WHERE entry_id = NEW.entry_id))
BEGIN
    SELECT RAISE(ABORT, 'ledger entry already exists, or its idempotency key was already used');
END;

CREATE TRIGGER ledger_entries_insert_rules BEFORE INSERT ON ledger_entries
BEGIN
    SELECT RAISE(ABORT, 'ledger entry account must exist and carry the entry unit')
     WHERE NEW.account_id <> 'platform-loss'
       AND NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id AND unit = NEW.unit);
    SELECT RAISE(ABORT, 'loss entries must belong to a hold in the same unit')
     WHERE NEW.account_id = 'platform-loss'
       AND (NEW.hold_id IS NULL OR NEW.kind NOT IN ('platform_loss', 'grace_absorbed'));
    SELECT RAISE(ABORT, 'ledger entry hold must exist and carry the entry unit')
     WHERE NEW.hold_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM ledger_holds WHERE hold_id = NEW.hold_id AND unit = NEW.unit);
    SELECT RAISE(ABORT, 'only a platform-loss entry may use the platform-loss account')
     WHERE NEW.account_id = 'platform-loss' AND NEW.kind NOT IN ('platform_loss', 'grace_absorbed');
    SELECT RAISE(ABORT, 'loss entries belong to the platform-loss account')
     WHERE NEW.account_id <> 'platform-loss' AND NEW.kind IN ('platform_loss', 'grace_absorbed');
END;

CREATE TRIGGER ledger_entries_append_only_update BEFORE UPDATE ON ledger_entries
BEGIN
    SELECT RAISE(ABORT, 'ledger_entries is append-only');
END;
CREATE TRIGGER ledger_entries_append_only_delete BEFORE DELETE ON ledger_entries
BEGIN
    SELECT RAISE(ABORT, 'ledger_entries is append-only');
END;

-- 防呆：同一个预留在同一个账户上的在途净额必须落在 [0, 预留额] 里。不会重复预留，也不会把同一笔预留释放两次、
-- 释放得比预留的还多——就算 Rust 层有 bug 也写不进去。
CREATE TRIGGER ledger_entries_held_bounds AFTER INSERT ON ledger_entries
WHEN NEW.held_micros <> 0 AND NEW.hold_id IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'a hold cannot be reserved twice, or released more than it reserved')
     WHERE (SELECT COALESCE(SUM(held_micros), 0) FROM ledger_entries
             WHERE hold_id = NEW.hold_id AND account_id = NEW.account_id)
           NOT BETWEEN 0 AND (SELECT reserved_micros FROM ledger_holds WHERE hold_id = NEW.hold_id);
END;

-- ---------------------------------------------------------------------------------------------
-- 宽限：每个付款账户每个账期最多一次（`voided_at_ms IS NULL` 的行至多一行，唯一索引保证）。
-- 宽限放行的预留如果最后没有向用户收费（释放了 / 结算为 0），这次宽限作废，用户还能再用。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_grace_uses (
    use_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id       TEXT NOT NULL,
    period           TEXT NOT NULL CHECK (period GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]'),
    hold_id          TEXT NOT NULL UNIQUE,
    unit             TEXT NOT NULL CHECK (unit IN ('THB', 'USD') OR unit GLOB 'QUOTA:*'),
    reserve_micros   INTEGER NOT NULL CHECK (reserve_micros >= 0),
    -- 放行时没被可用额度覆盖的部分（<= 一次预留）。
    shortfall_micros INTEGER NOT NULL CHECK (shortfall_micros > 0 AND shortfall_micros <= reserve_micros),
    floor_micros     INTEGER NOT NULL CHECK (floor_micros >= 0),
    used_at_ms       INTEGER NOT NULL CHECK (used_at_ms >= 0),
    voided_at_ms     INTEGER
) STRICT;

CREATE UNIQUE INDEX ledger_grace_one_active_per_period
    ON ledger_grace_uses (account_id, period) WHERE voided_at_ms IS NULL;

CREATE TRIGGER ledger_grace_uses_insert_no_conflict BEFORE INSERT ON ledger_grace_uses
WHEN EXISTS (SELECT 1 FROM ledger_grace_uses WHERE hold_id = NEW.hold_id)
  OR (NEW.use_id IS NOT NULL AND EXISTS (SELECT 1 FROM ledger_grace_uses WHERE use_id = NEW.use_id))
BEGIN
    SELECT RAISE(ABORT, 'grace use already exists');
END;
CREATE TRIGGER ledger_grace_uses_insert_rules BEFORE INSERT ON ledger_grace_uses
BEGIN
    SELECT RAISE(ABORT, 'a new grace use must not be voided')
     WHERE NEW.voided_at_ms IS NOT NULL;
    SELECT RAISE(ABORT, 'grace use must belong to an open hold of the same account and unit')
     WHERE NOT EXISTS (SELECT 1 FROM ledger_holds
                        WHERE hold_id = NEW.hold_id AND payer_account = NEW.account_id AND unit = NEW.unit
                          AND state = 'open' AND grace_floor_micros = NEW.floor_micros AND period = NEW.period);
END;
CREATE TRIGGER ledger_grace_uses_identity_immutable
BEFORE UPDATE OF use_id, account_id, period, hold_id, unit, reserve_micros, shortfall_micros, floor_micros, used_at_ms
ON ledger_grace_uses
BEGIN
    SELECT RAISE(ABORT, 'grace use identity is immutable');
END;
CREATE TRIGGER ledger_grace_uses_void_once BEFORE UPDATE OF voided_at_ms ON ledger_grace_uses
WHEN OLD.voided_at_ms IS NOT NULL OR NEW.voided_at_ms IS NULL
BEGIN
    SELECT RAISE(ABORT, 'a grace use can only be voided once');
END;
CREATE TRIGGER ledger_grace_uses_never_deleted BEFORE DELETE ON ledger_grace_uses
BEGIN
    SELECT RAISE(ABORT, 'grace uses are never deleted');
END;

-- ---------------------------------------------------------------------------------------------
-- 告警：可查询的记录（日志之外）。只追加，只存稳定的代码，没有任何请求正文。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_alerts (
    alert_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    kind         TEXT NOT NULL CHECK (kind IN (
        'credit_used', 'grace_used', 'platform_loss', 'credential_frozen',
        'freeze_not_applied', 'late_settlement', 'extra_unsettled')),
    account_id   TEXT NOT NULL,
    guarantor_id TEXT,
    credential_id TEXT,
    hold_id      TEXT,
    amount_micros INTEGER NOT NULL DEFAULT 0 CHECK (amount_micros >= 0),
    unit         TEXT NOT NULL CHECK (unit IN ('THB', 'USD') OR unit GLOB 'QUOTA:*'),
    detail       TEXT NOT NULL CHECK (length(detail) BETWEEN 1 AND 200),
    at_ms        INTEGER NOT NULL CHECK (at_ms >= 0)
) STRICT;

CREATE INDEX ledger_alerts_by_kind ON ledger_alerts (kind, alert_id);
CREATE INDEX ledger_alerts_by_account ON ledger_alerts (account_id, alert_id);

CREATE TRIGGER ledger_alerts_insert_no_conflict BEFORE INSERT ON ledger_alerts
WHEN NEW.alert_id IS NOT NULL AND EXISTS (SELECT 1 FROM ledger_alerts WHERE alert_id = NEW.alert_id)
BEGIN
    SELECT RAISE(ABORT, 'alert already exists');
END;
CREATE TRIGGER ledger_alerts_append_only_update BEFORE UPDATE ON ledger_alerts
BEGIN
    SELECT RAISE(ABORT, 'ledger_alerts is append-only');
END;
CREATE TRIGGER ledger_alerts_append_only_delete BEFORE DELETE ON ledger_alerts
BEGIN
    SELECT RAISE(ABORT, 'ledger_alerts is append-only');
END;

-- ---------------------------------------------------------------------------------------------
-- 免费额度发放的补贴池承诺：幂等键是 `subsidy:<主体>:<YYYY-MM>`（和 allocations 表里那条额度的键相同）。
-- 一个池一个账期发出去的免费额度之和不能超过该账期的总上限（发放时检查，花钱时还有第五项检查兜底）。
-- ---------------------------------------------------------------------------------------------
CREATE TABLE ledger_subsidy_grants (
    idempotency_key TEXT NOT NULL PRIMARY KEY CHECK (idempotency_key GLOB 'subsidy:?*:[0-9][0-9][0-9][0-9]-[01][0-9]'),
    pool_id         TEXT NOT NULL,
    principal_id    TEXT NOT NULL,
    member_account  TEXT NOT NULL,
    period          TEXT NOT NULL CHECK (period GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]'),
    unit            TEXT NOT NULL CHECK (unit IN ('THB', 'USD') OR unit GLOB 'QUOTA:*'),
    amount_micros   INTEGER NOT NULL CHECK (amount_micros >= 0),
    allocation_id   TEXT NOT NULL,
    granted_at_ms   INTEGER NOT NULL CHECK (granted_at_ms >= 0)
) STRICT;

CREATE INDEX ledger_subsidy_grants_by_pool ON ledger_subsidy_grants (pool_id, period);

CREATE TRIGGER ledger_subsidy_grants_insert_no_conflict BEFORE INSERT ON ledger_subsidy_grants
WHEN EXISTS (SELECT 1 FROM ledger_subsidy_grants WHERE idempotency_key = NEW.idempotency_key)
BEGIN
    SELECT RAISE(ABORT, 'subsidy grant already exists');
END;
CREATE TRIGGER ledger_subsidy_grants_insert_rules BEFORE INSERT ON ledger_subsidy_grants
BEGIN
    SELECT RAISE(ABORT, 'subsidy grant pool must be an existing subsidy pool of the same unit')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.pool_id AND kind = 'subsidy_pool' AND unit = NEW.unit);
    SELECT RAISE(ABORT, 'subsidy grant must have a matching free-tier allocation')
     WHERE NOT EXISTS (SELECT 1 FROM allocations
                        WHERE allocation_id = NEW.allocation_id AND idempotency_key = NEW.idempotency_key
                          AND purpose = 'free_tier' AND ceiling_micros = NEW.amount_micros);
END;
CREATE TRIGGER ledger_subsidy_grants_append_only_update BEFORE UPDATE ON ledger_subsidy_grants
BEGIN
    SELECT RAISE(ABORT, 'ledger_subsidy_grants is append-only');
END;
CREATE TRIGGER ledger_subsidy_grants_append_only_delete BEFORE DELETE ON ledger_subsidy_grants
BEGIN
    SELECT RAISE(ABORT, 'ledger_subsidy_grants is append-only');
END;

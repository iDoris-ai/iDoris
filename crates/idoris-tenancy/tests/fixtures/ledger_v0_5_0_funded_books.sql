-- 旧库夹具（只读，不要手工编辑）：v0.5.0 的账户 / 账本代码写下的计算服务账本，里面有充值、授信、花费类额度三样都有的账户。
--
-- 由 7e4afe8d（`git diff v0.5.0 HEAD -- crates/idoris-tenancy/src/accounts crates/idoris-compute-contracts` 为空，所以账户、账本
-- 代码和 v0.5.0 逐字一致）真实跑出来：
--   1. `AccountStore::open` 建库（账户迁移 1~2、账本迁移 1~4）；
--   2. 用现有的写入函数写数据：充值、授信、花费类额度、准入、结算（组织 `paid` 下五个成员 m-a ~ m-e，场景见
--      crates/idoris-tenancy/tests/ledger_v0_5_0_upgrade.rs 的头注释）；
--   3. `VACUUM INTO` 成单文件，再用 Python `sqlite3.iterdump()` 导成文本。
-- 表结构、触发器、索引和数据都是旧代码写下的原样。
--
-- 基线（v0.5.0）在时刻 1793534411000（= T0 + 11 秒，下面所有额度的窗口都已开始）读到的 `account_view`，是旧代码当时打印出来的，
-- 不是被测代码算的：
--   m-a  balance=None       ceiling_remaining=Some(-100)  credit_limit=200
--   m-b  balance=Some(-100) ceiling_remaining=Some(100)   credit_limit=100
--   m-c  balance=Some(40)   ceiling_remaining=Some(40)    credit_limit=0
--   m-d  balance=Some(-50)  ceiling_remaining=None        credit_limit=100
--   m-e  balance=Some(-150) ceiling_remaining=Some(100)   credit_limit=200
-- 基线写下的 `credit_used` 告警 6 条：m-a 100、200；m-b 100；m-d 50；m-e 100、200。
BEGIN TRANSACTION;
CREATE TABLE account_schema_migrations (version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT;
INSERT INTO "account_schema_migrations" VALUES(1,1795335498701);
INSERT INTO "account_schema_migrations" VALUES(2,1795335498703);
CREATE TABLE accounts (
    account_id    TEXT NOT NULL PRIMARY KEY
        CHECK (length(account_id) BETWEEN 1 AND 128 AND account_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
               AND instr(CAST(account_id AS BLOB), x'00') = 0),
    kind          TEXT NOT NULL CHECK (kind IN ('platform', 'subsidy_pool', 'org', 'sponsor', 'member')),
    parent_id     TEXT REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    unit          TEXT NOT NULL CHECK (
        instr(CAST(unit AS BLOB), x'00') = 0
        AND (unit IN ('THB', 'USD')
             OR (unit GLOB 'QUOTA:*' AND length(unit) BETWEEN 7 AND 70
                 AND substr(unit, 7) NOT GLOB '*[^A-Za-z0-9._-]*'))
    ),
    status        TEXT NOT NULL CHECK (status IN ('active', 'frozen', 'closed')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    -- 只有平台根没有父账户；任何账户都不能是自己的父账户。
    CHECK ((kind = 'platform') = (parent_id IS NULL)),
    CHECK (parent_id IS NULL OR parent_id <> account_id)
) STRICT;
INSERT INTO "accounts" VALUES('platform','platform',NULL,'THB','active',1793534400000);
INSERT INTO "accounts" VALUES('pool','subsidy_pool','platform','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('school','org','pool','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('teacher','sponsor','school','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('kid-a','member','teacher','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('kid-b','member','teacher','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('kid-c','member','school','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('paid','org','platform','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('emp','member','paid','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('m-a','member','paid','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('m-b','member','paid','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('m-c','member','paid','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('m-d','member','paid','THB','active',1793534400000);
INSERT INTO "accounts" VALUES('m-e','member','paid','THB','active',1793534400000);
CREATE TABLE allocation_revocations (
    allocation_id   TEXT NOT NULL PRIMARY KEY REFERENCES allocations (allocation_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    revoked_at_ms   INTEGER NOT NULL CHECK (revoked_at_ms >= 0),
    revoked_by_kind TEXT NOT NULL CHECK (revoked_by_kind IN ('credential', 'operator')),
    revoked_by_id   TEXT NOT NULL CHECK (
        length(revoked_by_id) BETWEEN 1 AND 128 AND revoked_by_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
        AND instr(CAST(revoked_by_id AS BLOB), x'00') = 0)
) STRICT;
CREATE TABLE allocations (
    allocation_id   TEXT NOT NULL PRIMARY KEY
        CHECK (length(allocation_id) BETWEEN 1 AND 128 AND allocation_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
               AND instr(CAST(allocation_id AS BLOB), x'00') = 0),
    parent_id       TEXT NOT NULL REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    child_id        TEXT NOT NULL REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    ceiling_unit    TEXT NOT NULL CHECK (
        instr(CAST(ceiling_unit AS BLOB), x'00') = 0
        AND (ceiling_unit IN ('THB', 'USD')
             OR (ceiling_unit GLOB 'QUOTA:*' AND length(ceiling_unit) BETWEEN 7 AND 70
                 AND substr(ceiling_unit, 7) NOT GLOB '*[^A-Za-z0-9._-]*'))
    ),
    ceiling_micros  INTEGER NOT NULL CHECK (ceiling_micros >= 0),
    purpose         TEXT NOT NULL CHECK (purpose IN ('free_tier', 'prepaid', 'credit_line', 'org_quota')),
    valid_from_ms   INTEGER NOT NULL CHECK (valid_from_ms >= 0),
    valid_until_ms  INTEGER NOT NULL,
    transferable    INTEGER NOT NULL DEFAULT 0 CHECK (transferable = 0),
    idempotency_key TEXT CHECK (
        idempotency_key IS NULL
        OR (length(idempotency_key) BETWEEN 1 AND 192 AND idempotency_key NOT GLOB '*[^A-Za-z0-9._:~-]*'
            AND instr(CAST(idempotency_key AS BLOB), x'00') = 0)
    ),
    created_at_ms   INTEGER NOT NULL CHECK (created_at_ms >= 0),
    CHECK (valid_from_ms < valid_until_ms),
    CHECK (parent_id <> child_id),
    -- 用途与单位相容（S0 `Purpose::allows_unit`）：预付是真金白银只能是货币；组织配额只能是 QUOTA。
    CHECK (purpose <> 'prepaid' OR ceiling_unit IN ('THB', 'USD')),
    CHECK (purpose <> 'org_quota' OR ceiling_unit GLOB 'QUOTA:*'),
    -- 免费额度必带幂等键，形状是 `subsidy:<主体>:<YYYY-MM>`（设计稿 §4：发放带幂等键 (成员, 月份)），
    -- 且 `subsidy:` 开头的键只能给免费额度。键里的主体是不是这个子账户所属成员的主体，要查成员关系表，
    -- 由 Rust 层（`AccountStore::create_allocation`）检查。
    -- 注意 CHECK 对 NULL 是放行的，所以这里必须显式写 `IS NOT NULL`，否则没带键的免费额度会溜进来。
    CHECK (purpose <> 'free_tier'
           OR (idempotency_key IS NOT NULL
               AND idempotency_key GLOB 'subsidy:?*:[0-9][0-9][0-9][0-9]-[01][0-9]')),
    CHECK (purpose = 'free_tier' OR idempotency_key IS NULL OR idempotency_key NOT GLOB 'subsidy:*')
) STRICT;
INSERT INTO "allocations" VALUES('e-0','platform','paid','THB',9000000,'prepaid',1793448000000,1798718400000,0,NULL,1793534400000);
INSERT INTO "allocations" VALUES('cl-dd596ea632004486adc48493279fbfa3','paid','m-a','THB',200,'credit_line',1793534400000,253402300800000,0,NULL,1793534400000);
INSERT INTO "allocations" VALUES('a-allow','paid','m-a','THB',100,'prepaid',1793534410000,1796126410000,0,NULL,1793534410000);
INSERT INTO "allocations" VALUES('cl-2aa78ef4f03741f8a9140323609954a4','paid','m-b','THB',100,'credit_line',1793534400000,253402300800000,0,NULL,1793534400000);
INSERT INTO "allocations" VALUES('b-allow','paid','m-b','THB',100,'prepaid',1793534410000,1796126410000,0,NULL,1793534410000);
INSERT INTO "allocations" VALUES('c-allow','paid','m-c','THB',100,'prepaid',1793448000000,1796126400000,0,NULL,1793534400000);
INSERT INTO "allocations" VALUES('cl-2404f88e423e4340a09d62b8e83542b5','paid','m-d','THB',100,'credit_line',1793534400000,253402300800000,0,NULL,1793534400000);
INSERT INTO "allocations" VALUES('cl-d59e5460bfbd4e0aabc7c1f3ac254399','paid','m-e','THB',200,'credit_line',1793534400000,253402300800000,0,NULL,1793534400000);
INSERT INTO "allocations" VALUES('e-allow','paid','m-e','THB',100,'prepaid',1793534410000,1796126410000,0,NULL,1793534410000);
CREATE TABLE control_audit (
    audit_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms         INTEGER NOT NULL CHECK (at_ms >= 0),
    -- 控制面请求的关联 id（响应头 X-iDoris-Record-Id），同一个请求的审计行与日志对得上。
    request_id    TEXT NOT NULL CHECK (
        length(request_id) BETWEEN 1 AND 64 AND request_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
        AND instr(CAST(request_id AS BLOB), x'00') = 0),
    -- 谁：凭证（actor_id = 凭证 id）、配对码（actor_id = 配对记录 id，**不是码本身**）、本机运维命令（actor_id = 'cli'）。
    actor_kind    TEXT NOT NULL CHECK (actor_kind IN ('credential', 'pairing_code', 'operator')),
    actor_id      TEXT NOT NULL CHECK (
        length(actor_id) BETWEEN 1 AND 128 AND actor_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
        AND instr(CAST(actor_id AS BLOB), x'00') = 0),
    -- 什么操作：API 层的操作名（create_account、issue_credential、redeem_pairing_code……），不是自由文本。
    op            TEXT NOT NULL CHECK (
        length(op) BETWEEN 1 AND 64 AND op NOT GLOB '*[^a-z_]*' AND instr(CAST(op AS BLOB), x'00') = 0),
    -- 目标资源：种类 + id。id 永远是标识符，不是密钥。
    target_kind   TEXT NOT NULL CHECK (
        target_kind IN ('account', 'principal', 'membership', 'credential', 'allocation', 'pairing_code', 'none')),
    target_id     TEXT CHECK (
        target_id IS NULL OR (length(target_id) BETWEEN 1 AND 192 AND target_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
                              AND instr(CAST(target_id AS BLOB), x'00') = 0)),
    -- 做授权判定时用的那个账户（目标账户，见 compute-contracts 的 control 模块）。按子树查审计就靠它。
    scope_account TEXT CHECK (
        scope_account IS NULL OR (length(scope_account) BETWEEN 1 AND 128
                                  AND scope_account NOT GLOB '*[^A-Za-z0-9._:~-]*'
                                  AND instr(CAST(scope_account AS BLOB), x'00') = 0)),
    -- 第二个相关 id：额度的授权方、成员关系的主体、凭证的绑定目标……
    related_id    TEXT CHECK (
        related_id IS NULL OR (length(related_id) BETWEEN 1 AND 192 AND related_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
                               AND instr(CAST(related_id AS BLOB), x'00') = 0)),
    amount_unit   TEXT CHECK (
        amount_unit IS NULL OR (
            instr(CAST(amount_unit AS BLOB), x'00') = 0
            AND (amount_unit IN ('THB', 'USD')
                 OR (amount_unit GLOB 'QUOTA:*' AND length(amount_unit) BETWEEN 7 AND 70
                     AND substr(amount_unit, 7) NOT GLOB '*[^A-Za-z0-9._-]*')))),
    amount_micros INTEGER,
    outcome       TEXT NOT NULL CHECK (outcome IN ('success', 'replayed', 'unchanged', 'denied', 'rejected', 'error')),
    reason_code   TEXT NOT NULL CHECK (
        length(reason_code) BETWEEN 1 AND 64 AND reason_code NOT GLOB '*[^a-z_]*'
        AND instr(CAST(reason_code AS BLOB), x'00') = 0),
    CHECK ((amount_unit IS NULL) = (amount_micros IS NULL)),
    -- 成功类的结果原因码固定是 'ok'；失败类不能是 'ok'。
    CHECK ((outcome IN ('success', 'replayed', 'unchanged')) = (reason_code = 'ok'))
) STRICT;
CREATE TABLE credential_admin_scopes (
    credential_id     TEXT NOT NULL REFERENCES credentials (credential_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    position          INTEGER NOT NULL CHECK (position >= 0),
    op                TEXT NOT NULL CHECK (length(op) BETWEEN 1 AND 64 AND op NOT GLOB '*[^a-z_]*'
                                           AND instr(CAST(op AS BLOB), x'00') = 0),
    subtree_id        TEXT NOT NULL REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    max_amount_unit   TEXT CHECK (
        max_amount_unit IS NULL
        OR (instr(CAST(max_amount_unit AS BLOB), x'00') = 0
            AND (max_amount_unit IN ('THB', 'USD')
                 OR (max_amount_unit GLOB 'QUOTA:*' AND length(max_amount_unit) BETWEEN 7 AND 70
                     AND substr(max_amount_unit, 7) NOT GLOB '*[^A-Za-z0-9._-]*')))
    ),
    max_amount_micros INTEGER,
    PRIMARY KEY (credential_id, position),
    CHECK ((max_amount_unit IS NULL) = (max_amount_micros IS NULL)),
    -- 额度上限只属于 grant_allocation，且不能为负（与 S0 `Credential::new` 一致）。
    CHECK (max_amount_micros IS NULL OR (op = 'grant_allocation' AND max_amount_micros >= 0))
) STRICT;
CREATE TABLE credential_issuance (
    credential_id  TEXT NOT NULL PRIMARY KEY REFERENCES credentials (credential_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    issued_by_kind TEXT NOT NULL CHECK (issued_by_kind IN ('credential', 'pairing_code', 'operator')),
    issued_by_id   TEXT NOT NULL CHECK (
        length(issued_by_id) BETWEEN 1 AND 128 AND issued_by_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
        AND instr(CAST(issued_by_id AS BLOB), x'00') = 0),
    issued_at_ms   INTEGER NOT NULL CHECK (issued_at_ms >= 0)
) STRICT;
CREATE TABLE credentials (
    credential_id TEXT NOT NULL PRIMARY KEY
        CHECK (length(credential_id) BETWEEN 1 AND 128 AND credential_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
               AND instr(CAST(credential_id AS BLOB), x'00') = 0),
    binding_kind  TEXT NOT NULL CHECK (binding_kind IN ('service', 'node', 'member')),
    account_id    TEXT REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    membership_id TEXT REFERENCES memberships (membership_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    status        TEXT NOT NULL CHECK (status IN ('active', 'frozen', 'revoked')),
    expires_at_ms INTEGER CHECK (expires_at_ms IS NULL OR expires_at_ms > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    revoked_at_ms INTEGER,
    scope_count   INTEGER NOT NULL DEFAULT 0 CHECK (scope_count BETWEEN 0 AND 64),
    CHECK (
        (binding_kind IN ('service', 'node') AND account_id IS NOT NULL AND membership_id IS NULL)
        OR (binding_kind = 'member' AND membership_id IS NOT NULL AND account_id IS NULL)
    ),
    CHECK ((status = 'revoked') = (revoked_at_ms IS NOT NULL)),
    CHECK (binding_kind <> 'member' OR scope_count = 0)
) STRICT;
INSERT INTO "credentials" VALUES('cred-t','member',NULL,'m-t','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-a','member',NULL,'m-a','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-b','member',NULL,'m-b','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-c','member',NULL,'m-c','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-e','member',NULL,'m-e','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-m-a','member',NULL,'m-m-a','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-m-b','member',NULL,'m-m-b','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-m-c','member',NULL,'m-m-c','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-m-d','member',NULL,'m-m-d','active',NULL,1793534400000,NULL,0);
INSERT INTO "credentials" VALUES('cred-m-e','member',NULL,'m-m-e','active',NULL,1793534400000,NULL,0);
CREATE TABLE ledger_account_limits (
    version             INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id          TEXT NOT NULL,
    inflight_cap_micros INTEGER CHECK (inflight_cap_micros IS NULL OR inflight_cap_micros >= 0),
    minute_cap_micros   INTEGER CHECK (minute_cap_micros IS NULL OR minute_cap_micros >= 0),
    hour_cap_micros     INTEGER CHECK (hour_cap_micros IS NULL OR hour_cap_micros >= 0),
    set_at_ms           INTEGER NOT NULL CHECK (set_at_ms >= 0)
) STRICT;
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
INSERT INTO "ledger_alerts" VALUES(1,'credit_used','m-a','paid','cred-m-a','a-1#0',100,'THB','overdraft_within_credit',1793534401000);
INSERT INTO "ledger_alerts" VALUES(2,'credit_used','m-a','paid','cred-m-a','a-2#0',200,'THB','overdraft_within_credit',1793534402000);
INSERT INTO "ledger_alerts" VALUES(3,'credit_used','m-b','paid','cred-m-b','b-2#0',100,'THB','overdraft_within_credit',1793534402000);
INSERT INTO "ledger_alerts" VALUES(4,'credit_used','m-d','paid','cred-m-d','d-2#0',50,'THB','overdraft_within_credit',1793534402000);
INSERT INTO "ledger_alerts" VALUES(5,'credit_used','m-e','paid','cred-m-e','e-1#0',100,'THB','overdraft_within_credit',1793534401000);
INSERT INTO "ledger_alerts" VALUES(6,'credit_used','m-e','paid','cred-m-e','e-2#0',200,'THB','overdraft_within_credit',1793534402000);
CREATE TABLE ledger_allocation_marks (
    allocation_id TEXT NOT NULL CHECK (length(allocation_id) BETWEEN 1 AND 128),
    -- created：额度创建；revoked：额度被撤销（只有被撤销的额度有）
    event         TEXT NOT NULL CHECK (event IN ('created', 'revoked')),
    -- 事件发生时账本里最后一条分录的号；分录号大于它的分录是事件之后才提交的
    after_entry   INTEGER NOT NULL CHECK (after_entry >= 0),
    PRIMARY KEY (allocation_id, event)
) STRICT;
INSERT INTO "ledger_allocation_marks" VALUES('e-0','created',1);
INSERT INTO "ledger_allocation_marks" VALUES('cl-dd596ea632004486adc48493279fbfa3','created',1);
INSERT INTO "ledger_allocation_marks" VALUES('a-allow','created',13);
INSERT INTO "ledger_allocation_marks" VALUES('cl-2aa78ef4f03741f8a9140323609954a4','created',14);
INSERT INTO "ledger_allocation_marks" VALUES('b-allow','created',26);
INSERT INTO "ledger_allocation_marks" VALUES('c-allow','created',27);
INSERT INTO "ledger_allocation_marks" VALUES('cl-2404f88e423e4340a09d62b8e83542b5','created',34);
INSERT INTO "ledger_allocation_marks" VALUES('cl-d59e5460bfbd4e0aabc7c1f3ac254399','created',46);
INSERT INTO "ledger_allocation_marks" VALUES('e-allow','created',58);
CREATE TABLE ledger_config (
    key   TEXT NOT NULL PRIMARY KEY CHECK (key IN ('billing_timezone', 'reservation_ttl_ms')),
    value TEXT NOT NULL CHECK (length(value) BETWEEN 1 AND 64)
) STRICT;
INSERT INTO "ledger_config" VALUES('billing_timezone','UTC');
INSERT INTO "ledger_config" VALUES('reservation_ttl_ms','1800000');
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
INSERT INTO "ledger_entries" VALUES(1,'paid',NULL,'fund','THB',0,0,1000000,0,1793534400000,1793534400000,'fund|f-paid');
INSERT INTO "ledger_entries" VALUES(2,'m-a','a-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'a-1#0|reserve|m-a');
INSERT INTO "ledger_entries" VALUES(3,'paid','a-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'a-1#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(4,'platform','a-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'a-1#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(5,'m-a','a-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'a-1#0|settle|m-a');
INSERT INTO "ledger_entries" VALUES(6,'paid','a-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'a-1#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(7,'platform','a-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'a-1#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(8,'m-a','a-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'a-2#0|reserve|m-a');
INSERT INTO "ledger_entries" VALUES(9,'paid','a-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'a-2#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(10,'platform','a-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'a-2#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(11,'m-a','a-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'a-2#0|settle|m-a');
INSERT INTO "ledger_entries" VALUES(12,'paid','a-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'a-2#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(13,'platform','a-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'a-2#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(14,'m-b',NULL,'fund','THB',0,0,100,0,1793534400000,1793534400000,'fund|f-m-b');
INSERT INTO "ledger_entries" VALUES(15,'m-b','b-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'b-1#0|reserve|m-b');
INSERT INTO "ledger_entries" VALUES(16,'paid','b-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'b-1#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(17,'platform','b-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'b-1#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(18,'m-b','b-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'b-1#0|settle|m-b');
INSERT INTO "ledger_entries" VALUES(19,'paid','b-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'b-1#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(20,'platform','b-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'b-1#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(21,'m-b','b-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'b-2#0|reserve|m-b');
INSERT INTO "ledger_entries" VALUES(22,'paid','b-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'b-2#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(23,'platform','b-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'b-2#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(24,'m-b','b-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'b-2#0|settle|m-b');
INSERT INTO "ledger_entries" VALUES(25,'paid','b-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'b-2#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(26,'platform','b-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'b-2#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(27,'m-c',NULL,'fund','THB',0,0,100,0,1793534400000,1793534400000,'fund|f-m-c');
INSERT INTO "ledger_entries" VALUES(28,'m-c','c-1#0','reserve','THB',60,0,0,0,1793534401000,1793534401000,'c-1#0|reserve|m-c');
INSERT INTO "ledger_entries" VALUES(29,'paid','c-1#0','reserve','THB',60,0,0,0,1793534401000,1793534401000,'c-1#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(30,'platform','c-1#0','reserve','THB',60,0,0,0,1793534401000,1793534401000,'c-1#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(31,'m-c','c-1#0','settle','THB',-60,60,0,0,1793534401000,1793534401500,'c-1#0|settle|m-c');
INSERT INTO "ledger_entries" VALUES(32,'paid','c-1#0','settle','THB',-60,60,0,0,1793534401000,1793534401500,'c-1#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(33,'platform','c-1#0','settle','THB',-60,60,0,0,1793534401000,1793534401500,'c-1#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(34,'m-d',NULL,'fund','THB',0,0,100,0,1793534400000,1793534400000,'fund|f-m-d');
INSERT INTO "ledger_entries" VALUES(35,'m-d','d-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'d-1#0|reserve|m-d');
INSERT INTO "ledger_entries" VALUES(36,'paid','d-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'d-1#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(37,'platform','d-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'d-1#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(38,'m-d','d-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'d-1#0|settle|m-d');
INSERT INTO "ledger_entries" VALUES(39,'paid','d-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'d-1#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(40,'platform','d-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'d-1#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(41,'m-d','d-2#0','reserve','THB',50,0,0,0,1793534402000,1793534402000,'d-2#0|reserve|m-d');
INSERT INTO "ledger_entries" VALUES(42,'paid','d-2#0','reserve','THB',50,0,0,0,1793534402000,1793534402000,'d-2#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(43,'platform','d-2#0','reserve','THB',50,0,0,0,1793534402000,1793534402000,'d-2#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(44,'m-d','d-2#0','settle','THB',-50,50,0,0,1793534402000,1793534402500,'d-2#0|settle|m-d');
INSERT INTO "ledger_entries" VALUES(45,'paid','d-2#0','settle','THB',-50,50,0,0,1793534402000,1793534402500,'d-2#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(46,'platform','d-2#0','settle','THB',-50,50,0,0,1793534402000,1793534402500,'d-2#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(47,'m-e','e-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'e-1#0|reserve|m-e');
INSERT INTO "ledger_entries" VALUES(48,'paid','e-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'e-1#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(49,'platform','e-1#0','reserve','THB',100,0,0,0,1793534401000,1793534401000,'e-1#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(50,'m-e','e-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'e-1#0|settle|m-e');
INSERT INTO "ledger_entries" VALUES(51,'paid','e-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'e-1#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(52,'platform','e-1#0','settle','THB',-100,100,0,0,1793534401000,1793534401500,'e-1#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(53,'m-e','e-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'e-2#0|reserve|m-e');
INSERT INTO "ledger_entries" VALUES(54,'paid','e-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'e-2#0|reserve|paid');
INSERT INTO "ledger_entries" VALUES(55,'platform','e-2#0','reserve','THB',100,0,0,0,1793534402000,1793534402000,'e-2#0|reserve|platform');
INSERT INTO "ledger_entries" VALUES(56,'m-e','e-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'e-2#0|settle|m-e');
INSERT INTO "ledger_entries" VALUES(57,'paid','e-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'e-2#0|settle|paid');
INSERT INTO "ledger_entries" VALUES(58,'platform','e-2#0','settle','THB',-100,100,0,0,1793534402000,1793534402500,'e-2#0|settle|platform');
INSERT INTO "ledger_entries" VALUES(59,'m-e',NULL,'fund','THB',0,0,50,0,1793534420000,1793534420000,'fund|f-m-e');
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
INSERT INTO "ledger_holds" VALUES('a-1#0','a-1#0','model','a-1',0,'m-a','cred-m-a','THB',100,'2026-11',1793534401000,1793536201000,NULL,'settled',NULL,1793534401500,100,0);
INSERT INTO "ledger_holds" VALUES('a-2#0','a-2#0','model','a-2',0,'m-a','cred-m-a','THB',100,'2026-11',1793534402000,1793536202000,NULL,'settled',NULL,1793534402500,100,0);
INSERT INTO "ledger_holds" VALUES('b-1#0','b-1#0','model','b-1',0,'m-b','cred-m-b','THB',100,'2026-11',1793534401000,1793536201000,NULL,'settled',NULL,1793534401500,100,0);
INSERT INTO "ledger_holds" VALUES('b-2#0','b-2#0','model','b-2',0,'m-b','cred-m-b','THB',100,'2026-11',1793534402000,1793536202000,NULL,'settled',NULL,1793534402500,100,0);
INSERT INTO "ledger_holds" VALUES('c-1#0','c-1#0','model','c-1',0,'m-c','cred-m-c','THB',60,'2026-11',1793534401000,1793536201000,NULL,'settled',NULL,1793534401500,60,0);
INSERT INTO "ledger_holds" VALUES('d-1#0','d-1#0','model','d-1',0,'m-d','cred-m-d','THB',100,'2026-11',1793534401000,1793536201000,NULL,'settled',NULL,1793534401500,100,0);
INSERT INTO "ledger_holds" VALUES('d-2#0','d-2#0','model','d-2',0,'m-d','cred-m-d','THB',50,'2026-11',1793534402000,1793536202000,NULL,'settled',NULL,1793534402500,50,0);
INSERT INTO "ledger_holds" VALUES('e-1#0','e-1#0','model','e-1',0,'m-e','cred-m-e','THB',100,'2026-11',1793534401000,1793536201000,NULL,'settled',NULL,1793534401500,100,0);
INSERT INTO "ledger_holds" VALUES('e-2#0','e-2#0','model','e-2',0,'m-e','cred-m-e','THB',100,'2026-11',1793534402000,1793536202000,NULL,'settled',NULL,1793534402500,100,0);
CREATE TABLE ledger_pool_caps (
    version   INTEGER PRIMARY KEY AUTOINCREMENT,
    pool_id   TEXT NOT NULL,
    period    TEXT NOT NULL CHECK (period GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]'),
    unit      TEXT NOT NULL CHECK (unit IN ('THB', 'USD') OR unit GLOB 'QUOTA:*'),
    cap_micros INTEGER NOT NULL CHECK (cap_micros >= 0),
    set_at_ms INTEGER NOT NULL CHECK (set_at_ms >= 0)
) STRICT;
CREATE TABLE ledger_schema_migrations (version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT;
INSERT INTO "ledger_schema_migrations" VALUES(1,1795335498734);
INSERT INTO "ledger_schema_migrations" VALUES(2,1795335498735);
INSERT INTO "ledger_schema_migrations" VALUES(3,1795335498735);
INSERT INTO "ledger_schema_migrations" VALUES(4,1795335498736);
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
CREATE TABLE memberships (
    membership_id TEXT NOT NULL PRIMARY KEY
        CHECK (length(membership_id) BETWEEN 1 AND 128 AND membership_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
               AND instr(CAST(membership_id AS BLOB), x'00') = 0),
    principal_id  TEXT NOT NULL REFERENCES principals (principal_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    org_id        TEXT NOT NULL REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    role          TEXT NOT NULL CHECK (role IN ('member', 'sponsor', 'admin')),
    account_id    TEXT REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    status        TEXT NOT NULL CHECK (status IN ('active', 'revoked')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    revoked_at_ms INTEGER,
    -- 管理员没有付款账户，成员 / 担保人必须有。
    CHECK ((role = 'admin') = (account_id IS NULL)),
    CHECK (account_id IS NULL OR account_id <> org_id),
    CHECK ((status = 'revoked') = (revoked_at_ms IS NOT NULL))
) STRICT;
INSERT INTO "memberships" VALUES('m-t','p-t','school','sponsor','teacher','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-a','p-a','school','member','kid-a','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-b','p-b','school','member','kid-b','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-c','p-c','school','member','kid-c','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-e','p-e','paid','member','emp','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-m-a','p-m-a','paid','member','m-a','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-m-b','p-m-b','paid','member','m-b','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-m-c','p-m-c','paid','member','m-c','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-m-d','p-m-d','paid','member','m-d','active',1793534400000,NULL);
INSERT INTO "memberships" VALUES('m-m-e','p-m-e','paid','member','m-e','active',1793534400000,NULL);
CREATE TABLE pairing_codes (
    pairing_id     TEXT NOT NULL PRIMARY KEY CHECK (
        length(pairing_id) BETWEEN 1 AND 128 AND pairing_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
        AND instr(CAST(pairing_id AS BLOB), x'00') = 0),
    code_hash      BLOB NOT NULL UNIQUE CHECK (length(code_hash) = 32),
    membership_id  TEXT NOT NULL REFERENCES memberships (membership_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    -- 签发这个码的凭证：兑换时要重新确认它还有资格。
    issued_by      TEXT NOT NULL REFERENCES credentials (credential_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    created_at_ms  INTEGER NOT NULL CHECK (created_at_ms >= 0),
    expires_at_ms  INTEGER NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('pending', 'redeemed', 'revoked')),
    redeemed_at_ms INTEGER,
    redeemed_credential_id TEXT REFERENCES credentials (credential_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    revoked_at_ms  INTEGER,
    device_privacy TEXT NOT NULL CHECK (json_valid(device_privacy) AND json_type(device_privacy) = 'array'),
    device_roles   TEXT NOT NULL CHECK (json_valid(device_roles) AND json_type(device_roles) = 'array'),
    device_tenant_id TEXT CHECK (
        device_tenant_id IS NULL OR (length(device_tenant_id) BETWEEN 1 AND 128 AND trim(device_tenant_id) = device_tenant_id)),
    -- 兑换出来的设备凭证的寿命（从兑换那一刻算）；NULL = 不设有效期（仍然不会超过签发者自己的有效期）。
    device_ttl_ms  INTEGER CHECK (device_ttl_ms IS NULL OR device_ttl_ms > 0),
    CHECK (expires_at_ms > created_at_ms),
    CHECK ((status = 'redeemed') = (redeemed_at_ms IS NOT NULL AND redeemed_credential_id IS NOT NULL)),
    CHECK ((status = 'revoked') = (revoked_at_ms IS NOT NULL)),
    CHECK (status <> 'redeemed' OR revoked_at_ms IS NULL)
) STRICT;
CREATE TABLE pairing_devices (
    credential_id TEXT NOT NULL PRIMARY KEY REFERENCES credentials (credential_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    membership_id TEXT NOT NULL REFERENCES memberships (membership_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    pairing_id    TEXT NOT NULL REFERENCES pairing_codes (pairing_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    -- 设备的显示名，用户自己填的文本：只进这张表，从不进审计和日志。
    label         TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 64),
    paired_at_ms  INTEGER NOT NULL CHECK (paired_at_ms >= 0)
) STRICT;
CREATE TABLE principal_registrations (
    principal_id  TEXT NOT NULL PRIMARY KEY REFERENCES principals (principal_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    org_id        TEXT NOT NULL REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    registered_at_ms INTEGER NOT NULL CHECK (registered_at_ms >= 0)
) STRICT;
CREATE TABLE principals (
    principal_id  TEXT NOT NULL PRIMARY KEY
        CHECK (length(principal_id) BETWEEN 1 AND 128 AND principal_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
               AND instr(CAST(principal_id AS BLOB), x'00') = 0),
    status        TEXT NOT NULL CHECK (status IN ('active', 'revoked')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    revoked_at_ms INTEGER,
    CHECK ((status = 'revoked') = (revoked_at_ms IS NOT NULL))
) STRICT;
INSERT INTO "principals" VALUES('p-t','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-a','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-b','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-c','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-e','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-m-a','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-m-b','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-m-c','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-m-d','active',1793534400000,NULL);
INSERT INTO "principals" VALUES('p-m-e','active',1793534400000,NULL);
CREATE INDEX accounts_by_parent ON accounts (parent_id);
CREATE UNIQUE INDEX accounts_single_platform_root ON accounts (kind) WHERE kind = 'platform';
CREATE INDEX memberships_by_principal ON memberships (principal_id);
CREATE INDEX memberships_by_org ON memberships (org_id);
CREATE UNIQUE INDEX memberships_account_bound_once ON memberships (account_id) WHERE account_id IS NOT NULL;
CREATE INDEX credentials_by_account ON credentials (account_id) WHERE account_id IS NOT NULL;
CREATE INDEX credentials_by_membership ON credentials (membership_id) WHERE membership_id IS NOT NULL;
CREATE INDEX credential_admin_scopes_by_subtree ON credential_admin_scopes (subtree_id);
CREATE INDEX allocations_by_child ON allocations (child_id);
CREATE INDEX allocations_by_parent ON allocations (parent_id);
CREATE UNIQUE INDEX allocations_idempotency_key_unique ON allocations (idempotency_key)
    WHERE idempotency_key IS NOT NULL;
CREATE INDEX control_audit_by_scope ON control_audit (scope_account, audit_id);
CREATE INDEX control_audit_by_actor ON control_audit (actor_id, audit_id);
CREATE INDEX credential_issuance_by_issuer ON credential_issuance (issued_by_id);
CREATE INDEX pairing_codes_by_membership ON pairing_codes (membership_id);
CREATE INDEX pairing_devices_by_membership ON pairing_devices (membership_id);
CREATE UNIQUE INDEX pairing_devices_one_per_code ON pairing_devices (pairing_id);
CREATE INDEX ledger_account_limits_by_account ON ledger_account_limits (account_id, version);
CREATE INDEX ledger_pool_caps_by_pool ON ledger_pool_caps (pool_id, period, version);
CREATE INDEX ledger_holds_open_expiry ON ledger_holds (expires_at_ms) WHERE state = 'open';
CREATE INDEX ledger_holds_by_request ON ledger_holds (request_id);
CREATE INDEX ledger_holds_by_payer ON ledger_holds (payer_account);
CREATE INDEX ledger_entries_by_account_window ON ledger_entries (account_id, window_at_ms);
CREATE INDEX ledger_entries_by_hold ON ledger_entries (hold_id);
CREATE UNIQUE INDEX ledger_grace_one_active_per_period
    ON ledger_grace_uses (account_id, period) WHERE voided_at_ms IS NULL;
CREATE INDEX ledger_alerts_by_kind ON ledger_alerts (kind, alert_id);
CREATE INDEX ledger_alerts_by_account ON ledger_alerts (account_id, alert_id);
CREATE INDEX ledger_subsidy_grants_by_pool ON ledger_subsidy_grants (pool_id, period);
CREATE TRIGGER accounts_insert_rules BEFORE INSERT ON accounts
WHEN NEW.parent_id IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'parent account must exist before its child')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.parent_id);
    SELECT RAISE(ABORT, 'account unit must equal its parent unit')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.parent_id AND unit = NEW.unit);
    SELECT RAISE(ABORT, 'account kind is not allowed under its parent kind')
     WHERE NOT EXISTS (
        SELECT 1 FROM accounts p
         WHERE p.account_id = NEW.parent_id
           AND (   (p.kind = 'platform'     AND NEW.kind IN ('org', 'sponsor', 'subsidy_pool', 'member'))
                OR (p.kind = 'subsidy_pool' AND NEW.kind IN ('org', 'sponsor', 'member'))
                OR (p.kind = 'org'          AND NEW.kind IN ('org', 'sponsor', 'member'))
                OR (p.kind = 'sponsor'      AND NEW.kind = 'member')));
    SELECT RAISE(ABORT, 'account tree is deeper than the maximum depth')
     WHERE (WITH RECURSIVE up (id, d) AS (
                SELECT NEW.parent_id, 1
                UNION ALL
                SELECT a.parent_id, up.d + 1
                  FROM accounts a JOIN up ON a.account_id = up.id
                 WHERE a.parent_id IS NOT NULL AND up.d < 64
            )
            SELECT max(d) FROM up) > 5;
END;
CREATE TRIGGER accounts_identity_immutable
BEFORE UPDATE OF account_id, kind, parent_id, unit, created_at_ms ON accounts
BEGIN
    SELECT RAISE(ABORT, 'account identity columns are immutable');
END;
CREATE TRIGGER accounts_closed_is_terminal BEFORE UPDATE OF status ON accounts
WHEN OLD.status = 'closed' AND NEW.status <> 'closed'
BEGIN
    SELECT RAISE(ABORT, 'a closed account cannot be reopened');
END;
CREATE TRIGGER principals_identity_immutable
BEFORE UPDATE OF principal_id, created_at_ms ON principals
BEGIN
    SELECT RAISE(ABORT, 'principal identity columns are immutable');
END;
CREATE TRIGGER principals_revoked_is_terminal BEFORE UPDATE ON principals
WHEN OLD.status = 'revoked'
 AND (NEW.status <> 'revoked' OR NEW.revoked_at_ms IS NOT OLD.revoked_at_ms)
BEGIN
    SELECT RAISE(ABORT, 'a revoked principal cannot be changed');
END;
CREATE TRIGGER memberships_insert_rules BEFORE INSERT ON memberships
BEGIN
    SELECT RAISE(ABORT, 'membership principal must exist')
     WHERE NOT EXISTS (SELECT 1 FROM principals WHERE principal_id = NEW.principal_id);
    SELECT RAISE(ABORT, 'membership org must exist')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.org_id);
    SELECT RAISE(ABORT, 'membership account must exist')
     WHERE NEW.account_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id);
    SELECT RAISE(ABORT, 'membership org must be an org or platform account')
     WHERE EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.org_id AND kind NOT IN ('org', 'platform'));
    SELECT RAISE(ABORT, 'membership role does not match its account kind')
     WHERE NEW.account_id IS NOT NULL
       AND EXISTS (SELECT 1 FROM accounts
                    WHERE account_id = NEW.account_id
                      AND kind <> CASE NEW.role WHEN 'member' THEN 'member' WHEN 'sponsor' THEN 'sponsor' END);
    SELECT RAISE(ABORT, 'membership account is not under its org')
     WHERE NEW.account_id IS NOT NULL
       AND EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id)
       AND EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.org_id)
       AND NOT EXISTS (
            WITH RECURSIVE up (id, d) AS (
                SELECT parent_id, 1 FROM accounts WHERE account_id = NEW.account_id
                UNION
                SELECT a.parent_id, up.d + 1
                  FROM accounts a JOIN up ON a.account_id = up.id
                 WHERE a.parent_id IS NOT NULL AND up.d < 64
            )
            SELECT 1 FROM up WHERE id = NEW.org_id);
END;
CREATE TRIGGER memberships_identity_immutable
BEFORE UPDATE OF membership_id, principal_id, org_id, role, account_id, created_at_ms ON memberships
BEGIN
    SELECT RAISE(ABORT, 'membership identity columns are immutable');
END;
CREATE TRIGGER memberships_revoked_is_terminal BEFORE UPDATE ON memberships
WHEN OLD.status = 'revoked'
 AND (NEW.status <> 'revoked' OR NEW.revoked_at_ms IS NOT OLD.revoked_at_ms)
BEGIN
    SELECT RAISE(ABORT, 'a revoked membership cannot be changed');
END;
CREATE TRIGGER credentials_insert_rules BEFORE INSERT ON credentials
BEGIN
    SELECT RAISE(ABORT, 'credential account must exist')
     WHERE NEW.account_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id);
    SELECT RAISE(ABORT, 'credential membership must exist')
     WHERE NEW.membership_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM memberships WHERE membership_id = NEW.membership_id);
    SELECT RAISE(ABORT, 'service and node credentials must be bound to an org account')
     WHERE NEW.binding_kind IN ('service', 'node')
       AND EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id AND kind <> 'org');
    SELECT RAISE(ABORT, 'member credentials must be bound to a member or sponsor membership')
     WHERE NEW.binding_kind = 'member'
       AND EXISTS (SELECT 1 FROM memberships WHERE membership_id = NEW.membership_id AND role = 'admin');
END;
CREATE TRIGGER credentials_binding_immutable
BEFORE UPDATE OF credential_id, binding_kind, account_id, membership_id, created_at_ms, scope_count ON credentials
BEGIN
    SELECT RAISE(ABORT, 'credential binding columns are immutable');
END;
CREATE TRIGGER credentials_revoked_is_terminal BEFORE UPDATE ON credentials
WHEN OLD.status = 'revoked'
 AND (NEW.status <> 'revoked' OR NEW.expires_at_ms IS NOT OLD.expires_at_ms OR NEW.revoked_at_ms IS NOT OLD.revoked_at_ms)
BEGIN
    SELECT RAISE(ABORT, 'a revoked credential cannot be changed');
END;
CREATE TRIGGER credential_admin_scopes_insert_rules BEFORE INSERT ON credential_admin_scopes
BEGIN
    SELECT RAISE(ABORT, 'admin scope credential must exist')
     WHERE NOT EXISTS (SELECT 1 FROM credentials WHERE credential_id = NEW.credential_id);
    SELECT RAISE(ABORT, 'admin scope subtree account must exist')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.subtree_id);
    SELECT RAISE(ABORT, 'only service and node credentials carry admin scopes')
     WHERE EXISTS (SELECT 1 FROM credentials WHERE credential_id = NEW.credential_id AND binding_kind = 'member');
    SELECT RAISE(ABORT, 'admin scopes can only be added when the credential is issued')
     WHERE NOT EXISTS (SELECT 1 FROM credentials
                        WHERE credential_id = NEW.credential_id AND NEW.position < scope_count);
    SELECT RAISE(ABORT, 'admin scope subtree is outside the credential binding')
     WHERE NOT EXISTS (
            WITH RECURSIVE up (id, d) AS (
                SELECT NEW.subtree_id, 0
                UNION
                SELECT a.parent_id, up.d + 1
                  FROM accounts a JOIN up ON a.account_id = up.id
                 WHERE a.parent_id IS NOT NULL AND up.d < 64
            )
            SELECT 1 FROM up
             WHERE id = (SELECT account_id FROM credentials WHERE credential_id = NEW.credential_id));
END;
CREATE TRIGGER credential_admin_scopes_append_only_update BEFORE UPDATE ON credential_admin_scopes
BEGIN
    SELECT RAISE(ABORT, 'credential admin scopes are append-only');
END;
CREATE TRIGGER credential_admin_scopes_append_only_delete BEFORE DELETE ON credential_admin_scopes
BEGIN
    SELECT RAISE(ABORT, 'credential admin scopes are append-only');
END;
CREATE TRIGGER allocations_insert_rules BEFORE INSERT ON allocations
BEGIN
    SELECT RAISE(ABORT, 'allocation parent account must exist')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.parent_id);
    SELECT RAISE(ABORT, 'allocation child account must exist')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.child_id);
    SELECT RAISE(ABORT, 'allocation child must be a direct child of its parent account')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.child_id AND parent_id = NEW.parent_id);
    SELECT RAISE(ABORT, 'allocation unit must equal the child account unit')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.child_id AND unit = NEW.ceiling_unit);
END;
CREATE TRIGGER allocations_append_only_update BEFORE UPDATE ON allocations
BEGIN
    SELECT RAISE(ABORT, 'allocations are append-only');
END;
CREATE TRIGGER allocations_append_only_delete BEFORE DELETE ON allocations
BEGIN
    SELECT RAISE(ABORT, 'allocations are append-only');
END;
CREATE TRIGGER principals_insert_no_conflict BEFORE INSERT ON principals
WHEN EXISTS (SELECT 1 FROM principals WHERE principal_id = NEW.principal_id)
BEGIN
    SELECT RAISE(ABORT, 'principal already exists');
END;
CREATE TRIGGER accounts_insert_no_conflict BEFORE INSERT ON accounts
WHEN EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.account_id)
  OR (NEW.kind = 'platform' AND EXISTS (SELECT 1 FROM accounts WHERE kind = 'platform'))
BEGIN
    SELECT RAISE(ABORT, 'account already exists, or the platform root already exists');
END;
CREATE TRIGGER memberships_insert_no_conflict BEFORE INSERT ON memberships
WHEN EXISTS (SELECT 1 FROM memberships WHERE membership_id = NEW.membership_id)
  OR (NEW.account_id IS NOT NULL AND EXISTS (SELECT 1 FROM memberships WHERE account_id = NEW.account_id))
BEGIN
    SELECT RAISE(ABORT, 'membership already exists, or its account is already bound');
END;
CREATE TRIGGER credentials_insert_no_conflict BEFORE INSERT ON credentials
WHEN EXISTS (SELECT 1 FROM credentials WHERE credential_id = NEW.credential_id)
BEGIN
    SELECT RAISE(ABORT, 'credential already exists');
END;
CREATE TRIGGER credential_admin_scopes_insert_no_conflict BEFORE INSERT ON credential_admin_scopes
WHEN EXISTS (
    SELECT 1 FROM credential_admin_scopes
     WHERE credential_id = NEW.credential_id AND position = NEW.position)
BEGIN
    SELECT RAISE(ABORT, 'credential admin scope already exists');
END;
CREATE TRIGGER allocations_insert_no_conflict BEFORE INSERT ON allocations
WHEN EXISTS (SELECT 1 FROM allocations WHERE allocation_id = NEW.allocation_id)
  OR (NEW.idempotency_key IS NOT NULL
      AND EXISTS (SELECT 1 FROM allocations WHERE idempotency_key = NEW.idempotency_key))
BEGIN
    SELECT RAISE(ABORT, 'allocation already exists, or its idempotency key was already used');
END;
CREATE TRIGGER principals_never_deleted BEFORE DELETE ON principals
BEGIN
    SELECT RAISE(ABORT, 'principals are never deleted; revoke them');
END;
CREATE TRIGGER accounts_never_deleted BEFORE DELETE ON accounts
BEGIN
    SELECT RAISE(ABORT, 'accounts are never deleted; close them');
END;
CREATE TRIGGER memberships_never_deleted BEFORE DELETE ON memberships
BEGIN
    SELECT RAISE(ABORT, 'memberships are never deleted; revoke them');
END;
CREATE TRIGGER credentials_never_deleted BEFORE DELETE ON credentials
BEGIN
    SELECT RAISE(ABORT, 'credentials are never deleted; revoke them');
END;
CREATE TRIGGER control_audit_guard_insert BEFORE INSERT ON control_audit
WHEN NEW.audit_id != -1
BEGIN
    SELECT RAISE(ABORT, 'control audit ids are assigned by the database');
END;
CREATE TRIGGER control_audit_id_positive AFTER INSERT ON control_audit
WHEN NEW.audit_id <= 0
BEGIN
    SELECT RAISE(ABORT, 'control audit ids must be positive');
END;
CREATE TRIGGER control_audit_no_update BEFORE UPDATE ON control_audit
BEGIN
    SELECT RAISE(ABORT, 'control audit is append-only');
END;
CREATE TRIGGER control_audit_no_delete BEFORE DELETE ON control_audit
BEGIN
    SELECT RAISE(ABORT, 'control audit is append-only');
END;
CREATE TRIGGER credential_issuance_insert_rules BEFORE INSERT ON credential_issuance
BEGIN
    SELECT RAISE(ABORT, 'issued credential must exist')
     WHERE NOT EXISTS (SELECT 1 FROM credentials WHERE credential_id = NEW.credential_id);
END;
CREATE TRIGGER credential_issuance_insert_no_conflict BEFORE INSERT ON credential_issuance
WHEN EXISTS (SELECT 1 FROM credential_issuance WHERE credential_id = NEW.credential_id)
BEGIN
    SELECT RAISE(ABORT, 'credential issuance already recorded');
END;
CREATE TRIGGER credential_issuance_no_update BEFORE UPDATE ON credential_issuance
BEGIN
    SELECT RAISE(ABORT, 'credential issuance records are append-only');
END;
CREATE TRIGGER credential_issuance_no_delete BEFORE DELETE ON credential_issuance
BEGIN
    SELECT RAISE(ABORT, 'credential issuance records are append-only');
END;
CREATE TRIGGER principal_registrations_insert_rules BEFORE INSERT ON principal_registrations
BEGIN
    SELECT RAISE(ABORT, 'registered principal must exist')
     WHERE NOT EXISTS (SELECT 1 FROM principals WHERE principal_id = NEW.principal_id);
    SELECT RAISE(ABORT, 'registering org must exist')
     WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.org_id);
END;
CREATE TRIGGER principal_registrations_insert_no_conflict BEFORE INSERT ON principal_registrations
WHEN EXISTS (SELECT 1 FROM principal_registrations WHERE principal_id = NEW.principal_id)
BEGIN
    SELECT RAISE(ABORT, 'principal registration already recorded');
END;
CREATE TRIGGER principal_registrations_no_update BEFORE UPDATE ON principal_registrations
BEGIN
    SELECT RAISE(ABORT, 'principal registrations are append-only');
END;
CREATE TRIGGER principal_registrations_no_delete BEFORE DELETE ON principal_registrations
BEGIN
    SELECT RAISE(ABORT, 'principal registrations are append-only');
END;
CREATE TRIGGER allocation_revocations_insert_rules BEFORE INSERT ON allocation_revocations
BEGIN
    SELECT RAISE(ABORT, 'revoked allocation must exist')
     WHERE NOT EXISTS (SELECT 1 FROM allocations WHERE allocation_id = NEW.allocation_id);
END;
CREATE TRIGGER allocation_revocations_insert_no_conflict BEFORE INSERT ON allocation_revocations
WHEN EXISTS (SELECT 1 FROM allocation_revocations WHERE allocation_id = NEW.allocation_id)
BEGIN
    SELECT RAISE(ABORT, 'allocation already revoked');
END;
CREATE TRIGGER allocation_revocations_no_update BEFORE UPDATE ON allocation_revocations
BEGIN
    SELECT RAISE(ABORT, 'allocation revocations are append-only');
END;
CREATE TRIGGER allocation_revocations_no_delete BEFORE DELETE ON allocation_revocations
BEGIN
    SELECT RAISE(ABORT, 'allocation revocations are append-only');
END;
CREATE TRIGGER pairing_codes_insert_rules BEFORE INSERT ON pairing_codes
BEGIN
    SELECT RAISE(ABORT, 'pairing membership must exist')
     WHERE NOT EXISTS (SELECT 1 FROM memberships WHERE membership_id = NEW.membership_id);
    SELECT RAISE(ABORT, 'pairing issuer credential must exist')
     WHERE NOT EXISTS (SELECT 1 FROM credentials WHERE credential_id = NEW.issued_by);
    SELECT RAISE(ABORT, 'a new pairing code must be pending')
     WHERE NEW.status <> 'pending';
    SELECT RAISE(ABORT, 'pairing codes can only be created for member or sponsor memberships')
     WHERE EXISTS (SELECT 1 FROM memberships WHERE membership_id = NEW.membership_id AND role = 'admin');
END;
CREATE TRIGGER pairing_codes_insert_no_conflict BEFORE INSERT ON pairing_codes
WHEN EXISTS (SELECT 1 FROM pairing_codes WHERE pairing_id = NEW.pairing_id)
  OR EXISTS (SELECT 1 FROM pairing_codes WHERE code_hash = NEW.code_hash)
BEGIN
    SELECT RAISE(ABORT, 'pairing code already exists');
END;
CREATE TRIGGER pairing_codes_state_machine BEFORE UPDATE ON pairing_codes
WHEN NOT (
    OLD.status = 'pending'
    AND NEW.status IN ('redeemed', 'revoked')
    AND NEW.pairing_id = OLD.pairing_id
    AND NEW.code_hash = OLD.code_hash
    AND NEW.membership_id = OLD.membership_id
    AND NEW.issued_by = OLD.issued_by
    AND NEW.created_at_ms = OLD.created_at_ms
    AND NEW.expires_at_ms = OLD.expires_at_ms
    AND NEW.device_privacy = OLD.device_privacy
    AND NEW.device_roles = OLD.device_roles
    AND NEW.device_tenant_id IS OLD.device_tenant_id
    AND NEW.device_ttl_ms IS OLD.device_ttl_ms
)
BEGIN
    SELECT RAISE(ABORT, 'a pairing code can only go from pending to redeemed or revoked, once');
END;
CREATE TRIGGER pairing_codes_no_delete BEFORE DELETE ON pairing_codes
BEGIN
    SELECT RAISE(ABORT, 'pairing codes are never deleted; they expire, are redeemed or are revoked');
END;
CREATE TRIGGER pairing_devices_insert_rules BEFORE INSERT ON pairing_devices
BEGIN
    SELECT RAISE(ABORT, 'device credential must exist')
     WHERE NOT EXISTS (SELECT 1 FROM credentials WHERE credential_id = NEW.credential_id);
    SELECT RAISE(ABORT, 'device membership must exist')
     WHERE NOT EXISTS (SELECT 1 FROM memberships WHERE membership_id = NEW.membership_id);
    SELECT RAISE(ABORT, 'device pairing record must exist')
     WHERE NOT EXISTS (SELECT 1 FROM pairing_codes WHERE pairing_id = NEW.pairing_id);
    SELECT RAISE(ABORT, 'a device credential must be bound to the membership it was paired for')
     WHERE NOT EXISTS (SELECT 1 FROM credentials
                        WHERE credential_id = NEW.credential_id
                          AND binding_kind = 'member' AND membership_id = NEW.membership_id);
END;
CREATE TRIGGER pairing_devices_insert_no_conflict BEFORE INSERT ON pairing_devices
WHEN EXISTS (SELECT 1 FROM pairing_devices WHERE credential_id = NEW.credential_id)
  OR EXISTS (SELECT 1 FROM pairing_devices WHERE pairing_id = NEW.pairing_id)
BEGIN
    SELECT RAISE(ABORT, 'device already paired, or this pairing code was already redeemed');
END;
CREATE TRIGGER pairing_devices_no_update BEFORE UPDATE ON pairing_devices
BEGIN
    SELECT RAISE(ABORT, 'pairing devices are append-only');
END;
CREATE TRIGGER pairing_devices_no_delete BEFORE DELETE ON pairing_devices
BEGIN
    SELECT RAISE(ABORT, 'pairing devices are append-only');
END;
CREATE TRIGGER ledger_config_never_deleted BEFORE DELETE ON ledger_config
BEGIN
    SELECT RAISE(ABORT, 'ledger configuration rows are never deleted');
END;
CREATE TRIGGER ledger_reserved_account_id BEFORE INSERT ON accounts
WHEN NEW.account_id = 'platform-loss'
BEGIN
    SELECT RAISE(ABORT, 'account id platform-loss is reserved for the ledger loss account');
END;
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
CREATE TRIGGER ledger_holds_identity_immutable
BEFORE UPDATE OF hold_id, attempt_id, component, request_id, attempt_index, payer_account, credential_id,
                 unit, reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros
ON ledger_holds
BEGIN
    SELECT RAISE(ABORT, 'hold identity and amounts are immutable');
END;
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
CREATE TRIGGER ledger_entries_held_bounds AFTER INSERT ON ledger_entries
WHEN NEW.held_micros <> 0 AND NEW.hold_id IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'a hold cannot be reserved twice, or released more than it reserved')
     WHERE (SELECT COALESCE(SUM(held_micros), 0) FROM ledger_entries
             WHERE hold_id = NEW.hold_id AND account_id = NEW.account_id)
           NOT BETWEEN 0 AND (SELECT reserved_micros FROM ledger_holds WHERE hold_id = NEW.hold_id);
END;
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
CREATE TRIGGER ledger_grace_one_active_insert_no_conflict
BEFORE INSERT ON ledger_grace_uses
WHEN NEW.voided_at_ms IS NULL
 AND EXISTS (SELECT 1 FROM ledger_grace_uses
              WHERE account_id = NEW.account_id AND period = NEW.period AND voided_at_ms IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'UNIQUE constraint failed: ledger_grace_uses.account_id, ledger_grace_uses.period (one active grace use per account per period)');
END;
CREATE TRIGGER ledger_allocation_marks_insert_no_conflict BEFORE INSERT ON ledger_allocation_marks
WHEN EXISTS (SELECT 1 FROM ledger_allocation_marks WHERE allocation_id = NEW.allocation_id AND event = NEW.event)
BEGIN
    SELECT RAISE(ABORT, 'allocation mark already exists');
END;
CREATE TRIGGER ledger_allocation_marks_insert_rules BEFORE INSERT ON ledger_allocation_marks
BEGIN
    SELECT RAISE(ABORT, 'an allocation mark must name an existing allocation')
     WHERE NOT EXISTS (SELECT 1 FROM allocations WHERE allocation_id = NEW.allocation_id);
    SELECT RAISE(ABORT, 'a revocation mark needs a revoked allocation')
     WHERE NEW.event = 'revoked'
       AND NOT EXISTS (SELECT 1 FROM allocation_revocations WHERE allocation_id = NEW.allocation_id);
    SELECT RAISE(ABORT, 'an allocation mark cannot point past the last ledger entry')
     WHERE NEW.after_entry > COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'ledger_entries'), 0);
END;
CREATE TRIGGER ledger_allocation_marks_append_only_update BEFORE UPDATE ON ledger_allocation_marks
BEGIN
    SELECT RAISE(ABORT, 'ledger_allocation_marks is append-only');
END;
CREATE TRIGGER ledger_allocation_marks_append_only_delete BEFORE DELETE ON ledger_allocation_marks
BEGIN
    SELECT RAISE(ABORT, 'ledger_allocation_marks is append-only');
END;
CREATE TRIGGER ledger_allocation_created_mark AFTER INSERT ON allocations
BEGIN
    INSERT INTO ledger_allocation_marks (allocation_id, event, after_entry)
    VALUES (NEW.allocation_id, 'created',
            COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'ledger_entries'), 0));
END;
CREATE TRIGGER ledger_allocation_revoked_mark AFTER INSERT ON allocation_revocations
BEGIN
    INSERT INTO ledger_allocation_marks (allocation_id, event, after_entry)
    VALUES (NEW.allocation_id, 'revoked',
            COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'ledger_entries'), 0));
END;
DELETE FROM "sqlite_sequence";
INSERT INTO "sqlite_sequence" VALUES('ledger_entries',59);
INSERT INTO "sqlite_sequence" VALUES('ledger_alerts',6);
COMMIT;

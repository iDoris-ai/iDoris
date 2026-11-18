-- S2 控制面（设计稿 §9 第 1 项）：审计、凭证来源、主体登记、额度撤销、云账号配对。
--
-- 这个文件由 `AccountStore::open` 在 0001 之后、同一个 `BEGIN IMMEDIATE` 里执行（版本 2，记在 `account_schema_migrations`）。
-- 和 0001 一样：全部是 STRICT 表；金额是「单位列 + 微单位整数列」；id 的字符集与 S0 的 `ids.rs` 一致；
-- 凡是靠「被引用的行必须已经存在」成立的规则，触发器里显式检查，不依赖外键（外键只是第二道防线）；
-- 凡是 INSERT OR REPLACE 能绕过的保护（它先删旧行再插，而 recursive_triggers 默认关闭，DELETE 触发器不触发），
-- 都用「不许冲突」触发器堵住（见 0001 的说明）。
--
-- ## 这里新增了什么
--
-- 1. `control_audit`：每个控制面写操作（成功、被拒、被驳回、出错）一行，**只追加**，数据库用触发器保证。不记密钥、token、正文。
-- 2. `credential_issuance`：每张凭证由谁签发（凭证 / 配对码 / 运维人员），事后查「这张凭证是谁发的」、撤销一个被盗的签发者时
--    找到它发出去的全部凭证。只追加。
-- 3. `principal_registrations`：一个主体最早由哪个组织登记。主体是全局的假名 id，没有这一行就没法判断
--    「一个还没有任何成员关系的主体，该由谁撤销」——只有最早登记它的组织能撤销，别的组织不能凭空撤销一个它不认识的主体。
-- 4. `allocation_revocations`：额度的撤销（「关授信」）。侧表、只追加；`allocations` 表本身一个字都不动。
-- 5. `pairing_codes` / `pairing_devices`：云账号配对（设计稿 §3「云账号绑定走『登录 / 配对』流程」）。
--    码只存哈希；状态只能 pending -> redeemed / pending -> revoked；兑换出来的每台设备一张凭证、各自可撤销。
--
-- ## 审计表的限制（和 event_log 一样，如实写在这里）
--
-- 触发器挡得住应用代码里的 UPDATE / DELETE / REPLACE，挡不住能跑 DDL（DROP TRIGGER、DROP TABLE）或者直接改库文件的一方。
-- 这不是防篡改账本：要防篡改得把审计行定期外送到别的机器（哈希链、对象存储），那是运维的事（`docs/design/compute-service-ops.md`）。

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

CREATE INDEX control_audit_by_scope ON control_audit (scope_account, audit_id);
CREATE INDEX control_audit_by_actor ON control_audit (actor_id, audit_id);

-- 只追加：行号必须由数据库分配（INTEGER PRIMARY KEY 在 BEFORE INSERT 触发器里没指定时是 -1，指定了就不是），
-- 所以任何带着显式 audit_id 的插入（包括 INSERT OR REPLACE 想覆盖旧行）都被拒绝；
-- 显式写 -1 或 0 的会在 AFTER INSERT 触发器里被拒绝（行号必须是正的）。
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

-- 凭证由谁签发。credential_id 同时是 `virtual_keys.key_id`。
CREATE TABLE credential_issuance (
    credential_id  TEXT NOT NULL PRIMARY KEY REFERENCES credentials (credential_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    issued_by_kind TEXT NOT NULL CHECK (issued_by_kind IN ('credential', 'pairing_code', 'operator')),
    issued_by_id   TEXT NOT NULL CHECK (
        length(issued_by_id) BETWEEN 1 AND 128 AND issued_by_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
        AND instr(CAST(issued_by_id AS BLOB), x'00') = 0),
    issued_at_ms   INTEGER NOT NULL CHECK (issued_at_ms >= 0)
) STRICT;

CREATE INDEX credential_issuance_by_issuer ON credential_issuance (issued_by_id);

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

-- 主体最早由哪个组织登记（见文件头的说明）。
CREATE TABLE principal_registrations (
    principal_id  TEXT NOT NULL PRIMARY KEY REFERENCES principals (principal_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    org_id        TEXT NOT NULL REFERENCES accounts (account_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    registered_at_ms INTEGER NOT NULL CHECK (registered_at_ms >= 0)
) STRICT;

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

-- 额度的撤销（「关授信」）：一张只追加的侧表，**不动 `allocations`**（0001 里额度整行不可改、不可删的触发器原样保留，
-- 额度表的列也没变）。有一行 = 这笔额度已被撤销；撤销不可逆（行不可改、不可删）。
-- 读额度的人必须排除有撤销行的额度：被撤销的额度不再给任何人授权（`AccountStore::allocations_into` 等读函数已经排除；
-- 自己写 SQL 读 allocations 的地方要自己加 `NOT EXISTS (SELECT 1 FROM allocation_revocations ...)`）。
CREATE TABLE allocation_revocations (
    allocation_id   TEXT NOT NULL PRIMARY KEY REFERENCES allocations (allocation_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    revoked_at_ms   INTEGER NOT NULL CHECK (revoked_at_ms >= 0),
    revoked_by_kind TEXT NOT NULL CHECK (revoked_by_kind IN ('credential', 'operator')),
    revoked_by_id   TEXT NOT NULL CHECK (
        length(revoked_by_id) BETWEEN 1 AND 128 AND revoked_by_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
        AND instr(CAST(revoked_by_id AS BLOB), x'00') = 0)
) STRICT;

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

-- 云账号配对码。码本身只在创建时出现一次，库里只有 SHA-256；`pairing_id` 是它的公开标识（日志、审计、撤销都用它）。
-- 一个码绑定一个成员关系（该账号的付款账户）和一组数据面上限；兑换出来的设备凭证不会超过这些上限。
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

CREATE INDEX pairing_codes_by_membership ON pairing_codes (membership_id);

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

-- 状态只能 pending -> redeemed 或 pending -> revoked；除了状态、兑换时间、兑换出来的凭证、撤销时间，其余列一个都不能改。
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

-- 配对出来的设备：一台设备一张凭证（credential_id 同时是 virtual_keys.key_id）。
-- 设备的有效 / 已解绑状态不在这里：就是 credentials.status（撤销 = 解绑）。只追加。
CREATE TABLE pairing_devices (
    credential_id TEXT NOT NULL PRIMARY KEY REFERENCES credentials (credential_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    membership_id TEXT NOT NULL REFERENCES memberships (membership_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    pairing_id    TEXT NOT NULL REFERENCES pairing_codes (pairing_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    -- 设备的显示名，用户自己填的文本：只进这张表，从不进审计和日志。
    label         TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 64),
    paired_at_ms  INTEGER NOT NULL CHECK (paired_at_ms >= 0)
) STRICT;

CREATE INDEX pairing_devices_by_membership ON pairing_devices (membership_id);
-- 一个码最多兑换出一台设备（并发兑换同一个码，数据库层也只让一个写进去）。
CREATE UNIQUE INDEX pairing_devices_one_per_code ON pairing_devices (pairing_id);

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

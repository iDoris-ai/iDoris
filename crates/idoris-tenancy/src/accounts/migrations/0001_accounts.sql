-- S1 账户存储，迁移 0001（设计稿 §3、§4、§9；契约见 crates/idoris-compute-contracts）。
--
-- 五张表对应 S0 的五个对象：主体、付款账户（额度委派树）、成员关系、凭证（及其管理权限）、额度。
-- 这个文件由 `AccountStore::open` 在一个 `BEGIN IMMEDIATE` 里执行，版本记在 `account_schema_migrations`。
-- 只有组织、云形态会打开账户存储；个人形态和存量 tenant 部署从不创建这些表。
--
-- 设计原则：Rust 层用 S0 的 `AccountTree::build` / `Directory::build` / `validate_allocation` 校验每一次写入，
-- 数据库层再拦一遍「明显的坏数据」，这样绕过 Rust 层的写入（手工 SQL、以后的 S2 代码里的 bug）也进不来。
-- 数据库层只放**写死不会变**的规则；会变的（比如 S2 可以新增的控制面操作名）不写 CHECK，由 Rust 层解析。
--
-- 全部是 STRICT 表：INTEGER 列只收整数，金额不可能被浮点污染。需要 SQLite >= 3.37（捆绑的是 3.53）。
--
-- 金额一律是「单位列 + 微单位整数列」（`*_unit` + `*_micros`，1 个单位 = 1e6 微单位），没有浮点、没有默认币种。
-- 单位的写法与 S0 的 `Unit` 一致：`THB`、`USD`、`QUOTA:<节点>`。
--
-- id 的字符集与 S0 的 `ids.rs` 一致：字母数字和 `. _ - : ~`，1..=128 字节；没有 `@`、空白、`/`、`#`
-- （假名 id 里出现邮箱形状的值几乎一定是把个人信息漏进来了）。
--
-- ## 写这个文件时踩到、并且都有测试守着的 SQLite 行为
--
-- 1. CHECK 在表达式为 NULL 时**放行**：凡是「必须非空」的规则都要显式写 `IS NOT NULL`。
-- 2. BEFORE INSERT 触发器先于 CHECK、NOT NULL、外键和冲突处理执行；同时违反多条规则的语句，报的是先执行的那一条。
--    同一事件上的多个触发器谁先谁后没有保证，所以不要让正确性依赖这个顺序。
-- 3. 外键默认按**语句**检查（语句结束时），多行 INSERT 里「先插子、后插父」能通过；`PRAGMA defer_foreign_keys`
--    还能把检查推迟到事务结束；`PRAGMA foreign_keys=OFF`（sqlite3 命令行的默认值）干脆不检查。
--    所以凡是靠「被引用的行必须已经存在」才成立的规则，触发器里都**显式检查被引用的行存在**，不依赖外键；
--    外键仍然保留，作为拿掉触发器之后的第二道防线。
-- 4. INSERT OR REPLACE 在主键 / 唯一约束冲突时先删旧行再插新行，而 `recursive_triggers` 默认关闭，
--    这时 DELETE 触发器不触发（见文件末尾「不许冲突」「永不删除」两组触发器）。
-- 5. `length()`、GLOB、LIKE 遇到字符串里的 NUL 字节就停了，所以 `abc\0@x.com` 能骗过只看这两个函数的 CHECK。
--    凡是 id、单位、幂等键、操作名的 CHECK 都多一条 `instr(CAST(col AS BLOB), x'00') = 0`。

CREATE TABLE principals (
    principal_id  TEXT NOT NULL PRIMARY KEY
        CHECK (length(principal_id) BETWEEN 1 AND 128 AND principal_id NOT GLOB '*[^A-Za-z0-9._:~-]*'
               AND instr(CAST(principal_id AS BLOB), x'00') = 0),
    status        TEXT NOT NULL CHECK (status IN ('active', 'revoked')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    revoked_at_ms INTEGER,
    CHECK ((status = 'revoked') = (revoked_at_ms IS NOT NULL))
) STRICT;

-- 付款账户。额度委派树：平台 -> 组织 / 担保人 / 补贴池 -> 成员子账户。
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

CREATE INDEX accounts_by_parent ON accounts (parent_id);
-- 单根：整棵树最多一个平台账户。
CREATE UNIQUE INDEX accounts_single_platform_root ON accounts (kind) WHERE kind = 'platform';

-- 新增账户时的树规则（与 S0 `AccountTree::build` 一致）：
--   * **父账户必须已经存在**（显式检查，不靠外键）。这一条同时让环不可能出现：一行只能引用比它先插入的行，
--     而下面的触发器又让 parent_id 永远不能改，所以多行 INSERT、INSERT…SELECT、`defer_foreign_keys`、
--     关掉外键的手工 SQL 都造不出环，也绕不过后面三条；
--   * 父子同一单位；
--   * 父子种类组合合法（`AccountKind::may_parent`）；
--   * 深度不超过 `MAX_ACCOUNT_DEPTH`（5，根为 0）。
-- 深度和种类矩阵是写死在这里的常量，`accounts/tests.rs` 里有测试逐格对照 S0 的函数，S0 改了这里会立刻红。
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

-- 账户的身份字段一经写下就不能改：父账户不能改，所以树里不可能出现环，也没有「搬家」带来的深度 / 单位问题。
CREATE TRIGGER accounts_identity_immutable
BEFORE UPDATE OF account_id, kind, parent_id, unit, created_at_ms ON accounts
BEGIN
    SELECT RAISE(ABORT, 'account identity columns are immutable');
END;

-- 关闭是终态。冻结可以解冻。
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

-- 撤销处理资格（同意撤回）不可逆：开通方重试「创建主体」不能把撤销的主体复活。
CREATE TRIGGER principals_revoked_is_terminal BEFORE UPDATE ON principals
WHEN OLD.status = 'revoked'
 AND (NEW.status <> 'revoked' OR NEW.revoked_at_ms IS NOT OLD.revoked_at_ms)
BEGIN
    SELECT RAISE(ABORT, 'a revoked principal cannot be changed');
END;

-- 成员关系：主体属于哪个组织、什么角色。一个主体可以有多个。
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

CREATE INDEX memberships_by_principal ON memberships (principal_id);
CREATE INDEX memberships_by_org ON memberships (org_id);
-- 一个付款账户至多属于一个成员关系。
CREATE UNIQUE INDEX memberships_account_bound_once ON memberships (account_id) WHERE account_id IS NOT NULL;

-- 新增成员关系时的结构规则（与 S0 `Directory::build` 一致）：主体、组织、付款账户都必须已经存在（显式检查，
-- 不靠外键）；所属组织必须是 org 或 platform；角色与账户种类对得上；账户必须在该组织的子树里。
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

-- 凭证：调用凭据的**元数据**。这里没有密钥，也没有密钥哈希：密钥只在签发时出现一次，
-- 哈希存在既有的 `virtual_keys` 表里，两边用同一个 id 对应（`credential_id` = `virtual_keys.key_id`），故意不加外键，
-- 因为 `virtual_keys` 由另一个存储拥有、有自己的迁移，S2 签发流程再决定怎么把两边收成一次写入。
-- 每张凭证只有一个绑定：service / node 绑定到一个 org 账户，member 绑定到一个成员关系，所以每次请求只落到一个付款上下文。
--
-- `scope_count`：这张凭证在**签发时**带了几条控制面权限（见下面的 credential_admin_scopes）。它写下后不能改，
-- 权限范围行只能填满 0..scope_count 这些位置，所以签发之后不可能再给一张已有的凭证追加权限
-- （追加权限绕过的是签发时的「下发不超过授权者本人」检查）。成员凭证没有控制面权限，scope_count 恒为 0。
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

CREATE INDEX credentials_by_account ON credentials (account_id) WHERE account_id IS NOT NULL;
CREATE INDEX credentials_by_membership ON credentials (membership_id) WHERE membership_id IS NOT NULL;

-- 绑定目标必须已经存在（显式检查，不靠外键），并且种类要对（与 S0 `resolve_payment_context` 一致，只是把它提前到创建时）：
-- service / node 必须绑到 org 账户；member 必须绑到有付款账户的成员关系（成员或担保人，不能是管理员）。
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

-- 付款绑定不能改：把一张凭证悄悄改指向别人的账户，就是把费用转嫁给别人。`scope_count` 同理（见上）。
CREATE TRIGGER credentials_binding_immutable
BEFORE UPDATE OF credential_id, binding_kind, account_id, membership_id, created_at_ms, scope_count ON credentials
BEGIN
    SELECT RAISE(ABORT, 'credential binding columns are immutable');
END;

-- 撤销不可逆。冻结可以解冻。
CREATE TRIGGER credentials_revoked_is_terminal BEFORE UPDATE ON credentials
WHEN OLD.status = 'revoked'
 AND (NEW.status <> 'revoked' OR NEW.expires_at_ms IS NOT OLD.expires_at_ms OR NEW.revoked_at_ms IS NOT OLD.revoked_at_ms)
BEGIN
    SELECT RAISE(ABORT, 'a revoked credential cannot be changed');
END;

-- 凭证的控制面权限：「操作 x 资源范围」（设计稿 §9）。操作名不写 CHECK：S0 允许 S2 增加新的操作，
-- 由 Rust 层解析成 `ControlOp`，认不得的名字读出来就是错误；这里只限制形状。
-- 写入后不可改、不可删：要收回权限就撤销凭证。
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

CREATE INDEX credential_admin_scopes_by_subtree ON credential_admin_scopes (subtree_id);

-- 权限范围行的规则（把 S0 / 存储层签发时检查的几条结构规则也放到数据库层，绕过 Rust 层的写入同样进不来）：
--   * 凭证和子树账户必须已经存在；
--   * 只有 service / node 凭证带控制面权限（成员凭证带了，读出来就是错误，连 API 都撤销不了它）；
--   * 只能填签发时声明的位置（position < credentials.scope_count），签发之后不能再追加；
--   * 子树必须在凭证自己绑定的账户之内（凭证不能管到自己绑定范围之外）。
-- 「下发的权限不超过授权者本人」要知道授权者是谁，数据库层看不到，由 S2 先用 S0 的 `check_credential_issuance` 校验。
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

-- 额度：父账户给子账户的**授权上限**，不是转移余额（设计稿 §3、§4）。所以没有「余额」列，叫 ceiling。
-- 每笔金额带 单位 / 微单位 / 用途 / 有效期 / 不可转让（`transferable` 恒为 0，且有 CHECK）。
-- 有效期是半开区间 [valid_from_ms, valid_until_ms)，必填。
-- 写入后不可改、不可删（追加式，和事件日志同一思路）：要改额度，写一条新的；撤销机制由 S2 另加列。
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

CREATE INDEX allocations_by_child ON allocations (child_id);
CREATE INDEX allocations_by_parent ON allocations (parent_id);
-- 补贴幂等键唯一：同一 (成员, 月份) 的 `subsidy:<主体>:<YYYY-MM>` 只能发放一次，并发重试也插不进第二条。
CREATE UNIQUE INDEX allocations_idempotency_key_unique ON allocations (idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- 父、子账户必须已经存在（显式检查，不靠外键）；只能沿树边下发；单位等于子账户的单位（S0 `validate_allocation`
-- 的前三条）。「不超过授权者本人」要看已有额度和有效期，留给 Rust 层的 `validate_allocation`。
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

-- ===========================================================================================
-- 堵住 REPLACE 和「删了再插」这两条绕过上面所有保护的路
-- ===========================================================================================
--
-- 1. INSERT OR REPLACE / REPLACE INTO 在主键或唯一约束冲突时，会先删掉旧行再插入新行；而 recursive_triggers
--    默认是关的，这时 DELETE 触发器不会触发。于是上面所有「不可改」「终态」「追加式」的保护都能被它绕过
--    （把一张凭证 REPLACE 成指向别人账户的新行，把撤销的主体 REPLACE 回 active，改写一笔额度的上限……）。
--    SQLite 里 BEFORE INSERT 触发器先于冲突处理执行，所以这里一律拒绝「会与已有行冲突的插入」，
--    REPLACE 没有机会生效。INSERT OR IGNORE 同样会被拒绝（调用方要先查再插，见 `AccountStore::ensure_principal`）。
--    每个主键和每个唯一索引都要有对应的检查；加新的唯一约束时在这里补一条。
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

-- 2. 身份表的行永远不删，撤销、关闭用状态表示。id 一旦用过就不能换一个含义再用：删掉再插入同一个 id，
--    就能把撤销的凭证变回有效、把凭证改绑到别人的账户、把账户搬到别的父账户下，历史账目会被悄悄改归属。
--    （allocations 和 credential_admin_scopes 的追加式触发器已经禁止了删除。）
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

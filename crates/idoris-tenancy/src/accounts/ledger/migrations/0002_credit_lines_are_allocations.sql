-- S3 准入账本，迁移 0002：授信与免费额度的权威数据源统一（PR #600 Codex 评审 #6 / #7）。
--
-- 由 `AccountStore::open` 在 0001 之后、同一条连接上应用（版本 2，记在 `ledger_schema_migrations`）。
-- 不改 0001 的任何一个字。
--
-- # 授信（#6）
--
-- 旧设计里授信有两份记录：S2 控制面开通的 `purpose = credit_line` 额度只写进 `allocations`，而准入读的是
-- `ledger_credit_lines`，所以 API 开通的授信从来不生效，两边的撤销也不联动。现在只有一份：
-- `allocations` 里 purpose = 'credit_line' 的额度——窗口内（`valid_from_ms <= now < valid_until_ms`）、没有被
-- `allocation_revocations` 撤销的，按子账户求和就是这个账户此刻的授信额度（见 `ledger/project.rs`）。
-- 存储层的 `set_credit_line` / `disable_credit_line` 也写这张表（设置 = 撤销还生效的旧额度、写一条新的；关掉 = 撤销），
-- 控制面的开通、有效期、撤销、读额度的接口因此和准入读到的是同一份记录。
--
-- # 免费额度的承诺（#7）
--
-- 控制面发放免费额度现在和存储层的 `issue_free_tier` 一样，在同一个事务里检查补贴池该账期的总上限并登记承诺
-- （`ledger_subsidy_grants`）。旧版本经控制面发出的免费额度没有承诺行（池的承诺账是 0，上限形同虚设），
-- 这里按额度补登一遍。
--
-- # 对存量数据的影响
--
-- 这个功能尚未上线，没有存量生产数据。开发 / 预发库里如果有旧数据：
--   * `ledger_credit_lines` 里每个账户**最新一条**、启用的授信迁成一笔 credit_line 额度（窗口 =
--     [设置时间, 旧的到期时间)；没有到期时间的用公元 10000 年——额度必须有到期时间，没有「永不过期」）。其余版本
--     （被替换的、关掉的）本来就不生效，不迁；已经过期的额度迁过去也读不到（窗口检查），无害。迁完旧表删除，不再有第二份记录。
--   * 经控制面发出、还没有承诺行的免费额度补登承诺行（池 = 成员账户祖先里最近的补贴池；找不到池的额度没有可记的池，跳过）。
--     补登只记账，不重新检查上限：旧数据里就算已经超发，之后的新发放会因为剩余是 0 被拒绝（偏保守）。
--
-- # 迁出来的授信额度的 id（PR #600 第二轮评审 Medium）
--
-- 额度 id 在 `allocations` 里是全局唯一的，而控制面的调用方可以随意取名。旧版本固定生成 `cl-v<旧版本号>`：旧库里如果恰好
-- 已经有一笔叫 `cl-v1` 的合法额度，这一条 INSERT 就撞上「allocation already exists」，整个迁移失败，账户库打不开，
-- 依赖它的启动全部失败。现在 id 在**同一条语句里**分配并验证唯一：先试 `cl-v<旧版本号>`，已经被占用就依次试
-- `cl-v<旧版本号>-1`、`-2`……直到找到一个库里还没有的（递归 CTE 一步步往后试，只有「库里已经有」才继续往后走）。
-- 旧版本号永远带在 id 的开头，所以 id 本身就是「旧版本 → 新额度」的对应，不需要另存一张映射表；不同旧版本的 id
-- 不会互相撞（`cl-v<版本>` 之后只可能跟 `-<序号>`，版本号不含 `-`），所以只需要对照库里已有的 id。
-- 每条要迁的授信恰好选中一个 id，INSERT 要么全部写入要么整个失败，不会悄悄漏掉哪一条。

WITH RECURSIVE
legacy (version, guarantor_id, account_id, unit, limit_micros, valid_from_ms, valid_until_ms) AS (
    SELECT l.version, l.guarantor_id, l.account_id, a.unit, l.limit_micros, l.set_at_ms,
           COALESCE(l.valid_until_ms, 253402300800000)
      FROM ledger_credit_lines l
      JOIN accounts a ON a.account_id = l.account_id
     WHERE l.enabled = 1
       AND l.version = (SELECT MAX(version) FROM ledger_credit_lines WHERE account_id = l.account_id)
       AND COALESCE(l.valid_until_ms, 253402300800000) > l.set_at_ms
),
candidate (version, n, id) AS (
    SELECT version, 0, 'cl-v' || version FROM legacy
    UNION ALL
    SELECT c.version, c.n + 1, 'cl-v' || c.version || '-' || (c.n + 1)
      FROM candidate c
     WHERE EXISTS (SELECT 1 FROM allocations WHERE allocation_id = c.id)
)
INSERT INTO allocations
    (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose,
     valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms)
SELECT c.id, g.guarantor_id, g.account_id, g.unit, g.limit_micros, 'credit_line',
       g.valid_from_ms, g.valid_until_ms, 0, NULL, g.valid_from_ms
  FROM legacy g
  JOIN candidate c ON c.version = g.version
 WHERE NOT EXISTS (SELECT 1 FROM allocations WHERE allocation_id = c.id);

DROP TABLE ledger_credit_lines;

INSERT INTO ledger_subsidy_grants
    (idempotency_key, pool_id, principal_id, member_account, period, unit, amount_micros, allocation_id, granted_at_ms)
SELECT f.idempotency_key, f.pool_id, substr(f.idempotency_key, 9, length(f.idempotency_key) - 16), f.child_id,
       substr(f.idempotency_key, -7), f.ceiling_unit, f.ceiling_micros, f.allocation_id, f.created_at_ms
  FROM (
        SELECT al.idempotency_key, al.child_id, al.ceiling_unit, al.ceiling_micros, al.allocation_id, al.created_at_ms,
               (WITH RECURSIVE up (id, kind, parent, d) AS (
                    SELECT account_id, kind, parent_id, 0 FROM accounts WHERE account_id = al.child_id
                    UNION ALL
                    SELECT p.account_id, p.kind, p.parent_id, up.d + 1
                      FROM accounts p JOIN up ON p.account_id = up.parent
                     WHERE up.d < 64)
                SELECT id FROM up WHERE kind = 'subsidy_pool' ORDER BY d LIMIT 1) AS pool_id
          FROM allocations al
         WHERE al.purpose = 'free_tier'
           AND NOT EXISTS (SELECT 1 FROM ledger_subsidy_grants g WHERE g.idempotency_key = al.idempotency_key)
       ) f
 WHERE f.pool_id IS NOT NULL;

-- # 宽限的部分唯一索引也要配 REPLACE 保护（和 S4 尝试记录的 `0002_no_replace.sql` 同一个缺口，PR #600 第二轮 S1 F5）
--
-- 0001 的 `ledger_grace_one_active_per_period (account_id, period) WHERE voided_at_ms IS NULL` 是部分唯一索引：
-- 「每个付款账户每个账期至多一行没作废的宽限」。0001 给 `ledger_grace_uses` 配的「不许冲突」触发器只看 hold_id 和 use_id，
-- 所以 `INSERT OR REPLACE` 一个新的（别的预留的）宽限，只会撞上这个索引，REPLACE 就把已经记下的那行宽限整行删掉
-- （DELETE 触发器不触发）再插入新的，宽限记录被悄悄换掉。BEFORE 触发器先于冲突处理，所以这里对「会与已有的没作废宽限冲突」
-- 的写入一律拒绝；报错文字沿用唯一约束的写法（`UNIQUE constraint failed: ...`），调用方和测试看到的还是一条唯一约束被违反。
-- 新写入的宽限必须是没作废的（`ledger_grace_uses_insert_rules`），作废只能由没作废变成作废（`void_once`），
-- 所以没有 UPDATE 能制造这种冲突，只需要补 INSERT 这一条。没有改动任何已有的行。
CREATE TRIGGER IF NOT EXISTS ledger_grace_one_active_insert_no_conflict
BEFORE INSERT ON ledger_grace_uses
WHEN NEW.voided_at_ms IS NULL
 AND EXISTS (SELECT 1 FROM ledger_grace_uses
              WHERE account_id = NEW.account_id AND period = NEW.period AND voided_at_ms IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'UNIQUE constraint failed: ledger_grace_uses.account_id, ledger_grace_uses.period (one active grace use per account per period)');
END;

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
--   * `ledger_credit_lines` 里每个账户**最新一条**、启用的授信迁成一笔 credit_line 额度（id `cl-v<旧版本号>`，窗口 =
--     [设置时间, 旧的到期时间)；没有到期时间的用公元 10000 年——额度必须有到期时间，没有「永不过期」）。其余版本
--     （被替换的、关掉的）本来就不生效，不迁；已经过期的额度迁过去也读不到（窗口检查），无害。迁完旧表删除，不再有第二份记录。
--   * 经控制面发出、还没有承诺行的免费额度补登承诺行（池 = 成员账户祖先里最近的补贴池；找不到池的额度没有可记的池，跳过）。
--     补登只记账，不重新检查上限：旧数据里就算已经超发，之后的新发放会因为剩余是 0 被拒绝（偏保守）。

INSERT INTO allocations
    (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose,
     valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms)
SELECT 'cl-v' || l.version, l.guarantor_id, l.account_id, a.unit, l.limit_micros, 'credit_line',
       l.set_at_ms, COALESCE(l.valid_until_ms, 253402300800000), 0, NULL, l.set_at_ms
  FROM ledger_credit_lines l
  JOIN accounts a ON a.account_id = l.account_id
 WHERE l.enabled = 1
   AND l.version = (SELECT MAX(version) FROM ledger_credit_lines WHERE account_id = l.account_id)
   AND COALESCE(l.valid_until_ms, 253402300800000) > l.set_at_ms;

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

-- S3 准入账本，迁移 0003：宽限的部分唯一索引也要挡住 `INSERT OR REPLACE`（PR #600 第二轮 S1 F5；第四轮复审把它从 0002 搬出来）。
--
-- 由 `AccountStore::open` 在 0002 之后、同一条连接上应用（版本 3，记在 `ledger_schema_migrations`）。不改 0001、0002 的任何一个字。
--
-- # 为什么是 0003，不是写进 0002
--
-- 第二轮修复最初把这个触发器直接追加进了 `0002_credit_lines_are_allocations.sql`。迁移器对版本表里已经登记的编号直接跳过，
-- 所以**新库**有这个触发器，但已经跑过旧 0002 的开发 / 预发库（版本表里已经有 2）升级之后得不到它。已经发布的编号迁移不能原地改，
-- 补保护只能追加新编号。账本 0002 里这个触发器的那一段现在去掉了，改在这里；`IF NOT EXISTS`：第二轮之后、这次修复之前建出来的库，
-- 0002 里已经带着它，升级到这里是空操作，保护只有一份。`tests/migration_upgrade.rs` 用首次发布版本的 0002 建库、写入数据、
-- 再用 HEAD 打开，核对升级后 REPLACE 被拒。
--
-- （账本 0002 里还有一处第二轮之后的改动：迁出的授信额度 id 不再撞已有额度。那一处**不能**搬到新编号：旧文本撞 id 时整个迁移失败、
-- 账户库打不开，失败的迁移不会登记版本，新编号救不了一个在 0002 就过不去的库；而它对已经成功升到 2 的库没有任何影响——
-- 旧文本能成功的地方，新文本生成的 id 和旧的完全一样。所以那一处保持在 0002 里，见 0002 的文件头。）
--
-- # 这个触发器管什么
--
-- 0001 的 `ledger_grace_one_active_per_period (account_id, period) WHERE voided_at_ms IS NULL` 是部分唯一索引：
-- 「每个付款账户每个账期至多一行没作废的宽限」。0001 给 `ledger_grace_uses` 配的「不许冲突」触发器只看 hold_id 和 use_id，
-- 所以 `INSERT OR REPLACE` 一个新的（别的预留的）宽限，只会撞上这个索引，REPLACE 就把已经记下的那行宽限整行删掉
-- （DELETE 触发器不触发）再插入新的，宽限记录被悄悄换掉。BEFORE 触发器先于冲突处理，所以这里对「会与已有的没作废宽限冲突」
-- 的写入一律拒绝；报错文字沿用唯一约束的写法（`UNIQUE constraint failed: ...`），调用方和测试看到的还是一条唯一约束被违反。
-- 新写入的宽限必须是没作废的（`ledger_grace_uses_insert_rules`），作废只能由没作废变成作废（`void_once`），
-- 所以没有 UPDATE 能制造这种冲突，只需要补 INSERT 这一条。没有改动任何已有的行。加新的部分唯一索引时在新的编号里补一条
-- （**不要改本文件**）。

CREATE TRIGGER IF NOT EXISTS ledger_grace_one_active_insert_no_conflict
BEFORE INSERT ON ledger_grace_uses
WHEN NEW.voided_at_ms IS NULL
 AND EXISTS (SELECT 1 FROM ledger_grace_uses
              WHERE account_id = NEW.account_id AND period = NEW.period AND voided_at_ms IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'UNIQUE constraint failed: ledger_grace_uses.account_id, ledger_grace_uses.period (one active grace use per account per period)');
END;

-- S3 准入账本，迁移 0004：额度的创建、撤销在账本里的先后（PR #600 第四轮复审 Medium 3；owner 2026-11-15 方案 C 的底线：
-- 新额度不抵旧债）。
--
-- 由 `AccountStore::open` 在 0003 之后、同一条连接上应用（版本 4，记在 `ledger_schema_migrations`）。不改 0001 ~ 0003 的任何一个字。
--
-- # 要解决的问题
--
-- 没充值、有过授信的账户，账本按「先额度后授信」回放它全期的花费（`ledger/project.rs`），一笔额度只该覆盖**它存在之后**的花费。
-- 旧口径拿毫秒比先后：额度的创建时间（`allocations.created_at_ms`）、撤销时间（`allocation_revocations.revoked_at_ms`）、
-- 花费的归属时间（`ledger_entries.window_at_ms`）都是调用方在**进事务之前**取的毫秒——HTTP 处理函数取时之后，请求还要等工作线程
-- 和写锁。排队让「先提交」和「时间更早」对不上：
--
--   * 发额度的请求先取到 t = 1000，随后排队；另一个请求在 t = 1100 花光并结算了 200 授信；发额度的事务后提交，保存的创建时间仍是 1000。
--     旧回放把 1100 的旧花费算成这笔后来才存在的额度覆盖的，债务从 200 降到 100；额度没人用、到期了，授信仍错误地空出 100。
--   * 同一毫秒里先花费、后发额度，毫秒分不出先后。
--   * 反方向：额度先提交、创建时间 5000；一笔花费排在后面提交，但取时更早（1000）。准入那一刻额度已经在库里、这笔用的就是它，
--     之后再读却因为 1000 < 5000 被算成额度创建之前的花费，授信平白多出债务。撤销同理（撤销的毫秒更晚，已撤销的额度回头覆盖了
--     撤销之后才提交的花费，把授信空出来）。
--
-- 毫秒是调用方给的、不保证和提交顺序一致，所以不能拿来比先后。
--
-- # 做法：记下「此刻账本里最后一条分录是第几号」
--
-- `ledger_entries.entry_id` 是 AUTOINCREMENT：分录在写事务里追加，SQLite 的写事务互相串行，所以分录号的大小就是提交的先后。
-- 额度创建、撤销的事务里读一下分录号的高水位（`sqlite_sequence` 里 `ledger_entries` 那一行，没有分录 = 0），记成这笔额度的序号
-- （`after_entry`）：分录号 **大于** 它的分录，是这笔额度创建（撤销）**之后**提交的。毫秒不参与先后的判断，同一毫秒不再有歧义。
--
-- 一笔花费（一个预留在一个账户上的全部分录：预留、结算、释放、过期）的先后看它**第一条**分录的号；迟到的结算是一笔新的花费
-- （记在结算那一刻），看它自己的那条。额度窗口（`valid_from_ms` / `valid_until_ms`）仍然是毫秒：那是额度自己声明的有效期，
-- 按花费的归属时间判断，不是哪个事务取的时间。
--
-- 记在触发器里：`allocations` / `allocation_revocations` 有好几处写入方（存储层、控制面、免费额度、授信设置、手工 SQL），
-- 触发器和额度行同一个事务提交，哪条路径写的都躲不开，调用方没有机会漏记或记错。
--
-- # 对存量数据的处理
--
-- 旧库里已经有的额度、撤销没有序号，只有毫秒，按旧口径折算（只看该额度所属账户自己的分录）：
-- **序号 = 这个账户上、`at_ms` 严格早于创建（撤销）时刻的最大分录号**（没有 = 0）。严格早于：同一毫秒的分录算在它之后，
-- 和旧口径「额度窗口从创建时刻起、含这一毫秒」一致。这是升级那一刻的一次性近似（旧数据本来就只有毫秒可用），之后新写入的额度、
-- 撤销都由触发器记下真正的序号。这个功能没有上线、没有生产库，存量只有开发 / 预发库。
--
-- 这张表只追加：不可改、不可删，`INSERT OR REPLACE` 也拒绝（和账本其它表同一个做法）。

CREATE TABLE ledger_allocation_marks (
    allocation_id TEXT NOT NULL CHECK (length(allocation_id) BETWEEN 1 AND 128),
    -- created：额度创建；revoked：额度被撤销（只有被撤销的额度有）
    event         TEXT NOT NULL CHECK (event IN ('created', 'revoked')),
    -- 事件发生时账本里最后一条分录的号；分录号大于它的分录是事件之后才提交的
    after_entry   INTEGER NOT NULL CHECK (after_entry >= 0),
    PRIMARY KEY (allocation_id, event)
) STRICT;

-- 存量：按旧口径折算（见上）。先回填，再装触发器。
INSERT INTO ledger_allocation_marks (allocation_id, event, after_entry)
SELECT a.allocation_id, 'created',
       COALESCE((SELECT MAX(e.entry_id) FROM ledger_entries e
                  WHERE e.account_id = a.child_id AND e.at_ms < a.created_at_ms), 0)
  FROM allocations a;

INSERT INTO ledger_allocation_marks (allocation_id, event, after_entry)
SELECT r.allocation_id, 'revoked',
       COALESCE((SELECT MAX(e.entry_id) FROM ledger_entries e
                  WHERE e.account_id = (SELECT a.child_id FROM allocations a WHERE a.allocation_id = r.allocation_id)
                    AND e.at_ms < r.revoked_at_ms), 0)
  FROM allocation_revocations r;

CREATE TRIGGER ledger_allocation_marks_insert_no_conflict BEFORE INSERT ON ledger_allocation_marks
WHEN EXISTS (SELECT 1 FROM ledger_allocation_marks WHERE allocation_id = NEW.allocation_id AND event = NEW.event)
BEGIN
    SELECT RAISE(ABORT, 'allocation mark already exists');
END;

-- 显式检查被引用的行存在（不靠外键，和账本其它表一样）；序号不能比账本里已有的分录更靠后。
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

-- 记序号：和额度行（撤销行）在同一个事务里，读的是此刻账本里最后一条分录的号。
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

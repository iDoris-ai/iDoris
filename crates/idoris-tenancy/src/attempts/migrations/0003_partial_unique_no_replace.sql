-- idoris.ai/api 计算服务 S4：带 WHERE 的（部分）唯一索引也要挡住 `INSERT OR REPLACE`（PR #600 第二轮 S1 F5；第四轮复审把它从 0002 搬出来）。
-- 由 `AttemptStore::open` 在 0002 之后应用（版本 3，记在 `compute_attempt_schema_migrations`）。不改 0001、0002 的任何一个字。
--
-- # 为什么是 0003，不是写进 0002
--
-- 第二轮修复最初把这三个触发器直接追加进了 `0002_no_replace.sql`。迁移器对版本表里已经登记的编号直接跳过，所以**新库**有这些触发器，
-- 但已经跑过旧 0002 的开发 / 预发库（版本表里已经有 2）升级之后一条都得不到：更换幂等键的 `INSERT OR REPLACE` 仍然能把已经落定的
-- 18_000 分录整行换掉（第四轮复审用仓库里的原始迁移在内存 SQLite 里复现：升级库新增触发器数是 0）。已经发布的编号迁移不能原地改，
-- 补保护只能追加新编号。`0002_no_replace.sql` 现在逐字回到首次发布时的内容，这里补上后来才加的三个触发器。
--
-- 全部用 `IF NOT EXISTS`：第二轮之后、这次修复之前建出来的库，0002 里已经带着它们（版本表是 [1, 2]），升级到这里是空操作，
-- 每个保护只有一份。`tests/migration_upgrade.rs` 用首次发布版本的 0002 建库、写入数据、再用 HEAD 打开，逐个核对。
--
-- # 这三个触发器管什么
--
-- 0002 给每张表的主键和**无条件**的唯一约束配了「不许冲突」触发器。0001 里还有两个**部分唯一索引**，它们同样是 REPLACE 的冲突点：
--
--   * `idx_compute_postings_one_user_price (request_id) WHERE kind = 'user_price' AND amount_micros > 0`
--     「每个请求至多一笔正的 user_price」：换一个新的幂等键（不指定 posting_id）执行 `INSERT OR REPLACE`，
--     posting_id、幂等键都不冲突，只撞这个索引，REPLACE 就把已经落定的那笔分录**整行删掉**再插入一笔金额随便写的新分录
--     （例如把 18_000 改成 1），`compute_postings` 只追加的触发器管不到这一行是怎么没的；
--   * `idx_compute_attempts_one_payer (request_id) WHERE pays_user = 1`
--     「每个请求至多一个向用户收费的尝试」：新尝试的 INSERT 有发布边界触发器挡着，但 `UPDATE OR REPLACE ... SET pays_user = 1`
--     撞上索引时，原来那个收费的尝试会被整行删掉。
--
-- 做法和 0002 一样：BEFORE 触发器先于冲突处理执行，所以对这两个索引的范围内（谓词和索引的 WHERE 完全一致）、
-- 会与已有行冲突的写入一律拒绝，REPLACE 没有机会生效。负数冲正（`amount_micros <= 0`）、`pays_user = 0` 的写入
-- 不在索引范围内，不受影响。加新的部分唯一索引时在新的编号里补一条（**不要改本文件**）。
--
-- 对存量数据没有影响：只加触发器，不动任何行。

CREATE TRIGGER IF NOT EXISTS compute_postings_one_user_price_no_conflict
BEFORE INSERT ON compute_postings
WHEN NEW.kind = 'user_price' AND NEW.amount_micros > 0
 AND EXISTS (SELECT 1 FROM compute_postings
              WHERE request_id = NEW.request_id AND kind = 'user_price' AND amount_micros > 0)
BEGIN SELECT RAISE(ABORT, 'a request has at most one positive user_price posting'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_one_payer_insert_no_conflict
BEFORE INSERT ON compute_attempts
WHEN NEW.pays_user = 1
 AND EXISTS (SELECT 1 FROM compute_attempts WHERE request_id = NEW.request_id AND pays_user = 1)
BEGIN SELECT RAISE(ABORT, 'a request has at most one paying attempt'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_one_payer_update_no_conflict
BEFORE UPDATE OF pays_user ON compute_attempts
WHEN NEW.pays_user = 1
 AND EXISTS (SELECT 1 FROM compute_attempts
              WHERE request_id = NEW.request_id AND pays_user = 1 AND attempt_id <> NEW.attempt_id)
BEGIN SELECT RAISE(ABORT, 'a request has at most one paying attempt'); END;

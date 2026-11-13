-- idoris.ai/api 计算服务 S4：堵住 `INSERT OR REPLACE`（PR-Daemon S1 F5）。
-- 由 `AttemptStore::open` 在 0001 之后应用（版本 2，记在 `compute_attempt_schema_migrations`）。不改 0001 的任何一个字。
--
-- 0001 里「只追加」「身份不可改」「请求关闭后冻结」都靠 BEFORE UPDATE / BEFORE DELETE 触发器。但 `INSERT OR REPLACE`
-- （`REPLACE INTO`）在主键或唯一约束冲突时，是**先删掉旧行、再插入新行**，而 `recursive_triggers` 默认关闭，
-- 这时 DELETE 触发器不会触发——所以上面所有保护都能被它绕过去：改写已经落定的结算分录、转移日志，
-- 改写已经关闭的请求的付款账户和结算摘要，改写一次尝试的预留额。
--
-- SQLite 里 BEFORE INSERT 触发器先于冲突处理执行，所以这里对每张表的**每个主键和每个唯一约束**各配一条
-- 「会与已有行冲突的插入一律拒绝」的触发器，REPLACE 没有机会生效（账户存储和账本的迁移是同一个做法，见
-- `accounts/migrations/0001_accounts.sql` 末尾）。`INSERT OR IGNORE` 同样会被拒绝——这个存储的代码都是先查再插
-- （见 `finalize_request` 的分录幂等），没有靠 OR IGNORE 的地方。加新的主键 / 唯一约束时在这里补一条。
--
-- 对存量数据没有影响：只加触发器，不动任何行；已有的行都满足主键 / 唯一约束，不会被这些触发器拦住。

CREATE TRIGGER IF NOT EXISTS compute_requests_insert_no_conflict
BEFORE INSERT ON compute_requests
WHEN EXISTS (SELECT 1 FROM compute_requests WHERE request_id = NEW.request_id)
BEGIN SELECT RAISE(ABORT, 'request already exists'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_insert_no_conflict
BEFORE INSERT ON compute_attempts
WHEN EXISTS (SELECT 1 FROM compute_attempts WHERE attempt_id = NEW.attempt_id)
  OR EXISTS (SELECT 1 FROM compute_attempts
              WHERE request_id = NEW.request_id AND attempt_index = NEW.attempt_index)
BEGIN SELECT RAISE(ABORT, 'attempt already exists'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempt_events_insert_no_conflict
BEFORE INSERT ON compute_attempt_events
WHEN NEW.event_seq IS NOT NULL
 AND EXISTS (SELECT 1 FROM compute_attempt_events WHERE event_seq = NEW.event_seq)
BEGIN SELECT RAISE(ABORT, 'attempt event already exists'); END;

CREATE TRIGGER IF NOT EXISTS compute_postings_insert_no_conflict
BEFORE INSERT ON compute_postings
WHEN (NEW.posting_id IS NOT NULL
      AND EXISTS (SELECT 1 FROM compute_postings WHERE posting_id = NEW.posting_id))
  OR EXISTS (SELECT 1 FROM compute_postings WHERE idempotency_key = NEW.idempotency_key)
BEGIN SELECT RAISE(ABORT, 'posting already exists, or its idempotency key was already used'); END;

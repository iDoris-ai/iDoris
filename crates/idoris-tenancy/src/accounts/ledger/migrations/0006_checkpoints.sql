-- 派生投影。保留原始分录供对账、历史窗口查询和重建；旧库按账户逐步生成检查点。
CREATE TABLE ledger_checkpoints (
    account_id TEXT PRIMARY KEY,
    through_entry INTEGER NOT NULL,
    held_micros INTEGER NOT NULL,
    committed_micros INTEGER NOT NULL,
    funds_micros INTEGER NOT NULL,
    uncovered_micros INTEGER NOT NULL CHECK (uncovered_micros >= 0),
    first_at_ms INTEGER NOT NULL,
    last_at_ms INTEGER NOT NULL,
    bucket_remaining TEXT NOT NULL
) STRICT;

CREATE INDEX ledger_entries_by_account_entry ON ledger_entries (account_id, entry_id);

-- 同一关闭预留的分录可能跨过水位。水位之后已汇总的分录用这张表跳过。
CREATE TABLE ledger_checkpoint_spill (
    entry_id INTEGER PRIMARY KEY,
    account_id TEXT NOT NULL,
    folded_at INTEGER NOT NULL CHECK (folded_at < entry_id)
) STRICT;
CREATE INDEX ledger_checkpoint_spill_by_account ON ledger_checkpoint_spill (account_id, entry_id);
CREATE INDEX ledger_checkpoint_spill_by_fold ON ledger_checkpoint_spill (account_id, folded_at, entry_id);

CREATE TRIGGER ledger_checkpoints_clear_spill AFTER DELETE ON ledger_checkpoints
BEGIN
    DELETE FROM ledger_checkpoint_spill WHERE account_id = OLD.account_id;
END;

-- 正常写入只改变未关闭的预留。若另一个写入者向已压缩的预留追加分录，
-- 或显式填入检查点之前的序号，下次投影从原始账本重建。
CREATE TRIGGER ledger_checkpoints_invalidate AFTER INSERT ON ledger_entries
WHEN EXISTS (
    SELECT 1 FROM ledger_checkpoints c WHERE c.account_id = NEW.account_id
      AND (NEW.entry_id <= c.through_entry OR EXISTS (
          SELECT 1 FROM ledger_entries e INDEXED BY ledger_entries_by_hold
           WHERE e.hold_id = NEW.hold_id AND e.account_id = NEW.account_id AND e.entry_id <= c.through_entry
      ))
)
BEGIN
    DELETE FROM ledger_checkpoints WHERE account_id = NEW.account_id;
END;

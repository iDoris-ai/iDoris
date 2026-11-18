-- idoris.ai/api 计算服务 S7：本机云端档的调用记录与对账游标。
-- 由 `CloudTierStore::open` 在一个 `BEGIN IMMEDIATE` 事务里应用，版本记在
-- `cloud_tier_schema_migrations`（与其它 store 的版本表互不共用）。
--
-- 设计依据：docs/design/compute-service-design.md §9「组织与云的对账」——用尝试 ID 和不可变账目做幂等游标；
-- 离线的组织节点不能自行增加可消费的云额度，本地离线时只消耗 QUOTA。
--
-- 这张表只是「本机自己的请求 id」和「云端回的 X-iDoris-Record-Id」的对应关系，加上云端报告的用量：
-- 它不是账本。这里没有余额、没有额度、没有任何「可消费」的东西；云端成本以云账本为准。
--
-- 硬规矩（触发器和 CHECK 强制，不靠调用方自觉）：
--   * 一次尝试一行，先写 `pending` 再出网（没有记录就不出网）；之后**恰好一次**从 `pending` 变成一个终态；
--     终态的行不能再改，任何行都不能删；
--   * 身份字段（seq、本机请求 id、尝试序号、开始时间）写下就不可改；
--   * 用量（tokens）只有云端真的答了（`served`）才可能有：未出网、不可达、超时的行永远没有金额，
--     所以离线期间不可能在本地预先记账；
--   * `served` 必有云端 record id 和 2xx 状态码；云端 record id 全表唯一（同一次云端请求不能对应两次本机请求）；
--   * 游标只增不减，而且不能越过还没有结局的调用（`pending`）。
-- 这里**没有任何正文**：不存 prompt、completion，也不存云端返回的错误正文；
-- 字符串列都只接受标识符形状的值（见 `CloudTierStore` 里的校验）。

CREATE TABLE IF NOT EXISTS cloud_tier_calls (
    -- 游标：单调递增、永不复用（AUTOINCREMENT）。对账按它往后读。
    seq               INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 本机这次请求的 X-iDoris-Record-Id。
    local_request_id  TEXT NOT NULL CHECK (length(local_request_id) BETWEEN 1 AND 128),
    -- 同一个本机请求的第几次尝试。S7 从不重试，永远是 1；列留着，将来重试时幂等键仍然成立。
    attempt_no        INTEGER NOT NULL DEFAULT 1 CHECK (attempt_no >= 1),
    started_at_ms     INTEGER NOT NULL CHECK (started_at_ms >= 0),
    outcome           TEXT NOT NULL CHECK (outcome IN (
        'pending', 'served', 'unreachable', 'timeout', 'cancelled', 'unauthorized', 'forbidden',
        'budget_exhausted', 'rate_limited', 'rejected', 'cloud_error', 'invalid_response', 'interrupted'
    )),
    finished_at_ms    INTEGER CHECK (finished_at_ms IS NULL OR finished_at_ms >= started_at_ms),
    -- 云端回的 X-iDoris-Record-Id（它那边的请求 id）；云端从未答过就是 NULL。
    cloud_record_id   TEXT CHECK (cloud_record_id IS NULL OR length(cloud_record_id) BETWEEN 1 AND 128),
    http_status       INTEGER CHECK (http_status IS NULL OR http_status BETWEEN 100 AND 599),
    -- 云端的稳定原因码（error.reason_code 或 X-iDoris-Reason），不是自由文本。
    cloud_reason      TEXT CHECK (cloud_reason IS NULL OR length(cloud_reason) BETWEEN 1 AND 128),
    -- 云端在响应体 usage 里报告的 token 数（token 当量，对应组织的 QUOTA 单位）。
    tokens_in         INTEGER CHECK (tokens_in  IS NULL OR tokens_in  >= 0),
    tokens_out        INTEGER CHECK (tokens_out IS NULL OR tokens_out >= 0),
    CHECK ((outcome = 'pending') = (finished_at_ms IS NULL)),
    -- 注意：CHECK 里表达式为 NULL 也算通过，所以每个条件都要显式 IS NOT NULL。
    CHECK (outcome <> 'served'
           OR (cloud_record_id IS NOT NULL AND http_status IS NOT NULL
               AND http_status BETWEEN 200 AND 299)),
    CHECK (outcome = 'served' OR (tokens_in IS NULL AND tokens_out IS NULL)),
    UNIQUE (local_request_id, attempt_no)
) STRICT;

-- 同一次云端请求只能对应一次本机请求。
CREATE UNIQUE INDEX IF NOT EXISTS cloud_tier_calls_cloud_record
    ON cloud_tier_calls (cloud_record_id) WHERE cloud_record_id IS NOT NULL;

CREATE TRIGGER IF NOT EXISTS cloud_tier_calls_no_delete
BEFORE DELETE ON cloud_tier_calls
BEGIN
    SELECT RAISE(ABORT, 'cloud tier call log is append-only');
END;

-- 只允许 pending -> 终态，一次；身份字段不可改。
CREATE TRIGGER IF NOT EXISTS cloud_tier_calls_finalize_once
BEFORE UPDATE ON cloud_tier_calls
BEGIN
    SELECT CASE
        WHEN OLD.outcome <> 'pending' THEN RAISE(ABORT, 'cloud tier call already has a final outcome')
        WHEN NEW.outcome = 'pending' THEN RAISE(ABORT, 'cloud tier call must move to a final outcome')
        WHEN NEW.seq <> OLD.seq
          OR NEW.local_request_id <> OLD.local_request_id
          OR NEW.attempt_no <> OLD.attempt_no
          OR NEW.started_at_ms <> OLD.started_at_ms
            THEN RAISE(ABORT, 'cloud tier call identity is immutable')
    END;
END;

-- 对账游标：每个对账方一行，记录「已经对完的最大 seq」。
CREATE TABLE IF NOT EXISTS cloud_tier_cursor (
    name           TEXT PRIMARY KEY NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    last_seq       INTEGER NOT NULL CHECK (last_seq >= 0),
    updated_at_ms  INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT;

CREATE TRIGGER IF NOT EXISTS cloud_tier_cursor_monotonic
BEFORE UPDATE ON cloud_tier_cursor
WHEN NEW.last_seq < OLD.last_seq OR NEW.name <> OLD.name
BEGIN
    SELECT RAISE(ABORT, 'cloud tier cursor can only move forward');
END;

CREATE TRIGGER IF NOT EXISTS cloud_tier_cursor_no_delete
BEFORE DELETE ON cloud_tier_cursor
BEGIN
    SELECT RAISE(ABORT, 'cloud tier cursor cannot be deleted');
END;

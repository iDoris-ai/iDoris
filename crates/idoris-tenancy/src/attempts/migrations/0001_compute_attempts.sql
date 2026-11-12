-- idoris.ai/api 计算服务 S4：请求、尝试、转移日志、结算分录。
-- 由 `AttemptStore::open` 在一个 `BEGIN IMMEDIATE` 事务里应用，版本记在
-- `compute_attempt_schema_migrations`（与其它 store 的版本表互不共用）。
--
-- 设计依据：docs/design/compute-service-design.md §6、§10；恢复后对账见
-- docs/design/compute-service-ops.md §7（attempts / postings 两张目标表）。
--
-- 硬规矩（触发器强制，不靠调用方自觉）：
--   * 转移日志、结算分录**只追加**：禁止 UPDATE / DELETE；冲正是一条新分录；
--   * 尝试的身份与定价字段一旦写下不可改；终态（not_sent / settled）不可再动；
--   * 状态只能沿 S0 `attempt::transition` 允许的边推进（edges 表见触发器；
--     与 S0 的一致性由 `attempts::tests::state_edges_match_the_contract_transition_table` 逐格核对）；
--   * 供应商请求号只写一次；`pays_user` 只能 0 -> 1；
--   * 首字节发布之后**不能再追加尝试**（发布边界，设计稿 §6）；
--   * 请求一旦关闭（outcome 非空），摘要不可再改。
-- 这里**没有任何正文**：不存 prompt、completion、工具参数，也不存上游返回的错误正文。

CREATE TABLE IF NOT EXISTS compute_requests (
    request_id         TEXT PRIMARY KEY NOT NULL,
    -- 付款上下文（S0 `PaymentContext` 的 JSON）：重建 `Request` 用。
    payment_json       TEXT NOT NULL CHECK (json_valid(payment_json)),
    payer_account      TEXT NOT NULL,
    credential_id      TEXT NOT NULL,
    price_unit         TEXT NOT NULL,
    form               TEXT NOT NULL,
    billing            TEXT NOT NULL CHECK (billing IN ('paid', 'subsidized', 'unbilled')),
    stream             INTEGER NOT NULL CHECK (stream IN (0, 1)),
    -- 租户的数据类别（逗号分隔的 snake_case，只用于审计与对账筛选）。
    data_classes       TEXT NOT NULL,
    created_at_ms      INTEGER NOT NULL CHECK (created_at_ms >= 0),
    -- 已向客户端输出第一个字节的尝试序号；写一次。
    published_attempt  INTEGER CHECK (published_attempt IS NULL OR published_attempt BETWEEN 0 AND 3),
    published_at_ms    INTEGER,
    -- NULL = 进行中。answered：有尝试发布过；unanswered：没有尝试发布；rejected：没有任何尝试发出（准入或路由阶段被拒）。
    outcome            TEXT CHECK (outcome IS NULL OR outcome IN ('answered', 'unanswered', 'rejected')),
    reason_code        TEXT,
    closed_at_ms       INTEGER,
    -- 结算摘要（来自 `SettlementPlan::user_charge`）；未发布为 NULL（用户付 0）。
    charged_micros     INTEGER CHECK (charged_micros IS NULL OR charged_micros >= 0),
    charged_basis      TEXT CHECK (charged_basis IS NULL OR charged_basis IN ('actual_usage', 'reserved_amount')),
    reserved_micros    INTEGER CHECK (reserved_micros IS NULL OR reserved_micros >= 0),
    overage_micros     INTEGER CHECK (overage_micros IS NULL OR overage_micros >= 0),
    CHECK ((published_attempt IS NULL) = (published_at_ms IS NULL)),
    CHECK ((outcome IS NULL) = (closed_at_ms IS NULL))
) STRICT;

CREATE TABLE IF NOT EXISTS compute_attempts (
    attempt_id            TEXT PRIMARY KEY NOT NULL,
    request_id            TEXT NOT NULL REFERENCES compute_requests (request_id),
    attempt_index         INTEGER NOT NULL CHECK (attempt_index BETWEEN 0 AND 3),
    -- 候选端点（S0 `EndpointKey`）。上游 URL 不在这里，也不在契约里。
    provider              TEXT NOT NULL,
    model                 TEXT NOT NULL,
    endpoint              TEXT NOT NULL,
    price_version         TEXT NOT NULL,
    effective_max_tokens  INTEGER NOT NULL CHECK (effective_max_tokens >= 1),
    -- 预留：对用户的价（占付款账户额度）与上游成本（挂账待核销）。
    reserved_price_micros INTEGER NOT NULL CHECK (reserved_price_micros >= 0),
    reserved_price_unit   TEXT NOT NULL,
    reserved_cost_micros  INTEGER NOT NULL CHECK (reserved_cost_micros >= 0),
    reserved_cost_unit    TEXT NOT NULL,
    -- 创建时的完整快照（S0 `Attempt` 的 JSON，含价目表）：不可变，重建 `Request` 用。
    snapshot_json         TEXT NOT NULL CHECK (json_valid(snapshot_json)),
    -- 当前状态（S0 `AttemptState` 的 JSON，带 usage / 结算依据），只能经 `transition` 推进。
    state                 TEXT NOT NULL CHECK (state IN (
        'admitted', 'sending', 'not_sent', 'maybe_executed',
        'executed_usage_known', 'executed_usage_unknown', 'settled')),
    state_json            TEXT NOT NULL CHECK (
        json_valid(state_json) AND json_extract(state_json, '$.state') = state),
    -- 执行确定性（S0 `Certainty`），由 state 推出，不单独存。
    certainty             TEXT GENERATED ALWAYS AS (CASE state
        WHEN 'maybe_executed' THEN 'maybe'
        WHEN 'executed_usage_known' THEN 'executed'
        WHEN 'executed_usage_unknown' THEN 'executed'
        WHEN 'settled' THEN 'executed'
        ELSE 'not_executed' END) VIRTUAL,
    -- 已知的 usage（来自 state_json，不单独存）。
    usage_prompt_tokens     INTEGER GENERATED ALWAYS AS (COALESCE(
        json_extract(state_json, '$.usage.prompt_tokens'),
        json_extract(state_json, '$.basis.actual_usage.prompt_tokens'))) VIRTUAL,
    usage_completion_tokens INTEGER GENERATED ALWAYS AS (COALESCE(
        json_extract(state_json, '$.usage.completion_tokens'),
        json_extract(state_json, '$.basis.actual_usage.completion_tokens'))) VIRTUAL,
    usage_cached_tokens     INTEGER GENERATED ALWAYS AS (COALESCE(
        json_extract(state_json, '$.usage.cached_prompt_tokens'),
        json_extract(state_json, '$.basis.actual_usage.cached_prompt_tokens'))) VIRTUAL,
    -- 向用户收费的那一次（= 发布了的那次）：0 -> 1，不回退。
    pays_user             INTEGER NOT NULL DEFAULT 0 CHECK (pays_user IN (0, 1)),
    -- 供应商给这次调用的请求号（对账用），写一次。
    supplier_request_id   TEXT,
    -- 对账提示：`duplicate_of:<attempt>`（别的尝试已经用过同一个请求号）、`id_changed_midstream`、`invalid_format`。
    supplier_id_anomaly   TEXT,
    -- 上游声称的 usage 原文（只有数字字段，截断）与判定：`valid` / `invalid:<原因>` / `absent`。写一次。
    usage_claim_json      TEXT CHECK (usage_claim_json IS NULL OR json_valid(usage_claim_json)),
    -- 失败标注（稳定的代码，不含上游正文；不改变状态，只是给对账的人看）。写一次。
    failure_kind          TEXT,
    failure_http_status   INTEGER,
    created_at_ms         INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms         INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    UNIQUE (request_id, attempt_index),
    CHECK (attempt_id = request_id || '#' || attempt_index)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_compute_attempts_supplier
    ON compute_attempts (provider, supplier_request_id)
    WHERE supplier_request_id IS NOT NULL;

-- 对账扫描用：还没落定的尝试。
CREATE INDEX IF NOT EXISTS idx_compute_attempts_unsettled
    ON compute_attempts (state)
    WHERE state IN ('admitted', 'sending', 'maybe_executed', 'executed_usage_known', 'executed_usage_unknown');

-- 每一次合法转移一行，只追加。
CREATE TABLE IF NOT EXISTS compute_attempt_events (
    event_seq   INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id  TEXT NOT NULL REFERENCES compute_attempts (attempt_id),
    from_state  TEXT NOT NULL,
    event_json  TEXT NOT NULL CHECK (json_valid(event_json)),
    to_state    TEXT NOT NULL,
    at_ms       INTEGER NOT NULL CHECK (at_ms >= 0)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_compute_attempt_events_attempt
    ON compute_attempt_events (attempt_id, event_seq);

-- 结算分录，只追加（ops §7.1 的 postings）。金额为带单位的微单位整数；冲正是一条新分录（负数），带新的键。
--   user_price     向用户收的价（只有发布了的那次尝试有）
--   platform_cost  按实际用量记的上游成本
--   hold           按预留额挂账、待核销的上游成本（maybe_executed / usage 未知）
--   release        从用户额度里释放的价预留
CREATE TABLE IF NOT EXISTS compute_postings (
    posting_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id       TEXT NOT NULL REFERENCES compute_requests (request_id),
    attempt_id       TEXT NOT NULL REFERENCES compute_attempts (attempt_id),
    kind             TEXT NOT NULL CHECK (kind IN ('user_price', 'platform_cost', 'hold', 'release')),
    role             TEXT NOT NULL CHECK (role IN ('answering', 'extra')),
    basis            TEXT NOT NULL CHECK (basis IN (
        'actual_usage', 'reserved_amount', 'reserved_pending_reconciliation', 'price_reservation')),
    unit             TEXT NOT NULL,
    amount_micros    INTEGER NOT NULL,
    idempotency_key  TEXT NOT NULL UNIQUE,
    created_at_ms    INTEGER NOT NULL CHECK (created_at_ms >= 0)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_compute_postings_attempt
    ON compute_postings (attempt_id, kind);

-- ---------------------------------------------------------------------------
-- 触发器
-- ---------------------------------------------------------------------------

CREATE TRIGGER IF NOT EXISTS compute_attempt_events_no_update
BEFORE UPDATE ON compute_attempt_events
BEGIN SELECT RAISE(ABORT, 'compute_attempt_events is append-only'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempt_events_no_delete
BEFORE DELETE ON compute_attempt_events
BEGIN SELECT RAISE(ABORT, 'compute_attempt_events is append-only'); END;

CREATE TRIGGER IF NOT EXISTS compute_postings_no_update
BEFORE UPDATE ON compute_postings
BEGIN SELECT RAISE(ABORT, 'compute_postings is append-only'); END;

CREATE TRIGGER IF NOT EXISTS compute_postings_no_delete
BEFORE DELETE ON compute_postings
BEGIN SELECT RAISE(ABORT, 'compute_postings is append-only'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_no_delete
BEFORE DELETE ON compute_attempts
BEGIN SELECT RAISE(ABORT, 'compute_attempts rows are never deleted'); END;

CREATE TRIGGER IF NOT EXISTS compute_requests_no_delete
BEFORE DELETE ON compute_requests
BEGIN SELECT RAISE(ABORT, 'compute_requests rows are never deleted'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_identity_immutable
BEFORE UPDATE OF attempt_id, request_id, attempt_index, provider, model, endpoint, price_version,
    effective_max_tokens, reserved_price_micros, reserved_price_unit, reserved_cost_micros,
    reserved_cost_unit, snapshot_json, created_at_ms
ON compute_attempts
BEGIN SELECT RAISE(ABORT, 'attempt identity and pricing are immutable'); END;

-- 终态（not_sent / settled）不可再动。
CREATE TRIGGER IF NOT EXISTS compute_attempts_terminal_frozen
BEFORE UPDATE OF state, state_json ON compute_attempts
WHEN OLD.state IN ('not_sent', 'settled')
BEGIN SELECT RAISE(ABORT, 'terminal attempt states are frozen'); END;

-- 状态只能沿 S0 `attempt::transition` 允许的边推进（共 10 条边）。
CREATE TRIGGER IF NOT EXISTS compute_attempts_state_edges
BEFORE UPDATE OF state ON compute_attempts
WHEN NEW.state <> OLD.state AND NOT (
       (OLD.state = 'admitted' AND NEW.state IN ('sending', 'not_sent'))
    OR (OLD.state = 'sending' AND NEW.state IN ('maybe_executed', 'not_sent'))
    OR (OLD.state = 'maybe_executed'
        AND NEW.state IN ('executed_usage_known', 'executed_usage_unknown', 'not_sent'))
    OR (OLD.state = 'executed_usage_unknown' AND NEW.state IN ('executed_usage_known', 'settled'))
    OR (OLD.state = 'executed_usage_known' AND NEW.state = 'settled'))
BEGIN SELECT RAISE(ABORT, 'illegal attempt state edge'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_supplier_id_write_once
BEFORE UPDATE OF supplier_request_id ON compute_attempts
WHEN OLD.supplier_request_id IS NOT NULL AND NEW.supplier_request_id IS NOT OLD.supplier_request_id
BEGIN SELECT RAISE(ABORT, 'supplier_request_id is write-once'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_annotations_write_once
BEFORE UPDATE OF usage_claim_json, failure_kind, failure_http_status ON compute_attempts
WHEN (OLD.usage_claim_json IS NOT NULL AND NEW.usage_claim_json IS NOT OLD.usage_claim_json)
  OR (OLD.failure_kind IS NOT NULL AND NEW.failure_kind IS NOT OLD.failure_kind)
  OR (OLD.failure_http_status IS NOT NULL AND NEW.failure_http_status IS NOT OLD.failure_http_status)
BEGIN SELECT RAISE(ABORT, 'attempt annotations are write-once'); END;

CREATE TRIGGER IF NOT EXISTS compute_attempts_pays_user_monotonic
BEFORE UPDATE OF pays_user ON compute_attempts
WHEN OLD.pays_user = 1 AND NEW.pays_user <> 1
BEGIN SELECT RAISE(ABORT, 'pays_user can only go from 0 to 1'); END;

-- 发布边界：请求一旦有尝试发布了第一个字节，就不能再追加尝试（透明重试 / 切换）。
CREATE TRIGGER IF NOT EXISTS compute_attempts_publish_boundary
BEFORE INSERT ON compute_attempts
WHEN (SELECT published_attempt FROM compute_requests WHERE request_id = NEW.request_id) IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'publish boundary: no attempt may be added after the first byte was published'); END;

-- 已关闭的请求不能再追加尝试。
CREATE TRIGGER IF NOT EXISTS compute_attempts_closed_request
BEFORE INSERT ON compute_attempts
WHEN (SELECT outcome FROM compute_requests WHERE request_id = NEW.request_id) IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'request is closed: no attempt may be added'); END;

-- 发布记录写一次。
CREATE TRIGGER IF NOT EXISTS compute_requests_publish_write_once
BEFORE UPDATE OF published_attempt, published_at_ms ON compute_requests
WHEN OLD.published_attempt IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'the publish record is write-once'); END;

-- 请求一旦关闭，除了「发布记录」之外的一切都冻结（对账的调整走新分录，不回头改摘要）。
CREATE TRIGGER IF NOT EXISTS compute_requests_closed_frozen
BEFORE UPDATE ON compute_requests
WHEN OLD.outcome IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'a closed request is frozen'); END;

CREATE TRIGGER IF NOT EXISTS compute_requests_identity_immutable
BEFORE UPDATE OF request_id, payment_json, payer_account, credential_id, price_unit, form,
    billing, stream, data_classes, created_at_ms
ON compute_requests
BEGIN SELECT RAISE(ABORT, 'request identity is immutable'); END;

//! idoris.ai/api 计算服务 **S7：本机云端档的调用记录与对账游标**（SQLite）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §9「组织与云的对账（评审 H2）：用尝试 ID 和不可变账目做幂等游标。
//! 离线的组织节点不能自行增加可消费的云额度。本地离线时只消耗 `QUOTA`。」以及 §4（`QUOTA` 的单位是 token 当量）。
//!
//! # 这里有什么，没有什么
//!
//! 每次经云端档出网的请求，本机记一行：**本机自己的请求 id** ↔ **云端回的 `X-iDoris-Record-Id`**（云端那边的请求 id，
//! 云端的尝试 id 是 `<它>#<序号>`，由请求 id 确定），加上云端在响应里报告的 token 用量。这是对账用的**指针**，不是账本：
//!
//! - 没有余额、没有额度、没有任何「可消费」的东西。云端成本以云账本为准（设计稿 §9 表）；
//! - 没有金额：云端的响应里没有价格，只有 `usage`（token）。所以这里记的是 token 当量，正好是组织 `QUOTA` 的单位；
//! - 没有正文：不存 prompt、completion，也不存云端返回的错误正文。字符串列只接受标识符形状的值。
//!
//! # 「幂等游标」是什么
//!
//! - `seq`（`AUTOINCREMENT`）单调递增、永不复用；对账方按 `seq` 往后读（[`CloudTierStore::calls_after`]）；
//! - 每个对账方一个命名游标（[`CloudTierStore::advance_cursor`]）：只增不减，重复推进到同一个位置是空操作，
//!   并且**不能越过还没有结局的调用**（`pending`），所以对账永远不会跳过一笔还在路上的钱；
//! - 同一个本机请求同一次尝试只有一行（`UNIQUE (local_request_id, attempt_no)`），同一个云端 record id 全表只出现一次
//!   （部分唯一索引）：重放同一个响应不会记两遍。
//!
//! 本仓库**没有**对账方（云端没有「读账目」的接口，S7 也不做）。游标和调用记录是它将来要读的东西，现在先把形状钉死。
//!
//! # 先写后出网，终态只写一次
//!
//! [`CloudTierStore::begin_call`] 在任何字节离开本机**之前**落一行 `pending`；之后 [`CloudTierStore::finish_call`]
//! 恰好一次把它推进到终态（触发器强制）。进程在中间崩溃，留下的 `pending` 行启动时由
//! [`CloudTierStore::recover_interrupted`] 标成 `interrupted`：「请求可能已经到了云端，但本机不知道它的 id」，
//! 这是对账时要人工核对的一类。
//!
//! # 离线
//!
//! 云端不可达时（`unreachable`、`timeout`），行里**没有**云端 id、没有 token：表的 CHECK 约束不允许它们带用量。
//! 所以本机离线时不可能在本地记出一笔「云端用了多少」，也没有任何地方增加可消费的额度。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

#[cfg(test)]
mod tests;

const MIGRATION_0001: &str = include_str!("migrations/0001_cloud_tier.sql");

/// 云端档调用记录的错误。错误信息里没有任何正文或密钥。
#[derive(Debug, Error)]
pub enum CloudTierStoreError {
    #[error("cloud tier store sqlite error: {0}")]
    Sql(#[from] rusqlite::Error),
    /// 某个字段不是标识符形状（可能带了正文）：拒绝写入，不是悄悄截断。
    #[error("cloud tier store: field {0} is not identifier-shaped")]
    InvalidField(&'static str),
    #[error("cloud tier call {local_request_id}#{attempt_no} already exists")]
    Duplicate {
        local_request_id: String,
        attempt_no: u32,
    },
    #[error("cloud tier call {0} not found")]
    NotFound(i64),
    /// 这一行已经有终态，而且和这次要写的不一样。
    #[error("cloud tier call {0} already has a different final outcome")]
    AlreadyFinal(i64),
    /// 游标推进被拒：越过了最大 seq，或越过了还没有结局的调用。
    #[error("cloud tier cursor cannot move to {to}: {why}")]
    CursorRefused { to: i64, why: &'static str },
    /// 这个云端 record id 已经对应了别的本机请求。
    #[error("cloud record id is already recorded for another local request")]
    CloudRecordConflict,
    #[error("cloud tier store: stored row is corrupt: {0}")]
    Corrupt(String),
}

/// 一次云端档调用的结局。线上（数据库里）的字符串**不可变**，只能加不能改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallOutcome {
    /// 云端答了，本机把回答交给了调用方。
    Served,
    /// 连接不上（DNS、连接被拒、TLS）：没有任何响应。
    Unreachable,
    /// 超时：没有在期限内收到完整的响应。
    Timeout,
    /// 调用方在云端答复之前断开了。
    Cancelled,
    /// 云端 401：凭证不被接受（撤销、过期、不认识）。
    Unauthorized,
    /// 云端 403：凭证有效但这次请求被拒（租户没开通、账户停用……）。
    Forbidden,
    /// 云端 402：云账户额度用完。
    BudgetExhausted,
    /// 云端 429：被限流。
    RateLimited,
    /// 云端 400 / 413 / 422：请求形状或模型名被拒。
    Rejected,
    /// 云端 5xx 或其它意外状态码。
    CloudError,
    /// 云端 2xx，但响应体不是一个可用的回答。
    InvalidResponse,
    /// 进程在出网途中崩溃，请求可能已经到了云端，但本机不知道它的 id。启动时由 `recover_interrupted` 标记。
    Interrupted,
}

impl CallOutcome {
    pub const ALL: [CallOutcome; 12] = [
        CallOutcome::Served,
        CallOutcome::Unreachable,
        CallOutcome::Timeout,
        CallOutcome::Cancelled,
        CallOutcome::Unauthorized,
        CallOutcome::Forbidden,
        CallOutcome::BudgetExhausted,
        CallOutcome::RateLimited,
        CallOutcome::Rejected,
        CallOutcome::CloudError,
        CallOutcome::InvalidResponse,
        CallOutcome::Interrupted,
    ];

    /// 数据库里的字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            CallOutcome::Served => "served",
            CallOutcome::Unreachable => "unreachable",
            CallOutcome::Timeout => "timeout",
            CallOutcome::Cancelled => "cancelled",
            CallOutcome::Unauthorized => "unauthorized",
            CallOutcome::Forbidden => "forbidden",
            CallOutcome::BudgetExhausted => "budget_exhausted",
            CallOutcome::RateLimited => "rate_limited",
            CallOutcome::Rejected => "rejected",
            CallOutcome::CloudError => "cloud_error",
            CallOutcome::InvalidResponse => "invalid_response",
            CallOutcome::Interrupted => "interrupted",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        CallOutcome::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == text)
    }
}

/// 一次调用在表里的状态：还在路上，或者终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Pending,
    Final(CallOutcome),
}

impl CallState {
    pub fn as_str(self) -> &'static str {
        match self {
            CallState::Pending => "pending",
            CallState::Final(outcome) => outcome.as_str(),
        }
    }
}

/// `begin_call` 发给调用方的凭条；`finish_call` 要它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTicket {
    pub seq: i64,
    pub local_request_id: String,
    pub attempt_no: u32,
}

/// 终态要写的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallFinish<'a> {
    pub outcome: CallOutcome,
    pub finished_at_ms: i64,
    pub cloud_record_id: Option<&'a str>,
    pub http_status: Option<u16>,
    pub cloud_reason: Option<&'a str>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

impl<'a> CallFinish<'a> {
    /// 没有云端答复的终态（不可达、超时、取消、中断）：没有云端 id，没有用量。
    pub fn without_answer(outcome: CallOutcome, finished_at_ms: i64) -> Self {
        Self {
            outcome,
            finished_at_ms,
            cloud_record_id: None,
            http_status: None,
            cloud_reason: None,
            tokens_in: None,
            tokens_out: None,
        }
    }
}

/// 表里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub seq: i64,
    pub local_request_id: String,
    pub attempt_no: u32,
    pub started_at_ms: i64,
    pub state: CallState,
    pub finished_at_ms: Option<i64>,
    pub cloud_record_id: Option<String>,
    pub http_status: Option<u16>,
    pub cloud_reason: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

/// 云端档的调用记录存储。与其它 store 共用同一个 SQLite 文件时互不干扰（各有各的版本表）。
pub struct CloudTierStore {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for CloudTierStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudTierStore { .. }")
    }
}

/// 标识符形状：1 到 128 个字符，只含字母数字和 `_ . : -`。云端 record id（UUID）和本机 record id 都满足。
fn is_identifier(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// 云端原因码形状：1 到 128 个字符，只含小写字母、数字、`_` 和 `,`（`X-iDoris-Reason` 可能是逗号连接的几个码）。
fn is_reason(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b','))
}

fn to_i64(value: u64, field: &'static str) -> Result<i64, CloudTierStoreError> {
    i64::try_from(value).map_err(|_| CloudTierStoreError::InvalidField(field))
}

impl CloudTierStore {
    /// 打开（必要时创建）数据库文件并应用迁移。连接不额外设置任何 PRAGMA（只有 5 秒的 busy_timeout，
    /// 和 rusqlite 的默认值相同）：个人形态和存量部署的连接设置一个字节都不变；
    /// 组织形态要用统一设置时走 [`CloudTierStore::new`]，传入 `open_standard` 打开的连接。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CloudTierStoreError> {
        let conn = Connection::open(path.as_ref())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::new(conn)
    }

    /// 内存库，测试用。
    pub fn open_in_memory() -> Result<Self, CloudTierStoreError> {
        Self::new(Connection::open_in_memory()?)
    }

    /// 在一条现成的连接上应用迁移。
    pub fn new(mut conn: Connection) -> Result<Self, CloudTierStoreError> {
        run_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 毒化的互斥锁只在上一个调用 panic 时出现；每个方法都是单个事务，要么提交要么随 drop 回滚。
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// **出网之前**调用：落一行 `pending`。同一个本机请求的同一次尝试只能开始一次。
    pub fn begin_call(
        &self,
        local_request_id: &str,
        attempt_no: u32,
        now_ms: i64,
    ) -> Result<CallTicket, CloudTierStoreError> {
        if !is_identifier(local_request_id) {
            return Err(CloudTierStoreError::InvalidField("local_request_id"));
        }
        if attempt_no == 0 {
            return Err(CloudTierStoreError::InvalidField("attempt_no"));
        }
        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT INTO cloud_tier_calls (local_request_id, attempt_no, started_at_ms, outcome)
             VALUES (?1, ?2, ?3, 'pending')",
            params![local_request_id, i64::from(attempt_no), now_ms],
        );
        match inserted {
            Ok(_) => Ok(CallTicket {
                seq: conn.last_insert_rowid(),
                local_request_id: local_request_id.to_string(),
                attempt_no,
            }),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(CloudTierStoreError::Duplicate {
                    local_request_id: local_request_id.to_string(),
                    attempt_no,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// 把一行从 `pending` 推进到终态，恰好一次。已经是同样的终态（重放）是空操作；
    /// 已经是不同的终态返回 [`CloudTierStoreError::AlreadyFinal`]，不覆盖。
    pub fn finish_call(
        &self,
        ticket: &CallTicket,
        finish: &CallFinish<'_>,
    ) -> Result<(), CloudTierStoreError> {
        if let Some(id) = finish.cloud_record_id
            && !is_identifier(id)
        {
            return Err(CloudTierStoreError::InvalidField("cloud_record_id"));
        }
        if let Some(reason) = finish.cloud_reason
            && !is_reason(reason)
        {
            return Err(CloudTierStoreError::InvalidField("cloud_reason"));
        }
        let tokens_in = finish
            .tokens_in
            .map(|value| to_i64(value, "tokens_in"))
            .transpose()?;
        let tokens_out = finish
            .tokens_out
            .map(|value| to_i64(value, "tokens_out"))
            .transpose()?;
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current =
            read_by_seq(&tx, ticket.seq)?.ok_or(CloudTierStoreError::NotFound(ticket.seq))?;
        if current.local_request_id != ticket.local_request_id
            || current.attempt_no != ticket.attempt_no
        {
            return Err(CloudTierStoreError::NotFound(ticket.seq));
        }
        if let CallState::Final(existing) = current.state {
            let same = existing == finish.outcome
                && current.cloud_record_id.as_deref() == finish.cloud_record_id
                && current.http_status == finish.http_status
                && current.cloud_reason.as_deref() == finish.cloud_reason
                && current.tokens_in == finish.tokens_in
                && current.tokens_out == finish.tokens_out;
            return if same {
                Ok(())
            } else {
                Err(CloudTierStoreError::AlreadyFinal(ticket.seq))
            };
        }
        let updated = tx.execute(
            "UPDATE cloud_tier_calls
                SET outcome = ?1, finished_at_ms = ?2, cloud_record_id = ?3, http_status = ?4,
                    cloud_reason = ?5, tokens_in = ?6, tokens_out = ?7
              WHERE seq = ?8",
            params![
                finish.outcome.as_str(),
                finish.finished_at_ms.max(current.started_at_ms),
                finish.cloud_record_id,
                finish.http_status.map(i64::from),
                finish.cloud_reason,
                tokens_in,
                tokens_out,
                ticket.seq,
            ],
        );
        match updated {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // 唯一索引（同一个云端 record id 已经对应别的本机请求）和 CHECK（比如 served 缺 id）都落在这里；
                // 前者是可以区分的，调用方据此把这次调用记成没有云端 id 的结局。
                return Err(match finish.cloud_record_id {
                    Some(id) if cloud_id_taken(&tx, id)? => {
                        CloudTierStoreError::CloudRecordConflict
                    }
                    _ => CloudTierStoreError::InvalidField("final outcome"),
                });
            }
            Err(error) => return Err(error.into()),
        }
        tx.commit()?;
        Ok(())
    }

    /// 启动时调用：上一个进程崩溃时留下的 `pending` 行没有人会再完成它们。标成 `interrupted`，返回行数。
    pub fn recover_interrupted(&self, now_ms: i64) -> Result<usize, CloudTierStoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE cloud_tier_calls
                SET outcome = 'interrupted', finished_at_ms = MAX(?1, started_at_ms)
              WHERE outcome = 'pending'",
            params![now_ms],
        )?;
        Ok(changed)
    }

    pub fn call_by_local_id(
        &self,
        local_request_id: &str,
        attempt_no: u32,
    ) -> Result<Option<CallRecord>, CloudTierStoreError> {
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM cloud_tier_calls WHERE local_request_id = ?1 AND attempt_no = ?2"
                ),
                params![local_request_id, i64::from(attempt_no)],
                read_row,
            )
            .optional()?;
        row.map(into_record).transpose()
    }

    pub fn call_by_cloud_id(
        &self,
        cloud_record_id: &str,
    ) -> Result<Option<CallRecord>, CloudTierStoreError> {
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM cloud_tier_calls WHERE cloud_record_id = ?1"),
                params![cloud_record_id],
                read_row,
            )
            .optional()?;
        row.map(into_record).transpose()
    }

    /// 按 `seq` 往后读：`seq` 严格大于 `after`，最多 `limit` 行（对账方的读法）。
    pub fn calls_after(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<CallRecord>, CloudTierStoreError> {
        let conn = self.lock();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM cloud_tier_calls WHERE seq > ?1 ORDER BY seq LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![after, limit], read_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(into_record(row?)?);
        }
        Ok(out)
    }

    pub fn call_count(&self) -> Result<u64, CloudTierStoreError> {
        let conn = self.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM cloud_tier_calls", [], |row| {
            row.get(0)
        })?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// 命名游标的当前位置；从未推进过是 0。
    pub fn cursor(&self, name: &str) -> Result<i64, CloudTierStoreError> {
        let conn = self.lock();
        let value: Option<i64> = conn
            .query_row(
                "SELECT last_seq FROM cloud_tier_cursor WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.unwrap_or(0))
    }

    /// 把命名游标推进到 `to_seq`（含）：**幂等**（已经在或超过这个位置是空操作，返回当前位置）；
    /// **只增不减**；**不能越过不存在的 seq，也不能越过还没有结局的调用**。
    pub fn advance_cursor(
        &self,
        name: &str,
        to_seq: i64,
        now_ms: i64,
    ) -> Result<i64, CloudTierStoreError> {
        if !is_identifier(name) || name.len() > 64 {
            return Err(CloudTierStoreError::InvalidField("cursor name"));
        }
        if to_seq < 0 {
            return Err(CloudTierStoreError::CursorRefused {
                to: to_seq,
                why: "negative position",
            });
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = tx
            .query_row(
                "SELECT last_seq FROM cloud_tier_cursor WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if to_seq <= current {
            return Ok(current);
        }
        let max_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM cloud_tier_calls",
            [],
            |row| row.get(0),
        )?;
        if to_seq > max_seq {
            return Err(CloudTierStoreError::CursorRefused {
                to: to_seq,
                why: "beyond the last recorded call",
            });
        }
        let unresolved: i64 = tx.query_row(
            "SELECT COUNT(*) FROM cloud_tier_calls WHERE seq <= ?1 AND outcome = 'pending'",
            params![to_seq],
            |row| row.get(0),
        )?;
        if unresolved > 0 {
            return Err(CloudTierStoreError::CursorRefused {
                to: to_seq,
                why: "a call before this position has no outcome yet",
            });
        }
        tx.execute(
            "INSERT INTO cloud_tier_cursor (name, last_seq, updated_at_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET last_seq = excluded.last_seq,
                                             updated_at_ms = excluded.updated_at_ms",
            params![name, to_seq, now_ms.max(0)],
        )?;
        tx.commit()?;
        Ok(to_seq)
    }
}

const COLUMNS: &str = "seq, local_request_id, attempt_no, started_at_ms, outcome, finished_at_ms, \
     cloud_record_id, http_status, cloud_reason, tokens_in, tokens_out";

type RawRow = (
    i64,
    String,
    i64,
    i64,
    String,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn into_record(raw: RawRow) -> Result<CallRecord, CloudTierStoreError> {
    let corrupt = |what: &str| CloudTierStoreError::Corrupt(what.to_string());
    let state = match raw.4.as_str() {
        "pending" => CallState::Pending,
        other => CallState::Final(
            CallOutcome::parse(other).ok_or_else(|| corrupt("unknown call outcome"))?,
        ),
    };
    let unsigned = |value: Option<i64>, what: &str| {
        value
            .map(|v| u64::try_from(v).map_err(|_| corrupt(what)))
            .transpose()
    };
    Ok(CallRecord {
        seq: raw.0,
        local_request_id: raw.1,
        attempt_no: u32::try_from(raw.2).map_err(|_| corrupt("attempt_no"))?,
        started_at_ms: raw.3,
        state,
        finished_at_ms: raw.5,
        cloud_record_id: raw.6,
        http_status: raw
            .7
            .map(|v| u16::try_from(v).map_err(|_| corrupt("http_status")))
            .transpose()?,
        cloud_reason: raw.8,
        tokens_in: unsigned(raw.9, "tokens_in")?,
        tokens_out: unsigned(raw.10, "tokens_out")?,
    })
}

fn read_by_seq(
    tx: &rusqlite::Transaction<'_>,
    seq: i64,
) -> Result<Option<CallRecord>, CloudTierStoreError> {
    let raw = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM cloud_tier_calls WHERE seq = ?1"),
            params![seq],
            read_row,
        )
        .optional()?;
    raw.map(into_record).transpose()
}

fn cloud_id_taken(
    tx: &rusqlite::Transaction<'_>,
    cloud_record_id: &str,
) -> Result<bool, CloudTierStoreError> {
    let found: Option<i64> = tx
        .query_row(
            "SELECT seq FROM cloud_tier_calls WHERE cloud_record_id = ?1",
            params![cloud_record_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn run_migrations(conn: &mut Connection) -> Result<(), CloudTierStoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS cloud_tier_schema_migrations (
            version       INTEGER PRIMARY KEY,
            applied_at_ms INTEGER NOT NULL
        )",
    )?;
    let applied: Option<i64> = tx
        .query_row(
            "SELECT version FROM cloud_tier_schema_migrations WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if applied.is_none() {
        tx.execute_batch(MIGRATION_0001)?;
        tx.execute(
            "INSERT INTO cloud_tier_schema_migrations (version, applied_at_ms)
             VALUES (1, CAST(strftime('%s','now') AS INTEGER) * 1000)",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

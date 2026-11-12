//! idoris.ai/api 计算服务 **S4：尝试记录与结算分录**（SQLite）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §6（请求状态机与失败切换）、§10（恢复后对账），
//! 以及 `docs/design/compute-service-ops.md` §7.1 的 `attempts` / `postings` 目标表。契约类型来自
//! `idoris-compute-contracts`（S0）；本模块**不重新实现**状态机或结算规则，只负责把它们持久化：
//!
//! - 每次尝试的状态变更**只能**走 S0 的 [`transition`]：[`AttemptStore::apply_events`] 在
//!   `BEGIN IMMEDIATE` 事务里读出当前状态、调用 `transition`、写回新状态并追加一行转移日志。
//!   非法转移返回 [`AttemptStoreError::Transition`]，库里什么都不变。
//! - 数据库本身再设一道防线（触发器，见 `migrations/0001_compute_attempts.sql`）：转移日志与结算分录只追加；
//!   尝试的身份与定价不可变；终态冻结；状态只能沿合法的边推进；供应商请求号写一次；
//!   **首字节发布之后不能再追加尝试**。触发器里的「边」与 S0 转移表的一致性由
//!   `tests::state_edges_match_the_contract_transition_table` 逐格核对。
//! - 结算：[`AttemptStore::finalize_request`] 接收 S0 [`settle_attempts`](idoris_compute_contracts::request::settle_attempts)
//!   算出的 [`SettlementPlan`]，在同一个事务里把可结算的尝试推进到 `settled`、追加分录、关闭请求。
//!   分录的幂等键是 `<尝试 id>|<类别>|settle`，同一份计划重放不会重复记账（金额不同则报
//!   [`AttemptStoreError::PostingConflict`]，不会悄悄覆盖）。
//!
//! # 表
//!
//! | 表 | 内容 |
//! |---|---|
//! | `compute_requests` | 一次用户请求：付款上下文、形态、计费性质、是否流式、发布记录、结局与结算摘要 |
//! | `compute_attempts` | 每次尝试一行：端点、价格版本、预留、状态（含 usage）、供应商请求号、失败标注 |
//! | `compute_attempt_events` | 每次合法转移一行，只追加 |
//! | `compute_postings` | 结算分录，只追加；`hold` 是按预留额挂账、待核销的成本（`maybe_executed` 的成本不得释放） |
//!
//! **这里没有任何正文**：不存 prompt、completion、工具参数，也不存上游返回的错误正文（设计稿 §7）。
//! 失败只记稳定的代码（`failure_kind`）和 HTTP 状态码。
//!
//! # 同步 API
//!
//! 与仓库里其它 SQLite store 一样是同步 API（`rusqlite`），内部一把 `Mutex<Connection>`。每个方法都是
//! 一个短事务，调用方可以在 `Drop` 里直接调用（客户端断开时的收尾）。

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use idoris_compute_contracts::attempt::{
    AttemptEvent, AttemptState, AttemptStateKind, SettleBasis, TransitionError, transition,
};
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::ids::{AttemptId, RequestId, SupplierRequestId, TimestampMs};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_compute_contracts::permit::{Billing, DataClass};
use idoris_compute_contracts::pricing::Usage;
use idoris_compute_contracts::request::{
    Attempt, AttemptRole, ChargeBasis, CostBasis, MAX_ATTEMPTS_PER_REQUEST, Request, SettlementPlan,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

#[cfg(test)]
mod tests;

const MIGRATION_0001: &str = include_str!("migrations/0001_compute_attempts.sql");

/// `usage_claim_json` 里上游原文的最大字节数（超出截断；只为对账时看一眼，不是账目）。
const MAX_USAGE_CLAIM_BYTES: usize = 512;

/// 尝试记录的错误。错误信息里没有任何正文或密钥。
#[derive(Debug, Error)]
pub enum AttemptStoreError {
    #[error("compute attempt store sqlite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("compute attempt store json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("request {0} not found")]
    RequestNotFound(RequestId),
    #[error("attempt {0} not found")]
    AttemptNotFound(AttemptId),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("invariant violated: {0}")]
    Invariant(&'static str),
    #[error("posting {key} already exists with a different amount")]
    PostingConflict { key: String },
    #[error("stored record is corrupt: {0}")]
    Corrupt(String),
}

/// 请求的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    /// 有尝试向客户端输出过第一个字节：用户为那一次付一次钱。
    Answered,
    /// 发出过尝试，但没有任何一次发布：用户付 0（比如全部失败、客户端在首字节前断开、输出没通过检查）。
    Unanswered,
    /// 没有任何尝试发出：准入或路由阶段就被拒了（比如没有合规候选）。
    Rejected,
}

impl RequestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestOutcome::Answered => "answered",
            RequestOutcome::Unanswered => "unanswered",
            RequestOutcome::Rejected => "rejected",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "answered" => Some(RequestOutcome::Answered),
            "unanswered" => Some(RequestOutcome::Unanswered),
            "rejected" => Some(RequestOutcome::Rejected),
            _ => None,
        }
    }
}

/// 关闭一个请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestClose<'a> {
    pub outcome: RequestOutcome,
    /// 稳定的原因字符串：S0 的原因码（`ReasonCode::as_str`，拒绝或降级时有），或路由层自己的稳定代码
    /// （比如 `upstream_unavailable`、`client_cancelled`）。
    pub reason: Option<&'a str>,
    pub at: TimestampMs,
}

/// 新请求的元数据。
#[derive(Debug, Clone)]
pub struct NewRequest<'a> {
    pub id: &'a RequestId,
    pub payment: &'a PaymentContext,
    pub form: DeploymentForm,
    pub billing: Billing,
    pub stream: bool,
    pub data_classes: &'a BTreeSet<DataClass>,
    pub created_at: TimestampMs,
}

/// 上游声称的 usage 以及我们对它的判定（写一次）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageClaim {
    /// `valid` / `invalid:<原因>` / `absent`。
    pub verdict: String,
    /// 上游给的 usage 对象原文（只含数字字段，调用方负责只放数字）。
    pub raw: Option<Value>,
}

/// 失败标注（不改变状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureNote {
    /// 稳定的失败代码，如 `connect_failed`、`http_status`、`stream_truncated`。
    pub kind: String,
    pub http_status: Option<u16>,
}

/// 记录供应商请求号的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplierIdOutcome {
    /// 新写入。
    Recorded,
    /// 已经记过同一个值（幂等，流里每个 chunk 都带同一个 id）。
    Unchanged,
    /// 新写入，但同一个供应商下别的尝试已经用过同一个请求号：两条记录没法一一对上供应商账单，已标注。
    DuplicateOf(AttemptId),
    /// 已经记过不同的值：保留第一个，没有覆盖，已标注。
    Conflict,
}

/// 分录类别。线上字符串固定，见迁移文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostingKind {
    UserPrice,
    PlatformCost,
    Hold,
    Release,
}

impl PostingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PostingKind::UserPrice => "user_price",
            PostingKind::PlatformCost => "platform_cost",
            PostingKind::Hold => "hold",
            PostingKind::Release => "release",
        }
    }
}

/// 分录的依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostingBasis {
    ActualUsage,
    ReservedAmount,
    ReservedPendingReconciliation,
    PriceReservation,
}

impl PostingBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            PostingBasis::ActualUsage => "actual_usage",
            PostingBasis::ReservedAmount => "reserved_amount",
            PostingBasis::ReservedPendingReconciliation => "reserved_pending_reconciliation",
            PostingBasis::PriceReservation => "price_reservation",
        }
    }
}

fn role_str(role: AttemptRole) -> &'static str {
    match role {
        AttemptRole::Answering => "answering",
        AttemptRole::Extra => "extra",
    }
}

/// 一条待写入的分录（还没有落库）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingDraft {
    pub attempt: AttemptId,
    pub kind: PostingKind,
    pub role: AttemptRole,
    pub basis: PostingBasis,
    pub amount: Money,
}

impl PostingDraft {
    /// 幂等键：同一次尝试同一类别只有一条结算分录。
    pub fn idempotency_key(&self) -> String {
        format!("{}|{}|settle", self.attempt, self.kind.as_str())
    }
}

/// 结算计划 → 分录（纯函数）。零金额不记。
///
/// - 发布了的那次：`user_price`（用户付的，一次）；
/// - 每次尝试的上游成本：按实际用量是 `platform_cost`；按预留额挂账待核销（`maybe_executed` / usage 未知）是 `hold`；
/// - 要从用户额度里释放的价预留：`release`。
pub fn postings_for_plan(plan: &SettlementPlan) -> Vec<PostingDraft> {
    let mut drafts = Vec::new();
    for line in &plan.lines {
        if let Some(charge) = &plan.user_charge
            && charge.attempt == line.attempt
            && !charge.charged.is_zero()
        {
            drafts.push(PostingDraft {
                attempt: line.attempt.clone(),
                kind: PostingKind::UserPrice,
                role: line.role,
                basis: match charge.basis {
                    ChargeBasis::ActualUsage => PostingBasis::ActualUsage,
                    ChargeBasis::ReservedAmount => PostingBasis::ReservedAmount,
                },
                amount: charge.charged.money().clone(),
            });
        }
        if !line.cost.is_zero() {
            let classified = match line.cost_basis {
                CostBasis::ActualUsage => {
                    Some((PostingKind::PlatformCost, PostingBasis::ActualUsage))
                }
                CostBasis::ReservedPendingReconciliation => Some((
                    PostingKind::Hold,
                    PostingBasis::ReservedPendingReconciliation,
                )),
                // 没有产生成本的尝试成本一定是 0，上面已经跳过。
                CostBasis::NotIncurred => None,
            };
            if let Some((kind, basis)) = classified {
                drafts.push(PostingDraft {
                    attempt: line.attempt.clone(),
                    kind,
                    role: line.role,
                    basis,
                    amount: line.cost.money().clone(),
                });
            }
        }
        if !line.price_reservation_released.is_zero() {
            drafts.push(PostingDraft {
                attempt: line.attempt.clone(),
                kind: PostingKind::Release,
                role: line.role,
                basis: PostingBasis::PriceReservation,
                amount: line.price_reservation_released.money().clone(),
            });
        }
    }
    drafts
}

/// 一次尝试的读视图（对账、测试、运维看）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRow {
    pub attempt: AttemptId,
    pub request: RequestId,
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub price_version: String,
    pub effective_max_tokens: u64,
    pub reserved_price: Money,
    pub reserved_cost: Money,
    pub state: AttemptState,
    pub pays_user: bool,
    pub supplier_request_id: Option<String>,
    pub supplier_id_anomaly: Option<String>,
    pub usage_claim: Option<Value>,
    pub failure_kind: Option<String>,
    pub failure_http_status: Option<u16>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl AttemptRow {
    pub fn state_kind(&self) -> AttemptStateKind {
        self.state.kind()
    }

    /// 已知的 usage（`executed_usage_known`，或按实际用量 `settled`）。
    pub fn usage(&self) -> Option<Usage> {
        match &self.state {
            AttemptState::ExecutedUsageKnown { usage } => Some(*usage),
            AttemptState::Settled {
                basis: SettleBasis::ActualUsage(usage),
            } => Some(*usage),
            _ => None,
        }
    }
}

/// 一个请求的读视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub request: RequestId,
    pub payer_account: String,
    pub credential: String,
    pub price_unit: String,
    pub form: String,
    pub billing: String,
    pub stream: bool,
    pub data_classes: String,
    pub created_at: TimestampMs,
    pub published_attempt: Option<u8>,
    pub published_at: Option<TimestampMs>,
    pub outcome: Option<RequestOutcome>,
    pub reason_code: Option<String>,
    pub closed_at: Option<TimestampMs>,
    pub charged_micros: Option<i64>,
    pub charged_basis: Option<String>,
    pub reserved_micros: Option<i64>,
    pub overage_micros: Option<i64>,
}

/// 一条分录的读视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingRow {
    pub request: RequestId,
    pub attempt: AttemptId,
    pub kind: String,
    pub role: String,
    pub basis: String,
    pub money: Money,
    pub idempotency_key: String,
}

/// 一条转移日志的读视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub attempt: AttemptId,
    pub from: AttemptStateKind,
    pub event: AttemptEvent,
    pub to: AttemptStateKind,
    pub at: TimestampMs,
}

/// 收尾的结果（给测试和日志看）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeReport {
    /// 这次调用新追加的分录数（重放时为 0）。
    pub postings_added: usize,
    /// 这次调用推进到 `settled` 的尝试。
    pub settled: Vec<AttemptId>,
    /// 请求此前就已关闭、这次是幂等重放。
    pub replayed: bool,
}

/// 尝试记录存储。
pub struct AttemptStore {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for AttemptStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttemptStore { .. }")
    }
}

impl AttemptStore {
    /// 打开（必要时创建）数据库文件并应用迁移。与其它 store 共用同一个 SQLite 文件时互不干扰
    /// （各有各的版本表）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AttemptStoreError> {
        let mut conn = Connection::open(path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::from_connection(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 内存库，测试用。
    pub fn open_in_memory() -> Result<Self, AttemptStoreError> {
        let mut conn = Connection::open_in_memory()?;
        Self::from_connection(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn from_connection(conn: &mut Connection) -> Result<(), AttemptStoreError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        run_migrations(conn)?;
        Ok(())
    }

    /// 毒化的互斥锁只在上一个调用 panic 时出现；每个方法都是单个事务，要么提交要么随 drop 回滚，
    /// 库不会被写坏（同 `BudgetLedger::lock`）。
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -----------------------------------------------------------------------
    // 写
    // -----------------------------------------------------------------------

    /// 请求被准入：写请求行和第一次尝试（`admitted`）。同一个请求 id 只能创建一次。
    pub fn create_request(
        &self,
        new: &NewRequest<'_>,
        first: &Attempt,
    ) -> Result<(), AttemptStoreError> {
        if first.id().request() != new.id || first.index().0 != 0 {
            return Err(AttemptStoreError::Invariant(
                "the first attempt must be index 0 of the new request",
            ));
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_request(&tx, new, None)?;
        insert_attempt(&tx, first, new.created_at)?;
        tx.commit()?;
        Ok(())
    }

    /// 请求在没有任何尝试发出之前就被拒绝了（准入或路由阶段，比如没有合规候选）：只写一行已关闭的请求，
    /// 留下审计痕迹。没有尝试，所以也没有分录。
    pub fn record_rejected_request(
        &self,
        new: &NewRequest<'_>,
        reason: &str,
    ) -> Result<(), AttemptStoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_request(
            &tx,
            new,
            Some(RequestClose {
                outcome: RequestOutcome::Rejected,
                reason: Some(reason),
                at: new.created_at,
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 追加下一次尝试（切换候选）。库里再核对一遍：序号连续、总数不超过上限、上一次尝试已不在
    /// `admitted`/`sending`；**发布边界与已关闭**由触发器兜底。
    pub fn append_attempt(
        &self,
        request: &RequestId,
        attempt: &Attempt,
        at: TimestampMs,
    ) -> Result<(), AttemptStoreError> {
        if attempt.id().request() != request {
            return Err(AttemptStoreError::Invariant(
                "the attempt does not belong to this request",
            ));
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_request_exists(&tx, request)?;
        let (count, last_state): (i64, Option<String>) = tx.query_row(
            "SELECT COUNT(*),
                    (SELECT state FROM compute_attempts WHERE request_id = ?1
                     ORDER BY attempt_index DESC LIMIT 1)
             FROM compute_attempts WHERE request_id = ?1",
            [request.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count == 0 || i64::from(attempt.index().0) != count {
            return Err(AttemptStoreError::Invariant(
                "attempt indexes must be consecutive",
            ));
        }
        if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_ATTEMPTS_PER_REQUEST {
            return Err(AttemptStoreError::Invariant(
                "too many attempts for one request",
            ));
        }
        if matches!(last_state.as_deref(), Some("admitted" | "sending")) {
            return Err(AttemptStoreError::Invariant(
                "the previous attempt is still in flight",
            ));
        }
        insert_attempt(&tx, attempt, at)?;
        tx.commit()?;
        Ok(())
    }

    /// 对一次尝试依次应用事件（一个事务）：每个事件都走 S0 的 [`transition`]，成功就追加一行转移日志，
    /// 最后写回新状态。任何一步非法都整体回滚。返回最终状态。
    ///
    /// 「先记状态再写字节」靠它：调用方在把请求写到网络之前先 `apply_events([Dispatch, WireWriteStarted])`，
    /// 提交成功才发。
    pub fn apply_events(
        &self,
        attempt: &AttemptId,
        events: &[AttemptEvent],
        at: TimestampMs,
    ) -> Result<AttemptState, AttemptStoreError> {
        if events.is_empty() {
            return Err(AttemptStoreError::Invariant("no events to apply"));
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = apply_events_in(&tx, attempt, events, at)?;
        tx.commit()?;
        Ok(state)
    }

    /// 记下供应商给这次调用的请求号。规则与 S0 `Request::record_supplier_request_id` 一致：
    /// 上游还没有任何东西回来（`admitted` / `sending`）不能记；同值幂等；不同值不覆盖（返回
    /// [`SupplierIdOutcome::Conflict`] 并标注）；别的尝试已经用过同一个请求号则标注
    /// [`SupplierIdOutcome::DuplicateOf`]（两条都保留，不拒绝写入：丢掉审计痕迹比重复更糟）。
    pub fn record_supplier_request_id(
        &self,
        attempt: &AttemptId,
        id: &SupplierRequestId,
    ) -> Result<SupplierIdOutcome, AttemptStoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, Option<String>, String)> = tx
            .query_row(
                "SELECT state, supplier_request_id, provider FROM compute_attempts WHERE attempt_id = ?1",
                [attempt.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((state, existing, provider)) = row else {
            return Err(AttemptStoreError::AttemptNotFound(attempt.clone()));
        };
        if matches!(state.as_str(), "admitted" | "sending") {
            return Err(AttemptStoreError::Invariant(
                "nothing has come back from the supplier yet",
            ));
        }
        let outcome = match existing {
            Some(current) if current == id.as_str() => SupplierIdOutcome::Unchanged,
            Some(_) => {
                tx.execute(
                    "UPDATE compute_attempts SET supplier_id_anomaly = COALESCE(supplier_id_anomaly, 'id_changed_midstream')
                     WHERE attempt_id = ?1",
                    [attempt.to_string()],
                )?;
                SupplierIdOutcome::Conflict
            }
            None => {
                let other: Option<String> = tx
                    .query_row(
                        "SELECT attempt_id FROM compute_attempts
                         WHERE provider = ?1 AND supplier_request_id = ?2 AND attempt_id <> ?3
                         ORDER BY created_at_ms, attempt_id LIMIT 1",
                        params![provider, id.as_str(), attempt.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "UPDATE compute_attempts SET supplier_request_id = ?2 WHERE attempt_id = ?1",
                    params![attempt.to_string(), id.as_str()],
                )?;
                match other {
                    Some(other) => {
                        let other: AttemptId = other.parse().map_err(|_| {
                            AttemptStoreError::Corrupt("stored attempt id is malformed".into())
                        })?;
                        tx.execute(
                            "UPDATE compute_attempts SET supplier_id_anomaly = COALESCE(supplier_id_anomaly, ?2)
                             WHERE attempt_id = ?1",
                            params![attempt.to_string(), format!("duplicate_of:{other}")],
                        )?;
                        SupplierIdOutcome::DuplicateOf(other)
                    }
                    None => SupplierIdOutcome::Recorded,
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// 上游给的请求号格式不对（超长、含不允许的字符）：没法作为 [`SupplierRequestId`] 记下，只留一个标注，
    /// 让对账的人知道这次调用的请求号需要人工对。
    pub fn note_supplier_id_invalid(&self, attempt: &AttemptId) -> Result<(), AttemptStoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE compute_attempts SET supplier_id_anomaly = COALESCE(supplier_id_anomaly, 'invalid_format')
             WHERE attempt_id = ?1",
            [attempt.to_string()],
        )?;
        Ok(())
    }

    /// 记下上游声称的 usage 与判定（写一次，已有就保持不变）。返回这次是否写入了。
    pub fn record_usage_claim(
        &self,
        attempt: &AttemptId,
        claim: &UsageClaim,
    ) -> Result<bool, AttemptStoreError> {
        let mut doc = json!({ "verdict": claim.verdict });
        if let Some(raw) = &claim.raw {
            let text = raw.to_string();
            doc["raw"] = if text.len() <= MAX_USAGE_CLAIM_BYTES {
                raw.clone()
            } else {
                Value::String(truncate_utf8(&text, MAX_USAGE_CLAIM_BYTES))
            };
        }
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE compute_attempts SET usage_claim_json = ?2 WHERE attempt_id = ?1 AND usage_claim_json IS NULL",
            params![attempt.to_string(), doc.to_string()],
        )?;
        Ok(changed == 1)
    }

    /// 记下失败标注（写一次，已有就保持不变）。不改变状态。
    pub fn record_failure(
        &self,
        attempt: &AttemptId,
        note: &FailureNote,
    ) -> Result<bool, AttemptStoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE compute_attempts SET failure_kind = ?2, failure_http_status = ?3
             WHERE attempt_id = ?1 AND failure_kind IS NULL",
            params![
                attempt.to_string(),
                note.kind,
                note.http_status.map(i64::from)
            ],
        )?;
        Ok(changed == 1)
    }

    /// 记下「已向客户端输出第一个字节」：该请求的发布记录写一次，该尝试 `pays_user` 置 1。
    ///
    /// 要求：尝试是最新一次、状态是有上游字节的状态（`maybe_executed` / `executed_*`）、请求还没有发布过也没关闭。
    /// 与 S0 `Request::publish_first_byte` 的前置条件一致。
    pub fn record_publish(
        &self,
        attempt: &AttemptId,
        at: TimestampMs,
    ) -> Result<(), AttemptStoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT state, attempt_index FROM compute_attempts WHERE attempt_id = ?1",
                [attempt.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((state, index)) = row else {
            return Err(AttemptStoreError::AttemptNotFound(attempt.clone()));
        };
        let latest: i64 = tx.query_row(
            "SELECT MAX(attempt_index) FROM compute_attempts WHERE request_id = ?1",
            [attempt.request().as_str()],
            |row| row.get(0),
        )?;
        if index != latest {
            return Err(AttemptStoreError::Invariant(
                "only the latest attempt may publish",
            ));
        }
        if !matches!(
            state.as_str(),
            "maybe_executed" | "executed_usage_known" | "executed_usage_unknown"
        ) {
            return Err(AttemptStoreError::Invariant(
                "an attempt without upstream bytes cannot publish",
            ));
        }
        let changed = tx.execute(
            "UPDATE compute_requests SET published_attempt = ?2, published_at_ms = ?3
             WHERE request_id = ?1 AND published_attempt IS NULL AND outcome IS NULL",
            params![attempt.request().as_str(), index, at.millis()],
        )?;
        if changed != 1 {
            return Err(AttemptStoreError::Invariant(
                "the request is already published or closed",
            ));
        }
        tx.execute(
            "UPDATE compute_attempts SET pays_user = 1, updated_at_ms = ?2 WHERE attempt_id = ?1",
            params![attempt.to_string(), at.millis()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// **收尾**：一个事务里把 `settle` 里的尝试推进到 `settled`（S0 `Settle` 事件，经 `transition`）、
    /// 追加 [`postings_for_plan`] 算出的分录、写结算摘要并关闭请求。
    ///
    /// - `plan` 来自 S0 `Request::plan_settlement`（纯函数），这里不重新算钱；
    /// - 幂等：请求已经关闭时，只核对分录，不再推进、不再追加；金额对不上返回 `PostingConflict`；
    /// - `maybe_executed` 的尝试不会出现在 `settle` 里（没有事件能结算它）：它的成本以 `hold` 分录挂账，
    ///   状态保持 `maybe_executed`，等对账。
    pub fn finalize_request(
        &self,
        plan: &SettlementPlan,
        settle: &[AttemptId],
        close: RequestClose<'_>,
    ) -> Result<FinalizeReport, AttemptStoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let request = &plan.request;
        let published: Option<i64> = tx
            .query_row(
                "SELECT published_attempt FROM compute_requests WHERE request_id = ?1",
                [request.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| AttemptStoreError::RequestNotFound(request.clone()))?;
        let already_closed: Option<String> = tx.query_row(
            "SELECT outcome FROM compute_requests WHERE request_id = ?1",
            [request.as_str()],
            |row| row.get(0),
        )?;
        let replayed = already_closed.is_some();
        if let Some(stored) = already_closed.as_deref()
            && stored != close.outcome.as_str()
        {
            return Err(AttemptStoreError::Invariant(
                "the request was already closed with a different outcome",
            ));
        }

        // 计划与库里的发布记录必须一致：发布了的尝试才有 user_charge。
        match (&plan.user_charge, published) {
            (Some(charge), Some(index)) if i64::from(charge.attempt.index().0) == index => {}
            (None, None) => {}
            _ => {
                return Err(AttemptStoreError::Invariant(
                    "the settlement plan disagrees with the recorded publish",
                ));
            }
        }
        let expected_outcome = if published.is_some() {
            RequestOutcome::Answered
        } else {
            RequestOutcome::Unanswered
        };
        if close.outcome != expected_outcome {
            return Err(AttemptStoreError::Invariant(
                "the close outcome disagrees with the recorded publish",
            ));
        }

        let mut settled = Vec::new();
        if !replayed {
            for attempt in settle {
                let state = load_state(&tx, attempt)?;
                if state.kind() == AttemptStateKind::Settled {
                    continue;
                }
                apply_events_in(&tx, attempt, &[AttemptEvent::Settle], close.at)?;
                settled.push(attempt.clone());
            }
        }

        let mut postings_added = 0;
        for draft in postings_for_plan(plan) {
            let key = draft.idempotency_key();
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT amount_micros FROM compute_postings WHERE idempotency_key = ?1",
                    [&key],
                    |row| row.get(0),
                )
                .optional()?;
            match existing {
                Some(amount) if amount == draft.amount.micros => {}
                Some(_) => return Err(AttemptStoreError::PostingConflict { key }),
                None => {
                    tx.execute(
                        "INSERT INTO compute_postings
                           (request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            request.as_str(),
                            draft.attempt.to_string(),
                            draft.kind.as_str(),
                            role_str(draft.role),
                            draft.basis.as_str(),
                            draft.amount.unit.to_string(),
                            draft.amount.micros,
                            key,
                            close.at.millis(),
                        ],
                    )?;
                    postings_added += 1;
                }
            }
        }

        if !replayed {
            let (charged, basis, reserved, overage) = match &plan.user_charge {
                Some(charge) => (
                    Some(charge.charged.micros()),
                    Some(match charge.basis {
                        ChargeBasis::ActualUsage => "actual_usage",
                        ChargeBasis::ReservedAmount => "reserved_amount",
                    }),
                    Some(charge.reserved.micros()),
                    Some(charge.overage.micros()),
                ),
                None => (None, None, None, None),
            };
            tx.execute(
                "UPDATE compute_requests
                 SET outcome = ?2, reason_code = ?3, closed_at_ms = ?4,
                     charged_micros = ?5, charged_basis = ?6, reserved_micros = ?7, overage_micros = ?8
                 WHERE request_id = ?1",
                params![
                    request.as_str(),
                    close.outcome.as_str(),
                    close.reason,
                    close.at.millis(),
                    charged,
                    basis,
                    reserved,
                    overage,
                ],
            )?;
        }
        tx.commit()?;
        Ok(FinalizeReport {
            postings_added,
            settled,
            replayed,
        })
    }

    // -----------------------------------------------------------------------
    // 读
    // -----------------------------------------------------------------------

    /// 把库里的请求重建成 S0 的 [`Request`]（经 serde 重新校验全部结构不变式）。恢复后对账和测试用。
    pub fn load_request(&self, id: &RequestId) -> Result<Option<Request>, AttemptStoreError> {
        let conn = self.lock();
        let head: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT payment_json, published_attempt FROM compute_requests WHERE request_id = ?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((payment_json, published)) = head else {
            return Ok(None);
        };
        let mut stmt = conn.prepare(
            "SELECT snapshot_json, state_json, supplier_request_id
             FROM compute_attempts WHERE request_id = ?1 ORDER BY attempt_index",
        )?;
        let rows = stmt.query_map([id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut attempts = Vec::new();
        for row in rows {
            let (snapshot, state, supplier) = row?;
            let mut attempt: Value = serde_json::from_str(&snapshot)?;
            attempt["state"] = serde_json::from_str(&state)?;
            if let Some(supplier) = supplier {
                attempt["supplier_request_id"] = Value::String(supplier);
            }
            attempts.push(attempt);
        }
        let mut doc = json!({
            "id": id.as_str(),
            "payment": serde_json::from_str::<Value>(&payment_json)?,
            "attempts": attempts,
        });
        if let Some(index) = published {
            doc["published"] = Value::from(index);
        }
        let request: Request = serde_json::from_value(doc)
            .map_err(|e| AttemptStoreError::Corrupt(format!("cannot rebuild the request: {e}")))?;
        Ok(Some(request))
    }

    pub fn request_row(&self, id: &RequestId) -> Result<Option<RequestRow>, AttemptStoreError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT request_id, payer_account, credential_id, price_unit, form, billing, stream,
                    data_classes, created_at_ms, published_attempt, published_at_ms, outcome,
                    reason_code, closed_at_ms, charged_micros, charged_basis, reserved_micros, overage_micros
             FROM compute_requests WHERE request_id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<i64>>(16)?,
                    row.get::<_, Option<i64>>(17)?,
                ))
            },
        )
        .optional()?
        .map(|r| {
            Ok(RequestRow {
                request: RequestId::new(r.0).map_err(corrupt)?,
                payer_account: r.1,
                credential: r.2,
                price_unit: r.3,
                form: r.4,
                billing: r.5,
                stream: r.6 == 1,
                data_classes: r.7,
                created_at: TimestampMs(r.8),
                published_attempt: r.9.and_then(|v| u8::try_from(v).ok()),
                published_at: r.10.map(TimestampMs),
                outcome: r.11.as_deref().and_then(RequestOutcome::parse),
                reason_code: r.12,
                closed_at: r.13.map(TimestampMs),
                charged_micros: r.14,
                charged_basis: r.15,
                reserved_micros: r.16,
                overage_micros: r.17,
            })
        })
        .transpose()
    }

    /// 一个请求的全部尝试，按序号。
    pub fn attempt_rows(&self, request: &RequestId) -> Result<Vec<AttemptRow>, AttemptStoreError> {
        self.query_attempts(
            "WHERE request_id = ?1 ORDER BY attempt_index",
            &[request.as_str()],
        )
    }

    /// 处在这些状态里的尝试（对账扫描用）：最多 `limit` 行，按创建时间。
    pub fn attempts_in_states(
        &self,
        states: &[AttemptStateKind],
        limit: usize,
    ) -> Result<Vec<AttemptRow>, AttemptStoreError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let list = states
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.query_attempts(
            &format!("WHERE state IN ({list}) ORDER BY created_at_ms, attempt_id LIMIT {limit}"),
            &[],
        )
    }

    /// 同一个供应商下用同一个请求号的所有尝试（对账：账单上有一个请求号，找本地对应的尝试）。
    pub fn attempts_by_supplier_request_id(
        &self,
        provider: &str,
        supplier_request_id: &str,
    ) -> Result<Vec<AttemptRow>, AttemptStoreError> {
        self.query_attempts(
            "WHERE provider = ?1 AND supplier_request_id = ?2 ORDER BY created_at_ms, attempt_id",
            &[provider, supplier_request_id],
        )
    }

    fn query_attempts(
        &self,
        tail: &str,
        args: &[&str],
    ) -> Result<Vec<AttemptRow>, AttemptStoreError> {
        let conn = self.lock();
        let sql = format!(
            "SELECT attempt_id, request_id, provider, model, endpoint, price_version,
                    effective_max_tokens, reserved_price_micros, reserved_price_unit,
                    reserved_cost_micros, reserved_cost_unit, state_json, pays_user,
                    supplier_request_id, supplier_id_anomaly, usage_claim_json,
                    failure_kind, failure_http_status, created_at_ms, updated_at_ms
             FROM compute_attempts {tail}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok(RawAttempt {
                attempt_id: row.get(0)?,
                request_id: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                endpoint: row.get(4)?,
                price_version: row.get(5)?,
                effective_max_tokens: row.get(6)?,
                reserved_price_micros: row.get(7)?,
                reserved_price_unit: row.get(8)?,
                reserved_cost_micros: row.get(9)?,
                reserved_cost_unit: row.get(10)?,
                state_json: row.get(11)?,
                pays_user: row.get(12)?,
                supplier_request_id: row.get(13)?,
                supplier_id_anomaly: row.get(14)?,
                usage_claim_json: row.get(15)?,
                failure_kind: row.get(16)?,
                failure_http_status: row.get(17)?,
                created_at_ms: row.get(18)?,
                updated_at_ms: row.get(19)?,
            })
        })?;
        let mut out = Vec::new();
        for raw in rows {
            out.push(raw?.into_row()?);
        }
        Ok(out)
    }

    /// 还没有关闭的请求（崩溃恢复扫描用）：创建时间早于 `created_before`，最多 `limit` 个，按创建时间。
    ///
    /// 进程崩溃时正在进行的请求会留在这里：尝试停在 `maybe_executed` / `executed_*`，没有结算。
    /// 恢复时用 [`AttemptStore::load_request`] 重建、`Request::plan_settlement` 算计划、
    /// [`AttemptStore::finalize_request`] 收尾（幂等）；`maybe_executed` 的成本保持挂账等对账。
    pub fn open_requests(
        &self,
        created_before: TimestampMs,
        limit: usize,
    ) -> Result<Vec<RequestId>, AttemptStoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM compute_requests
             WHERE outcome IS NULL AND created_at_ms < ?1
             ORDER BY created_at_ms, request_id LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![
                created_before.millis(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(RequestId::new(row?).map_err(corrupt)?);
        }
        Ok(out)
    }

    /// 一个请求的全部分录，按写入顺序。
    pub fn postings(&self, request: &RequestId) -> Result<Vec<PostingRow>, AttemptStoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key
             FROM compute_postings WHERE request_id = ?1 ORDER BY posting_id",
        )?;
        let rows = stmt.query_map([request.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (request, attempt, kind, role, basis, unit, micros, key) = row?;
            out.push(PostingRow {
                request: RequestId::new(request).map_err(corrupt)?,
                attempt: attempt.parse().map_err(corrupt)?,
                kind,
                role,
                basis,
                money: Money::new(unit.parse().map_err(corrupt)?, micros),
                idempotency_key: key,
            });
        }
        Ok(out)
    }

    /// 一次尝试的转移日志，按顺序。
    pub fn events(&self, attempt: &AttemptId) -> Result<Vec<EventRow>, AttemptStoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT from_state, event_json, to_state, at_ms FROM compute_attempt_events
             WHERE attempt_id = ?1 ORDER BY event_seq",
        )?;
        let rows = stmt.query_map([attempt.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (from, event, to, at) = row?;
            out.push(EventRow {
                attempt: attempt.clone(),
                from: parse_kind(&from)?,
                event: serde_json::from_str(&event)?,
                to: parse_kind(&to)?,
                at: TimestampMs(at),
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// 内部实现
// ---------------------------------------------------------------------------

fn corrupt(error: impl std::fmt::Display) -> AttemptStoreError {
    AttemptStoreError::Corrupt(error.to_string())
}

fn run_migrations(conn: &mut Connection) -> Result<(), AttemptStoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS compute_attempt_schema_migrations (
            version       INTEGER PRIMARY KEY,
            applied_at_ms INTEGER NOT NULL
        )",
    )?;
    let applied: Option<i64> = tx
        .query_row(
            "SELECT version FROM compute_attempt_schema_migrations WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if applied.is_none() {
        tx.execute_batch(MIGRATION_0001)?;
        tx.execute(
            "INSERT INTO compute_attempt_schema_migrations (version, applied_at_ms)
             VALUES (1, CAST(strftime('%s','now') AS INTEGER) * 1000)",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn wire_text<T: Serialize>(value: &T) -> Result<String, AttemptStoreError> {
    match serde_json::to_value(value)? {
        Value::String(text) => Ok(text),
        other => Ok(other.to_string()),
    }
}

fn data_classes_text(classes: &BTreeSet<DataClass>) -> Result<String, AttemptStoreError> {
    let mut parts = Vec::new();
    for class in classes {
        parts.push(wire_text(class)?);
    }
    Ok(parts.join(","))
}

fn insert_request(
    tx: &rusqlite::Transaction<'_>,
    new: &NewRequest<'_>,
    close: Option<RequestClose<'_>>,
) -> Result<(), AttemptStoreError> {
    let payment = serde_json::to_string(new.payment)?;
    tx.execute(
        "INSERT INTO compute_requests
           (request_id, payment_json, payer_account, credential_id, price_unit, form, billing,
            stream, data_classes, created_at_ms, outcome, reason_code, closed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            new.id.as_str(),
            payment,
            new.payment.payer().as_str(),
            new.payment.credential().as_str(),
            new.payment.unit().to_string(),
            wire_text(&new.form)?,
            wire_text(&new.billing)?,
            i64::from(new.stream),
            data_classes_text(new.data_classes)?,
            new.created_at.millis(),
            close.map(|c| c.outcome.as_str()),
            close.and_then(|c| c.reason),
            close.map(|c| c.at.millis()),
        ],
    )?;
    Ok(())
}

fn insert_attempt(
    tx: &rusqlite::Transaction<'_>,
    attempt: &Attempt,
    at: TimestampMs,
) -> Result<(), AttemptStoreError> {
    if attempt.state() != &AttemptState::Admitted || attempt.supplier_request_id().is_some() {
        return Err(AttemptStoreError::Invariant(
            "a new attempt must start in the admitted state",
        ));
    }
    let reservation = attempt.reservation();
    let endpoint = attempt.endpoint();
    let max_tokens = i64::try_from(attempt.effective_max_tokens())
        .map_err(|_| AttemptStoreError::Invariant("max_tokens does not fit an integer column"))?;
    tx.execute(
        "INSERT INTO compute_attempts
           (attempt_id, request_id, attempt_index, provider, model, endpoint, price_version,
            effective_max_tokens, reserved_price_micros, reserved_price_unit, reserved_cost_micros,
            reserved_cost_unit, snapshot_json, state, state_json, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
        params![
            attempt.id().to_string(),
            attempt.id().request().as_str(),
            i64::from(attempt.index().0),
            endpoint.provider.as_str(),
            endpoint.model.as_str(),
            endpoint.endpoint.as_str(),
            attempt.price_version().as_str(),
            max_tokens,
            reservation.price.micros(),
            reservation.price.unit().to_string(),
            reservation.cost.micros(),
            reservation.cost.unit().to_string(),
            serde_json::to_string(attempt)?,
            attempt.state().kind().as_str(),
            serde_json::to_string(attempt.state())?,
            at.millis(),
        ],
    )?;
    Ok(())
}

fn ensure_request_exists(
    tx: &rusqlite::Transaction<'_>,
    request: &RequestId,
) -> Result<(), AttemptStoreError> {
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM compute_requests WHERE request_id = ?1",
            [request.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    exists
        .map(|_| ())
        .ok_or_else(|| AttemptStoreError::RequestNotFound(request.clone()))
}

fn load_state(
    tx: &rusqlite::Transaction<'_>,
    attempt: &AttemptId,
) -> Result<AttemptState, AttemptStoreError> {
    let state_json: Option<String> = tx
        .query_row(
            "SELECT state_json FROM compute_attempts WHERE attempt_id = ?1",
            [attempt.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let state_json =
        state_json.ok_or_else(|| AttemptStoreError::AttemptNotFound(attempt.clone()))?;
    Ok(serde_json::from_str(&state_json)?)
}

/// 在已开的事务里依次应用事件；**状态变更的唯一入口**，每一步都走 S0 的 [`transition`]。
///
/// 每个事件各写一次状态：数据库的状态边触发器逐边核对，批量应用不能跳过中间状态。
fn apply_events_in(
    tx: &rusqlite::Transaction<'_>,
    attempt: &AttemptId,
    events: &[AttemptEvent],
    at: TimestampMs,
) -> Result<AttemptState, AttemptStoreError> {
    let mut state = load_state(tx, attempt)?;
    for event in events {
        let next = transition(&state, event)?;
        tx.execute(
            "INSERT INTO compute_attempt_events (attempt_id, from_state, event_json, to_state, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                attempt.to_string(),
                state.kind().as_str(),
                serde_json::to_string(event)?,
                next.kind().as_str(),
                at.millis(),
            ],
        )?;
        tx.execute(
            "UPDATE compute_attempts SET state = ?2, state_json = ?3, updated_at_ms = ?4 WHERE attempt_id = ?1",
            params![
                attempt.to_string(),
                next.kind().as_str(),
                serde_json::to_string(&next)?,
                at.millis(),
            ],
        )?;
        state = next;
    }
    Ok(state)
}

fn parse_kind(text: &str) -> Result<AttemptStateKind, AttemptStoreError> {
    AttemptStateKind::ALL
        .into_iter()
        .find(|kind| kind.as_str() == text)
        .ok_or_else(|| AttemptStoreError::Corrupt(format!("unknown attempt state {text:?}")))
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

struct RawAttempt {
    attempt_id: String,
    request_id: String,
    provider: String,
    model: String,
    endpoint: String,
    price_version: String,
    effective_max_tokens: i64,
    reserved_price_micros: i64,
    reserved_price_unit: String,
    reserved_cost_micros: i64,
    reserved_cost_unit: String,
    state_json: String,
    pays_user: i64,
    supplier_request_id: Option<String>,
    supplier_id_anomaly: Option<String>,
    usage_claim_json: Option<String>,
    failure_kind: Option<String>,
    failure_http_status: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl RawAttempt {
    fn into_row(self) -> Result<AttemptRow, AttemptStoreError> {
        let unit =
            |text: &str| -> Result<Unit, AttemptStoreError> { text.parse().map_err(corrupt) };
        Ok(AttemptRow {
            attempt: self.attempt_id.parse().map_err(corrupt)?,
            request: RequestId::new(self.request_id).map_err(corrupt)?,
            provider: self.provider,
            model: self.model,
            endpoint: self.endpoint,
            price_version: self.price_version,
            effective_max_tokens: u64::try_from(self.effective_max_tokens).map_err(corrupt)?,
            reserved_price: Money::new(
                unit(&self.reserved_price_unit)?,
                self.reserved_price_micros,
            ),
            reserved_cost: Money::new(unit(&self.reserved_cost_unit)?, self.reserved_cost_micros),
            state: serde_json::from_str(&self.state_json)?,
            pays_user: self.pays_user == 1,
            supplier_request_id: self.supplier_request_id,
            supplier_id_anomaly: self.supplier_id_anomaly,
            usage_claim: self
                .usage_claim_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            failure_kind: self.failure_kind,
            failure_http_status: self.failure_http_status.and_then(|v| u16::try_from(v).ok()),
            created_at: TimestampMs(self.created_at_ms),
            updated_at: TimestampMs(self.updated_at_ms),
        })
    }
}

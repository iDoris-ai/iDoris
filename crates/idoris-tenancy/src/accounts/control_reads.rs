//! 控制面的只读查询（设计稿 §9：「查询（只读，范围内）」）。
//!
//! 每个查询都先认证调用者（和写操作同一套：凭证必须此刻可用），再对**被查询的账户**做 `read` 授权
//! （`idoris-compute-contracts::control::authorize`，目标账户必须在调用者某条 `read` 权限的子树里）。
//! 范围外和不存在是同一个 `out_of_scope`。
//!
//! 读操作**不写审计**（设计稿只要求写操作有审计；读审计会让每次轮询都写库）。被拒的读也不留行。
//!
//! 返回里永远没有密钥、验证器哈希、配对码、配对码哈希；设备的显示名只在 `read_credentials` 里给有 `read` 权限的人看。

use idoris_compute_contracts::account::{Account, Membership, Principal};
use idoris_compute_contracts::control::ControlReason;
use idoris_compute_contracts::credential::{ControlOp, Credential};
use idoris_compute_contracts::ids::{AccountId, CredentialId, PrincipalId, TimestampMs};
use idoris_compute_contracts::money::{Money, Unit};
use rusqlite::{Connection, OptionalExtension};

use super::control::membership_target;
use super::control::{CallerInfo, Failure, Who, authorize_op};
use super::error::AccountStoreError;
use super::rows::{
    AllocationRecord, RawAccount, RawAllocation, RawMembership, load_account, load_credential,
    load_memberships_of, load_principal,
};
use super::store::AccountStore;

/// 单次查询最多返回多少行。超过了就截断并告诉调用方（`truncated`），不返回半截的、看起来完整的结果。
pub const MAX_READ_ROWS: usize = 500;

/// 一个账户和它的直接子账户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    pub account: Account,
    pub children: Vec<AccountId>,
}

/// 账户的子树（含自己），按深度、id 排序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtree {
    pub accounts: Vec<Account>,
    pub truncated: bool,
}

/// 流入、流出一个账户的额度（含已撤销的，带撤销时间，调用方必须自己看）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationsView {
    pub inbound: Vec<AllocationRecord>,
    pub outbound: Vec<AllocationRecord>,
    pub truncated: bool,
}

/// 配对出来的设备的信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub pairing_id: String,
    pub label: String,
    pub paired_at_ms: i64,
}

/// 凭证的元数据视图：永远没有密钥，也没有验证器哈希。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialView {
    pub credential: Credential,
    /// 谁签发的：（种类，id）。
    pub issued_by: Option<(String, String)>,
    pub device: Option<DeviceView>,
    /// 数据面验证器（`virtual_keys` 那一行）的状态：`active` / `revoked` / `missing`。凭证行的状态是控制面
    /// 管的；运维用 `idoris key revoke` 只撤销验证器的话，凭证行仍是 `active`，但这张凭证已经不可用，
    /// 所以两个状态都要看。
    pub key_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialsView {
    pub credentials: Vec<CredentialView>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipsView {
    pub memberships: Vec<Membership>,
    pub truncated: bool,
}

/// 一行审计记录（只读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub audit_id: i64,
    pub at_ms: i64,
    pub request_id: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub op: String,
    pub target_kind: String,
    pub target_id: Option<String>,
    pub scope_account: Option<String>,
    pub related_id: Option<String>,
    pub amount: Option<Money>,
    pub outcome: String,
    pub reason_code: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditView {
    pub rows: Vec<AuditRow>,
    /// 还有更早的记录：用最后一行的 `audit_id` 作为下一次的 `before`。
    pub has_more: bool,
}

fn read_scope(
    conn: &Connection,
    caller: &super::control::Caller,
    now: TimestampMs,
    target: &AccountId,
) -> Result<(), Failure> {
    authorize_op(conn, caller, now, ControlOp::Read, target, None).map(|_| ())
}

/// 把存储层的错误当作内部错误（读路径里没有「调用者的请求不合法」这一类）。
fn internal(error: impl Into<AccountStoreError>) -> Failure {
    Failure::from(error.into())
}

impl AccountStore {
    /// 调用者自己：凭证元数据和数据面上限。只需要凭证此刻可用，不需要任何管理权限。
    pub fn control_whoami(&self, who: &Who<'_>, now: TimestampMs) -> Result<CallerInfo, Failure> {
        self.control_read(who, now, |_conn, caller| {
            match (caller.credential(), caller.ceilings()) {
                (Some(credential), Some(ceilings)) => Ok(CallerInfo {
                    credential: credential.clone(),
                    ceilings: ceilings.clone(),
                }),
                _ => Err(Failure::new(ControlReason::NotAControlCredential)),
            }
        })
    }

    pub fn control_read_account(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        id: &AccountId,
    ) -> Result<AccountView, Failure> {
        self.control_read(who, now, |conn, caller| {
            read_scope(conn, caller, now, id)?;
            let account = load_account(conn, id)
                .map_err(internal)?
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            let mut stmt = conn
                .prepare("SELECT account_id FROM accounts WHERE parent_id = ?1 ORDER BY account_id")
                .map_err(internal)?;
            let children = stmt
                .query_map([id.as_str()], |row| row.get::<_, String>(0))
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?
                .into_iter()
                .map(AccountId::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(AccountStoreError::from)?;
            Ok(AccountView { account, children })
        })
    }

    pub fn control_read_subtree(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        root: &AccountId,
    ) -> Result<Subtree, Failure> {
        self.control_read(who, now, |conn, caller| {
            read_scope(conn, caller, now, root)?;
            let mut stmt = conn
                .prepare(
                    "WITH RECURSIVE sub (id, depth) AS ( \
                         SELECT account_id, 0 FROM accounts WHERE account_id = ?1 \
                         UNION ALL \
                         SELECT a.account_id, sub.depth + 1 FROM accounts a JOIN sub ON a.parent_id = sub.id \
                          WHERE sub.depth < 8) \
                     SELECT a.account_id, a.kind, a.parent_id, a.unit, a.status \
                       FROM accounts a JOIN sub ON a.account_id = sub.id \
                      ORDER BY sub.depth, a.account_id LIMIT ?2",
                )
                .map_err(internal)?;
            let limit = i64::try_from(MAX_READ_ROWS + 1).unwrap_or(i64::MAX);
            let raws = stmt
                .query_map(rusqlite::params![root.as_str(), limit], RawAccount::read)
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?;
            let truncated = raws.len() > MAX_READ_ROWS;
            let accounts = raws
                .into_iter()
                .take(MAX_READ_ROWS)
                .map(RawAccount::into_account)
                .collect::<Result<Vec<_>, _>>()
                .map_err(Failure::from)?;
            Ok(Subtree { accounts, truncated })
        })
    }

    pub fn control_read_allocations(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        account: &AccountId,
    ) -> Result<AllocationsView, Failure> {
        self.control_read(who, now, |conn, caller| {
            read_scope(conn, caller, now, account)?;
            let (inbound, in_cut) = allocation_records(conn, "child_id", account)?;
            let (outbound, out_cut) = allocation_records(conn, "parent_id", account)?;
            Ok(AllocationsView {
                inbound,
                outbound,
                truncated: in_cut || out_cut,
            })
        })
    }

    /// 绑定在 `account` 上的凭证（`Service` / `Node` 绑账户；`Member` 绑成员关系的付款账户）。
    pub fn control_read_credentials(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        account: &AccountId,
    ) -> Result<CredentialsView, Failure> {
        self.control_read(who, now, |conn, caller| {
            read_scope(conn, caller, now, account)?;
            let mut stmt = conn
                .prepare(
                    "SELECT credential_id FROM credentials WHERE account_id = ?1 \
                     UNION \
                     SELECT c.credential_id FROM credentials c \
                       JOIN memberships m ON m.membership_id = c.membership_id \
                      WHERE m.account_id = ?1 \
                     ORDER BY 1 LIMIT ?2",
                )
                .map_err(internal)?;
            let limit = i64::try_from(MAX_READ_ROWS + 1).unwrap_or(i64::MAX);
            let ids = stmt
                .query_map(rusqlite::params![account.as_str(), limit], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?;
            let truncated = ids.len() > MAX_READ_ROWS;
            let mut credentials = Vec::new();
            for id in ids.into_iter().take(MAX_READ_ROWS) {
                let id = CredentialId::new(id).map_err(AccountStoreError::from)?;
                let credential = load_credential(conn, &id)
                    .map_err(internal)?
                    .ok_or_else(|| Failure::internal("credential vanished inside a read"))?;
                let issued_by: Option<(String, String)> = conn
                    .query_row(
                        "SELECT issued_by_kind, issued_by_id FROM credential_issuance WHERE credential_id = ?1",
                        [id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                let device: Option<DeviceView> = conn
                    .query_row(
                        "SELECT pairing_id, label, paired_at_ms FROM pairing_devices WHERE credential_id = ?1",
                        [id.as_str()],
                        |row| {
                            Ok(DeviceView {
                                pairing_id: row.get(0)?,
                                label: row.get(1)?,
                                paired_at_ms: row.get(2)?,
                            })
                        },
                    )
                    .optional()
                    .map_err(internal)?;
                let key_status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM virtual_keys WHERE key_id = ?1",
                        [id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(internal)?;
                credentials.push(CredentialView {
                    credential,
                    issued_by,
                    device,
                    key_status: key_status.unwrap_or_else(|| "missing".to_string()),
                });
            }
            Ok(CredentialsView {
                credentials,
                truncated,
            })
        })
    }

    /// 付款账户是 `account` 或所属组织是 `account` 的成员关系。
    pub fn control_read_memberships(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        account: &AccountId,
    ) -> Result<MembershipsView, Failure> {
        self.control_read(who, now, |conn, caller| {
            read_scope(conn, caller, now, account)?;
            let mut stmt = conn
                .prepare(
                    "SELECT membership_id, principal_id, org_id, role, account_id, status FROM memberships \
                      WHERE account_id = ?1 OR org_id = ?1 ORDER BY membership_id LIMIT ?2",
                )
                .map_err(internal)?;
            let limit = i64::try_from(MAX_READ_ROWS + 1).unwrap_or(i64::MAX);
            let raws = stmt
                .query_map(rusqlite::params![account.as_str(), limit], RawMembership::read)
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?;
            let truncated = raws.len() > MAX_READ_ROWS;
            let memberships = raws
                .into_iter()
                .take(MAX_READ_ROWS)
                .map(RawMembership::into_membership)
                .collect::<Result<Vec<_>, _>>()
                .map_err(Failure::from)?;
            Ok(MembershipsView {
                memberships,
                truncated,
            })
        })
    }

    /// 主体的状态。主体不属于任何子树，所以范围是：调用者对它登记时的组织、或它任何一个成员关系的账户有 `read` 权限。
    pub fn control_read_principal(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        id: &PrincipalId,
    ) -> Result<Principal, Failure> {
        self.control_read(who, now, |conn, caller| {
            let principal = load_principal(conn, id)
                .map_err(internal)?
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            let mut targets: Vec<AccountId> = Vec::new();
            let registrant: Option<String> = conn
                .query_row(
                    "SELECT org_id FROM principal_registrations WHERE principal_id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            if let Some(org) = registrant {
                targets.push(AccountId::new(org).map_err(AccountStoreError::from)?);
            }
            for membership in load_memberships_of(conn, id).map_err(internal)? {
                targets.push(membership_target(&membership).clone());
            }
            if caller.is_operator() {
                return Ok(principal);
            }
            let mut last = Failure::new(ControlReason::OutOfScope);
            for target in &targets {
                match read_scope(conn, caller, now, target) {
                    Ok(()) => return Ok(principal),
                    Err(failure) => last = failure,
                }
            }
            // 范围外、不存在、没有任何可判定的目标：同一个答案。凭证状态类的失败（账户被冻结）原样报。
            Err(
                if last.reason.is_denial() && last.reason != ControlReason::OutOfScope {
                    last
                } else {
                    Failure::new(ControlReason::OutOfScope)
                },
            )
        })
    }

    /// 审计记录：`scope_account` 在 `account` 的子树里的行，新的在前。`before` 是上一页最后一行的 `audit_id`。
    pub fn control_read_audit(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        account: &AccountId,
        limit: usize,
        before: Option<i64>,
    ) -> Result<AuditView, Failure> {
        let limit = limit.clamp(1, MAX_READ_ROWS);
        self.control_read(who, now, |conn, caller| {
            read_scope(conn, caller, now, account)?;
            let mut stmt = conn
                .prepare(
                    "WITH RECURSIVE sub (id, depth) AS ( \
                         SELECT account_id, 0 FROM accounts WHERE account_id = ?1 \
                         UNION ALL \
                         SELECT a.account_id, sub.depth + 1 FROM accounts a JOIN sub ON a.parent_id = sub.id \
                          WHERE sub.depth < 8) \
                     SELECT audit_id, at_ms, request_id, actor_kind, actor_id, op, target_kind, target_id, \
                            scope_account, related_id, amount_unit, amount_micros, outcome, reason_code \
                       FROM control_audit \
                      WHERE scope_account IN (SELECT id FROM sub) AND (?2 IS NULL OR audit_id < ?2) \
                      ORDER BY audit_id DESC LIMIT ?3",
                )
                .map_err(internal)?;
            let fetch = i64::try_from(limit + 1).unwrap_or(i64::MAX);
            let raws = stmt
                .query_map(rusqlite::params![account.as_str(), before, fetch], |row| {
                    Ok((
                        AuditRowRaw {
                            audit_id: row.get(0)?,
                            at_ms: row.get(1)?,
                            request_id: row.get(2)?,
                            actor_kind: row.get(3)?,
                            actor_id: row.get(4)?,
                            op: row.get(5)?,
                            target_kind: row.get(6)?,
                            target_id: row.get(7)?,
                            scope_account: row.get(8)?,
                            related_id: row.get(9)?,
                            outcome: row.get(12)?,
                            reason_code: row.get(13)?,
                        },
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                    ))
                })
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?;
            let has_more = raws.len() > limit;
            let mut rows = Vec::new();
            for (raw, unit, micros) in raws.into_iter().take(limit) {
                let amount = match (unit, micros) {
                    (Some(unit), Some(micros)) => Some(Money::new(
                        unit.parse::<Unit>()
                            .map_err(|error| Failure::internal(error.to_string()))?,
                        micros,
                    )),
                    _ => None,
                };
                rows.push(AuditRow {
                    audit_id: raw.audit_id,
                    at_ms: raw.at_ms,
                    request_id: raw.request_id,
                    actor_kind: raw.actor_kind,
                    actor_id: raw.actor_id,
                    op: raw.op,
                    target_kind: raw.target_kind,
                    target_id: raw.target_id,
                    scope_account: raw.scope_account,
                    related_id: raw.related_id,
                    amount,
                    outcome: raw.outcome,
                    reason_code: raw.reason_code,
                });
            }
            Ok(AuditView { rows, has_more })
        })
    }
}

struct AuditRowRaw {
    audit_id: i64,
    at_ms: i64,
    request_id: String,
    actor_kind: String,
    actor_id: String,
    op: String,
    target_kind: String,
    target_id: Option<String>,
    scope_account: Option<String>,
    related_id: Option<String>,
    outcome: String,
    reason_code: String,
}

/// `column`（`parent_id` 或 `child_id`，调用方写死）等于 `account` 的额度，含已撤销的。
fn allocation_records(
    conn: &Connection,
    column: &'static str,
    account: &AccountId,
) -> Result<(Vec<AllocationRecord>, bool), Failure> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {cols}, {revoked} FROM allocations WHERE {column} = ?1 \
              ORDER BY allocation_id LIMIT ?2",
            cols = super::rows::ALLOCATION_COLUMNS,
            revoked = super::rows::REVOKED_AT
        ))
        .map_err(internal)?;
    let limit = i64::try_from(MAX_READ_ROWS + 1).unwrap_or(i64::MAX);
    let raws = stmt
        .query_map(rusqlite::params![account.as_str(), limit], |row| {
            Ok((RawAllocation::read(row)?, row.get::<_, Option<i64>>(9)?))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let truncated = raws.len() > MAX_READ_ROWS;
    let records = raws
        .into_iter()
        .take(MAX_READ_ROWS)
        .map(|(raw, revoked_at_ms)| {
            Ok(AllocationRecord {
                allocation: raw.into_allocation()?,
                revoked_at_ms,
            })
        })
        .collect::<Result<Vec<_>, AccountStoreError>>()?;
    Ok((records, truncated))
}

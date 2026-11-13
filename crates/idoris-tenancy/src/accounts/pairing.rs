//! 云账号配对（设计稿 §3：「云账号绑定走『登录 / 配对』流程。解绑、撤销、多设备行为都在 S2 里定义」）的
//! 存储层，以及数据面认证用的 [`AccountStore::credential_gate`]。**只做云端这一侧**：本地 iDoris 怎么拿着码来兑换、
//! 怎么存凭证，是 S7 的事。
//!
//! # 流程（状态图见交付说明）
//!
//! ```text
//!   已登录的云账号（由有 issue_credential 权限的凭证代为操作，比如登录服务）
//!        │ create_pairing_code(成员关系, 有效期, 数据面上限)
//!        ▼
//!     pending ──────────── now >= expires_at ───────────▶ expired（不改行，读的时候按时间判）
//!        │  │
//!        │  └─ revoke_pairing_code ──▶ revoked（终态）
//!        │ redeem(码, 设备名)：码有效、未过期、签发者此刻仍有资格、成员关系 / 主体 / 账户仍有效、设备数没满
//!        ▼
//!     redeemed（终态）＋ 一张绑定到该成员关系付款账户的设备凭证（Member 绑定，只有数据面权限）
//!        │
//!        ├─ 设备自己 unpair / 有权限的凭证 revoke_credential ──▶ 凭证 revoked（终态）：解绑
//!        └─ 成员关系 / 主体被撤销、账户被冻结或关闭 ──▶ 所有设备凭证在数据面立即被拒（不改凭证行）
//! ```
//!
//! # 规则
//!
//! - 码**只存 SHA-256**，一次性、短时（默认 10 分钟，30 秒到 15 分钟之间）、可撤销。兑换是一个
//!   `BEGIN IMMEDIATE` 事务：并发兑换同一个码，只有一个事务看到 `pending`，其余看到 `redeemed`
//!   （数据库层另有「一个码最多一台设备」的唯一索引兜底）。
//! - **每台设备一张凭证**，各自可撤销；一个设备被解绑不影响同一账号的其它设备。同一个成员关系最多
//!   [`MAX_ACTIVE_DEVICES_PER_MEMBERSHIP`] 台有效设备（设计稿没有给数，这是 S2 的取值，见交付说明）。
//! - 设备凭证不能超过签发者：数据面上限（隐私等级、角色、租户）在**创建码**时就按签发者校验并记在码上，
//!   有效期不晚于签发者；兑换时**重新验证签发者还有资格**（凭证没被撤销 / 冻结 / 过期，仍有覆盖这个成员关系
//!   付款账户的 `issue_credential` 权限，绑定账户链仍是 Active）：签发者的权限被收回后，它发出去还没兑换的码一并作废。
//! - 码、密钥、设备显示名都不进审计和日志。

use idoris_compute_contracts::account::{MembershipRole, MembershipStatus, PrincipalStatus};
use idoris_compute_contracts::control::{ControlReason, WriteResult, authorize};
use idoris_compute_contracts::credential::resolve_payment_context;
use idoris_compute_contracts::credential::{
    ControlOp, Credential, CredentialBinding, CredentialStatus,
};
use idoris_compute_contracts::ids::{AccountId, CredentialId, MembershipId, TimestampMs};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_contracts::common::PrivacyClass;
use rusqlite::{Connection, OptionalExtension};
use subtle::ConstantTimeEq;

use super::control::{
    Actor, AuditSpec, Caller, Ctx, Failure, Identity, Meta, NewKey, TargetKind, Who, Written,
    authorize_op, check_ceilings, insert_issuance, insert_virtual_key, load_key_by_id,
    revoke_virtual_key, slice_directory,
};
use super::error::AccountStoreError;
use super::rows::{load_ancestors, load_credential, load_membership, load_principal};
use super::store::{AccountStore, create_credential_in, directory_slice, set_credential_status_in};
use crate::virtual_key::VirtualKeyHash;

/// 配对码的有效期范围和默认值（毫秒）。
pub const MIN_PAIRING_TTL_MS: i64 = 30 * 1000;
pub const DEFAULT_PAIRING_TTL_MS: i64 = 10 * 60 * 1000;
pub const MAX_PAIRING_TTL_MS: i64 = 15 * 60 * 1000;

/// 同一个成员关系（云账号）最多同时有几台有效设备。设计稿没有给数；取 10：够一个人的电脑、笔记本、家里和办公室的
/// 机器，又不至于让一个被盗的登录服务凭证无限制地给账号挂设备。
///
/// 「有效」见 [`occupied_device_slots`]：配对出来的设备和直接签发的成员凭证合起来数；已撤销、已过期、验证器已撤销
/// 或过期的不占名额，冻结的占着（解冻之后还能用，不能因此超限）。
pub const MAX_ACTIVE_DEVICES_PER_MEMBERSHIP: i64 = 10;

/// 设备显示名的最大长度（字符）。
pub const MAX_DEVICE_LABEL_CHARS: usize = 64;

/// 配对码的状态。`expired` 不是库里的状态：`pending` 且 `now >= expires_at` 就是过期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStatus {
    Pending,
    Redeemed,
    Revoked,
}

impl PairingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PairingStatus::Pending => "pending",
            PairingStatus::Redeemed => "redeemed",
            PairingStatus::Revoked => "revoked",
        }
    }

    fn parse(text: &str) -> Result<Self, AccountStoreError> {
        match text {
            "pending" => Ok(PairingStatus::Pending),
            "redeemed" => Ok(PairingStatus::Redeemed),
            "revoked" => Ok(PairingStatus::Revoked),
            other => Err(AccountStoreError::Corrupt(format!(
                "unknown pairing status {other:?}"
            ))),
        }
    }
}

/// 库里的一行配对码（不含码的哈希）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRecord {
    pub pairing_id: String,
    pub membership: MembershipId,
    pub issued_by: CredentialId,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub status: PairingStatus,
    pub redeemed_credential: Option<CredentialId>,
    pub device_privacy: Vec<PrivacyClass>,
    pub device_roles: Vec<String>,
    pub device_tenant_id: Option<String>,
    pub device_ttl_ms: Option<i64>,
}

/// 认出来的配对码（`Who::PairingCode`）：整行记录。
pub(super) type PairingRow = PairingRecord;

const PAIRING_COLUMNS: &str = "pairing_id, membership_id, issued_by, created_at_ms, expires_at_ms, \
     status, redeemed_credential_id, device_privacy, device_roles, device_tenant_id, device_ttl_ms, code_hash";

fn read_pairing(row: &rusqlite::Row<'_>) -> rusqlite::Result<(PairingRecord, Vec<u8>)> {
    // 列的解析错误（JSON、id 字符集）统一在 `into_record` 里报 `Corrupt`；这里只搬运原始值。
    let raw = (
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, i64>(3)?,
        row.get::<_, i64>(4)?,
        row.get::<_, String>(5)?,
        row.get::<_, Option<String>>(6)?,
        row.get::<_, String>(7)?,
        row.get::<_, String>(8)?,
        row.get::<_, Option<String>>(9)?,
        row.get::<_, Option<i64>>(10)?,
        row.get::<_, Vec<u8>>(11)?,
    );
    Ok((
        PairingRecord {
            pairing_id: raw.0,
            membership: MembershipId::new(raw.1).map_err(to_sql_error)?,
            issued_by: CredentialId::new(raw.2).map_err(to_sql_error)?,
            created_at_ms: raw.3,
            expires_at_ms: raw.4,
            status: PairingStatus::parse(&raw.5).map_err(to_sql_error)?,
            redeemed_credential: raw
                .6
                .map(CredentialId::new)
                .transpose()
                .map_err(to_sql_error)?,
            device_privacy: serde_json::from_str(&raw.7).map_err(to_sql_error)?,
            device_roles: serde_json::from_str(&raw.8).map_err(to_sql_error)?,
            device_tenant_id: raw.9,
            device_ttl_ms: raw.10,
        },
        raw.11,
    ))
}

fn to_sql_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

pub(super) fn load_pairing_by_id(
    conn: &Connection,
    pairing_id: &str,
) -> Result<Option<PairingRecord>, AccountStoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {PAIRING_COLUMNS} FROM pairing_codes WHERE pairing_id = ?1"),
            [pairing_id],
            read_pairing,
        )
        .optional()?
        .map(|(record, _hash)| record))
}

/// 按码的哈希找。找到之后再用常数时间比较一次哈希（数据库按索引找已经只会命中相等的行；这一步是纵深防御，
/// 也让「比较密钥一律常数时间」不依赖数据库的比较实现）。
fn load_pairing_by_hash(
    conn: &Connection,
    code_hash: &[u8; 32],
) -> Result<Option<PairingRecord>, AccountStoreError> {
    let found = conn
        .query_row(
            &format!("SELECT {PAIRING_COLUMNS} FROM pairing_codes WHERE code_hash = ?1"),
            [&code_hash[..]],
            read_pairing,
        )
        .optional()?;
    Ok(found
        .and_then(|(record, stored)| bool::from(stored.ct_eq(&code_hash[..])).then_some(record)))
}

/// 把配对码认成调用者（`Who::PairingCode`）。认不出（格式对但查无此码）不审计；认出来之后码已用、已撤销、
/// 已过期，都有审计（actor = 配对记录 id）。
pub(super) fn resolve_pairing_caller(
    conn: &Connection,
    code_hash: &[u8; 32],
    now: TimestampMs,
) -> Result<Caller, (Option<Actor>, Failure)> {
    let row = match load_pairing_by_hash(conn, code_hash) {
        Ok(Some(row)) => row,
        Ok(None) => return Err((None, Failure::new(ControlReason::PairingCodeInvalid))),
        Err(error) => return Err((None, Failure::from(error))),
    };
    let actor = Actor::PairingCode(row.pairing_id.clone());
    let deny = |reason: ControlReason| Err((Some(actor.clone()), Failure::new(reason)));
    match row.status {
        PairingStatus::Redeemed => return deny(ControlReason::PairingCodeUsed),
        PairingStatus::Revoked => return deny(ControlReason::PairingCodeRevoked),
        PairingStatus::Pending => {}
    }
    // 半开区间 [created, expires)：刚好等于 expires 的那一毫秒已经过期。
    if now.0 >= row.expires_at_ms {
        return deny(ControlReason::PairingCodeExpired);
    }
    Ok(Caller {
        actor,
        identity: Identity::PairingCode(Box::new(row)),
    })
}

// ================================================================================================
// 创建、撤销配对码
// ================================================================================================

/// 创建配对码。码和 `pairing_id` 由调用方生成（HTTP 层）：存储层只拿到哈希。
#[derive(Debug, Clone)]
pub struct CreatePairingCode {
    pub pairing_id: String,
    pub code_hash: [u8; 32],
    pub membership: MembershipId,
    pub ttl_ms: i64,
    pub allowed_privacy: Vec<PrivacyClass>,
    pub allowed_roles: Vec<String>,
    /// `None` = 继承签发者的租户绑定。
    pub tenant_id: Option<String>,
    /// 兑换出来的设备凭证的寿命（从兑换时算）；`None` = 不设（仍不会晚于签发者）。
    pub device_ttl_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokePairingCode {
    pub pairing_id: String,
}

/// 兑换配对码。设备凭证的密钥由调用方（HTTP 层）生成，存储层只拿到 id 和哈希。
#[derive(Debug, Clone)]
pub struct RedeemPairingCode {
    pub code_hash: [u8; 32],
    pub key_id: String,
    pub key_hash: VirtualKeyHash,
    /// 设备显示名（用户自己填的文本）：只进 `pairing_devices`，不进审计和日志。
    pub label: String,
}

/// 兑换的结果：设备凭证（元数据，不含密钥）和它属于的成员关系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub credential: Credential,
    pub membership: MembershipId,
    pub pairing_id: String,
}

/// 这个成员关系此刻占着名额的设备凭证数：绑定到它的成员凭证（配对出来的设备，和直接签发的成员凭证）里，
/// **还可能再被用上的**那些。
///
/// * 凭证已撤销、已过期（`now >= expires_at`），或者数据面验证器（`virtual_keys`）已撤销、已过期（比如运维用
///   `idoris key revoke` 只撤销了验证器）：永远不可用了，不占名额——否则累计配对 10 张短期凭证、全部过期之后，
///   账号就卡在 `device_limit_reached`，旧设备过期了又没法自己解绑，只能管理员逐张清理（Codex r1 #9）；
/// * **冻结的占着名额**：冻结可以解冻，解冻之后它又能用，不能因为冻结期间空出过名额而让解冻之后超限；
/// * 没有验证器行的凭证（存储层直接建的）按有效算，宁可多占不少占。
pub(super) fn occupied_device_slots(
    conn: &Connection,
    membership: &MembershipId,
    now: TimestampMs,
) -> Result<i64, Failure> {
    Ok(conn.query_row(
        "SELECT count(*) FROM credentials c
           LEFT JOIN virtual_keys k ON k.key_id = c.credential_id
          WHERE c.binding_kind = 'member' AND c.membership_id = ?1
            AND c.status IN ('active', 'frozen')
            AND (c.expires_at_ms IS NULL OR c.expires_at_ms > ?2)
            AND (k.key_id IS NULL
                 OR (k.status = 'active' AND (k.expires_at_ms IS NULL OR k.expires_at_ms > ?2)))",
        rusqlite::params![membership.as_str(), now.0],
        |row| row.get(0),
    )?)
}

/// 付款账户链上第一个不是 Active 的账户。
fn first_inactive(conn: &Connection, account: &AccountId) -> Result<bool, Failure> {
    let chain = load_ancestors(conn, account)?;
    if chain.is_empty() {
        return Err(Failure::new(ControlReason::OutOfScope));
    }
    Ok(chain
        .iter()
        .any(|found| found.status != idoris_compute_contracts::account::AccountStatus::Active))
}

/// 成员关系必须能付款（成员或担保人）、没被撤销，主体没被撤销，付款账户链是 Active。返回付款账户。
fn require_live_payer(
    conn: &Connection,
    membership: &idoris_compute_contracts::account::Membership,
) -> Result<AccountId, Failure> {
    let account = match (membership.role, &membership.account) {
        (MembershipRole::Member | MembershipRole::Sponsor, Some(account)) => account.clone(),
        _ => {
            return Err(Failure::with_detail(
                ControlReason::InvalidMembership,
                "only member and sponsor memberships have a payment account to pair a device to",
            ));
        }
    };
    if membership.status == MembershipStatus::Revoked {
        return Err(Failure::new(ControlReason::MembershipRevoked));
    }
    if load_principal(conn, &membership.principal)?
        .is_some_and(|principal| principal.status == PrincipalStatus::Revoked)
    {
        return Err(Failure::new(ControlReason::PrincipalRevoked));
    }
    if first_inactive(conn, &account)? {
        return Err(Failure::new(ControlReason::AccountInactive));
    }
    Ok(account)
}

impl AccountStore {
    /// 创建配对码。授权看成员关系的付款账户（`issue_credential`：配对码是「延迟签发」的一张设备凭证）；
    /// 本机运维不能创建配对码（码要记下是哪张凭证签发的，兑换时重新验证它）。
    pub fn control_create_pairing_code(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &CreatePairingCode,
    ) -> Result<Written<PairingRecord>, Failure> {
        let spec = AuditSpec::new("create_pairing_code", TargetKind::PairingCode)
            .target(&request.pairing_id)
            .related(&request.membership);
        self.control_write(who, meta, spec, |conn, caller, spec| {
            let (Some(issuer), Some(ceilings)) = (caller.credential(), caller.ceilings()) else {
                return Err(Failure::new(ControlReason::OpNotGranted));
            };
            let Some(found) = load_membership(conn, &request.membership)? else {
                return Err(Failure::new(ControlReason::OutOfScope));
            };
            let account = found
                .account
                .clone()
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            spec.scope_account = Some(account.to_string());
            authorize_op(conn, caller, meta.now, ControlOp::IssueCredential, &account, None)?;
            let payer = require_live_payer(conn, &found)?;
            debug_assert_eq!(payer, account);
            if !(MIN_PAIRING_TTL_MS..=MAX_PAIRING_TTL_MS).contains(&request.ttl_ms) {
                return Err(Failure::with_detail(
                    ControlReason::InvalidRequest,
                    format!(
                        "ttl must be between {MIN_PAIRING_TTL_MS} and {MAX_PAIRING_TTL_MS} milliseconds"
                    ),
                ));
            }
            if request.device_ttl_ms.is_some_and(|ttl| ttl <= 0) {
                return Err(Failure::with_detail(
                    ControlReason::InvalidRequest,
                    "the device credential lifetime must be positive",
                ));
            }
            // 数据面上限不超过签发者（和直接签发凭证同一条规则）。
            let tenant = request.tenant_id.clone().or_else(|| ceilings.tenant_id.clone());
            check_ceilings(ceilings, &request.allowed_privacy, &request.allowed_roles, &tenant)?;
            if request.allowed_privacy.is_empty() {
                return Err(Failure::with_detail(
                    ControlReason::InvalidRequest,
                    "at least one privacy class is required",
                ));
            }
            let expires_at = meta
                .now
                .checked_add_ms(request.ttl_ms)
                .ok_or_else(|| Failure::new(ControlReason::InvalidRequest))?;
            // 码本身也不能比签发者活得久（签发者过期后它发出去的码没有意义）。
            if let Some(cap) = issuer.expires_at()
                && expires_at > cap
            {
                return Err(Failure::new(ControlReason::ExpiryExceedsGrantor));
            }
            let privacy_json = serde_json::to_string(&request.allowed_privacy)
                .map_err(|error| Failure::internal(error.to_string()))?;
            let roles_json = serde_json::to_string(&request.allowed_roles)
                .map_err(|error| Failure::internal(error.to_string()))?;
            conn.execute(
                "INSERT INTO pairing_codes \
                    (pairing_id, code_hash, membership_id, issued_by, created_at_ms, expires_at_ms, status, \
                     device_privacy, device_roles, device_tenant_id, device_ttl_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    request.pairing_id,
                    &request.code_hash[..],
                    request.membership.as_str(),
                    issuer.id().as_str(),
                    meta.now.0,
                    expires_at.0,
                    privacy_json,
                    roles_json,
                    tenant,
                    request.device_ttl_ms,
                ],
            )?;
            let record = load_pairing_by_id(conn, &request.pairing_id)?
                .ok_or_else(|| Failure::internal("pairing code vanished inside its own transaction"))?;
            Ok(Written {
                result: WriteResult::Created,
                value: record,
            })
        })
    }

    /// 撤销还没兑换的配对码。授权和创建一样（成员关系的付款账户上的 `issue_credential`）。已经兑换的码不能撤销
    /// （要解绑设备，撤销它的凭证）。
    pub fn control_revoke_pairing_code(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &RevokePairingCode,
    ) -> Result<Written<PairingRecord>, Failure> {
        let spec = AuditSpec::new("revoke_pairing_code", TargetKind::PairingCode)
            .target(&request.pairing_id);
        self.control_write(who, meta, spec, |conn, caller, spec| {
            let Some(record) = load_pairing_by_id(conn, &request.pairing_id)? else {
                return Err(Failure::new(ControlReason::OutOfScope));
            };
            let found = load_membership(conn, &record.membership)?
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            let account = found
                .account
                .clone()
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            spec.scope_account = Some(account.to_string());
            spec.related_id = Some(record.membership.to_string());
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::IssueCredential,
                &account,
                None,
            )?;
            match record.status {
                PairingStatus::Revoked => {
                    return Ok(Written {
                        result: WriteResult::Unchanged,
                        value: record,
                    });
                }
                PairingStatus::Redeemed => {
                    return Err(Failure::new(ControlReason::PairingCodeUsed));
                }
                PairingStatus::Pending => {}
            }
            conn.execute(
                "UPDATE pairing_codes SET status = 'revoked', revoked_at_ms = ?2 \
                 WHERE pairing_id = ?1 AND status = 'pending'",
                rusqlite::params![request.pairing_id, meta.now.0],
            )?;
            let updated = load_pairing_by_id(conn, &request.pairing_id)?.ok_or_else(|| {
                Failure::internal("pairing code vanished inside its own transaction")
            })?;
            Ok(Written {
                result: WriteResult::Updated,
                value: updated,
            })
        })
    }

    /// 兑换配对码，得到一张设备凭证。调用者是码本身（`Who::PairingCode`）。
    ///
    /// 在**一个** `BEGIN IMMEDIATE` 事务里：认出码（pending、未过期）-> 重新验证签发者还有资格 -> 成员关系 / 主体 /
    /// 账户仍有效 -> 设备数没满 -> 写设备凭证、`virtual_keys` 验证器、签发来源、设备记录 -> 把码标成 redeemed -> 审计。
    /// 并发兑换同一个码：事务串行，第二个看到 `redeemed`，得到 `pairing_code_used`。
    pub fn control_redeem_pairing_code(
        &self,
        meta: &Meta<'_>,
        request: &RedeemPairingCode,
    ) -> Result<Written<PairedDevice>, Failure> {
        let label_ok = {
            let chars = request.label.chars().count();
            (1..=MAX_DEVICE_LABEL_CHARS).contains(&chars)
                && request.label.trim() == request.label
                && !request.label.chars().any(char::is_control)
        };
        if !label_ok {
            // 在碰库之前就拒绝：没有审计行（认不出调用者就不审计）；HTTP 层把它当 invalid_request。
            return Err(Failure::with_detail(
                ControlReason::InvalidRequest,
                "the device label must be 1 to 64 printable characters without leading or trailing space",
            ));
        }
        let spec = AuditSpec::new("redeem_pairing_code", TargetKind::PairingCode);
        let who = Who::PairingCode(&request.code_hash);
        self.control_write(&who, meta, spec, |conn, caller, spec| {
            let Identity::PairingCode(row) = &caller.identity else {
                return Err(Failure::internal("redeem ran without a pairing code"));
            };
            spec.target_id = Some(row.pairing_id.clone());
            spec.related_id = Some(row.membership.to_string());
            // 1. 签发者此刻还有资格：凭证行（撤销 / 冻结 / 过期），以及它在 `virtual_keys` 里的验证器
            //    （`idoris key revoke` 只撤销验证器、不动凭证行；验证器的有效期、数据面上限也可能变）。
            //    和数据面认证、控制面认证看的是同一份东西（`resolve_caller`），不能只看其中一半（Codex r1 #2）。
            let issuer = load_credential(conn, &row.issued_by)?
                .ok_or_else(|| Failure::internal("the pairing code's issuer credential is missing"))?;
            issuer
                .check_usable_at(meta.now)
                .map_err(|error| Failure::new(ControlReason::from(&error)))?;
            let Some(issuer_key) = load_key_by_id(conn, row.issued_by.as_str())? else {
                // 没有验证器 = 这张凭证已经认证不了任何请求：和已撤销同一个答案
                return Err(Failure::new(ControlReason::CredentialRevoked));
            };
            if !issuer_key.active {
                return Err(Failure::new(ControlReason::CredentialRevoked));
            }
            if issuer_key
                .ceilings
                .expires_at_ms
                .is_some_and(|expires| expires <= meta.now.0)
            {
                return Err(Failure::new(ControlReason::CredentialExpired));
            }

            // 2. 成员关系、主体、账户仍有效
            let found = load_membership(conn, &row.membership)?
                .ok_or_else(|| Failure::internal("the pairing code's membership is missing"))?;
            let account = require_live_payer(conn, &found)?;
            spec.scope_account = Some(account.to_string());
            // 3. 签发者仍有覆盖这个付款账户的 issue_credential 权限，绑定账户链仍是 Active
            let issuer_anchor = super::control::binding_anchor(conn, issuer.binding())?
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            let dir = slice_directory(conn, &[&account, &issuer_anchor], &[], Vec::new())?;
            authorize(
                &dir,
                &issuer,
                meta.now,
                ControlOp::IssueCredential,
                &account,
                None,
            )
            .map_err(Failure::new)?;
            // 码上记的设备数据面上限（创建时已经按签发者校验过）不能超过签发者**此刻**验证器上的上限
            check_ceilings(
                &issuer_key.ceilings,
                &row.device_privacy,
                &row.device_roles,
                &row.device_tenant_id,
            )?;
            // 4. 设备数没满（已撤销、已过期、验证器已撤销的不占名额，冻结的占着；见 `occupied_device_slots`）
            if occupied_device_slots(conn, &row.membership, meta.now)?
                >= MAX_ACTIVE_DEVICES_PER_MEMBERSHIP
            {
                return Err(Failure::new(ControlReason::DeviceLimitReached));
            }
            // 5. 设备凭证：Member 绑定、没有管理权限；有效期 = min(设备寿命, 签发者有效期)
            let by_ttl = match row.device_ttl_ms {
                Some(ttl) => Some(
                    meta.now
                        .checked_add_ms(ttl)
                        .ok_or_else(|| Failure::new(ControlReason::InvalidRequest))?,
                ),
                None => None,
            };
            let expires_at = match (by_ttl, issuer.expires_at()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            let credential = Credential::new(
                CredentialId::new(request.key_id.clone()).map_err(|error| {
                    Failure::with_detail(ControlReason::InvalidRequest, error.to_string())
                })?,
                CredentialBinding::Member {
                    membership: row.membership.clone(),
                },
                CredentialStatus::Active,
                expires_at,
                Vec::new(),
            )
            .map_err(|error| Failure::with_detail(ControlReason::InvalidCredential, error.to_string()))?;
            insert_virtual_key(
                conn,
                &NewKey {
                    key_id: &request.key_id,
                    hash: &request.key_hash,
                    owner: &format!("device:{}", row.pairing_id),
                    tenant_id: row.device_tenant_id.as_deref(),
                    privacy: &row.device_privacy,
                    roles: &row.device_roles,
                    expires_at,
                },
            )?;
            create_credential_in(conn, &credential, meta.now)
                .map_err(|error| Failure::from_store(error, Ctx::Credential))?;
            insert_issuance(conn, &request.key_id, &caller.actor, meta.now)?;
            conn.execute(
                "INSERT INTO pairing_devices (credential_id, membership_id, pairing_id, label, paired_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    request.key_id,
                    row.membership.as_str(),
                    row.pairing_id,
                    request.label,
                    meta.now.0,
                ],
            )?;
            let changed = conn.execute(
                "UPDATE pairing_codes \
                    SET status = 'redeemed', redeemed_at_ms = ?2, redeemed_credential_id = ?3 \
                  WHERE pairing_id = ?1 AND status = 'pending'",
                rusqlite::params![row.pairing_id, meta.now.0, request.key_id],
            )?;
            if changed != 1 {
                // 同一个事务里读到 pending、写时却不是：不会发生；发生了就整个回滚。
                return Err(Failure::new(ControlReason::PairingCodeUsed));
            }
            Ok(Written {
                result: WriteResult::Created,
                value: PairedDevice {
                    credential,
                    membership: row.membership.clone(),
                    pairing_id: row.pairing_id.clone(),
                },
            })
        })
    }

    /// 设备自己解绑：撤销**调用者自己**这张凭证（及 `virtual_keys` 里的验证器）。不需要任何管理权限——
    /// 一台设备永远有权离开自己的账号——但只能撤销自己，而且只对配对出来的设备凭证有效。
    pub fn control_unpair_device(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
    ) -> Result<Written<Credential>, Failure> {
        let spec = AuditSpec::new("unpair_device", TargetKind::Credential);
        self.control_write(who, meta, spec, |conn, caller, spec| {
            let Some(credential) = caller.credential() else {
                return Err(Failure::new(ControlReason::NotADevice));
            };
            spec.target_id = Some(credential.id().to_string());
            let device: Option<String> = conn
                .query_row(
                    "SELECT membership_id FROM pairing_devices WHERE credential_id = ?1",
                    [credential.id().as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(membership) = device else {
                return Err(Failure::new(ControlReason::NotADevice));
            };
            spec.related_id = Some(membership.clone());
            if let Some(found) = load_membership(
                conn,
                &MembershipId::new(membership).map_err(AccountStoreError::from)?,
            )? {
                spec.scope_account = found.account.map(|account| account.to_string());
            }
            set_credential_status_in(conn, credential.id(), CredentialStatus::Revoked, meta.now)
                .map_err(|error| Failure::from_store(error, Ctx::Credential))?;
            revoke_virtual_key(conn, credential.id().as_str(), meta.now)?;
            let updated = load_credential(conn, credential.id())?.ok_or_else(|| {
                Failure::internal("credential vanished inside its own transaction")
            })?;
            Ok(Written {
                result: WriteResult::Updated,
                value: updated,
            })
        })
    }
}

// ================================================================================================
// 数据面：凭证闸门
// ================================================================================================

/// 数据面认证之后的一道闸门（见 [`AccountStore::credential_gate`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// 库里没有这张凭证（本地用 `idoris key issue` 签发的数据密钥）：账户存储对它没有意见，按原有规则放行。
    NotManaged,
    /// 凭证、它绑定的成员关系 / 主体 / 账户链此刻都有效。
    Allowed,
    /// 不可用，原因是数据面的稳定原因码（`credential_revoked`、`membership_revoked`、`account_inactive`……）。
    Denied(ReasonCode),
}

impl AccountStore {
    /// **数据面认证的最后一道闸门**：撤销即时生效靠它。数据面每次认证通过 `virtual_keys` 之后，都要问一次：
    /// 这张密钥对应的云凭证此刻还有效吗？每次都读库，不缓存，所以撤销 / 冻结 / 过期、成员关系被撤销、主体被撤销、
    /// 付款账户（或任何祖先）被冻结或关闭，**下一次请求就被拒**，不需要逐张去撤销凭证。
    ///
    /// - 库里没有这张凭证：[`GateVerdict::NotManaged`]（本地数据密钥，不归账户存储管）；
    /// - `Member` 凭证：沿成员关系检查（成员关系、主体、付款账户及全部祖先）；
    /// - `Node` 凭证：绑定的组织账户及全部祖先是 Active；
    /// - `Service` 凭证：同上（每次请求带的成员 id 由 S5 在解析付款上下文时用 [`AccountStore::payment_context`] 逐次验证，
    ///   那里同样不缓存）。
    pub fn credential_gate(
        &self,
        key_id: &str,
        now: TimestampMs,
    ) -> Result<GateVerdict, AccountStoreError> {
        let Ok(id) = CredentialId::new(key_id) else {
            return Ok(GateVerdict::NotManaged);
        };
        self.read(|conn| {
            let Some(credential) = load_credential(conn, &id)? else {
                return Ok(GateVerdict::NotManaged);
            };
            if let Err(error) = credential.check_usable_at(now) {
                return Ok(GateVerdict::Denied(error.reason_code()));
            }
            match credential.binding() {
                CredentialBinding::Service { account } => {
                    let chain = load_ancestors(conn, account)?;
                    if chain.is_empty() {
                        return Ok(GateVerdict::Denied(ReasonCode::PaymentContextInvalid));
                    }
                    if chain.iter().any(|found| {
                        found.status != idoris_compute_contracts::account::AccountStatus::Active
                    }) {
                        return Ok(GateVerdict::Denied(ReasonCode::AccountInactive));
                    }
                    Ok(GateVerdict::Allowed)
                }
                CredentialBinding::Node { .. } | CredentialBinding::Member { .. } => {
                    let dir = directory_slice(conn, &credential, None)?;
                    match resolve_payment_context(&dir, &credential, None, now) {
                        Ok(_) => Ok(GateVerdict::Allowed),
                        Err(error) => Ok(GateVerdict::Denied(error.reason_code())),
                    }
                }
            }
        })
    }
}

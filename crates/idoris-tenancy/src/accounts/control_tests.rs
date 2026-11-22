//! 控制面存储层的测试：授权矩阵、越权签发、审计只追加、幂等与额度撤销、读。
//! 配对与数据面闸门在 `pairing_tests.rs`，共用这里的夹具。
//!
//! 夹具的树（全部泰铢）：
//!
//! ```text
//! platform
//! ├── pool (补贴池)
//! │   └── eduai (组织)
//! │       ├── teacher (担保人)
//! │       │   └── s1 (成员，kid1，m-kid1)
//! │       └── s2 (成员，kid2，m-kid2)
//! └── school-b (组织)
//!     └── b1 (成员，kb1，m-kb1)
//! ```
//!
//! 凭证都由本机运维签发（运维不受授权约束），再用它们去调用控制面。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, Membership, MembershipRole, MembershipStatus,
};
use idoris_compute_contracts::control::{ControlReason, WriteResult};
use idoris_compute_contracts::credential::{
    AdminScope, ControlOp, CredentialBinding, CredentialStatus,
};
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, BillingMonth, CredentialId, MembershipId, PrincipalId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_contracts::common::PrivacyClass;
use rusqlite::Connection;
use rusqlite::trace::{TraceEvent, TraceEventCodes};

use super::{
    AccountStore, CreateAccount, CreateMembership, CreatePrincipal, Failure, GrantAllocation,
    IssueCredential, Meta, RevokeAllocation, RevokeMembership, RevokePrincipal, SetAccountStatus,
    SetCredentialStatus, Who, Written,
};
use crate::test_support::TempDir;
use crate::virtual_key::MintedVirtualKey;
use crate::virtual_key::store::VirtualKeyStore;

pub(super) const T0: i64 = 1_793_534_400_000; // 2026-11-01T12:00:00Z
pub(super) const DAY_MS: i64 = 86_400_000;

pub(super) fn aid(text: &str) -> AccountId {
    AccountId::new(text).unwrap()
}
pub(super) fn pid(text: &str) -> PrincipalId {
    PrincipalId::new(text).unwrap()
}
pub(super) fn mid(text: &str) -> MembershipId {
    MembershipId::new(text).unwrap()
}
pub(super) fn cid(text: &str) -> CredentialId {
    CredentialId::new(text).unwrap()
}
pub(super) fn thb(micros: i64) -> Money {
    Money::new(Unit::Thb, micros)
}
pub(super) fn ts(ms: i64) -> TimestampMs {
    TimestampMs(ms)
}
pub(super) fn sc(op: ControlOp, subtree: &str, cap: Option<i64>) -> AdminScope {
    AdminScope {
        op,
        subtree: aid(subtree),
        max_amount: cap.map(thb),
    }
}

/// 一张签发出来的凭证：id 和明文密钥（只有测试手里有）。
pub(super) struct Minted {
    pub(super) key: MintedVirtualKey,
}

impl Minted {
    pub(super) fn id(&self) -> &str {
        &self.key.key_id
    }
    pub(super) fn who(&self) -> Who<'_> {
        Who::Secret(&self.key.hash)
    }
    pub(super) fn secret(&self) -> &str {
        self.key.secret.expose_secret()
    }
}

pub(super) struct Env {
    pub(super) _dir: TempDir,
    pub(super) path: PathBuf,
    pub(super) store: AccountStore,
    /// 保持打开：`virtual_keys` 表由它建出来，和生产里的启动顺序一致（先开虚拟 key 库，再开账户库）。
    pub(super) keys: VirtualKeyStore,
    counter: std::cell::Cell<u32>,
}

impl Env {
    pub(super) fn new() -> Self {
        let dir = TempDir::new("control");
        let path = dir.db("control.sqlite3");
        let keys = VirtualKeyStore::open(&path).unwrap();
        let store = AccountStore::open(&path).unwrap();
        Self {
            _dir: dir,
            path,
            store,
            keys,
            counter: std::cell::Cell::new(0),
        }
    }

    /// 每次调用一个新的请求号。
    pub(super) fn rid(&self) -> String {
        let next = self.counter.get() + 1;
        self.counter.set(next);
        format!("req-{next}")
    }

    pub(super) fn meta<'a>(&self, now: i64, rid: &'a str) -> Meta<'a> {
        Meta {
            now: ts(now),
            request_id: rid,
        }
    }

    pub(super) fn raw(&self) -> Connection {
        let conn = Connection::open(&self.path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    pub(super) fn operator() -> Who<'static> {
        Who::Operator("test-operator")
    }

    // ---------- 夹具：树、成员、凭证 ----------

    pub(super) fn account(&self, id: &str, kind: AccountKind, parent: Option<&str>) {
        let account = Account {
            id: aid(id),
            kind,
            parent: parent.map(aid),
            unit: Unit::Thb,
            status: AccountStatus::Active,
        };
        self.store.create_account(&account, ts(T0)).unwrap();
    }

    pub(super) fn tree(&self) {
        self.account("platform", AccountKind::Platform, None);
        self.account("pool", AccountKind::SubsidyPool, Some("platform"));
        self.account("eduai", AccountKind::Org, Some("pool"));
        self.account("teacher", AccountKind::Sponsor, Some("eduai"));
        self.account("s1", AccountKind::Member, Some("teacher"));
        self.account("s2", AccountKind::Member, Some("eduai"));
        self.account("school-b", AccountKind::Org, Some("platform"));
        self.account("b1", AccountKind::Member, Some("school-b"));
        for (principal, membership, org, role, account) in [
            ("kid1", "m-kid1", "eduai", MembershipRole::Member, "s1"),
            ("kid2", "m-kid2", "eduai", MembershipRole::Member, "s2"),
            ("tch", "m-tch", "eduai", MembershipRole::Sponsor, "teacher"),
            ("kb1", "m-kb1", "school-b", MembershipRole::Member, "b1"),
        ] {
            self.store
                .ensure_principal(&pid(principal), ts(T0))
                .unwrap();
            self.store
                .create_membership(
                    &Membership {
                        id: mid(membership),
                        principal: pid(principal),
                        org: aid(org),
                        role,
                        account: Some(aid(account)),
                        status: MembershipStatus::Active,
                    },
                    ts(T0),
                )
                .unwrap();
        }
    }

    /// 本机运维签发一张凭证。
    pub(super) fn issue_as_operator(&self, spec: Issue) -> Minted {
        let key = MintedVirtualKey::mint();
        let rid = self.rid();
        let written = self
            .store
            .control_issue_credential(&Self::operator(), &self.meta(T0, &rid), &spec.request(&key))
            .unwrap();
        assert_eq!(written.result, WriteResult::Created);
        Minted { key }
    }

    /// 额度链：平台 -> 补贴池 10000，补贴池 -> eduai 1000，eduai -> teacher 100，平台 -> school-b 500
    /// （都是预付，窗口 T0 起 30 天）。
    pub(super) fn allocation_chain(&self) {
        for (id, parent, child, micros) in [
            ("a-pool", "platform", "pool", 10_000_000_000_i64),
            ("a-edu", "pool", "eduai", 1_000_000_000),
            ("a-teacher", "eduai", "teacher", 100_000_000),
            ("a-b", "platform", "school-b", 500_000_000),
        ] {
            let rid = self.rid();
            self.store
                .control_grant_allocation(
                    &Self::operator(),
                    &self.meta(T0, &rid),
                    &grant(id, parent, child, micros),
                )
                .unwrap();
        }
    }

    pub(super) fn audit(&self) -> Vec<AuditEntry> {
        let conn = self.raw();
        let mut stmt = conn
            .prepare(
                "SELECT actor_kind, actor_id, op, target_kind, target_id, scope_account, related_id, \
                        outcome, reason_code, request_id FROM control_audit ORDER BY audit_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(AuditEntry {
                actor_kind: row.get(0)?,
                actor_id: row.get(1)?,
                op: row.get(2)?,
                target_kind: row.get(3)?,
                target_id: row.get(4)?,
                scope_account: row.get(5)?,
                related_id: row.get(6)?,
                outcome: row.get(7)?,
                reason_code: row.get(8)?,
                request_id: row.get(9)?,
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    pub(super) fn last_audit(&self) -> AuditEntry {
        self.audit().pop().expect("an audit row")
    }
}

/// 签发一张凭证要的参数（带默认值的构造器，让夹具读起来短一点）。
#[derive(Clone)]
pub(super) struct Issue {
    pub(super) binding: CredentialBinding,
    pub(super) scopes: Vec<AdminScope>,
    pub(super) expires: Option<i64>,
    pub(super) privacy: Vec<PrivacyClass>,
    pub(super) roles: Vec<String>,
    pub(super) tenant: Option<String>,
}

impl Issue {
    pub(super) fn service(account: &str, scopes: Vec<AdminScope>) -> Self {
        Self {
            binding: CredentialBinding::Service {
                account: aid(account),
            },
            scopes,
            expires: None,
            privacy: vec![PrivacyClass::LocalOnly],
            roles: vec!["fast".into()],
            tenant: Some("tenant-edu".into()),
        }
    }

    pub(super) fn member(membership: &str) -> Self {
        Self {
            binding: CredentialBinding::Member {
                membership: mid(membership),
            },
            scopes: vec![],
            ..Self::service("eduai", vec![])
        }
    }

    pub(super) fn expiring(mut self, at: i64) -> Self {
        self.expires = Some(at);
        self
    }

    pub(super) fn privacy(mut self, privacy: &[PrivacyClass]) -> Self {
        self.privacy = privacy.to_vec();
        self
    }

    pub(super) fn roles(mut self, roles: &[&str]) -> Self {
        self.roles = roles.iter().map(|r| (*r).to_string()).collect();
        self
    }

    pub(super) fn tenant(mut self, tenant: Option<&str>) -> Self {
        self.tenant = tenant.map(str::to_string);
        self
    }

    pub(super) fn request(&self, key: &MintedVirtualKey) -> IssueCredential {
        IssueCredential {
            key_id: key.key_id.clone(),
            key_hash: key.hash,
            binding: self.binding.clone(),
            admin_scopes: self.scopes.clone(),
            expires_at: self.expires.map(ts),
            owner: "fixture".into(),
            tenant_id: self.tenant.clone(),
            allowed_privacy: self.privacy.clone(),
            allowed_roles: self.roles.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AuditEntry {
    pub(super) actor_kind: String,
    pub(super) actor_id: String,
    pub(super) op: String,
    pub(super) target_kind: String,
    pub(super) target_id: Option<String>,
    pub(super) scope_account: Option<String>,
    pub(super) related_id: Option<String>,
    pub(super) outcome: String,
    pub(super) reason_code: String,
    pub(super) request_id: String,
}

pub(super) fn grant(id: &str, parent: &str, child: &str, micros: i64) -> GrantAllocation {
    GrantAllocation {
        id: AllocationId::new(id).unwrap(),
        parent: aid(parent),
        child: aid(child),
        amount: thb(micros),
        purpose: Purpose::Prepaid,
        valid_from: ts(T0),
        valid_until: ts(T0 + 30 * DAY_MS),
        month: None,
    }
}

pub(super) fn failure_reason<T: std::fmt::Debug>(
    result: Result<Written<T>, Failure>,
) -> ControlReason {
    result.expect_err("expected a failure").reason
}

/// 所有 `ControlOp` 的全权限（指定子树）：发额度的单笔上限用 `grant_cap`。
pub(super) fn all_ops(subtree: &str, grant_cap: i64) -> Vec<AdminScope> {
    ControlOp::ALL
        .into_iter()
        .map(|op| {
            let cap = (op == ControlOp::GrantAllocation).then_some(grant_cap);
            sc(op, subtree, cap)
        })
        .collect()
}

// ================================================================================================
// 一个统一的「动作」接口，让授权矩阵对每个操作用同一套断言
// ================================================================================================

#[derive(Debug, Clone)]
pub(super) enum Action {
    CreatePrincipal {
        principal: &'static str,
        org: &'static str,
    },
    RevokePrincipal {
        principal: &'static str,
        org: &'static str,
    },
    CreateAccount {
        id: &'static str,
        parent: &'static str,
    },
    SetAccountStatus {
        account: &'static str,
        status: AccountStatus,
    },
    CreateMembership {
        id: &'static str,
        principal: &'static str,
        org: &'static str,
        account: &'static str,
    },
    RevokeMembership {
        id: &'static str,
    },
    IssueCredential {
        membership: &'static str,
    },
    SetCredential {
        credential: String,
        status: CredentialStatus,
    },
    GrantAllocation {
        id: &'static str,
        parent: &'static str,
        child: &'static str,
        micros: i64,
    },
    RevokeAllocation {
        id: &'static str,
    },
    Read {
        account: &'static str,
    },
}

impl Action {
    /// 执行。成功返回结果类别，失败返回原因码。
    pub(super) fn run(
        &self,
        env: &Env,
        who: &Who<'_>,
        now: i64,
    ) -> Result<WriteResult, ControlReason> {
        let rid = env.rid();
        let meta = env.meta(now, &rid);
        let store = &env.store;
        let out: Result<WriteResult, Failure> = match self {
            Action::CreatePrincipal { principal, org } => store
                .control_create_principal(
                    who,
                    &meta,
                    &CreatePrincipal {
                        principal: pid(principal),
                        org: aid(org),
                    },
                )
                .map(|w| w.result),
            Action::RevokePrincipal { principal, org } => store
                .control_revoke_principal(
                    who,
                    &meta,
                    &RevokePrincipal {
                        principal: pid(principal),
                        org: aid(org),
                    },
                )
                .map(|w| w.result),
            Action::CreateAccount { id, parent } => store
                .control_create_account(
                    who,
                    &meta,
                    &CreateAccount {
                        account: Account {
                            id: aid(id),
                            kind: AccountKind::Member,
                            parent: Some(aid(parent)),
                            unit: Unit::Thb,
                            status: AccountStatus::Active,
                        },
                    },
                )
                .map(|w| w.result),
            Action::SetAccountStatus { account, status } => store
                .control_set_account_status(
                    who,
                    &meta,
                    &SetAccountStatus {
                        account: aid(account),
                        status: *status,
                    },
                )
                .map(|w| w.result),
            Action::CreateMembership {
                id,
                principal,
                org,
                account,
            } => store
                .control_create_membership(
                    who,
                    &meta,
                    &CreateMembership {
                        membership: Membership {
                            id: mid(id),
                            principal: pid(principal),
                            org: aid(org),
                            role: MembershipRole::Member,
                            account: Some(aid(account)),
                            status: MembershipStatus::Active,
                        },
                    },
                )
                .map(|w| w.result),
            Action::RevokeMembership { id } => store
                .control_revoke_membership(
                    who,
                    &meta,
                    &RevokeMembership {
                        membership: mid(id),
                    },
                )
                .map(|w| w.result),
            Action::IssueCredential { membership } => {
                let key = MintedVirtualKey::mint();
                store
                    .control_issue_credential(who, &meta, &Issue::member(membership).request(&key))
                    .map(|w| w.result)
            }
            Action::SetCredential { credential, status } => store
                .control_set_credential_status(
                    who,
                    &meta,
                    &SetCredentialStatus {
                        credential: cid(credential),
                        status: *status,
                    },
                )
                .map(|w| w.result),
            Action::GrantAllocation {
                id,
                parent,
                child,
                micros,
            } => store
                .control_grant_allocation(who, &meta, &grant(id, parent, child, *micros))
                .map(|w| w.result),
            Action::RevokeAllocation { id } => store
                .control_revoke_allocation(
                    who,
                    &meta,
                    &RevokeAllocation {
                        id: AllocationId::new(*id).unwrap(),
                    },
                )
                .map(|w| w.result),
            Action::Read { account } => store
                .control_read_account(who, ts(now), &aid(account))
                .map(|_| WriteResult::Unchanged),
        };
        out.map_err(|failure| failure.reason)
    }

    pub(super) fn is_read(&self) -> bool {
        matches!(self, Action::Read { .. })
    }
}

/// 矩阵夹具：三张供冻结 / 撤销用的成员凭证（分别在 teacher 子树里、eduai 里 teacher 子树外、别的组织里）。
pub(super) struct MatrixCredentials {
    inside: String,
    sibling: String,
    foreign: String,
}

fn matrix_setup(env: &Env) -> MatrixCredentials {
    env.tree();
    env.allocation_chain();
    let operator = Env::operator();
    // IssueCredential / ManageMembership 要的账户先建好
    for (id, parent) in [
        ("m-acct-in", "teacher"),
        ("m-acct-sib", "eduai"),
        ("m-acct-for", "school-b"),
    ] {
        let rid = env.rid();
        env.store
            .control_create_account(
                &operator,
                &env.meta(T0, &rid),
                &CreateAccount {
                    account: Account {
                        id: aid(id),
                        kind: AccountKind::Member,
                        parent: Some(aid(parent)),
                        unit: Unit::Thb,
                        status: AccountStatus::Active,
                    },
                },
            )
            .unwrap();
    }
    MatrixCredentials {
        inside: env
            .issue_as_operator(Issue::member("m-kid1"))
            .id()
            .to_string(),
        sibling: env
            .issue_as_operator(Issue::member("m-kid2"))
            .id()
            .to_string(),
        foreign: env
            .issue_as_operator(Issue::member("m-kb1").tenant(Some("tenant-edu")))
            .id()
            .to_string(),
    }
}

/// 每个操作的三个动作：目标在 teacher 子树里 / 在 eduai 里但在 teacher 子树外 / 在别的组织里。
fn actions_for(op: ControlOp, creds: &MatrixCredentials) -> (Action, Action, Action) {
    let set = |id: &str, status: CredentialStatus| Action::SetCredential {
        credential: id.to_string(),
        status,
    };
    match op {
        ControlOp::CreatePrincipal => (
            Action::CreatePrincipal {
                principal: "p-in",
                org: "teacher",
            },
            Action::CreatePrincipal {
                principal: "p-sib",
                org: "eduai",
            },
            Action::CreatePrincipal {
                principal: "p-for",
                org: "school-b",
            },
        ),
        ControlOp::RevokePrincipal => (
            Action::RevokePrincipal {
                principal: "kid1",
                org: "teacher",
            },
            Action::RevokePrincipal {
                principal: "kid2",
                org: "eduai",
            },
            Action::RevokePrincipal {
                principal: "kb1",
                org: "school-b",
            },
        ),
        ControlOp::CreateAccount => (
            Action::CreateAccount {
                id: "acct-in",
                parent: "teacher",
            },
            Action::CreateAccount {
                id: "acct-sib",
                parent: "eduai",
            },
            Action::CreateAccount {
                id: "acct-for",
                parent: "school-b",
            },
        ),
        ControlOp::SetAccountStatus => (
            Action::SetAccountStatus {
                account: "s1",
                status: AccountStatus::Frozen,
            },
            Action::SetAccountStatus {
                account: "s2",
                status: AccountStatus::Frozen,
            },
            Action::SetAccountStatus {
                account: "b1",
                status: AccountStatus::Frozen,
            },
        ),
        ControlOp::ManageMembership => (
            Action::CreateMembership {
                id: "m-in",
                principal: "kid1",
                org: "eduai",
                account: "m-acct-in",
            },
            Action::CreateMembership {
                id: "m-sib",
                principal: "kid1",
                org: "eduai",
                account: "m-acct-sib",
            },
            Action::CreateMembership {
                id: "m-for",
                principal: "kb1",
                org: "school-b",
                account: "m-acct-for",
            },
        ),
        ControlOp::IssueCredential => (
            Action::IssueCredential {
                membership: "m-kid1",
            },
            Action::IssueCredential {
                membership: "m-kid2",
            },
            Action::IssueCredential {
                membership: "m-kb1",
            },
        ),
        ControlOp::FreezeCredential => (
            set(&creds.inside, CredentialStatus::Frozen),
            set(&creds.sibling, CredentialStatus::Frozen),
            set(&creds.foreign, CredentialStatus::Frozen),
        ),
        ControlOp::RevokeCredential => (
            set(&creds.inside, CredentialStatus::Revoked),
            set(&creds.sibling, CredentialStatus::Revoked),
            set(&creds.foreign, CredentialStatus::Revoked),
        ),
        ControlOp::GrantAllocation => (
            Action::GrantAllocation {
                id: "g-in",
                parent: "teacher",
                child: "s1",
                micros: 1_000_000,
            },
            Action::GrantAllocation {
                id: "g-sib",
                parent: "eduai",
                child: "s2",
                micros: 1_000_000,
            },
            Action::GrantAllocation {
                id: "g-for",
                parent: "school-b",
                child: "b1",
                micros: 1_000_000,
            },
        ),
        ControlOp::Read => (
            Action::Read { account: "s1" },
            Action::Read { account: "s2" },
            Action::Read { account: "b1" },
        ),
    }
}

/// 只有 teacher 子树权限的调用者：每个操作各一条 `teacher` 子树上的权限（发额度单笔上限 5 THB）。
fn teacher_wide(env: &Env) -> Minted {
    env.issue_as_operator(Issue::service("eduai", all_ops("teacher", 5_000_000)))
}

#[test]
fn authorization_matrix_every_operation() {
    for op in ControlOp::ALL {
        let env = Env::new();
        let creds = matrix_setup(&env);
        let (inside, sibling, foreign) = actions_for(op, &creds);
        let caller = teacher_wide(&env);
        let audited = !inside.is_read();

        // 1. 范围内放行（写操作有成功的审计行）
        let before = env.audit().len();
        let result = inside.run(&env, &caller.who(), T0 + 1);
        assert!(
            result.is_ok(),
            "{op:?}: in scope must be allowed, got {result:?}"
        );
        if audited {
            let audit = env.audit();
            assert_eq!(audit.len(), before + 1, "{op:?}");
            let last = audit.last().unwrap();
            assert_eq!(last.outcome, "success", "{op:?}");
            assert_eq!(last.reason_code, "ok", "{op:?}");
            assert_eq!(last.actor_kind, "credential");
            assert_eq!(last.actor_id, caller.id());
        } else {
            assert_eq!(env.audit().len(), before, "reads are not audited");
        }

        // 2. 子树外（同一个组织里、teacher 子树之外）和别的组织：一律 out_of_scope，写操作留下 denied 审计
        for (label, action) in [("sibling", &sibling), ("foreign", &foreign)] {
            let before = env.audit().len();
            let result = action.run(&env, &caller.who(), T0 + 2);
            assert_eq!(result, Err(ControlReason::OutOfScope), "{op:?} {label}");
            if audited {
                let audit = env.audit();
                assert_eq!(
                    audit.len(),
                    before + 1,
                    "{op:?} {label}: a denied write is audited"
                );
                let last = audit.last().unwrap();
                assert_eq!(last.outcome, "denied", "{op:?} {label}");
                assert_eq!(last.reason_code, "out_of_scope", "{op:?} {label}");
                assert_eq!(last.actor_id, caller.id());
            } else {
                assert_eq!(env.audit().len(), before);
            }
        }

        // 3. 没有这个操作的权限（持有的是别的操作）：op_not_granted
        let other_op = ControlOp::ALL
            .into_iter()
            .find(|candidate| *candidate != op && *candidate != ControlOp::GrantAllocation)
            .unwrap();
        let wrong =
            env.issue_as_operator(Issue::service("eduai", vec![sc(other_op, "eduai", None)]));
        let before = env.audit().len();
        assert_eq!(
            inside.run(&env, &wrong.who(), T0 + 3),
            Err(ControlReason::OpNotGranted),
            "{op:?} wrong op"
        );
        if audited {
            let audit = env.audit();
            assert_eq!(audit.len(), before + 1);
            assert_eq!(audit.last().unwrap().reason_code, "op_not_granted");
        }
        // 没有任何权限
        let none = env.issue_as_operator(Issue::service("eduai", vec![]));
        assert_eq!(
            inside.run(&env, &none.who(), T0 + 3),
            Err(ControlReason::OpNotGranted),
            "{op:?} no scopes"
        );

        // 3b. 下发超过授权者：调用者能在 eduai 签发凭证，但这个操作自己的权限只在 teacher 子树（发额度另有单笔上限）。
        //     想签一张带「更大的」同一操作权限的凭证：更大的子树、（发额度时）更高的上限 / 没有上限，全部被拒，并留审计。
        let own = |subtree: &str, cap: Option<i64>| sc(op, subtree, cap);
        let own_cap = (op == ControlOp::GrantAllocation).then_some(5_000_000);
        let mut own_scopes = vec![own("teacher", own_cap)];
        if op != ControlOp::IssueCredential {
            own_scopes.push(sc(ControlOp::IssueCredential, "eduai", None));
        } else {
            // 对 issue_credential 自己：它的权限范围就是 eduai，更大的就是 eduai 之上的补贴池
            own_scopes = vec![sc(ControlOp::IssueCredential, "eduai", None)];
        }
        let delegator = env.issue_as_operator(Issue::service("eduai", own_scopes));
        let mut attempts: Vec<(&str, AdminScope)> = vec![(
            "wider subtree",
            own(
                if op == ControlOp::IssueCredential {
                    "pool"
                } else {
                    "eduai"
                },
                own_cap,
            ),
        )];
        if op == ControlOp::GrantAllocation {
            attempts.push(("higher cap", own("teacher", Some(5_000_001))));
            attempts.push(("no cap", own("teacher", None)));
        }
        for (label, scope) in attempts {
            let before = env.audit().len();
            let key = MintedVirtualKey::mint();
            let rid = env.rid();
            let result = env.store.control_issue_credential(
                &delegator.who(),
                &env.meta(T0 + 3, &rid),
                &Issue::service("eduai", vec![scope]).request(&key),
            );
            assert_eq!(
                failure_reason(result),
                ControlReason::ScopeExceedsGrantor,
                "{op:?} {label}"
            );
            let audit = env.audit();
            assert_eq!(audit.len(), before + 1, "{op:?} {label}: audited");
            let last = audit.last().unwrap();
            assert_eq!(
                (last.outcome.as_str(), last.reason_code.as_str()),
                ("denied", "scope_exceeds_grantor"),
                "{op:?} {label}"
            );
            assert_eq!(last.actor_id, delegator.id());
            assert!(
                env.keys
                    .authenticate(&key.secret, T0 + 3)
                    .unwrap()
                    .is_none(),
                "{op:?} {label}: nothing was issued"
            );
        }
        // 对照：和自己一样大的权限可以下发（说明上面的拒绝来自「更大」，不是签发本身坏了）
        let same = Issue::service("eduai", vec![own("teacher", own_cap)]);
        if op != ControlOp::IssueCredential {
            let key = MintedVirtualKey::mint();
            let rid = env.rid();
            assert!(
                env.store
                    .control_issue_credential(
                        &delegator.who(),
                        &env.meta(T0 + 3, &rid),
                        &same.request(&key)
                    )
                    .is_ok(),
                "{op:?}: an equal scope may be delegated"
            );
        }

        // 4. 被撤销 / 冻结 / 已过期的调用者：拒绝，原因码对应；写操作留下 denied 审计，actor 是它的凭证 id
        for (label, expected) in [
            ("revoked", ControlReason::CredentialRevoked),
            ("frozen", ControlReason::CredentialFrozen),
            ("expired", ControlReason::CredentialExpired),
        ] {
            let issue = Issue::service("eduai", all_ops("teacher", 5_000_000));
            let victim = if label == "expired" {
                env.issue_as_operator(issue.expiring(T0 + 1_000))
            } else {
                env.issue_as_operator(issue)
            };
            let now = match label {
                "revoked" => {
                    // 走控制面自己的撤销路径（同时标记 virtual_keys）
                    let revoker = env.issue_as_operator(Issue::service(
                        "eduai",
                        vec![sc(ControlOp::RevokeCredential, "eduai", None)],
                    ));
                    assert_eq!(
                        Action::SetCredential {
                            credential: victim.id().to_string(),
                            status: CredentialStatus::Revoked
                        }
                        .run(&env, &revoker.who(), T0 + 4),
                        Ok(WriteResult::Updated)
                    );
                    T0 + 5
                }
                "frozen" => {
                    let freezer = env.issue_as_operator(Issue::service(
                        "eduai",
                        vec![sc(ControlOp::FreezeCredential, "eduai", None)],
                    ));
                    assert_eq!(
                        Action::SetCredential {
                            credential: victim.id().to_string(),
                            status: CredentialStatus::Frozen
                        }
                        .run(&env, &freezer.who(), T0 + 4),
                        Ok(WriteResult::Updated)
                    );
                    T0 + 5
                }
                _ => T0 + 1_000, // 刚好到期的那一毫秒
            };
            let before = env.audit().len();
            let result = inside.run(&env, &victim.who(), now);
            assert_eq!(result, Err(expected), "{op:?} {label}");
            if audited {
                let audit = env.audit();
                assert_eq!(audit.len(), before + 1, "{op:?} {label}: audited");
                let last = audit.last().unwrap();
                assert_eq!(last.outcome, "denied");
                assert_eq!(last.reason_code, expected.as_str());
                assert_eq!(last.actor_id, victim.id());
            }
        }
    }
}

// ================================================================================================
// 越权签发 / 下发超过授权者
// ================================================================================================

/// 「中间人」：可以在 eduai 签发凭证，但自己的发额度 / 读 / 冻结账户权限只在 teacher 子树里，
/// 数据面只有 local_only + fast，有效期到 T0 + 10 天。
fn middle(env: &Env) -> Minted {
    env.issue_as_operator(
        Issue::service(
            "eduai",
            vec![
                sc(ControlOp::IssueCredential, "eduai", None),
                sc(ControlOp::GrantAllocation, "teacher", Some(5_000_000)),
                sc(ControlOp::Read, "teacher", None),
                sc(ControlOp::SetAccountStatus, "teacher", None),
            ],
        )
        .expiring(T0 + 10 * DAY_MS),
    )
}

fn issue_as(
    env: &Env,
    who: &Who<'_>,
    issue: &Issue,
    now: i64,
) -> Result<Written<idoris_compute_contracts::credential::Credential>, Failure> {
    let key = MintedVirtualKey::mint();
    let rid = env.rid();
    env.store
        .control_issue_credential(who, &env.meta(now, &rid), &issue.request(&key))
}

#[test]
fn a_caller_can_never_issue_a_credential_with_more_authority_than_its_own() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let caller = middle(&env);
    let who = caller.who();
    let base = || Issue::service("eduai", vec![]).expiring(T0 + 5 * DAY_MS);

    // 没有越权的对照组：和自己一样大或更小的权限，签发成功
    let ok = issue_as(&env, &who, &base().tenant(None).clone(), T0 + 1);
    assert!(
        ok.is_ok(),
        "a credential without admin scopes is fine: {ok:?}"
    );
    for (label, scopes) in [
        (
            "same scope",
            vec![sc(ControlOp::GrantAllocation, "teacher", Some(5_000_000))],
        ),
        (
            "smaller cap",
            vec![sc(ControlOp::GrantAllocation, "teacher", Some(1))],
        ),
        ("narrower subtree", vec![sc(ControlOp::Read, "s1", None)]),
    ] {
        let mut issue = base();
        issue.scopes = scopes;
        let result = issue_as(&env, &who, &issue, T0 + 1);
        assert!(result.is_ok(), "{label}: {result:?}");
    }

    // 越权：每一种都被拒绝，原因是 scope_exceeds_grantor，并且各留一行 denied 审计
    let attempts: Vec<(&str, Vec<AdminScope>)> = vec![
        (
            "wider subtree",
            vec![sc(ControlOp::GrantAllocation, "eduai", Some(5_000_000))],
        ),
        (
            "higher cap",
            vec![sc(ControlOp::GrantAllocation, "teacher", Some(5_000_001))],
        ),
        (
            "uncapped",
            vec![sc(ControlOp::GrantAllocation, "teacher", None)],
        ),
        (
            "an operation the caller does not hold",
            vec![sc(ControlOp::RevokeCredential, "teacher", None)],
        ),
        (
            "read on a wider subtree",
            vec![sc(ControlOp::Read, "eduai", None)],
        ),
        (
            "the platform's side",
            vec![sc(ControlOp::Read, "pool", None)],
        ),
        (
            "one fine scope and one too big",
            vec![
                sc(ControlOp::Read, "teacher", None),
                sc(ControlOp::SetAccountStatus, "eduai", None),
            ],
        ),
    ];
    for (label, scopes) in attempts {
        let before = env.audit().len();
        let mut issue = base();
        issue.scopes = scopes;
        let reason = failure_reason(issue_as(&env, &who, &issue, T0 + 2));
        assert_eq!(reason, ControlReason::ScopeExceedsGrantor, "{label}");
        let audit = env.audit();
        assert_eq!(audit.len(), before + 1, "{label}");
        let last = audit.last().unwrap();
        assert_eq!(last.op, "issue_credential");
        assert_eq!(last.outcome, "denied");
        assert_eq!(last.reason_code, "scope_exceeds_grantor");
        assert_eq!(last.actor_id, caller.id());
    }

    // 绑定账户在授权者的 issue_credential 范围之外：out_of_scope（先于任何别的检查）
    let reason = failure_reason(issue_as(
        &env,
        &who,
        &Issue::service("school-b", vec![]).expiring(T0 + DAY_MS),
        T0 + 3,
    ));
    assert_eq!(reason, ControlReason::OutOfScope);
    // 绑定一个不存在的成员关系：同一个答案
    let reason = failure_reason(issue_as(&env, &who, &Issue::member("m-ghost"), T0 + 3));
    assert_eq!(reason, ControlReason::OutOfScope);
    // 成员凭证不能带管理权限（S0 的 Credential::new）
    let mut with_scopes = Issue::member("m-kid1").expiring(T0 + DAY_MS);
    with_scopes.scopes = vec![sc(ControlOp::Read, "s1", None)];
    assert_eq!(
        failure_reason(issue_as(&env, &who, &with_scopes, T0 + 3)),
        ControlReason::InvalidCredential
    );
}

#[test]
fn a_credential_that_may_only_grant_in_one_subtree_cannot_sign_anything_or_reach_outside() {
    // 越权签发的核心场景：一张只有「给 teacher 子树发额度（单笔 5 THB）」权限的凭证，
    // 尝试签更大的凭证、给子树外发额度、给自己加权限、签配对码，全部被拒，并且各留一行审计，什么都没有被创建。
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let grant_only = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::GrantAllocation, "teacher", Some(5_000_000))],
    ));
    let who = grant_only.who();
    let credentials_before: i64 = env
        .raw()
        .query_row("SELECT count(*) FROM credentials", [], |r| r.get(0))
        .unwrap();
    let audit_before = env.audit().len();
    let mut attempted = 0;
    let mut expect =
        |result: Result<WriteResult, ControlReason>, expected: ControlReason, what: &str| {
            attempted += 1;
            assert_eq!(result, Err(expected), "{what}");
        };
    // 签一张权限更大的凭证（绑自己的账户）：它没有 issue_credential 权限，连签发都不行
    let bigger = Issue::service("eduai", all_ops("eduai", 1_000_000_000));
    let key = MintedVirtualKey::mint();
    let rid = env.rid();
    expect(
        env.store
            .control_issue_credential(&who, &env.meta(T0 + 1, &rid), &bigger.request(&key))
            .map(|w| w.result)
            .map_err(|f| f.reason),
        ControlReason::OpNotGranted,
        "a bigger credential for its own org",
    );
    // 「给自己加权限」：签一张和自己一样的凭证再多一条权限，同样不行
    let mut superset = Issue::service(
        "eduai",
        vec![
            sc(ControlOp::GrantAllocation, "teacher", Some(5_000_000)),
            sc(ControlOp::Read, "teacher", None),
        ],
    );
    superset.tenant = Some("tenant-edu".into());
    let key = MintedVirtualKey::mint();
    let rid = env.rid();
    expect(
        env.store
            .control_issue_credential(&who, &env.meta(T0 + 1, &rid), &superset.request(&key))
            .map(|w| w.result)
            .map_err(|f| f.reason),
        ControlReason::OpNotGranted,
        "a superset of itself",
    );
    // 给成员签凭证（子树里）、签配对码：同样没有 issue_credential 权限
    expect(
        Action::IssueCredential {
            membership: "m-kid1",
        }
        .run(&env, &who, T0 + 1),
        ControlReason::OpNotGranted,
        "a member credential",
    );
    // 给子树外的账户发额度
    for (id, parent, child) in [
        ("o-1", "eduai", "s2"),
        ("o-2", "pool", "eduai"),
        ("o-3", "school-b", "b1"),
        ("o-4", "platform", "pool"),
    ] {
        expect(
            Action::GrantAllocation {
                id,
                parent,
                child,
                micros: 1,
            }
            .run(&env, &who, T0 + 1),
            ControlReason::OutOfScope,
            id,
        );
    }
    // 超过自己的单笔上限
    expect(
        Action::GrantAllocation {
            id: "o-5",
            parent: "teacher",
            child: "s1",
            micros: 5_000_001,
        }
        .run(&env, &who, T0 + 1),
        ControlReason::AmountExceedsAuthority,
        "over its cap",
    );
    // 别的操作：全部没有权限
    expect(
        Action::SetAccountStatus {
            account: "s1",
            status: AccountStatus::Frozen,
        }
        .run(&env, &who, T0 + 1),
        ControlReason::OpNotGranted,
        "freeze an account",
    );
    expect(
        Action::CreateAccount {
            id: "o-acct",
            parent: "teacher",
        }
        .run(&env, &who, T0 + 1),
        ControlReason::OpNotGranted,
        "create an account",
    );
    expect(
        Action::SetCredential {
            credential: grant_only.id().to_string(),
            status: CredentialStatus::Revoked,
        }
        .run(&env, &who, T0 + 1),
        ControlReason::OpNotGranted,
        "revoke itself",
    );
    // 全部被拒：每次一行 denied 审计，actor 是它；没有任何凭证、额度被创建
    let audit = env.audit();
    assert_eq!(audit.len(), audit_before + attempted);
    for row in &audit[audit_before..] {
        assert_eq!(
            (row.outcome.as_str(), row.actor_id.as_str()),
            ("denied", grant_only.id()),
            "{row:?}"
        );
    }
    let credentials_after: i64 = env
        .raw()
        .query_row("SELECT count(*) FROM credentials", [], |r| r.get(0))
        .unwrap();
    assert_eq!(credentials_after, credentials_before);
    // 对照：它自己的权限范围内，发额度是通的
    assert_eq!(
        Action::GrantAllocation {
            id: "ok-1",
            parent: "teacher",
            child: "s1",
            micros: 5_000_000
        }
        .run(&env, &who, T0 + 2),
        Ok(WriteResult::Created)
    );
}

#[test]
fn a_caller_cannot_add_authority_to_itself_or_widen_the_data_plane_ceilings() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let caller = middle(&env);
    let who = caller.who();
    let scopes_of = |env: &Env, id: &str| -> Vec<(String, String, Option<i64>)> {
        let conn = env.raw();
        let mut stmt = conn
            .prepare(
                "SELECT op, subtree_id, max_amount_micros FROM credential_admin_scopes \
                 WHERE credential_id = ?1 ORDER BY position",
            )
            .unwrap();
        stmt.query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = scopes_of(&env, caller.id());
    assert_eq!(before.len(), 4);

    // 给自己「加」权限：没有这样的操作；唯一的办法是签发一张更大的凭证（上面已被拒绝），
    // 数据库层同样拒绝给已有的凭证追加权限行。
    let raw = env.raw();
    let result = raw.execute(
        "INSERT INTO credential_admin_scopes (credential_id, position, op, subtree_id) \
         VALUES (?1, 4, 'revoke_credential', 'eduai')",
        [caller.id()],
    );
    assert!(result.is_err(), "scopes can only be written at issuance");
    let result = raw.execute(
        "UPDATE credential_admin_scopes SET subtree_id = 'pool' WHERE credential_id = ?1",
        [caller.id()],
    );
    assert!(result.is_err());
    assert_eq!(scopes_of(&env, caller.id()), before);

    // 数据面上限：不能比授权者宽
    let mut base = Issue::service("eduai", vec![]).expiring(T0 + 5 * DAY_MS);
    base.tenant = None; // 继承授权者的租户
    for (label, issue) in [
        (
            "wider privacy",
            base.clone()
                .privacy(&[PrivacyClass::LocalOnly, PrivacyClass::Any]),
        ),
        ("wider roles", base.clone().roles(&["fast", "deep"])),
        ("another tenant", base.clone().tenant(Some("tenant-b"))),
    ] {
        let before = env.audit().len();
        assert_eq!(
            failure_reason(issue_as(&env, &who, &issue, T0 + 1)),
            ControlReason::CeilingExceedsGrantor,
            "{label}"
        );
        assert_eq!(env.audit().len(), before + 1);
        assert_eq!(env.last_audit().reason_code, "ceiling_exceeds_grantor");
    }
    // 继承租户、更窄的上限：可以
    let narrower = base.clone().roles(&[]);
    let issued = issue_as(&env, &who, &narrower, T0 + 1).unwrap();
    let conn = env.raw();
    let (tenant, roles): (Option<String>, String) = conn
        .query_row(
            "SELECT tenant_id, allowed_roles FROM virtual_keys WHERE key_id = ?1",
            [issued.value.id().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        tenant.as_deref(),
        Some("tenant-edu"),
        "the tenant binding is inherited"
    );
    assert_eq!(roles, "[]");

    // 有效期：授权者有有效期，新凭证必须有，且不晚于它
    for (label, issue) in [
        ("no expiry", {
            let mut issue = base.clone();
            issue.expires = None;
            issue
        }),
        (
            "expires after the issuer",
            base.clone().expiring(T0 + 10 * DAY_MS + 1),
        ),
    ] {
        assert_eq!(
            failure_reason(issue_as(&env, &who, &issue, T0 + 1)),
            ControlReason::ExpiryExceedsGrantor,
            "{label}"
        );
    }
    // 刚好和授权者同一时刻到期：可以
    assert!(issue_as(&env, &who, &base.clone().expiring(T0 + 10 * DAY_MS), T0 + 1).is_ok());
    // 已经过期的新凭证没有意义
    assert_eq!(
        failure_reason(issue_as(&env, &who, &base.clone().expiring(T0), T0 + 1)),
        ControlReason::InvalidCredential
    );
}

#[test]
fn a_caller_without_a_tenant_binding_cannot_hand_one_out() {
    let env = Env::new();
    env.tree();
    let no_tenant = env.issue_as_operator(
        Issue::service("eduai", vec![sc(ControlOp::IssueCredential, "eduai", None)]).tenant(None),
    );
    let who = no_tenant.who();
    let wants_tenant = Issue::service("eduai", vec![]).tenant(Some("tenant-edu"));
    assert_eq!(
        failure_reason(issue_as(&env, &who, &wants_tenant, T0 + 1)),
        ControlReason::CeilingExceedsGrantor
    );
    assert!(
        issue_as(
            &env,
            &who,
            &Issue::service("eduai", vec![]).tenant(None),
            T0 + 1
        )
        .is_ok()
    );
}

#[test]
fn allocations_are_bounded_by_the_callers_cap_the_grantors_own_ceiling_and_the_subtree() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let caller = middle(&env);
    let who = caller.who();
    let run = |id: &'static str, parent: &'static str, child: &'static str, micros: i64| {
        Action::GrantAllocation {
            id,
            parent,
            child,
            micros,
        }
        .run(&env, &who, T0 + 1)
    };
    // teacher -> s1：上限内（caller 的上限 5 THB；teacher 自己拿到 100 THB）
    assert_eq!(
        run("g-1", "teacher", "s1", 5_000_000),
        Ok(WriteResult::Created)
    );
    // 超过 caller 的单笔上限 1 微单位
    assert_eq!(
        run("g-2", "teacher", "s1", 5_000_001),
        Err(ControlReason::AmountExceedsAuthority)
    );
    // 给子树外的账户发额度：授权方 eduai 不在 teacher 子树里（「给自己所在的组织发额度」的变种）
    assert_eq!(run("g-3", "eduai", "s2", 1), Err(ControlReason::OutOfScope));
    assert_eq!(
        run("g-4", "pool", "eduai", 1),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        run("g-5", "school-b", "b1", 1),
        Err(ControlReason::OutOfScope)
    );
    // 重发同一笔（相同内容）是 replayed；同一个 id 内容不同是 already_exists
    assert_eq!(
        run("g-1", "teacher", "s1", 5_000_000),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        run("g-1", "teacher", "s1", 4_000_000),
        Err(ControlReason::AlreadyExists)
    );

    // 授权方自己的额度是第二道上限（validate_allocation）：一个没有上限的调用者也不能让 teacher 发出比它拿到的更多
    let unlimited = env.issue_as_operator(Issue::service(
        "eduai",
        vec![AdminScope {
            op: ControlOp::GrantAllocation,
            subtree: aid("eduai"),
            max_amount: None,
        }],
    ));
    let who = unlimited.who();
    let big = Action::GrantAllocation {
        id: "g-6",
        parent: "teacher",
        child: "s1",
        micros: 100_000_001,
    }
    .run(&env, &who, T0 + 1);
    assert_eq!(big, Err(ControlReason::AllocationExceedsGrantor));
    // 授权方有流入额度、上限内：可以
    assert_eq!(
        Action::GrantAllocation {
            id: "g-7",
            parent: "eduai",
            child: "s2",
            micros: 1
        }
        .run(&env, &who, T0 + 1),
        Ok(WriteResult::Created)
    );
    // 「给自己所在的组织发额度」：接收方 eduai 在这个没有上限的调用者的子树里，但授权方（补贴池 / 平台）不在——
    // 授权看授权方，所以被拒，库里也没有多出任何额度。
    let allocations_before: i64 = env
        .raw()
        .query_row("SELECT count(*) FROM allocations", [], |r| r.get(0))
        .unwrap();
    for (id, parent, child) in [
        ("self-1", "pool", "eduai"),
        ("self-2", "platform", "pool"),
        ("self-3", "platform", "school-b"),
    ] {
        assert_eq!(
            Action::GrantAllocation {
                id,
                parent,
                child,
                micros: 1
            }
            .run(&env, &who, T0 + 1),
            Err(ControlReason::OutOfScope),
            "{parent} -> {child}"
        );
    }
    let allocations_after: i64 = env
        .raw()
        .query_row("SELECT count(*) FROM allocations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(allocations_after, allocations_before);
    // 每次拒绝都留审计
    let denied: Vec<_> = env
        .audit()
        .into_iter()
        .filter(|row| row.op == "grant_allocation" && row.outcome == "denied")
        .collect();
    assert!(denied.len() >= 4, "{denied:?}");
}

#[test]
fn free_tier_allocations_are_idempotent_per_member_and_month() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 100_000_000)));
    let who = root.who();
    // 免费额度的窗口必须恰好是这个月（账本时区，默认 UTC）：授权方 eduai 自己的额度要盖住整个月，
    // 补贴池要给这两个月各设总上限（没设就是 0，什么都发不出去）。
    const NOV_START: i64 = T0 - 12 * 3_600_000;
    const DEC_START: i64 = NOV_START + 30 * DAY_MS;
    const JAN_START: i64 = DEC_START + 31 * DAY_MS;
    for (id, parent, child, micros) in [
        ("a-pool-wide", "platform", "pool", 10_000_000_000_i64),
        ("a-edu-wide", "pool", "eduai", 1_000_000_000),
    ] {
        let rid = env.rid();
        env.store
            .control_grant_allocation(
                &Env::operator(),
                &env.meta(T0, &rid),
                &GrantAllocation {
                    valid_from: ts(T0 - DAY_MS),
                    valid_until: ts(T0 + 90 * DAY_MS),
                    ..grant(id, parent, child, micros)
                },
            )
            .unwrap();
    }
    for month in ["2026-11", "2026-12"] {
        env.store
            .set_pool_cap(
                &aid("pool"),
                month.parse::<BillingMonth>().unwrap(),
                &thb(100_000_000),
                ts(T0),
            )
            .unwrap();
    }
    let free = |id: &str, month: &str, micros: i64| {
        let mut request = grant(id, "eduai", "s2", micros);
        request.purpose = Purpose::FreeTier;
        request.month = Some(month.parse::<BillingMonth>().unwrap());
        (request.valid_from, request.valid_until) = match month {
            "2026-11" => (ts(NOV_START), ts(DEC_START)),
            _ => (ts(DEC_START), ts(JAN_START)),
        };
        request
    };
    let rid = env.rid();
    let first = env
        .store
        .control_grant_allocation(
            &who,
            &env.meta(T0, &rid),
            &free("f-1", "2026-11", 2_000_000),
        )
        .unwrap();
    assert_eq!(first.result, WriteResult::Created);
    // 同一个 (成员, 月份)、同样内容，换一个 id 重发：返回已有的那一笔，库里仍然只有一笔
    let rid = env.rid();
    let again = env
        .store
        .control_grant_allocation(
            &who,
            &env.meta(T0 + 1, &rid),
            &free("f-2", "2026-11", 2_000_000),
        )
        .unwrap();
    assert_eq!(again.result, WriteResult::Replayed);
    assert_eq!(again.value.allocation.id, first.value.allocation.id);
    // 同一个 id 再发一次（客户端重试同一个请求）
    let rid = env.rid();
    let same_id = env
        .store
        .control_grant_allocation(
            &who,
            &env.meta(T0 + 1, &rid),
            &free("f-1", "2026-11", 2_000_000),
        )
        .unwrap();
    assert_eq!(same_id.result, WriteResult::Replayed);
    // 同一个 (成员, 月份)、不同金额：幂等键冲突
    let rid = env.rid();
    assert_eq!(
        failure_reason(env.store.control_grant_allocation(
            &who,
            &env.meta(T0 + 1, &rid),
            &free("f-3", "2026-11", 3_000_000)
        )),
        ControlReason::IdempotencyConflict
    );
    // 另一个月份：新的一笔
    let rid = env.rid();
    assert_eq!(
        env.store
            .control_grant_allocation(
                &who,
                &env.meta(T0 + 1, &rid),
                &free("f-4", "2026-12", 2_000_000)
            )
            .unwrap()
            .result,
        WriteResult::Created
    );
    let conn = env.raw();
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM allocations WHERE purpose = 'free_tier'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 2);
    // 免费额度要 month；别的用途不能带 month
    let mut missing = grant("f-5", "eduai", "s2", 1);
    missing.purpose = Purpose::FreeTier;
    let rid = env.rid();
    assert_eq!(
        failure_reason(
            env.store
                .control_grant_allocation(&who, &env.meta(T0, &rid), &missing)
        ),
        ControlReason::InvalidRequest
    );
    let mut stray = grant("f-6", "eduai", "s2", 1);
    stray.month = Some("2026-11".parse().unwrap());
    let rid = env.rid();
    assert_eq!(
        failure_reason(
            env.store
                .control_grant_allocation(&who, &env.meta(T0, &rid), &stray)
        ),
        ControlReason::InvalidRequest
    );
}

#[test]
fn a_revoked_allocation_no_longer_authorizes_anything_and_cannot_be_revived() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 100_000_000)));
    let who = root.who();
    // teacher 流入额度 a-teacher（eduai -> teacher 100 THB）；teacher -> s1 依赖它
    assert_eq!(
        Action::GrantAllocation {
            id: "g-1",
            parent: "teacher",
            child: "s1",
            micros: 1_000_000
        }
        .run(&env, &who, T0 + 1),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        env.store.allocations_into(&aid("teacher")).unwrap().len(),
        1
    );
    // 撤销 a-teacher
    assert_eq!(
        Action::RevokeAllocation { id: "a-teacher" }.run(&env, &who, T0 + 2),
        Ok(WriteResult::Updated)
    );
    // 读函数不再返回它；带撤销时间的记录还在
    assert!(
        env.store
            .allocations_into(&aid("teacher"))
            .unwrap()
            .is_empty()
    );
    assert!(
        env.store
            .get_allocation(&AllocationId::new("a-teacher").unwrap())
            .unwrap()
            .is_none()
    );
    let record = env
        .store
        .allocation_record(&AllocationId::new("a-teacher").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.revoked_at_ms, Some(T0 + 2));
    // teacher 没有了授权方的额度：不能再往下发（validate_allocation 不再把被撤销的当作授权）
    assert_eq!(
        Action::GrantAllocation {
            id: "g-2",
            parent: "teacher",
            child: "s1",
            micros: 1
        }
        .run(&env, &root.who(), T0 + 3),
        Err(ControlReason::GrantorHasNoAuthority)
    );
    // 再撤销一次：unchanged，不是错误
    assert_eq!(
        Action::RevokeAllocation { id: "a-teacher" }.run(&env, &who, T0 + 4),
        Ok(WriteResult::Unchanged)
    );
    // 把 a-teacher 原样重发：不能让它复活
    assert_eq!(
        Action::GrantAllocation {
            id: "a-teacher",
            parent: "eduai",
            child: "teacher",
            micros: 100_000_000
        }
        .run(&env, &who, T0 + 5),
        Err(ControlReason::AllocationRevoked)
    );
    // 用别的 id 发一笔同样的新额度：是新的一笔，teacher 又有了授权方的额度
    assert_eq!(
        Action::GrantAllocation {
            id: "a-teacher-2",
            parent: "eduai",
            child: "teacher",
            micros: 100_000_000
        }
        .run(&env, &who, T0 + 6),
        Ok(WriteResult::Created)
    );
    // 撤销要有「发这笔额度」的资格：授权方 eduai 在 teacher 子树之外
    let teacher_only =
        env.issue_as_operator(Issue::service("eduai", all_ops("teacher", 100_000_000)));
    assert_eq!(
        Action::RevokeAllocation { id: "a-teacher-2" }.run(&env, &teacher_only.who(), T0 + 7),
        Err(ControlReason::OutOfScope)
    );
    // 额度大于调用者的单笔上限，也撤销不了（撤销的资格 = 发这笔额度的资格）
    let small = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    assert_eq!(
        Action::RevokeAllocation { id: "a-teacher-2" }.run(&env, &small.who(), T0 + 7),
        Err(ControlReason::AmountExceedsAuthority)
    );
    // 流入 eduai 本身的额度（补贴池 -> eduai）：接收方 eduai 在调用者的子树里，授权方补贴池不在——
    // 组织的管理凭证不能撤销（也不能自己发）补贴池给它的额度
    assert_eq!(
        Action::RevokeAllocation { id: "a-edu" }.run(&env, &who, T0 + 7),
        Err(ControlReason::OutOfScope)
    );
    assert!(
        env.store
            .get_allocation(&AllocationId::new("a-edu").unwrap())
            .unwrap()
            .is_some(),
        "the pool's allocation to the organization is untouched"
    );
    // 数据库层：撤销记录只追加
    let raw = env.raw();
    assert!(
        raw.execute("DELETE FROM allocation_revocations", [])
            .is_err()
    );
    assert!(
        raw.execute("UPDATE allocation_revocations SET revoked_at_ms = 0", [])
            .is_err()
    );
    // allocations 表本身一个字都没动
    let revoked_in_allocations: i64 = raw
        .query_row(
            "SELECT count(*) FROM allocations WHERE allocation_id = 'a-teacher'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revoked_in_allocations, 1);
}

#[test]
fn principal_revocation_cannot_reach_into_other_organizations_or_squatted_ids() {
    let env = Env::new();
    env.tree();
    let edu = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    // 别的组织不能撤销 eduai 的学生
    assert_eq!(
        Action::RevokePrincipal {
            principal: "kid1",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 1),
        Err(ControlReason::OutOfScope)
    );
    // 不存在的主体和范围外是同一个答案
    assert_eq!(
        Action::RevokePrincipal {
            principal: "ghost",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 1),
        Err(ControlReason::OutOfScope)
    );
    // 一个还没有成员关系的主体：只有最早登记它的组织能撤销
    assert_eq!(
        Action::CreatePrincipal {
            principal: "fresh",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 1),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        Action::CreatePrincipal {
            principal: "fresh",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 1),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "fresh",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 1),
        Err(ControlReason::OutOfScope)
    );
    // 别的组织也「登记」同一个 id：拿不到那个主体（也看不到它的状态），只有固定的「已存在」，登记人仍然是 eduai
    // （PR-Daemon S2 F3：旧代码这里返回 Replayed，原样把别的组织的主体交给了调用者）
    assert_eq!(
        Action::CreatePrincipal {
            principal: "fresh",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 2),
        Err(ControlReason::AlreadyExists)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "fresh",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 2),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "fresh",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 3),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "fresh",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 4),
        Ok(WriteResult::Unchanged)
    );
    // 已撤销的主体不能再建成员关系，也不会被「再次登记」复活
    assert_eq!(
        Action::CreatePrincipal {
            principal: "fresh",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 5),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        env.store
            .get_principal(&pid("fresh"))
            .unwrap()
            .unwrap()
            .status,
        idoris_compute_contracts::account::PrincipalStatus::Revoked
    );
    env.account("acct-fresh", AccountKind::Member, Some("eduai"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-fresh",
            principal: "fresh",
            org: "eduai",
            account: "acct-fresh"
        }
        .run(&env, &edu.who(), T0 + 6),
        Err(ControlReason::PrincipalRevoked)
    );
    // 一个主体在两个组织都有成员关系：一个组织不能把它整个撤销，只能撤销自己这边的成员关系。
    // 这个足迹要由对两边都有授权的一方（本机运维）建出来：school-b 自己给 eduai 在用的老主体 `kid2` 挂成员关系会被拒绝
    // （PR #600 第二轮 S2 F3，见 `another_organization_cannot_attach_itself_to_a_legacy_principal_in_use_elsewhere`）
    env.account("acct-shared", AccountKind::Member, Some("school-b"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-shared",
            principal: "kid2",
            org: "school-b",
            account: "acct-shared"
        }
        .run(&env, &Env::operator(), T0 + 7),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "kid2",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 8),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "kid2",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 8),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        Action::RevokeMembership { id: "m-shared" }.run(&env, &other.who(), T0 + 9),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        Action::RevokeMembership { id: "m-shared" }.run(&env, &other.who(), T0 + 10),
        Ok(WriteResult::Unchanged)
    );
    // 撤销自己这边之后，仍然有一个别的组织的成员关系（即使是已撤销的，也算「在用」）：eduai 还是不能整体撤销
    assert_eq!(
        Action::RevokePrincipal {
            principal: "kid2",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 11),
        Err(ControlReason::OutOfScope)
    );
}

/// 一个组织里登记的主体 `victim`（还没有成员关系）；返回两个组织各自的管理凭证。
fn principal_world(env: &Env) -> (Minted, Minted) {
    env.tree();
    let edu = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    assert_eq!(
        Action::CreatePrincipal {
            principal: "victim",
            org: "eduai"
        }
        .run(env, &edu.who(), T0 + 1),
        Ok(WriteResult::Created)
    );
    (edu, other)
}

fn admin_membership(id: &str, principal: &str, org: &str) -> Membership {
    Membership {
        id: mid(id),
        principal: pid(principal),
        org: aid(org),
        role: MembershipRole::Admin,
        account: None,
        status: MembershipStatus::Active,
    }
}

#[test]
fn an_outsider_cannot_tell_whether_a_principal_is_active_revoked_or_missing_when_attaching_a_membership()
 {
    // r1-M2：建成员关系时，主体状态如果在归属方授权之前返回，外部组织就能区分
    // 活跃（out_of_scope）、已撤销（principal_revoked）、不存在（invalid_membership）。
    // 三种失败必须逐字节相同；归属方自己仍然看得到原来的具体原因。
    let env = Env::new();
    let (edu, other) = principal_world(&env);
    assert_eq!(
        Action::CreatePrincipal {
            principal: "dead",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 2),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "dead",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 3),
        Ok(WriteResult::Updated)
    );
    env.account("acct-b", AccountKind::Member, Some("school-b"));

    let attach = |principal: &str, id: &str| {
        let rid = env.rid();
        env.store.control_create_membership(
            &other.who(),
            &env.meta(T0 + 4, &rid),
            &CreateMembership {
                membership: Membership {
                    id: mid(id),
                    principal: pid(principal),
                    org: aid("school-b"),
                    role: MembershipRole::Member,
                    account: Some(aid("acct-b")),
                    status: MembershipStatus::Active,
                },
            },
        )
    };
    let active = attach("victim", "m-active").expect_err("active principal");
    let revoked = attach("dead", "m-dead").expect_err("revoked principal");
    let missing = attach("ghost", "m-ghost").expect_err("missing principal");
    assert_eq!(active, Failure::new(ControlReason::OutOfScope));
    assert_eq!(active, revoked, "活跃和已撤销必须是同一个失败");
    assert_eq!(active, missing, "活跃和不存在必须是同一个失败");
    assert_eq!(active.to_string(), revoked.to_string());
    assert_eq!(active.to_string(), missing.to_string());

    env.account("acct-dead", AccountKind::Member, Some("eduai"));
    let rid = env.rid();
    let owner = env
        .store
        .control_create_membership(
            &edu.who(),
            &env.meta(T0 + 5, &rid),
            &CreateMembership {
                membership: Membership {
                    id: mid("m-owner-dead"),
                    principal: pid("dead"),
                    org: aid("eduai"),
                    role: MembershipRole::Member,
                    account: Some(aid("acct-dead")),
                    status: MembershipStatus::Active,
                },
            },
        )
        .expect_err("the owner still learns the concrete reason");
    assert_eq!(
        owner,
        Failure::with_detail(
            ControlReason::PrincipalRevoked,
            "principal dead has been revoked"
        )
    );

    let rid = env.rid();
    let operator = env
        .store
        .control_create_membership(
            &Env::operator(),
            &env.meta(T0 + 6, &rid),
            &CreateMembership {
                membership: admin_membership("m-op-ghost", "ghost", "school-b"),
            },
        )
        .expect_err("the operator still sees that the principal is not registered");
    assert_eq!(
        operator,
        Failure::with_detail(
            ControlReason::InvalidMembership,
            "principal ghost is not registered"
        )
    );
}

/// 在 [`principal_world`] 的基础上：eduai 另外登记并撤销了主体 `dead`（`victim` 活跃，`ghost` 不存在）；
/// school-b 有一个成员账户 `acct-b`。
fn principals_of_another_organization(env: &Env, edu: &Minted) {
    assert_eq!(
        Action::CreatePrincipal {
            principal: "dead",
            org: "eduai"
        }
        .run(env, &edu.who(), T0 + 2),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "dead",
            org: "eduai"
        }
        .run(env, &edu.who(), T0 + 3),
        Ok(WriteResult::Updated)
    );
    env.account("acct-b", AccountKind::Member, Some("school-b"));
}

fn member_membership(id: &str, principal: &str, org: &str, account: &str) -> Membership {
    Membership {
        id: mid(id),
        principal: pid(principal),
        org: aid(org),
        role: MembershipRole::Member,
        account: Some(aid(account)),
        status: MembershipStatus::Active,
    }
}

/// 调用一次建成员关系，返回失败（成功就 panic）。
fn attach_failure(env: &Env, who: &Who<'_>, now: i64, membership: Membership) -> Failure {
    let rid = env.rid();
    env.store
        .control_create_membership(who, &env.meta(now, &rid), &CreateMembership { membership })
        .expect_err("the attach must be refused")
}

/// PR #600 第四轮复审新问题 2（a）：成员关系的 `org` 是客户端提交的。school-b 的管理员提交
/// `role = member、account = school-b 自己的账户、org = eduai（别人的组织）、principal = eduai 登记的主体`：
/// 授权只看付款账户（school-b 自己的），通过；归属检查却相信客户端的 `org = eduai`，把「归属方是 eduai」当成
/// 「新成员关系就在归属方的子树里」，跳过了对 eduai 的授权，之后才报主体状态——活跃的主体得到 `invalid_membership`
/// （结构校验挡下来），已撤销的得到 `principal_revoked`，不存在的得到 `out_of_scope`。
/// 修复：先校验 `org` 与付款账户的关系（`org` 必须是付款账户的祖先），再谈主体；三种主体得到逐字节相同的失败。
#[test]
fn forging_the_org_field_does_not_reveal_whether_a_principal_is_active_revoked_or_missing() {
    let env = Env::new();
    let (edu, other) = principal_world(&env);
    principals_of_another_organization(&env, &edu);

    let attempt = |principal: &str, id: &str| {
        attach_failure(
            &env,
            &other.who(),
            T0 + 4,
            // org 是 eduai（伪造），付款账户是 school-b 自己的 acct-b
            member_membership(id, principal, "eduai", "acct-b"),
        )
    };
    let active = attempt("victim", "m-forged-active");
    let revoked = attempt("dead", "m-forged-dead");
    let missing = attempt("ghost", "m-forged-ghost");
    assert_eq!(
        active.reason,
        ControlReason::InvalidMembership,
        "the org is not above the account: a plain structural rejection"
    );
    assert_eq!(active, revoked, "活跃和已撤销必须是同一个失败");
    assert_eq!(active, missing, "活跃和不存在必须是同一个失败");
    assert_eq!(active.to_string(), revoked.to_string());
    assert_eq!(active.to_string(), missing.to_string());
    for failure in [&active, &revoked, &missing] {
        for name in ["victim", "dead", "ghost"] {
            assert!(
                !failure.detail.contains(name),
                "the detail must not echo the principal: {failure:?}"
            );
        }
    }
    // 审计行也一样：结果、原因码相同（只有请求里自己提交的主体 id 不同）
    let audit: Vec<(String, String)> = env
        .audit()
        .into_iter()
        .filter(|row| row.op == "create_membership")
        .map(|row| (row.outcome, row.reason_code))
        .collect();
    assert_eq!(audit.len(), 3);
    assert!(audit.iter().all(|row| *row == audit[0]), "{audit:?}");
    for id in ["m-forged-active", "m-forged-dead", "m-forged-ghost"] {
        assert!(env.store.get_membership(&mid(id)).unwrap().is_none());
    }

    // 管理员关系（没有付款账户）用组织授权：对 eduai 没有权限，三种主体同样是同一个 out_of_scope
    let admin = |principal: &str, id: &str| {
        attach_failure(
            &env,
            &other.who(),
            T0 + 5,
            admin_membership(id, principal, "eduai"),
        )
    };
    let (a, r, m) = (
        admin("victim", "m-admin-active"),
        admin("dead", "m-admin-dead"),
        admin("ghost", "m-admin-ghost"),
    );
    assert_eq!(a, Failure::new(ControlReason::OutOfScope));
    assert_eq!(a, r);
    assert_eq!(a, m);

    // 对照：自己的组织、自己的账户、自己登记的主体，照常建得出来；归属方自己照旧看得到具体原因
    assert_eq!(
        Action::CreatePrincipal {
            principal: "bee",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 6),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-bee",
            principal: "bee",
            org: "school-b",
            account: "acct-b"
        }
        .run(&env, &other.who(), T0 + 7),
        Ok(WriteResult::Created)
    );
    env.account("acct-dead", AccountKind::Member, Some("eduai"));
    let owner = attach_failure(
        &env,
        &edu.who(),
        T0 + 8,
        member_membership("m-owner-dead", "dead", "eduai", "acct-dead"),
    );
    assert_eq!(owner.reason, ControlReason::PrincipalRevoked);
}

/// PR #600 第四轮复审新问题 2（b）：只有 `manage_membership`、没有任何 `revoke_principal` 权限的合法凭证，
/// 即使提交真实的 `org`（自己的组织），归属方授权失败的内部错误也被直接透传：已有的外部主体得到 `op_not_granted`，
/// 不存在的得到 `out_of_scope`，调用方就能探测别的组织登记过哪些主体。主体可见性的授权失败一律是无细节的 `out_of_scope`。
#[test]
fn a_credential_with_only_manage_membership_cannot_tell_an_external_principal_from_a_missing_one() {
    let env = Env::new();
    let (edu, _other) = principal_world(&env);
    principals_of_another_organization(&env, &edu);
    // 没有登记记录、但已经被 eduai 在用的老主体（夹具里的 kid2）
    let narrow = env.issue_as_operator(Issue::service(
        "school-b",
        vec![sc(ControlOp::ManageMembership, "school-b", None)],
    ));
    // 对照：这张凭证连 `revoke_principal` 这个操作都没有，所以内部的授权判定会得到 `op_not_granted`
    assert_eq!(
        Action::RevokePrincipal {
            principal: "victim",
            org: "school-b"
        }
        .run(&env, &narrow.who(), T0 + 3),
        Err(ControlReason::OpNotGranted)
    );

    let attempt = |principal: &str, id: &str| {
        attach_failure(
            &env,
            &narrow.who(),
            T0 + 4,
            member_membership(id, principal, "school-b", "acct-b"),
        )
    };
    let expected = Failure::new(ControlReason::OutOfScope);
    for (principal, id) in [
        ("victim", "m-narrow-active"),
        ("dead", "m-narrow-dead"),
        ("ghost", "m-narrow-ghost"),
        ("kid2", "m-narrow-legacy"),
    ] {
        let failure = attempt(principal, id);
        assert_eq!(failure, expected, "{principal}");
        assert_eq!(failure.to_string(), expected.to_string(), "{principal}");
    }
    for id in [
        "m-narrow-active",
        "m-narrow-dead",
        "m-narrow-ghost",
        "m-narrow-legacy",
    ] {
        assert!(env.store.get_membership(&mid(id)).unwrap().is_none());
    }
    let audit: Vec<(String, String)> = env
        .audit()
        .into_iter()
        .filter(|row| row.op == "create_membership")
        .map(|row| (row.outcome, row.reason_code))
        .collect();
    assert_eq!(audit.len(), 4);
    assert!(
        audit
            .iter()
            .all(|row| *row == ("denied".to_string(), "out_of_scope".to_string())),
        "{audit:?}"
    );
}

#[test]
fn a_database_failure_while_checking_visibility_is_not_turned_into_a_plain_out_of_scope() {
    // 「调用者没有资格」这一类折成「看不见」；数据库忙、内部错误是基础设施问题，原样往上走，让调用方重试 / 告警，
    // 不能被悄悄当成「主体不在范围内」。
    use super::control::authority_is_visible;
    assert_eq!(authority_is_visible(Ok(())), Ok(true));
    for reason in [
        ControlReason::OpNotGranted,
        ControlReason::OutOfScope,
        ControlReason::AccountInactive,
        ControlReason::AmountExceedsAuthority,
    ] {
        assert_eq!(
            authority_is_visible::<()>(Err(Failure::new(reason))),
            Ok(false),
            "{reason}"
        );
    }
    for failure in [
        Failure::new(ControlReason::StoreBusy),
        Failure::internal("directory slice is inconsistent"),
        Failure::new(ControlReason::AuditUnavailable),
    ] {
        assert_eq!(
            authority_is_visible::<()>(Err(failure.clone())),
            Err(failure)
        );
    }
}

#[test]
fn another_organization_cannot_attach_itself_to_a_principal_it_did_not_register() {
    // Codex r1 #1 / PR-Daemon S2 F3：eduai 登记了主体 `victim`（还没有成员关系）。school-b 的管理员给它加一个属于
    // school-b 的 admin 成员关系——旧代码只检查目标账户的权限（school-b 自己的），放行；之后撤销时有了成员关系，
    // 就不再看登记它的组织，school-b 一个人就把 eduai 的主体永久撤销了。
    let env = Env::new();
    let (edu, other) = principal_world(&env);
    let rid = env.rid();
    let attach = env.store.control_create_membership(
        &other.who(),
        &env.meta(T0 + 2, &rid),
        &CreateMembership {
            membership: admin_membership("m-evil", "victim", "school-b"),
        },
    );
    assert_eq!(
        failure_reason(attach),
        ControlReason::OutOfScope,
        "跨组织关联已有主体需要对登记它的组织有授权"
    );
    assert_eq!(env.last_audit().outcome, "denied");
    assert!(env.store.get_membership(&mid("m-evil")).unwrap().is_none());
    // 登记它的组织自己（或它子树里的组织）仍然可以建
    let rid = env.rid();
    assert_eq!(
        env.store
            .control_create_membership(
                &edu.who(),
                &env.meta(T0 + 3, &rid),
                &CreateMembership {
                    membership: admin_membership("m-edu-admin", "victim", "eduai")
                },
            )
            .unwrap()
            .result,
        WriteResult::Created
    );
    // 对登记它的组织有授权的调用者（这里是本机运维）可以把主体关联到别的组织
    let rid = env.rid();
    assert_eq!(
        env.store
            .control_create_membership(
                &Env::operator(),
                &env.meta(T0 + 4, &rid),
                &CreateMembership {
                    membership: admin_membership("m-b-admin", "victim", "school-b")
                },
            )
            .unwrap()
            .result,
        WriteResult::Created
    );
}

#[test]
fn another_organization_cannot_attach_itself_to_a_legacy_principal_in_use_elsewhere() {
    // PR #600 第二轮 S2 F3（PARTIAL）：没有登记记录的老主体（存储层直接建的，比如夹具里 `kid2` 已经是 eduai 的成员）。
    // 上一轮对它整个跳过了「关联已有主体需要授权」：旧主体 `kid2` 已有 eduai 的成员关系时，school-b 的管理员仍能给它加一个
    // school-b 的成员关系，eduai 随后因为对 school-b 没有撤销权限，再也撤销不了它。
    // 没有登记记录就用已经在用它的组织当归属方：要给它加别的组织的成员关系，要么新成员关系就在归属方的子树里，
    // 要么调用者对归属方（已有成员关系的账户）有 `revoke_principal` 权限。
    let env = Env::new();
    env.tree();
    let edu = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    env.account("acct-b-new", AccountKind::Member, Some("school-b"));
    let attach = Action::CreateMembership {
        id: "m-evil",
        principal: "kid2",
        org: "school-b",
        account: "acct-b-new",
    };
    assert_eq!(
        attach.run(&env, &other.who(), T0 + 1),
        Err(ControlReason::OutOfScope),
        "school-b 不能给 eduai 在用的老主体挂自己的成员关系"
    );
    assert_eq!(env.last_audit().outcome, "denied");
    assert!(env.store.get_membership(&mid("m-evil")).unwrap().is_none());
    // Codex 的原场景是给它加一个 school-b 的 admin 关系（没有付款账户，授权看组织）：同样被拒
    let rid = env.rid();
    let admin_attach = env.store.control_create_membership(
        &other.who(),
        &env.meta(T0 + 1, &rid),
        &CreateMembership {
            membership: admin_membership("m-evil-admin", "kid2", "school-b"),
        },
    );
    assert_eq!(failure_reason(admin_attach), ControlReason::OutOfScope);
    assert!(
        env.store
            .get_membership(&mid("m-evil-admin"))
            .unwrap()
            .is_none()
    );
    // 归属方自己（它的子树里）当然可以再给它挂成员关系
    env.account("acct-edu-new", AccountKind::Member, Some("eduai"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-edu-second",
            principal: "kid2",
            org: "eduai",
            account: "acct-edu-new"
        }
        .run(&env, &edu.who(), T0 + 2),
        Ok(WriteResult::Created)
    );
    // 对归属方有授权的一方（本机运维）可以把它关联到别的组织
    assert_eq!(
        attach.run(&env, &Env::operator(), T0 + 3),
        Ok(WriteResult::Created)
    );
    // 没有任何组织在用、也没有登记记录的老主体没有归属方可保护：照旧谁都可以第一个挂上去
    env.store.ensure_principal(&pid("loose"), ts(T0)).unwrap();
    env.account("acct-b-loose", AccountKind::Member, Some("school-b"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-loose",
            principal: "loose",
            org: "school-b",
            account: "acct-b-loose"
        }
        .run(&env, &other.who(), T0 + 4),
        Ok(WriteResult::Created)
    );
    // 新的归属方一旦挂上去，eduai 也不能再把别的组织挂到这个主体上去
    env.account("acct-edu-loose", AccountKind::Member, Some("eduai"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-edu-loose",
            principal: "loose",
            org: "eduai",
            account: "acct-edu-loose"
        }
        .run(&env, &edu.who(), T0 + 5),
        Err(ControlReason::OutOfScope)
    );
}

#[test]
fn a_denied_attach_to_a_legacy_principal_leaves_its_owner_able_to_revoke_it() {
    // 上面那个测试的另一半：被拒的关联没有留下任何足迹，所以归属方照样能整体撤销主体
    let env = Env::new();
    env.tree();
    let edu = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    env.account("acct-b-new", AccountKind::Member, Some("school-b"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-evil",
            principal: "kid2",
            org: "school-b",
            account: "acct-b-new"
        }
        .run(&env, &other.who(), T0 + 1),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        Action::RevokePrincipal {
            principal: "kid2",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 2),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        env.store
            .get_principal(&pid("kid2"))
            .unwrap()
            .unwrap()
            .status,
        idoris_compute_contracts::account::PrincipalStatus::Revoked
    );
}

#[test]
fn revoking_a_principal_always_needs_authority_over_the_organization_that_registered_it() {
    // 就算一个别的组织的成员关系已经挂在主体上（比如运维建的、或旧数据），也不能靠它绕开登记组织的授权：
    // 有成员关系的主体，撤销时登记它的组织照样要检查（旧代码只在「没有任何成员关系」时才看登记组织）
    let env = Env::new();
    let (edu, other) = principal_world(&env);
    let rid = env.rid();
    env.store
        .control_create_membership(
            &Env::operator(),
            &env.meta(T0 + 2, &rid),
            &CreateMembership {
                membership: admin_membership("m-b-admin", "victim", "school-b"),
            },
        )
        .unwrap();
    assert_eq!(
        Action::RevokePrincipal {
            principal: "victim",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 3),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        env.store
            .get_principal(&pid("victim"))
            .unwrap()
            .unwrap()
            .status,
        idoris_compute_contracts::account::PrincipalStatus::Active
    );
    // 登记组织的凭证也不能越过别人的成员关系（一个组织不能撤销别的组织也在用的主体）
    assert_eq!(
        Action::RevokePrincipal {
            principal: "victim",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 4),
        Err(ControlReason::OutOfScope)
    );
    // 本机运维不受授权约束
    assert_eq!(
        Action::RevokePrincipal {
            principal: "victim",
            org: "eduai"
        }
        .run(&env, &Env::operator(), T0 + 5),
        Ok(WriteResult::Updated)
    );
}

#[test]
fn registering_an_existing_principal_never_discloses_another_organizations_principal_or_its_status()
{
    // PR-Daemon S2 F3 补充：重复登记一个别的组织登记的主体，旧代码原样返回它（带着状态）
    let env = Env::new();
    let (edu, other) = principal_world(&env);
    let create = |who: &Who<'_>, org: &'static str, now: i64| {
        Action::CreatePrincipal {
            principal: "victim",
            org,
        }
        .run(&env, who, now)
    };
    // 登记它的组织重发：照旧是 replayed
    assert_eq!(
        create(&edu.who(), "eduai", T0 + 2),
        Ok(WriteResult::Replayed)
    );
    // 别的组织：什么都不返回，只有固定的「已存在」，不带状态
    assert_eq!(
        create(&other.who(), "school-b", T0 + 3),
        Err(ControlReason::AlreadyExists)
    );
    // 主体被撤销之后，别的组织得到的答案一模一样（看不出状态）
    assert_eq!(
        Action::RevokePrincipal {
            principal: "victim",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 4),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        create(&other.who(), "school-b", T0 + 5),
        Err(ControlReason::AlreadyExists)
    );
    let rid = env.rid();
    let failure = env
        .store
        .control_create_principal(
            &other.who(),
            &env.meta(T0 + 6, &rid),
            &CreatePrincipal {
                principal: pid("victim"),
                org: aid("school-b"),
            },
        )
        .unwrap_err();
    assert!(
        failure.detail.is_empty(),
        "no detail that could reveal the principal: {failure:?}"
    );
    // 对登记组织有授权的调用者（eduai 对它子树里的 teacher 登记的主体）看得到
    assert_eq!(
        Action::CreatePrincipal {
            principal: "by-teacher",
            org: "teacher"
        }
        .run(&env, &edu.who(), T0 + 7),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        Action::CreatePrincipal {
            principal: "by-teacher",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 8),
        Ok(WriteResult::Replayed)
    );
    // 没有登记人的老主体（存储层直接建的）：HTTP 调用者拿不到它
    env.store.ensure_principal(&pid("legacy"), ts(T0)).unwrap();
    assert_eq!(
        Action::CreatePrincipal {
            principal: "legacy",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 9),
        Err(ControlReason::AlreadyExists)
    );
    assert_eq!(
        Action::CreatePrincipal {
            principal: "legacy",
            org: "eduai"
        }
        .run(&env, &Env::operator(), T0 + 9),
        Ok(WriteResult::Replayed)
    );
}

#[test]
fn an_unauthenticated_write_does_not_take_the_database_write_lock() {
    // PR-Daemon S2 低：认不出的密钥在鉴权之前就 `BEGIN IMMEDIATE` 拿了账本库的写锁，公网上的垃圾请求能和真实流量
    // 抢锁。现在先在只读事务里认人，认不出就直接返回，不碰写锁。另开一条连接握着写锁，短 busy_timeout：
    // 旧代码这里等不到锁，返回 store_busy；现在认不出的密钥是 unauthenticated，认得出的才需要写锁。
    let env = Env::new();
    env.tree();
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let store =
        AccountStore::open_with_busy_timeout(&env.path, std::time::Duration::from_millis(50))
            .unwrap();
    let holder = env.raw();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let unknown = MintedVirtualKey::mint();
    let rid = env.rid();
    let meta = env.meta(T0 + 1, &rid);
    let request = CreatePrincipal {
        principal: pid("someone"),
        org: aid("eduai"),
    };
    let failure = store
        .control_create_principal(&Who::Secret(&unknown.hash), &meta, &request)
        .unwrap_err();
    assert_eq!(failure.reason, ControlReason::Unauthenticated);
    // 对照：认得出的调用者要写，写锁被别人握着，就是 store_busy
    let failure = store
        .control_create_principal(&admin.who(), &meta, &request)
        .unwrap_err();
    assert_eq!(failure.reason, ControlReason::StoreBusy);
    holder.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        store
            .control_create_principal(&admin.who(), &meta, &request)
            .unwrap()
            .result,
        WriteResult::Created
    );
}

#[test]
fn an_account_status_change_cannot_disable_the_callers_own_binding_or_its_ancestors() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let who = root.who();
    for status in [AccountStatus::Frozen, AccountStatus::Closed] {
        // 自己绑定的组织
        assert_eq!(
            Action::SetAccountStatus {
                account: "eduai",
                status
            }
            .run(&env, &who, T0 + 1),
            Err(ControlReason::WouldDisableCaller)
        );
        // 祖先：不在权限范围里，先报 out_of_scope
        assert_eq!(
            Action::SetAccountStatus {
                account: "pool",
                status
            }
            .run(&env, &who, T0 + 1),
            Err(ControlReason::OutOfScope)
        );
    }
    // 子树里的账户：冻结、解冻、关闭（终态）
    assert_eq!(
        Action::SetAccountStatus {
            account: "s2",
            status: AccountStatus::Frozen
        }
        .run(&env, &who, T0 + 2),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        Action::SetAccountStatus {
            account: "s2",
            status: AccountStatus::Frozen
        }
        .run(&env, &who, T0 + 3),
        Ok(WriteResult::Unchanged)
    );
    assert_eq!(
        Action::SetAccountStatus {
            account: "s2",
            status: AccountStatus::Active
        }
        .run(&env, &who, T0 + 4),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        Action::SetAccountStatus {
            account: "s2",
            status: AccountStatus::Closed
        }
        .run(&env, &who, T0 + 5),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        Action::SetAccountStatus {
            account: "s2",
            status: AccountStatus::Active
        }
        .run(&env, &who, T0 + 6),
        Err(ControlReason::IllegalTransition)
    );
    // 组织被别人（运维）冻结之后，它自己的管理凭证不能解冻自己：先报 account_inactive
    env.store
        .set_account_status(&aid("eduai"), AccountStatus::Frozen)
        .unwrap();
    assert_eq!(
        Action::SetAccountStatus {
            account: "s1",
            status: AccountStatus::Frozen
        }
        .run(&env, &who, T0 + 7),
        Err(ControlReason::AccountInactive)
    );
    assert_eq!(env.last_audit().reason_code, "account_inactive");
}

#[test]
fn accounts_and_memberships_are_idempotent_and_validated_by_the_contract() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let who = root.who();
    assert_eq!(
        Action::CreateAccount {
            id: "new-1",
            parent: "eduai"
        }
        .run(&env, &who, T0 + 1),
        Ok(WriteResult::Created)
    );
    // 同样的重发是 replayed；同一个 id 不同定义是 already_exists
    assert_eq!(
        Action::CreateAccount {
            id: "new-1",
            parent: "eduai"
        }
        .run(&env, &who, T0 + 2),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        Action::CreateAccount {
            id: "new-1",
            parent: "teacher"
        }
        .run(&env, &who, T0 + 3),
        Err(ControlReason::AlreadyExists)
    );
    // 另一个组织已经有的 id：同样是 already_exists（全局唯一的 id）
    assert_eq!(
        Action::CreateAccount {
            id: "b1",
            parent: "eduai"
        }
        .run(&env, &who, T0 + 3),
        Err(ControlReason::AlreadyExists)
    );
    // S0 的账户树校验：成员账户下面不能再挂账户（叶子）
    assert_eq!(
        Action::CreateAccount {
            id: "under-leaf",
            parent: "s2"
        }
        .run(&env, &who, T0 + 4),
        Err(ControlReason::InvalidAccount)
    );
    let last = env.last_audit();
    assert_eq!(
        (last.outcome.as_str(), last.reason_code.as_str()),
        ("rejected", "invalid_account")
    );
    // 新建账户不能一出生就是冻结的
    let rid = env.rid();
    let frozen = env.store.control_create_account(
        &who,
        &env.meta(T0 + 5, &rid),
        &CreateAccount {
            account: Account {
                id: aid("born-frozen"),
                kind: AccountKind::Member,
                parent: Some(aid("eduai")),
                unit: Unit::Thb,
                status: AccountStatus::Frozen,
            },
        },
    );
    assert_eq!(failure_reason(frozen), ControlReason::InvalidAccount);
    // 没有凭证能建平台根（没有父账户）；运维可以
    let rid = env.rid();
    let root_account = Account {
        id: aid("second-root"),
        kind: AccountKind::Platform,
        parent: None,
        unit: Unit::Thb,
        status: AccountStatus::Active,
    };
    assert_eq!(
        failure_reason(env.store.control_create_account(
            &who,
            &env.meta(T0 + 6, &rid),
            &CreateAccount {
                account: root_account.clone()
            }
        )),
        ControlReason::OutOfScope
    );
    let rid = env.rid();
    assert_eq!(
        failure_reason(env.store.control_create_account(
            &Env::operator(),
            &env.meta(T0 + 6, &rid),
            &CreateAccount {
                account: root_account
            }
        )),
        // 平台根已经有一个了：S0 的单根规则
        ControlReason::InvalidAccount
    );
    // 成员关系：先建账户、再建成员关系；主体必须已经登记
    assert_eq!(
        Action::CreateMembership {
            id: "m-new",
            principal: "kid1",
            org: "eduai",
            account: "new-1"
        }
        .run(&env, &who, T0 + 7),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-new",
            principal: "kid1",
            org: "eduai",
            account: "new-1"
        }
        .run(&env, &who, T0 + 8),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-new",
            principal: "kid2",
            org: "eduai",
            account: "new-1"
        }
        .run(&env, &who, T0 + 9),
        Err(ControlReason::AlreadyExists)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-ghost",
            principal: "nobody",
            org: "eduai",
            account: "m-acct-x"
        }
        .run(&env, &who, T0 + 10),
        // 账户不存在：授权先看（不存在 = 范围外）
        Err(ControlReason::OutOfScope)
    );
    env.account("acct-y", AccountKind::Member, Some("eduai"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-ghost",
            principal: "nobody",
            org: "eduai",
            account: "acct-y"
        }
        .run(&env, &who, T0 + 11),
        // 主体不存在，而这张凭证不是它的归属方（不存在的主体没有归属方）：和「活跃 / 已撤销但不归自己」
        // 同一个 out_of_scope。原来这里是 invalid_membership（「principal nobody is not registered」），
        // 外部组织能靠它判断主体有没有登记（r1-M2）。本机运维仍然看得到那句细节。
        Err(ControlReason::OutOfScope)
    );
    // 一个付款账户只能属于一个成员关系
    assert_eq!(
        Action::CreateMembership {
            id: "m-dup",
            principal: "kid2",
            org: "eduai",
            account: "new-1"
        }
        .run(&env, &who, T0 + 12),
        Err(ControlReason::InvalidMembership)
    );
    // 撤销成员关系
    assert_eq!(
        Action::RevokeMembership { id: "m-new" }.run(&env, &who, T0 + 13),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        Action::RevokeMembership { id: "m-kb1" }.run(&env, &who, T0 + 14),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        Action::RevokeMembership { id: "m-ghost" }.run(&env, &who, T0 + 14),
        Err(ControlReason::OutOfScope)
    );
}

#[test]
fn accounts_cannot_be_created_under_a_frozen_or_closed_parent() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let who = root.who();
    for status in [AccountStatus::Frozen, AccountStatus::Closed] {
        let id = if status == AccountStatus::Frozen {
            "under-frozen"
        } else {
            "under-closed"
        };
        env.store.set_account_status(&aid("s2"), status).unwrap();
        env.store
            .set_account_status(&aid("teacher"), status)
            .unwrap();
        assert_eq!(
            Action::CreateAccount {
                id,
                parent: "teacher"
            }
            .run(&env, &who, T0 + 1),
            Err(ControlReason::InvalidAccount),
            "{status:?}"
        );
        let last = env.last_audit();
        assert_eq!(
            (last.outcome.as_str(), last.reason_code.as_str()),
            ("rejected", "invalid_account")
        );
        if status == AccountStatus::Frozen {
            env.store
                .set_account_status(&aid("teacher"), AccountStatus::Active)
                .unwrap();
            env.store
                .set_account_status(&aid("s2"), AccountStatus::Active)
                .unwrap();
            assert_eq!(
                Action::CreateAccount {
                    id,
                    parent: "teacher"
                }
                .run(&env, &who, T0 + 2),
                Ok(WriteResult::Created),
                "thawed"
            );
        }
    }
}

#[test]
fn revoking_only_the_key_verifier_with_the_local_command_still_blocks_the_credential() {
    // `idoris key revoke <key_id>` 只撤销 virtual_keys 里的验证器，不动 credentials 行。
    let env = Env::new();
    env.tree();
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let device = env.issue_as_operator(Issue::member("m-kid1"));
    assert!(env.keys.revoke(device.id(), T0 + 5).unwrap());
    // 数据面：虚拟 key 认证已经认不出它
    assert!(
        env.keys
            .authenticate(&device.key.secret, T0 + 6)
            .unwrap()
            .is_none()
    );
    // 控制面：同样当作已撤销（并留审计）
    assert_eq!(
        Action::Read { account: "eduai" }.run(&env, &device.who(), T0 + 6),
        Err(ControlReason::CredentialRevoked)
    );
    // 查询里两个状态都看得到：凭证行还是 active，验证器已撤销
    let view = env
        .store
        .control_read_credentials(&admin.who(), ts(T0 + 7), &aid("s1"))
        .unwrap();
    let entry = view
        .credentials
        .iter()
        .find(|c| c.credential.id().as_str() == device.id())
        .unwrap();
    assert_eq!(entry.credential.status(), CredentialStatus::Active);
    assert_eq!(entry.key_status, "revoked");
    let other = view
        .credentials
        .iter()
        .find(|c| c.credential.id().as_str() != device.id());
    assert!(other.is_none_or(|c| c.key_status == "active"));
}

#[test]
fn credentials_can_be_frozen_unfrozen_and_revoked_and_revocation_is_final() {
    let env = Env::new();
    env.tree();
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let device = env.issue_as_operator(Issue::member("m-kid1"));
    let who = admin.who();
    let set = |status| Action::SetCredential {
        credential: device.id().to_string(),
        status,
    };
    // 冻结之前，设备凭证本人可以问「我是谁」（whoami 不需要管理权限，只要凭证此刻可用）
    assert!(env.store.control_whoami(&device.who(), ts(T0)).is_ok());
    assert_eq!(
        set(CredentialStatus::Frozen).run(&env, &who, T0 + 1),
        Ok(WriteResult::Updated)
    );
    // 冻结之后：连 whoami 也被拒（不依赖授权判定，认证这一步就拒绝）
    assert_eq!(
        env.store
            .control_whoami(&device.who(), ts(T0 + 1))
            .unwrap_err()
            .reason,
        ControlReason::CredentialFrozen
    );
    assert_eq!(
        set(CredentialStatus::Frozen).run(&env, &who, T0 + 2),
        Ok(WriteResult::Unchanged)
    );
    assert_eq!(
        set(CredentialStatus::Active).run(&env, &who, T0 + 3),
        Ok(WriteResult::Updated)
    );
    assert!(env.store.control_whoami(&device.who(), ts(T0 + 3)).is_ok());
    // 过期同样：认证这一步就拒绝（右端开）
    let expiring = env.issue_as_operator(Issue::member("m-kid2").expiring(T0 + 1_000));
    assert!(
        env.store
            .control_whoami(&expiring.who(), ts(T0 + 999))
            .is_ok()
    );
    assert_eq!(
        env.store
            .control_whoami(&expiring.who(), ts(T0 + 1_000))
            .unwrap_err()
            .reason,
        ControlReason::CredentialExpired
    );
    // 撤销：数据库里的验证器同时标成已撤销（数据面认证直接认不出它）
    assert!(
        env.keys
            .authenticate(&device.key.secret, T0 + 3)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        set(CredentialStatus::Revoked).run(&env, &who, T0 + 4),
        Ok(WriteResult::Updated)
    );
    assert!(
        env.keys
            .authenticate(&device.key.secret, T0 + 5)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        set(CredentialStatus::Revoked).run(&env, &who, T0 + 6),
        Ok(WriteResult::Unchanged)
    );
    // 撤销是终态
    assert_eq!(
        set(CredentialStatus::Active).run(&env, &who, T0 + 7),
        Err(ControlReason::IllegalTransition)
    );
    assert_eq!(
        set(CredentialStatus::Frozen).run(&env, &who, T0 + 8),
        Err(ControlReason::IllegalTransition)
    );
    // 一张凭证可以撤销自己（有权限的话）；撤销之后这张凭证调用什么都被拒绝，并留下审计
    assert_eq!(
        Action::SetCredential {
            credential: admin.id().to_string(),
            status: CredentialStatus::Revoked
        }
        .run(&env, &who, T0 + 9),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        Action::Read { account: "eduai" }.run(&env, &who, T0 + 10),
        Err(ControlReason::CredentialRevoked)
    );
    assert_eq!(
        Action::CreateAccount {
            id: "late",
            parent: "eduai"
        }
        .run(&env, &who, T0 + 10),
        Err(ControlReason::CredentialRevoked)
    );
    let last = env.last_audit();
    assert_eq!(
        (last.actor_id.as_str(), last.outcome.as_str()),
        (admin.id(), "denied")
    );
}

#[test]
fn a_local_data_key_without_a_credential_row_has_no_control_authority() {
    let env = Env::new();
    env.tree();
    // 用 `idoris key issue` 同样的方式签发一把本地数据密钥：virtual_keys 里有，credentials 里没有
    let key = MintedVirtualKey::mint();
    env.keys
        .insert_active(
            &key.key_id,
            key.hash,
            &crate::virtual_key::store::VirtualKeyScope {
                owner: "local".into(),
                tenant_id: Some("tenant-edu".into()),
                allowed_privacy: vec![PrivacyClass::LocalOnly],
                allowed_roles: vec!["fast".into()],
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    let who = Who::Secret(&key.hash);
    assert_eq!(
        Action::Read { account: "eduai" }.run(&env, &who, T0),
        Err(ControlReason::NotAControlCredential)
    );
    assert_eq!(
        Action::CreateAccount {
            id: "x",
            parent: "eduai"
        }
        .run(&env, &who, T0),
        Err(ControlReason::NotAControlCredential)
    );
    let last = env.last_audit();
    assert_eq!(last.actor_id, key.key_id);
    assert_eq!(last.reason_code, "not_a_control_credential");
    // 完全不认识的密钥：unauthenticated，而且**不写审计**（没有凭证 id，不能让垃圾请求把库写满）
    let stranger = MintedVirtualKey::mint();
    let before = env.audit().len();
    for _ in 0..5 {
        assert_eq!(
            Action::CreateAccount {
                id: "y",
                parent: "eduai"
            }
            .run(&env, &Who::Secret(&stranger.hash), T0),
            Err(ControlReason::Unauthenticated)
        );
    }
    assert_eq!(env.audit().len(), before);
}

#[test]
fn the_operator_path_creates_the_platform_root_and_is_audited_as_an_operator() {
    let env = Env::new();
    let operator = Env::operator();
    let rid = env.rid();
    let written = env
        .store
        .control_create_account(
            &operator,
            &env.meta(T0, &rid),
            &CreateAccount {
                account: Account {
                    id: aid("platform"),
                    kind: AccountKind::Platform,
                    parent: None,
                    unit: Unit::Thb,
                    status: AccountStatus::Active,
                },
            },
        )
        .unwrap();
    assert_eq!(written.result, WriteResult::Created);
    let last = env.last_audit();
    assert_eq!(last.actor_kind, "operator");
    assert_eq!(last.actor_id, "test-operator");
    assert_eq!(last.op, "create_account");
    assert_eq!(last.scope_account, None);
}

// ================================================================================================
// 审计：只追加、被拒也有、没有任何密钥
// ================================================================================================

#[test]
fn the_audit_table_rejects_update_delete_and_replace() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    Action::CreateAccount {
        id: "n1",
        parent: "eduai",
    }
    .run(&env, &root.who(), T0 + 1)
    .unwrap();
    Action::CreateAccount {
        id: "n2",
        parent: "school-b",
    }
    .run(&env, &root.who(), T0 + 2)
    .unwrap_err();
    let raw = env.raw();
    let count = |sql: &str| -> i64 { raw.query_row(sql, [], |row| row.get(0)).unwrap() };
    let rows = count("SELECT count(*) FROM control_audit");
    assert!(rows >= 2);
    let snapshot = |raw: &Connection| -> String {
        let mut stmt = raw
            .prepare("SELECT audit_id || '|' || at_ms || '|' || actor_id || '|' || op || '|' || outcome || '|' || reason_code FROM control_audit ORDER BY audit_id")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
    };
    let before = snapshot(&raw);
    for (label, sql) in [
        (
            "update outcome",
            "UPDATE control_audit SET outcome = 'success'",
        ),
        (
            "update one row",
            "UPDATE control_audit SET reason_code = 'ok' WHERE audit_id = 1",
        ),
        (
            "update the id",
            "UPDATE control_audit SET audit_id = audit_id + 1000",
        ),
        ("delete all", "DELETE FROM control_audit"),
        ("delete one", "DELETE FROM control_audit WHERE audit_id = 1"),
        (
            "replace an existing row",
            "INSERT OR REPLACE INTO control_audit (audit_id, at_ms, request_id, actor_kind, actor_id, op, target_kind, outcome, reason_code) \
             VALUES (1, 1, 'r', 'operator', 'cli', 'forged', 'none', 'success', 'ok')",
        ),
        (
            "insert with an explicit id",
            "INSERT INTO control_audit (audit_id, at_ms, request_id, actor_kind, actor_id, op, target_kind, outcome, reason_code) \
             VALUES (9999, 1, 'r', 'operator', 'cli', 'forged', 'none', 'success', 'ok')",
        ),
        (
            "insert with id zero",
            "INSERT INTO control_audit (audit_id, at_ms, request_id, actor_kind, actor_id, op, target_kind, outcome, reason_code) \
             VALUES (0, 1, 'r', 'operator', 'cli', 'forged', 'none', 'success', 'ok')",
        ),
        (
            "insert or ignore with an existing id",
            "INSERT OR IGNORE INTO control_audit (audit_id, at_ms, request_id, actor_kind, actor_id, op, target_kind, outcome, reason_code) \
             VALUES (1, 1, 'r', 'operator', 'cli', 'forged', 'none', 'success', 'ok')",
        ),
    ] {
        assert!(
            raw.execute(sql, []).is_err(),
            "{label} must be refused by the database"
        );
    }
    assert_eq!(snapshot(&raw), before, "nothing changed");
    // 对照组：正常追加（数据库分配行号）能写进去，说明上面的失败确实来自触发器
    raw.execute(
        "INSERT INTO control_audit (at_ms, request_id, actor_kind, actor_id, op, target_kind, outcome, reason_code) \
         VALUES (1, 'r', 'operator', 'cli', 'append', 'none', 'success', 'ok')",
        [],
    )
    .unwrap();
    assert_eq!(count("SELECT count(*) FROM control_audit"), rows + 1);
    // 结果和原因码必须一致：成功类只能是 ok，失败类不能是 ok
    assert!(raw
        .execute(
            "INSERT INTO control_audit (at_ms, request_id, actor_kind, actor_id, op, target_kind, outcome, reason_code) \
             VALUES (1, 'r', 'operator', 'cli', 'x', 'none', 'denied', 'ok')",
            [],
        )
        .is_err());
    // 形状：操作名只能是 [a-z_]，标识符里不能有空白 / @ / NUL
    for bad in ["Create", "create account", "a@b", "a\0b"] {
        assert!(
            raw.execute(
                "INSERT INTO control_audit (at_ms, request_id, actor_kind, actor_id, op, target_kind, outcome, reason_code) \
                 VALUES (1, 'r', 'operator', 'cli', ?1, 'none', 'success', 'ok')",
                [bad],
            )
            .is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn a_write_whose_audit_row_cannot_be_written_does_not_happen() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    // 让审计写入失败：另一条连接给审计表加一个总是报错的触发器
    env.raw()
        .execute_batch(
            "CREATE TRIGGER sabotage BEFORE INSERT ON control_audit \
             BEGIN SELECT RAISE(ABORT, 'audit is down'); END;",
        )
        .unwrap();
    let before_accounts: i64 = env
        .raw()
        .query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        Action::CreateAccount {
            id: "should-not-exist",
            parent: "eduai"
        }
        .run(&env, &root.who(), T0 + 1),
        Err(ControlReason::AuditUnavailable)
    );
    let after_accounts: i64 = env
        .raw()
        .query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after_accounts, before_accounts, "no audit, no write");
    assert!(
        env.store
            .get_account(&aid("should-not-exist"))
            .unwrap()
            .is_none()
    );
    // 被拒的操作同样：审计写不进去就报 audit_unavailable
    assert_eq!(
        Action::CreateAccount {
            id: "x",
            parent: "school-b"
        }
        .run(&env, &root.who(), T0 + 2),
        Err(ControlReason::AuditUnavailable)
    );
    // 修好之后恢复正常
    env.raw().execute_batch("DROP TRIGGER sabotage;").unwrap();
    assert_eq!(
        Action::CreateAccount {
            id: "now-fine",
            parent: "eduai"
        }
        .run(&env, &root.who(), T0 + 3),
        Ok(WriteResult::Created)
    );
}

#[test]
fn a_rejected_operation_leaves_no_partial_write_but_is_audited() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    // 签发一张注定失败的凭证：数据面验证器已经写进去、凭证元数据却因为 S1 的校验失败（把 Service 绑到不是 Org 的账户上）。
    // 授权先通过（teacher 在 eduai 的子树里），失败发生在写入中途；保存点回滚后 virtual_keys 里不能留下孤儿验证器。
    let key = MintedVirtualKey::mint();
    let rid = env.rid();
    let mut issue = Issue::service("teacher", vec![]);
    issue.tenant = None;
    let result = env.store.control_issue_credential(
        &root.who(),
        &env.meta(T0 + 1, &rid),
        &issue.request(&key),
    );
    assert_eq!(failure_reason(result), ControlReason::InvalidCredential);
    let orphan: i64 = env
        .raw()
        .query_row(
            "SELECT count(*) FROM virtual_keys WHERE key_id = ?1",
            [&key.key_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        orphan, 0,
        "the verifier written before the failure was rolled back"
    );
    assert!(
        env.keys
            .authenticate(&key.secret, T0 + 1)
            .unwrap()
            .is_none()
    );
    let last = env.last_audit();
    assert_eq!(
        (last.outcome.as_str(), last.reason_code.as_str()),
        ("rejected", "invalid_credential")
    );
    assert_eq!(last.target_id.as_deref(), Some(key.key_id.as_str()));
}

#[test]
fn the_audit_log_never_contains_a_secret_a_hash_or_a_device_label() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 100_000_000)));
    let mut secrets: Vec<String> = vec![root.secret().to_string()];
    // 一批成功和失败的操作，其中签发的每张凭证的密钥都收集起来
    for i in 0..4 {
        let issued = MintedVirtualKey::mint();
        let rid = env.rid();
        let issue = Issue::member("m-kid1");
        let result = env.store.control_issue_credential(
            &root.who(),
            &env.meta(T0 + i, &rid),
            &issue.request(&issued),
        );
        assert!(result.is_ok());
        secrets.push(issued.secret.expose_secret().to_string());
        secrets.push(format!("{:?}", issued.hash.as_bytes()));
        let hex: String = issued
            .hash
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        secrets.push(hex);
    }
    for (account, parent) in [("a1", "eduai"), ("a2", "school-b"), ("s2", "eduai")] {
        let _ = Action::CreateAccount {
            id: account,
            parent,
        }
        .run(&env, &root.who(), T0 + 10);
    }
    let stranger = MintedVirtualKey::mint();
    let _ = Action::Read { account: "eduai" }.run(&env, &Who::Secret(&stranger.hash), T0);
    // 把整张审计表的每一个单元格拼成一个大字符串，逐个密钥搜索
    let conn = env.raw();
    let mut stmt = conn.prepare("SELECT * FROM control_audit").unwrap();
    let columns = stmt.column_count();
    let mut dump = String::new();
    let mut rows = stmt.query([]).unwrap();
    let mut row_count = 0;
    while let Some(row) = rows.next().unwrap() {
        row_count += 1;
        for index in 0..columns {
            let cell: rusqlite::types::Value = row.get(index).unwrap();
            dump.push_str(&format!("{cell:?}|"));
        }
        dump.push('\n');
    }
    assert!(row_count >= 8, "{row_count}");
    for secret in &secrets {
        assert!(
            !dump.contains(secret.as_str()),
            "the audit log contains a secret"
        );
    }
    assert!(!dump.contains("idk_"), "no bearer token text of any kind");
    assert!(!dump.contains("Bearer"));
    // 失败原因里也不带账户之外的细节文本：reason_code 列只有稳定原因码
    let reasons: Vec<String> = conn
        .prepare("SELECT DISTINCT reason_code FROM control_audit")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for reason in reasons {
        assert!(
            reason == "ok" || reason.parse::<ControlReason>().is_ok(),
            "unexpected reason text {reason:?}"
        );
    }
}

#[test]
#[allow(clippy::type_complexity)]
fn every_audit_row_carries_who_what_target_result_reason_and_time() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    Action::CreateAccount {
        id: "n1",
        parent: "eduai",
    }
    .run(&env, &root.who(), T0 + 5)
    .unwrap();
    Action::CreateAccount {
        id: "n2",
        parent: "school-b",
    }
    .run(&env, &root.who(), T0 + 6)
    .unwrap_err();
    let conn = env.raw();
    let rows: Vec<(i64, String, String, String, String, Option<String>, Option<String>, String, String)> = conn
        .prepare(
            "SELECT at_ms, request_id, actor_kind, actor_id, op, target_id, scope_account, outcome, reason_code \
             FROM control_audit WHERE op = 'create_account' ORDER BY audit_id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, T0 + 5);
    assert_eq!(rows[0].2, "credential");
    assert_eq!(rows[0].3, root.id());
    assert_eq!(rows[0].5.as_deref(), Some("n1"));
    assert_eq!(rows[0].6.as_deref(), Some("eduai"));
    assert_eq!((rows[0].7.as_str(), rows[0].8.as_str()), ("success", "ok"));
    assert_eq!(rows[1].0, T0 + 6);
    assert_eq!(rows[1].5.as_deref(), Some("n2"));
    assert_eq!(rows[1].6.as_deref(), Some("school-b"));
    assert_eq!(
        (rows[1].7.as_str(), rows[1].8.as_str()),
        ("denied", "out_of_scope")
    );
    assert_ne!(
        rows[0].1, rows[1].1,
        "each request has its own correlation id"
    );
}

// ================================================================================================
// 读
// ================================================================================================

#[test]
fn reads_are_limited_to_the_callers_read_subtree() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let reader = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::Read, "eduai", None)],
    ));
    let who = reader.who();
    let now = ts(T0 + 1);
    let view = env
        .store
        .control_read_account(&who, now, &aid("eduai"))
        .unwrap();
    assert_eq!(view.account.id, aid("eduai"));
    assert_eq!(view.children, vec![aid("s2"), aid("teacher")]);
    let tree = env
        .store
        .control_read_subtree(&who, now, &aid("eduai"))
        .unwrap();
    let ids: Vec<_> = tree.accounts.iter().map(|a| a.id.to_string()).collect();
    assert_eq!(ids, ["eduai", "s2", "teacher", "s1"]);
    assert!(!tree.truncated);
    let allocations = env
        .store
        .control_read_allocations(&who, now, &aid("teacher"))
        .unwrap();
    assert_eq!(allocations.inbound.len(), 1);
    let memberships = env
        .store
        .control_read_memberships(&who, now, &aid("eduai"))
        .unwrap();
    assert_eq!(memberships.memberships.len(), 3);
    let credentials = env
        .store
        .control_read_credentials(&who, now, &aid("eduai"))
        .unwrap();
    assert!(
        credentials
            .credentials
            .iter()
            .any(|c| c.credential.id().as_str() == reader.id())
    );
    assert_eq!(
        env.store
            .control_read_principal(&who, now, &pid("kid1"))
            .unwrap()
            .id,
        pid("kid1")
    );
    // 范围外
    for account in ["school-b", "b1", "pool", "platform", "ghost"] {
        let id = aid(account);
        assert_eq!(
            env.store
                .control_read_account(&who, now, &id)
                .unwrap_err()
                .reason,
            ControlReason::OutOfScope
        );
        assert_eq!(
            env.store
                .control_read_subtree(&who, now, &id)
                .unwrap_err()
                .reason,
            ControlReason::OutOfScope
        );
        assert_eq!(
            env.store
                .control_read_allocations(&who, now, &id)
                .unwrap_err()
                .reason,
            ControlReason::OutOfScope
        );
        assert_eq!(
            env.store
                .control_read_credentials(&who, now, &id)
                .unwrap_err()
                .reason,
            ControlReason::OutOfScope
        );
        assert_eq!(
            env.store
                .control_read_memberships(&who, now, &id)
                .unwrap_err()
                .reason,
            ControlReason::OutOfScope
        );
        assert_eq!(
            env.store
                .control_read_audit(&who, now, &id, 10, None)
                .unwrap_err()
                .reason,
            ControlReason::OutOfScope
        );
    }
    assert_eq!(
        env.store
            .control_read_principal(&who, now, &pid("kb1"))
            .unwrap_err()
            .reason,
        ControlReason::OutOfScope
    );
    assert_eq!(
        env.store
            .control_read_principal(&who, now, &pid("ghost"))
            .unwrap_err()
            .reason,
        ControlReason::OutOfScope
    );
    // 一个没有 read 权限的凭证：op_not_granted
    let writer = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::CreateAccount, "eduai", None)],
    ));
    assert_eq!(
        env.store
            .control_read_account(&writer.who(), now, &aid("eduai"))
            .unwrap_err()
            .reason,
        ControlReason::OpNotGranted
    );
    // 撤销之后读也被拒
    env.store
        .set_credential_status(&cid(reader.id()), CredentialStatus::Frozen, ts(T0))
        .unwrap();
    assert_eq!(
        env.store
            .control_read_account(&who, now, &aid("eduai"))
            .unwrap_err()
            .reason,
        ControlReason::CredentialFrozen
    );
    // whoami：自己的凭证元数据和数据面上限，不含密钥
    let info = env.store.control_whoami(&writer.who(), now).unwrap();
    assert_eq!(info.credential.id().as_str(), writer.id());
    assert_eq!(info.ceilings.allowed_roles, vec!["fast".to_string()]);
    assert!(!format!("{info:?}").contains(writer.secret()));
}

/// 同一类探测的读路径：读主体的状态，调用者没有 `read` 这个操作时，已有的主体得到 `op_not_granted`（授权判定先于一切），
/// 不存在的主体却在授权判定之前就得到 `out_of_scope`——没有 `read` 权限的凭证就能区分「有这个主体」和「没有」。
/// 契约（`authorize` 的检查顺序）早就写明：没有这个操作权限的调用者，无论目标存在与否都得到同一个答案。
#[test]
fn a_credential_without_the_read_operation_cannot_tell_a_principal_that_exists_from_one_that_does_not()
 {
    let env = Env::new();
    env.tree();
    let narrow = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::ManageMembership, "eduai", None)],
    ));
    let read = |principal: &str| {
        env.store
            .control_read_principal(&narrow.who(), ts(T0 + 1), &pid(principal))
            .expect_err("a credential without `read` reads nothing")
    };
    // kid1 在 eduai 里，kb1 在别的组织，ghost 不存在
    let expected = Failure::new(ControlReason::OpNotGranted);
    for principal in ["kid1", "kb1", "ghost"] {
        assert_eq!(read(principal), expected, "{principal}");
    }
    // 对照：有 `read` 权限的凭证，范围外和不存在仍然是同一个 out_of_scope
    let reader = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::Read, "eduai", None)],
    ));
    for principal in ["kb1", "ghost"] {
        assert_eq!(
            env.store
                .control_read_principal(&reader.who(), ts(T0 + 1), &pid(principal))
                .unwrap_err(),
            Failure::new(ControlReason::OutOfScope),
            "{principal}"
        );
    }
    // 凭证状态类的失败（绑定账户被冻结）对存在和不存在的主体同样一致
    env.store
        .set_account_status(&aid("eduai"), AccountStatus::Frozen)
        .unwrap();
    let frozen: Vec<Failure> = ["kid1", "kb1", "ghost"]
        .into_iter()
        .map(|principal| {
            env.store
                .control_read_principal(&reader.who(), ts(T0 + 2), &pid(principal))
                .unwrap_err()
        })
        .collect();
    assert_eq!(frozen[0], Failure::new(ControlReason::AccountInactive));
    assert!(
        frozen.iter().all(|failure| *failure == frozen[0]),
        "{frozen:?}"
    );
}

#[test]
fn the_audit_can_be_read_by_subtree_and_paged() {
    let env = Env::new();
    env.tree();
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    for (i, id) in ["n0", "n1", "n2", "n3", "n4"].into_iter().enumerate() {
        Action::CreateAccount {
            id,
            parent: "eduai",
        }
        .run(&env, &root.who(), T0 + i64::try_from(i).unwrap())
        .unwrap();
    }
    Action::CreateAccount {
        id: "b-n",
        parent: "school-b",
    }
    .run(&env, &other.who(), T0 + 9)
    .unwrap();
    let now = ts(T0 + 20);
    let page1 = env
        .store
        .control_read_audit(&root.who(), now, &aid("eduai"), 3, None)
        .unwrap();
    assert_eq!(page1.rows.len(), 3);
    assert!(page1.has_more);
    assert!(
        page1.rows.windows(2).all(|w| w[0].audit_id > w[1].audit_id),
        "newest first"
    );
    let page2 = env
        .store
        .control_read_audit(
            &root.who(),
            now,
            &aid("eduai"),
            3,
            Some(page1.rows.last().unwrap().audit_id),
        )
        .unwrap();
    assert!(
        page2
            .rows
            .iter()
            .all(|row| row.audit_id < page1.rows.last().unwrap().audit_id)
    );
    // school-b 的操作不在 eduai 子树的审计里
    let all = env
        .store
        .control_read_audit(&root.who(), now, &aid("eduai"), 100, None)
        .unwrap();
    assert!(
        all.rows
            .iter()
            .all(|row| row.scope_account.as_deref() != Some("school-b"))
    );
    assert!(
        all.rows
            .iter()
            .all(|row| row.target_id.as_deref() != Some("b-n"))
    );
    // 他们看不到彼此的
    let theirs = env
        .store
        .control_read_audit(&other.who(), now, &aid("school-b"), 100, None)
        .unwrap();
    assert!(
        theirs
            .rows
            .iter()
            .any(|row| row.target_id.as_deref() == Some("b-n"))
    );
    assert_eq!(
        env.store
            .control_read_audit(&other.who(), now, &aid("eduai"), 100, None)
            .unwrap_err()
            .reason,
        ControlReason::OutOfScope
    );
}

#[test]
fn control_tables_are_append_only_or_state_machines_at_the_database_level() {
    let env = Env::new();
    env.tree();
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let raw = env.raw();
    // 签发来源：只追加
    for sql in [
        "UPDATE credential_issuance SET issued_by_id = 'someone-else'",
        "DELETE FROM credential_issuance",
        "INSERT OR REPLACE INTO credential_issuance (credential_id, issued_by_kind, issued_by_id, issued_at_ms) \
         SELECT credential_id, 'operator', 'forged', 1 FROM credential_issuance",
    ] {
        assert!(raw.execute(sql, []).is_err(), "{sql}");
    }
    // 主体登记：只追加
    env.store.ensure_principal(&pid("reg"), ts(T0)).unwrap();
    Action::CreatePrincipal {
        principal: "reg2",
        org: "eduai",
    }
    .run(&env, &admin.who(), T0)
    .unwrap();
    for sql in [
        "UPDATE principal_registrations SET org_id = 'school-b'",
        "DELETE FROM principal_registrations",
        "INSERT OR REPLACE INTO principal_registrations (principal_id, org_id, registered_at_ms) VALUES ('reg2', 'school-b', 1)",
    ] {
        assert!(raw.execute(sql, []).is_err(), "{sql}");
    }
    // 对照：新的登记可以写
    raw.execute(
        "INSERT INTO principal_registrations (principal_id, org_id, registered_at_ms) VALUES ('reg', 'eduai', 1)",
        [],
    )
    .unwrap();
}

#[test]
fn issued_credentials_remember_who_issued_them() {
    let env = Env::new();
    env.tree();
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let issued = issue_as(&env, &admin.who(), &Issue::member("m-kid1"), T0 + 1).unwrap();
    let view = env
        .store
        .control_read_credentials(&admin.who(), ts(T0 + 2), &aid("s1"))
        .unwrap();
    let entry = view
        .credentials
        .iter()
        .find(|c| c.credential.id() == issued.value.id())
        .unwrap();
    assert_eq!(
        entry.issued_by,
        Some(("credential".to_string(), admin.id().to_string()))
    );
    // 运维签发的凭证记的是 operator
    let conn = env.raw();
    let kind: String = conn
        .query_row(
            "SELECT issued_by_kind FROM credential_issuance WHERE credential_id = ?1",
            [admin.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "operator");
}

/// 对照：数据库层的 `virtual_keys` 行和 `VirtualKeyStore::authenticate` 的往返（控制面直接写那张表，
/// 这条守着「两边的列没有漂移」）。
#[test]
fn a_credential_issued_by_the_control_plane_authenticates_through_the_regular_key_store() {
    let env = Env::new();
    env.tree();
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let issue = Issue::member("m-kid2")
        .roles(&["fast"])
        .privacy(&[PrivacyClass::LocalOnly])
        .expiring(T0 + DAY_MS);
    let key = MintedVirtualKey::mint();
    let rid = env.rid();
    env.store
        .control_issue_credential(&admin.who(), &env.meta(T0 + 1, &rid), &issue.request(&key))
        .unwrap();
    let identity = env.keys.authenticate(&key.secret, T0 + 2).unwrap().unwrap();
    assert_eq!(identity.key_id, key.key_id);
    assert_eq!(identity.scope.tenant_id.as_deref(), Some("tenant-edu"));
    assert_eq!(
        identity.scope.allowed_privacy,
        vec![PrivacyClass::LocalOnly]
    );
    assert_eq!(identity.scope.allowed_roles, vec!["fast".to_string()]);
    assert_eq!(identity.scope.expires_at_ms, Some(T0 + DAY_MS));
    assert!(
        identity.scope.admin_scopes.is_empty(),
        "the legacy free-string column stays empty"
    );
    assert!(
        env.keys
            .authenticate(&key.secret, T0 + DAY_MS)
            .unwrap()
            .is_none(),
        "expired"
    );
    // 登记过的凭证：状态闸门通过
    assert_eq!(
        env.store.credential_gate(&key.key_id, ts(T0 + 2)).unwrap(),
        super::GateVerdict::Allowed
    );
}

// ------------------------------------------------------------------------------------------------
// issue #603 N7：按 id 查的写操作，「存在」与「不存在」对无权调用者不可区分
// ------------------------------------------------------------------------------------------------

/// 建成员关系：外部组织拿别的组织的成员关系 id 去建（付款账户是自己的），旧代码在「调用者是不是主体的归属方」之前就报
/// `already_exists`，靠它能探测别的组织有哪些成员关系 id。现在先判可见性：不是归属方的调用者，对已有的 id 和不存在的 id
/// 得到同一个（无细节的）`out_of_scope`。
#[test]
fn creating_a_membership_never_probes_whether_a_membership_id_exists_for_an_outsider() {
    let env = Env::new();
    env.tree();
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    env.store
        .create_account(
            &Account {
                id: aid("acct-b2"),
                kind: AccountKind::Member,
                parent: Some(aid("school-b")),
                unit: Unit::Thb,
                status: AccountStatus::Active,
            },
            ts(T0),
        )
        .unwrap();
    let create = |id: &str, principal: &str| {
        let rid = env.rid();
        env.store
            .control_create_membership(
                &other.who(),
                &env.meta(T0 + 1, &rid),
                &CreateMembership {
                    membership: Membership {
                        id: mid(id),
                        principal: pid(principal),
                        org: aid("school-b"),
                        role: MembershipRole::Member,
                        account: Some(aid("acct-b2")),
                        status: MembershipStatus::Active,
                    },
                },
            )
            .expect_err("an outsider attaching a principal it does not own must be refused")
    };
    // m-kid1 是 eduai 的成员关系（已有），kid1 是 eduai 的主体；m-probe-missing 不存在
    let existing = create("m-kid1", "kid1");
    let missing = create("m-probe-missing", "kid1");
    assert_eq!(existing, missing, "{existing:?} vs {missing:?}");
    assert_eq!(existing, Failure::new(ControlReason::OutOfScope));
    // 对照：管得到它的人（eduai 的管理员）重发同一个成员关系仍是 replayed，定义不同仍是 already_exists
    let root = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    assert_eq!(
        Action::CreateMembership {
            id: "m-kid1",
            principal: "kid1",
            org: "eduai",
            account: "s1"
        }
        .run(&env, &root.who(), T0 + 2),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-kid1",
            principal: "kid1",
            org: "eduai",
            account: "s2"
        }
        .run(&env, &root.who(), T0 + 3),
        Err(ControlReason::AlreadyExists)
    );
}

/// 撤销成员关系 / 撤销额度 / 改凭证状态：没有这个操作权限的调用者，目标存在、在别的组织、不存在，得到同一个失败
/// （旧代码：不存在的目标在授权判定之前就是 `out_of_scope`，已有的是 `op_not_granted`）。
#[test]
fn id_keyed_writes_do_not_tell_a_caller_without_the_operation_whether_the_id_exists() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    // 只有 read：三个操作都没有
    let reader = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::Read, "eduai", None)],
    ));
    let victim = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    let expected = Failure::new(ControlReason::OpNotGranted);
    // 成员关系：m-kid1（自己的）、m-kb1（别的组织）、m-ghost
    for id in ["m-kid1", "m-kb1", "m-ghost"] {
        let rid = env.rid();
        let got = env
            .store
            .control_revoke_membership(
                &reader.who(),
                &env.meta(T0 + 1, &rid),
                &RevokeMembership {
                    membership: mid(id),
                },
            )
            .map(|_| ());
        assert_eq!(got, Err(expected.clone()), "revoke_membership {id}");
    }
    // 额度：a-teacher（自己的）、a-b（别的组织）、a-ghost
    for id in ["a-teacher", "a-b", "a-ghost"] {
        let rid = env.rid();
        let got = env
            .store
            .control_revoke_allocation(
                &reader.who(),
                &env.meta(T0 + 1, &rid),
                &RevokeAllocation {
                    id: AllocationId::new(id).unwrap(),
                },
            )
            .map(|_| ());
        assert_eq!(got, Err(expected.clone()), "revoke_allocation {id}");
    }
    // 凭证：reader 自己的、别的组织的、不存在的；撤销与冻结两种
    for status in [CredentialStatus::Revoked, CredentialStatus::Frozen] {
        for id in [
            reader.id().to_string(),
            victim.id().to_string(),
            "k-ghost".to_string(),
        ] {
            let rid = env.rid();
            let got = env
                .store
                .control_set_credential_status(
                    &reader.who(),
                    &env.meta(T0 + 1, &rid),
                    &SetCredentialStatus {
                        credential: cid(&id),
                        status,
                    },
                )
                .map(|_| ());
            assert_eq!(
                got,
                Err(expected.clone()),
                "set_credential_status {status:?} {id}"
            );
        }
    }
    // 对照：有操作权限的调用者，范围外和不存在仍然是同一个 out_of_scope；范围内的照常
    let edu = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1_000_000_000)));
    for (action, label) in [
        (
            Action::RevokeMembership { id: "m-kb1" },
            "foreign membership",
        ),
        (
            Action::RevokeMembership { id: "m-ghost" },
            "missing membership",
        ),
        (Action::RevokeAllocation { id: "a-b" }, "foreign allocation"),
        (
            Action::RevokeAllocation { id: "a-ghost" },
            "missing allocation",
        ),
    ] {
        assert_eq!(
            action.run(&env, &edu.who(), T0 + 2),
            Err(ControlReason::OutOfScope),
            "{label}"
        );
    }
    assert_eq!(
        Action::RevokeMembership { id: "m-kid1" }.run(&env, &edu.who(), T0 + 3),
        Ok(WriteResult::Updated)
    );
}

// ------------------------------------------------------------------------------------------------
// issue #603 N7 同类（PR-Daemon 在 #608 评审里指出）：还有三处先按 id 查、后授权——签发成员凭证（`Member` 绑定）、
// 创建配对码、撤销配对码。配对码的两条在 `pairing_tests.rs`。
// ------------------------------------------------------------------------------------------------

/// 一个只有 `read` 的调用者，和一个有 `issue_credential`、但自己绑定的组织（school-c）已被冻结的调用者。
fn callers_that_cannot_issue(env: &Env) -> [(&'static str, Minted, ControlReason); 2] {
    let reader = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::Read, "eduai", None)],
    ));
    env.account("school-c", AccountKind::Org, Some("platform"));
    let frozen = env.issue_as_operator(Issue::service(
        "school-c",
        vec![sc(ControlOp::IssueCredential, "school-c", None)],
    ));
    assert_eq!(
        Action::SetAccountStatus {
            account: "school-c",
            status: AccountStatus::Frozen
        }
        .run(env, &Env::operator(), T0 + 1),
        Ok(WriteResult::Updated)
    );
    [
        ("only read", reader, ControlReason::OpNotGranted),
        ("frozen anchor", frozen, ControlReason::AccountInactive),
    ]
}

/// 签发成员凭证要先按成员关系 id 取付款账户，才轮到授权。旧代码：没有 `issue_credential` 权限（或绑定账户被冻结）的调用者，
/// 不存在的成员关系得到 `out_of_scope`，已有的（自己组织的、别的组织的）得到 `op_not_granted` / `account_inactive`，
/// 靠它探测别的组织有哪些成员关系 id。现在先做与目标无关的授权判定（`require_op_capability`），三种 id 得到同一个失败。
#[test]
fn issuing_a_member_credential_does_not_tell_a_caller_that_cannot_issue_whether_the_membership_exists()
 {
    let env = Env::new();
    env.tree();
    let issue = |who: &Who<'_>, membership: &str| -> Failure {
        let key = MintedVirtualKey::mint();
        let rid = env.rid();
        env.store
            .control_issue_credential(
                who,
                &env.meta(T0 + 2, &rid),
                &Issue::member(membership).request(&key),
            )
            .expect_err("a caller that cannot issue must be refused")
    };
    for (label, caller, expected) in callers_that_cannot_issue(&env) {
        // m-kid1：eduai 的成员关系；m-kb1：school-b 的；m-ghost：不存在
        for id in ["m-kid1", "m-kb1", "m-ghost"] {
            assert_eq!(
                issue(&caller.who(), id),
                Failure::new(expected),
                "{label}: {id}"
            );
        }
    }
    // 对照：有权限的调用者，别的组织的和不存在的成员关系仍是同一个 out_of_scope，自己组织的照常签发
    let edu = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    for id in ["m-kb1", "m-ghost"] {
        assert_eq!(
            issue(&edu.who(), id),
            Failure::new(ControlReason::OutOfScope),
            "{id}"
        );
    }
    assert_eq!(
        Action::IssueCredential {
            membership: "m-kid1"
        }
        .run(&env, &edu.who(), T0 + 3),
        Ok(WriteResult::Created)
    );
    // 本机运维不受授权约束：成员关系不存在仍是 out_of_scope
    assert_eq!(
        Action::IssueCredential {
            membership: "m-ghost"
        }
        .run(&env, &Env::operator(), T0 + 4),
        Err(ControlReason::OutOfScope)
    );
}

// ------------------------------------------------------------------------------------------------
// issue #603 N7（PR-Daemon 在 #608 评审里指出的残余）：建成员关系时，调用者用自己名下已撤销的主体
// ------------------------------------------------------------------------------------------------

/// 旧代码：调用者用自己登记、已经撤销的主体建成员关系。别的组织已有的成员关系 id 在「主体状态」之前就被 `out_of_scope` 拒绝，
/// 不存在的 id 走到主体状态得到 `principal_revoked`——不用写入就能探测别的组织有哪些成员关系 id。
/// 现在调用者管不到的成员关系对调用者按「不存在」走，先过主体状态，最后才以 `out_of_scope` 拒绝；两种 id 得到同一个失败。
///
/// **残余（无法消除，只钉住）**：主体是活跃的时候，不存在的 id 会创建成功、别的组织已有的 id 得到 `out_of_scope`，
/// 「创建成功与否」本身仍然可探测——成员关系 id 是客户端自选的全局命名空间。
#[test]
fn creating_a_membership_with_the_callers_own_revoked_principal_does_not_probe_membership_ids() {
    let env = Env::new();
    let (_edu, other) = principal_world(&env);
    // school-b 登记并撤销自己的主体 bee；再登记一个活跃的 cat
    for principal in ["bee", "cat"] {
        assert_eq!(
            Action::CreatePrincipal {
                principal,
                org: "school-b"
            }
            .run(&env, &other.who(), T0 + 2),
            Ok(WriteResult::Created)
        );
    }
    assert_eq!(
        Action::RevokePrincipal {
            principal: "bee",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 3),
        Ok(WriteResult::Updated)
    );
    env.account("acct-b", AccountKind::Member, Some("school-b"));
    // m-kid1 是 eduai 的成员关系（已有，school-b 管不到）；m-probe-missing 不存在
    let existing = attach_failure(
        &env,
        &other.who(),
        T0 + 4,
        member_membership("m-kid1", "bee", "school-b", "acct-b"),
    );
    let missing = attach_failure(
        &env,
        &other.who(),
        T0 + 4,
        member_membership("m-probe-missing", "bee", "school-b", "acct-b"),
    );
    assert_eq!(existing, missing, "{existing:?} vs {missing:?}");
    assert_eq!(existing.to_string(), missing.to_string());
    assert_eq!(
        existing,
        Failure::with_detail(
            ControlReason::PrincipalRevoked,
            "principal bee has been revoked"
        )
    );
    // 残余：活跃的主体，别的组织已有的 id 是 out_of_scope，不存在的 id 创建成功
    assert_eq!(
        attach_failure(
            &env,
            &other.who(),
            T0 + 5,
            member_membership("m-kid1", "cat", "school-b", "acct-b"),
        ),
        Failure::new(ControlReason::OutOfScope)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-cat",
            principal: "cat",
            org: "school-b",
            account: "acct-b"
        }
        .run(&env, &other.who(), T0 + 6),
        Ok(WriteResult::Created)
    );
}

/// 上面那条的对照，防止「管不到的 id 按不存在走」被推广到调用者管得到的成员关系：自己已有的成员关系，主体事后被撤销，
/// 同样的创建请求照旧是幂等的 `replayed`（成员关系行本身没变），定义不同照旧是 `already_exists`，不是 `principal_revoked`。
#[test]
fn replaying_the_creation_of_an_own_membership_stays_idempotent_after_its_principal_is_revoked() {
    let env = Env::new();
    let (_edu, other) = principal_world(&env);
    env.account("acct-b", AccountKind::Member, Some("school-b"));
    env.account("acct-b2", AccountKind::Member, Some("school-b"));
    for action in [
        Action::CreatePrincipal {
            principal: "bee",
            org: "school-b",
        },
        Action::CreateMembership {
            id: "m-bee",
            principal: "bee",
            org: "school-b",
            account: "acct-b",
        },
        Action::RevokePrincipal {
            principal: "bee",
            org: "school-b",
        },
    ] {
        assert!(action.run(&env, &other.who(), T0 + 2).is_ok(), "{action:?}");
    }
    assert_eq!(
        Action::CreateMembership {
            id: "m-bee",
            principal: "bee",
            org: "school-b",
            account: "acct-b"
        }
        .run(&env, &other.who(), T0 + 3),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-bee",
            principal: "bee",
            org: "school-b",
            account: "acct-b2"
        }
        .run(&env, &other.who(), T0 + 4),
        Err(ControlReason::AlreadyExists)
    );
}

// ------------------------------------------------------------------------------------------------
// Codex 对 fix/603-compute-followups（25a30d59）的预审：H1（付款账户已被占用）、M2（审计里目标派生的字段）、
// M3（失败路径上的数据库工作量）。HTTP 层的 H1 在 `idoris-router` 的 `control/tests.rs`，配对码的审计在 `pairing_tests.rs`。
// ------------------------------------------------------------------------------------------------

/// 库里全部成员关系的 id（升序）。
pub(super) fn membership_ids(env: &Env) -> Vec<String> {
    let conn = env.raw();
    let mut stmt = conn
        .prepare("SELECT membership_id FROM memberships ORDER BY membership_id")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 夹具：school-b 登记了主体 bee（随后撤销）、cat、dan（活跃）；付款账户 acct-b 已经属于 cat 的成员关系 m-cat，acct-c 空闲。
/// 返回 school-b 的管理凭证。
fn school_b_with_a_bound_payment_account(env: &Env) -> Minted {
    let (_edu, other) = principal_world(env);
    for principal in ["bee", "cat", "dan"] {
        assert_eq!(
            Action::CreatePrincipal {
                principal,
                org: "school-b"
            }
            .run(env, &other.who(), T0 + 2),
            Ok(WriteResult::Created)
        );
    }
    assert_eq!(
        Action::RevokePrincipal {
            principal: "bee",
            org: "school-b"
        }
        .run(env, &other.who(), T0 + 3),
        Ok(WriteResult::Updated)
    );
    env.account("acct-b", AccountKind::Member, Some("school-b"));
    env.account("acct-c", AccountKind::Member, Some("school-b"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-cat",
            principal: "cat",
            org: "school-b",
            account: "acct-b"
        }
        .run(env, &other.who(), T0 + 4),
        Ok(WriteResult::Created)
    );
    other
}

/// H1：付款账户已经属于别的成员关系时，对调用者管不到的已有 id（m-kid1 是 eduai 的）和不存在的 id，建成员关系得到同一个失败。
/// 旧代码：管不到的 id 直接 `out_of_scope`，不存在的 id 走到 `AccountAlreadyBound`，得到 `invalid_membership`，
/// 调用者借一个已被占用的付款账户就能区分「别的组织有没有这个成员关系 id」。
#[test]
fn a_bound_payment_account_gives_a_hidden_and_a_missing_membership_id_the_same_failure() {
    let env = Env::new();
    let other = school_b_with_a_bound_payment_account(&env);
    let before = membership_ids(&env);
    let hidden = attach_failure(
        &env,
        &other.who(),
        T0 + 5,
        member_membership("m-kid1", "dan", "school-b", "acct-b"),
    );
    let missing = attach_failure(
        &env,
        &other.who(),
        T0 + 5,
        member_membership("m-probe-missing", "dan", "school-b", "acct-b"),
    );
    assert_eq!(
        hidden.reason,
        ControlReason::InvalidMembership,
        "{hidden:?}"
    );
    assert_eq!(hidden.reason, missing.reason, "{hidden:?} vs {missing:?}");
    // 详情里只有调用者自己提交的那个 id 不同；占着账户的是调用者自己管的 m-cat
    assert_eq!(
        hidden.detail.replace("m-kid1", "<id>"),
        missing.detail.replace("m-probe-missing", "<id>")
    );
    assert!(
        hidden.detail.contains("acct-b") && hidden.detail.contains("m-cat"),
        "{hidden:?}"
    );
    // 两次都没有建出任何东西
    assert_eq!(membership_ids(&env), before);
}

/// 在 `body` 执行期间记下这个存储的连接上执行的全部 SQL 语句。用的是 `sqlite3_sql`（原文，参数没有展开），所以语句文本里没有调用者
/// 提交的 id，序列可以直接比较。
pub(super) fn traced<T>(env: &Env, body: impl FnOnce() -> T) -> (T, Vec<String>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    env.store
        .lock()
        .trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            Some(move |event: TraceEvent<'_>| {
                if let TraceEvent::Stmt(statement, _) = event {
                    sink.lock().unwrap().push(statement.sql().into_owned());
                }
            }),
        )
        .unwrap();
    let out = body();
    env.store
        .lock()
        .trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            None::<fn(TraceEvent<'_>)>,
        )
        .unwrap();
    let statements = log.lock().unwrap().clone();
    (out, statements)
}

/// M3：失败发生在「主体已撤销」或「付款账户已被占用」时，调用者管不到的已有 id 和不存在的 id 让数据库执行完全一样的语句序列。
/// 旧代码先读出那一行再去做授权（多出一串查询），响应时间和数据库工作量都能把两种 id 区分开。这里比语句序列，不比毫秒。
#[test]
fn hidden_and_missing_membership_ids_run_the_same_statements_when_attaching_fails() {
    let env = Env::new();
    let other = school_b_with_a_bound_payment_account(&env);
    for (label, principal, account) in [
        ("revoked principal", "bee", "acct-c"),
        ("occupied payment account", "dan", "acct-b"),
    ] {
        let (hidden, hidden_sql) = traced(&env, || {
            attach_failure(
                &env,
                &other.who(),
                T0 + 5,
                member_membership("m-kid1", principal, "school-b", account),
            )
        });
        let (missing, missing_sql) = traced(&env, || {
            attach_failure(
                &env,
                &other.who(),
                T0 + 5,
                member_membership("m-probe-missing", principal, "school-b", account),
            )
        });
        assert_eq!(
            hidden.reason, missing.reason,
            "{label}: {hidden:?} vs {missing:?}"
        );
        assert!(
            hidden_sql.len() > 5,
            "{label}: the trace must actually see the statements: {hidden_sql:?}"
        );
        assert_eq!(
            hidden_sql, missing_sql,
            "{label}: a membership id the caller cannot manage must cost the database what a missing one costs"
        );
    }
}

/// 一个读范围（eduai）比各项写范围（teacher 子树）宽的凭证：它能读 eduai 的审计，但管不到 eduai 下 teacher 之外的东西。
pub(super) fn reader_wider_than_its_writer(env: &Env) -> Minted {
    env.issue_as_operator(Issue::service(
        "eduai",
        vec![
            sc(ControlOp::Read, "eduai", None),
            sc(ControlOp::IssueCredential, "teacher", None),
            sc(ControlOp::ManageMembership, "teacher", None),
            sc(ControlOp::FreezeCredential, "teacher", None),
            sc(ControlOp::RevokeCredential, "teacher", None),
            sc(ControlOp::GrantAllocation, "teacher", Some(1_000_000_000)),
        ],
    ))
}

/// 最近一条审计行的金额列（`AuditEntry` 不带它）。
fn last_audit_amount(env: &Env) -> (Option<String>, Option<i64>) {
    env.raw()
        .query_row(
            "SELECT amount_unit, amount_micros FROM control_audit ORDER BY audit_id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

/// M2：按 id 查的写操作被拒（目标不在调用者的写范围里）时，审计行里由目标派生的字段（`scope_account`、`related_id`、金额）
/// 要等目标授权通过之后才填。旧代码在授权判定之前就填了：目标存在（在调用者的读范围里、写范围外）的那一行带着目标的账户和关联 id，
/// 调用者按 eduai 子树读审计就看得到；目标不存在的那一行是空的、读不到，两种被拒留下的记录不一样。
#[test]
fn a_denied_id_keyed_write_leaves_the_same_audit_row_whether_or_not_the_target_exists() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let wide = reader_wider_than_its_writer(&env);
    // 目标凭证：绑定 eduai，在 wide 的读范围里、写范围外
    let target_credential = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let cases: Vec<(&str, Action, Action)> = vec![
        (
            "issue_credential",
            Action::IssueCredential {
                membership: "m-kid2",
            },
            Action::IssueCredential {
                membership: "m-ghost",
            },
        ),
        (
            "revoke_membership",
            Action::RevokeMembership { id: "m-kid2" },
            Action::RevokeMembership { id: "m-ghost" },
        ),
        (
            "revoke_credential",
            Action::SetCredential {
                credential: target_credential.id().to_string(),
                status: CredentialStatus::Revoked,
            },
            Action::SetCredential {
                credential: "k-ghost".to_string(),
                status: CredentialStatus::Revoked,
            },
        ),
        (
            "revoke_allocation",
            Action::RevokeAllocation { id: "a-teacher" },
            Action::RevokeAllocation { id: "a-ghost" },
        ),
    ];
    let mut now = T0 + 1;
    for (op, existing, missing) in &cases {
        let mut rows = Vec::new();
        for action in [existing, missing] {
            now += 1;
            assert_eq!(
                action.run(&env, &wide.who(), now),
                Err(ControlReason::OutOfScope),
                "{op}: {action:?}"
            );
            let row = env.last_audit();
            assert_eq!(row.op, *op);
            assert_eq!(
                (row.outcome.as_str(), row.reason_code.as_str()),
                ("denied", "out_of_scope"),
                "{op}"
            );
            assert_eq!(
                (row.scope_account.clone(), row.related_id.clone()),
                (None, None),
                "{op}: a denied row must not carry anything derived from the target: {row:?}"
            );
            assert_eq!(last_audit_amount(&env), (None, None), "{op}");
            rows.push(row);
        }
        // 两种被拒的审计行，除了请求里自己写的目标 id 和请求号，逐字段相同
        let normalize = |row: &AuditEntry| AuditEntry {
            target_id: None,
            request_id: String::new(),
            ..row.clone()
        };
        assert_eq!(normalize(&rows[0]), normalize(&rows[1]), "{op}");
    }
    // 调用者按自己的读范围读审计：这些被拒的尝试，不论目标存不存在，都不在它能读到的行里
    let view = env
        .store
        .control_read_audit(&wide.who(), ts(now + 1), &aid("eduai"), 200, None)
        .unwrap();
    let seen: Vec<&str> = view
        .rows
        .iter()
        .filter(|row| row.outcome == "denied")
        .map(|row| row.op.as_str())
        .collect();
    assert!(
        seen.is_empty(),
        "denied attempts are not readable through the target-derived scope: {seen:?}"
    );
}

/// 范围限定的读（`rows::load_membership_within`，一条 SQL 里按子树过滤）和授权判定（契约的 `authorize`）用两套写法表达同一个
/// 「调用者管得到哪些成员关系」。这条把两者的一致钉住：只管 teacher 子树的凭证，对子树里的成员关系（付款账户在 teacher 下）
/// 照旧能重放、能得到冲突；管理员关系没有付款账户，范围看所属组织，管着 eduai 的凭证对它也照旧能重放；子树之外的
/// （s2 在 eduai 下、teacher 之外）对它是「不存在」。
#[test]
fn a_manager_with_a_narrow_scope_replays_and_conflicts_inside_it_and_is_blind_outside_it() {
    let env = Env::new();
    env.tree();
    // teacher 子树里再加一个空闲的成员账户 s3
    env.account("s3", AccountKind::Member, Some("teacher"));
    let narrow = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::ManageMembership, "teacher", None)],
    ));
    let edu = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    // 子树里已有的 m-kid1（kid1 -> s1，org eduai）：同样的定义是 replayed，不同的定义是 already_exists
    assert_eq!(
        Action::CreateMembership {
            id: "m-kid1",
            principal: "kid1",
            org: "eduai",
            account: "s1"
        }
        .run(&env, &narrow.who(), T0 + 1),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-kid1",
            principal: "kid2",
            org: "eduai",
            account: "s1"
        }
        .run(&env, &narrow.who(), T0 + 2),
        Err(ControlReason::AlreadyExists)
    );
    // 管理员关系：没有付款账户，范围看所属组织。管着 eduai 的凭证建出来之后，同样的请求是 replayed
    assert_eq!(
        Action::CreatePrincipal {
            principal: "boss",
            org: "eduai"
        }
        .run(&env, &edu.who(), T0 + 3),
        Ok(WriteResult::Created)
    );
    let admin = |id: &str| CreateMembership {
        membership: admin_membership(id, "boss", "eduai"),
    };
    let attach_admin = |who: &Who<'_>, id: &str, now: i64| {
        let rid = env.rid();
        env.store
            .control_create_membership(who, &env.meta(now, &rid), &admin(id))
            .map(|written| written.result)
            .map_err(|failure| failure.reason)
    };
    assert_eq!(
        attach_admin(&edu.who(), "m-boss", T0 + 4),
        Ok(WriteResult::Created)
    );
    assert_eq!(
        attach_admin(&edu.who(), "m-boss", T0 + 5),
        Ok(WriteResult::Replayed)
    );
    // 子树之外：m-kid2（s2 在 eduai 下、teacher 之外）和管理员关系 m-boss（组织 eduai 不在 teacher 之下）对窄凭证都是「不存在」，
    // 用它们的 id 建一个自己子树里的成员关系（s3 空闲）：先过结构校验，最后才是 out_of_scope
    let before = membership_ids(&env);
    for hidden in ["m-kid2", "m-boss"] {
        assert_eq!(
            Action::CreateMembership {
                id: hidden,
                principal: "kid1",
                org: "eduai",
                account: "s3"
            }
            .run(&env, &narrow.who(), T0 + 6),
            Err(ControlReason::OutOfScope),
            "{hidden}"
        );
    }
    assert_eq!(membership_ids(&env), before, "nothing was created");
    // 范围判定只认同一个操作（ManageMembership）的范围：同一张凭证上别的操作的范围（IssueCredential、Read 管着 eduai）不让
    // eduai 下 teacher 之外的 m-kid2 对它可见
    let mixed = env.issue_as_operator(Issue::service(
        "eduai",
        vec![
            sc(ControlOp::ManageMembership, "teacher", None),
            sc(ControlOp::IssueCredential, "eduai", None),
            sc(ControlOp::Read, "eduai", None),
        ],
    ));
    assert_eq!(
        Action::CreateMembership {
            id: "m-kid2",
            principal: "kid1",
            org: "eduai",
            account: "s3"
        }
        .run(&env, &mixed.who(), T0 + 7),
        Err(ControlReason::OutOfScope)
    );
    // 本机运维不受授权约束，看得到全部成员关系：同一个定义是 replayed，不同的定义是 already_exists
    assert_eq!(
        Action::CreateMembership {
            id: "m-kid2",
            principal: "kid2",
            org: "eduai",
            account: "s2"
        }
        .run(&env, &Env::operator(), T0 + 8),
        Ok(WriteResult::Replayed)
    );
    assert_eq!(
        Action::CreateMembership {
            id: "m-kid2",
            principal: "kid1",
            org: "eduai",
            account: "s2"
        }
        .run(&env, &Env::operator(), T0 + 9),
        Err(ControlReason::AlreadyExists)
    );
    assert_eq!(membership_ids(&env), before, "still nothing was created");
    // 残余（无法消除，只钉住）：不存在的 id 同样的请求创建成功
    assert_eq!(
        Action::CreateMembership {
            id: "m-fresh",
            principal: "kid1",
            org: "eduai",
            account: "s3"
        }
        .run(&env, &narrow.who(), T0 + 10),
        Ok(WriteResult::Created)
    );
}

// ================================================================================================
// 按 id 查目标的写操作：调用者管不到的已有 id 和不存在的 id，数据库执行的语句一模一样（Codex 预审 M3 的泛化）
// ================================================================================================

/// 调用者对 `op` 只管 teacher 子树，对别的每个操作都管着整个 eduai：对 `op` 来说，eduai 下 teacher 之外的、别的组织的、不存在的目标
/// 都管不到，而「别的操作的范围」不能让它们变得可见。
pub(super) fn narrow_for(env: &Env, op: ControlOp) -> Minted {
    narrow_for_root(env, op, "teacher")
}

/// 和 [`narrow_for`] 一样，只是对 `op` 管的子树根可以自己指定（要在根下面再深几层的目标时用）。
pub(super) fn narrow_for_root(env: &Env, op: ControlOp, root: &str) -> Minted {
    let mut scopes: Vec<AdminScope> = all_ops("eduai", 1_000_000_000)
        .into_iter()
        .filter(|scope| scope.op != op)
        .collect();
    let cap = (op == ControlOp::GrantAllocation).then_some(1_000_000_000);
    scopes.push(sc(op, root, cap));
    env.issue_as_operator(Issue::service("eduai", scopes))
}

/// 用一个新的请求号执行一次存储调用，只留下结果类别：成功是 `Ok(())`，失败是原因码。
pub(super) fn call<T>(
    env: &Env,
    now: i64,
    op: impl FnOnce(&Meta<'_>) -> Result<Written<T>, Failure>,
) -> Result<(), ControlReason> {
    let rid = env.rid();
    let meta = env.meta(now, &rid);
    op(&meta).map(|_| ()).map_err(|failure| failure.reason)
}

/// 同一个动作对每个「调用者管不到的已有目标」和对一个不存在的目标各执行一遍：结果都是 `out_of_scope`，数据库执行的语句序列一模一样
/// （比语句，不比毫秒）。管不到的已有 id 要是比不存在的 id 多做几条查询，响应时间和数据库工作量就能把两者区分开。
pub(super) fn assert_hidden_costs_what_missing_costs(
    env: &Env,
    label: &str,
    run: impl Fn(&str) -> Result<(), ControlReason>,
    hidden: &[&str],
    missing: &str,
) {
    let (result, missing_sql) = traced(env, || run(missing));
    assert_eq!(
        result,
        Err(ControlReason::OutOfScope),
        "{label}: missing `{missing}`"
    );
    assert!(
        missing_sql.len() > 5,
        "{label}: the trace must actually see the statements: {missing_sql:?}"
    );
    for id in hidden {
        let (result, sql) = traced(env, || run(id));
        assert_eq!(
            result,
            Err(ControlReason::OutOfScope),
            "{label}: hidden `{id}`"
        );
        assert_eq!(
            sql, missing_sql,
            "{label}: `{id}` is outside what the caller manages and must cost the database what a missing id costs"
        );
    }
}

#[test]
fn revoking_a_hidden_or_missing_membership_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    let narrow = narrow_for(&env, ControlOp::ManageMembership);
    let who = narrow.who();
    assert_hidden_costs_what_missing_costs(
        &env,
        "revoke_membership",
        |id| {
            call(&env, T0 + 1, |meta| {
                env.store.control_revoke_membership(
                    &who,
                    meta,
                    &RevokeMembership {
                        membership: mid(id),
                    },
                )
            })
        },
        &["m-kid2", "m-kb1"],
        "m-ghost",
    );
    // 管得到的（付款账户在 teacher 子树里）照常撤销
    assert_eq!(
        Action::RevokeMembership { id: "m-kid1" }.run(&env, &who, T0 + 2),
        Ok(WriteResult::Updated)
    );
}

#[test]
fn issuing_a_credential_for_a_hidden_or_missing_binding_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    let narrow = narrow_for(&env, ControlOp::IssueCredential);
    let who = narrow.who();
    // 成员凭证：绑定的成员关系管不到 / 不存在
    assert_hidden_costs_what_missing_costs(
        &env,
        "issue_credential (member binding)",
        |membership| {
            let key = MintedVirtualKey::mint();
            call(&env, T0 + 1, |meta| {
                env.store.control_issue_credential(
                    &who,
                    meta,
                    &Issue::member(membership).request(&key),
                )
            })
        },
        &["m-kid2", "m-kb1"],
        "m-ghost",
    );
    // 服务凭证：绑定的账户管不到 / 不存在
    assert_hidden_costs_what_missing_costs(
        &env,
        "issue_credential (service binding)",
        |account| {
            let key = MintedVirtualKey::mint();
            call(&env, T0 + 1, |meta| {
                env.store.control_issue_credential(
                    &who,
                    meta,
                    &Issue::service(account, vec![]).request(&key),
                )
            })
        },
        &["eduai", "school-b"],
        "ghost-org",
    );
    // 管得到的成员关系照常签发
    assert_eq!(
        Action::IssueCredential {
            membership: "m-kid1"
        }
        .run(&env, &who, T0 + 2),
        Ok(WriteResult::Created)
    );
}

#[test]
fn changing_the_status_of_a_hidden_or_missing_credential_runs_the_same_statements() {
    let env = Env::new();
    let creds = matrix_setup(&env);
    for (status, op) in [
        (CredentialStatus::Frozen, ControlOp::FreezeCredential),
        (CredentialStatus::Active, ControlOp::FreezeCredential),
        (CredentialStatus::Revoked, ControlOp::RevokeCredential),
    ] {
        let narrow = narrow_for(&env, op);
        let who = narrow.who();
        assert_hidden_costs_what_missing_costs(
            &env,
            &format!("set_credential_status {status:?}"),
            |credential| {
                call(&env, T0 + 1, |meta| {
                    env.store.control_set_credential_status(
                        &who,
                        meta,
                        &SetCredentialStatus {
                            credential: cid(credential),
                            status,
                        },
                    )
                })
            },
            &[creds.sibling.as_str(), creds.foreign.as_str()],
            "k-ghost",
        );
    }
    // 管得到的（成员关系的付款账户在 teacher 子树里）照常冻结
    let narrow = narrow_for(&env, ControlOp::FreezeCredential);
    assert_eq!(
        Action::SetCredential {
            credential: creds.inside.clone(),
            status: CredentialStatus::Frozen
        }
        .run(&env, &narrow.who(), T0 + 2),
        Ok(WriteResult::Updated)
    );
}

#[test]
fn revoking_a_hidden_or_missing_allocation_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    // eduai 下面再嵌一个组织 dept：eduai -> dept -> team -> s-x。调用者撤销额度只管 dept 子树，别的操作管着整个 eduai
    env.account("dept", AccountKind::Org, Some("eduai"));
    env.account("team", AccountKind::Sponsor, Some("dept"));
    env.account("s-x", AccountKind::Member, Some("team"));
    for (id, parent, child, micros) in [
        ("a-dept", "eduai", "dept", 500_000_000),
        ("a-team", "dept", "team", 20_000_000),
        ("a-leaf", "team", "s-x", 1_000_000),
    ] {
        let rid = env.rid();
        env.store
            .control_grant_allocation(
                &Env::operator(),
                &env.meta(T0, &rid),
                &grant(id, parent, child, micros),
            )
            .unwrap();
    }
    let narrow = narrow_for_root(&env, ControlOp::GrantAllocation, "dept");
    let who = narrow.who();
    // 授权方在 dept 子树之外的（eduai、pool、platform）管不到
    assert_hidden_costs_what_missing_costs(
        &env,
        "revoke_allocation",
        |id| {
            call(&env, T0 + 1, |meta| {
                env.store.control_revoke_allocation(
                    &who,
                    meta,
                    &RevokeAllocation {
                        id: AllocationId::new(id).unwrap(),
                    },
                )
            })
        },
        &["a-dept", "a-edu", "a-b"],
        "a-ghost",
    );
    // 管得到的照常撤销：授权方在子树根自己（dept），或者在根下面一层 / 两层（要沿 parent_id 往上走才碰到根）；
    // 权限上有额度上限也不影响「管得到」
    for id in ["a-team", "a-leaf"] {
        assert_eq!(
            Action::RevokeAllocation { id }.run(&env, &who, T0 + 2),
            Ok(WriteResult::Updated),
            "{id}"
        );
    }
}

#[test]
fn revoking_a_hidden_or_missing_principal_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    env.account("s5", AccountKind::Member, Some("teacher"));
    env.account("s6", AccountKind::Member, Some("eduai"));
    env.account("s7", AccountKind::Member, Some("teacher"));
    let attach = |membership: &str, principal: &str, account: &str| {
        env.store
            .create_membership(
                &Membership {
                    id: mid(membership),
                    principal: pid(principal),
                    org: aid("eduai"),
                    role: MembershipRole::Member,
                    account: Some(aid(account)),
                    status: MembershipStatus::Active,
                },
                ts(T0),
            )
            .unwrap();
    };
    // 足迹一半在 teacher 子树里、一半在外面的主体
    env.store
        .ensure_principal(&pid("p-partial"), ts(T0))
        .unwrap();
    attach("m-p1", "p-partial", "s5");
    attach("m-p2", "p-partial", "s6");
    // 登记它的组织（eduai）在 teacher 子树之外，成员关系在里面
    assert_eq!(
        Action::CreatePrincipal {
            principal: "p-reg",
            org: "eduai"
        }
        .run(&env, &Env::operator(), T0),
        Ok(WriteResult::Created)
    );
    attach("m-r1", "p-reg", "s7");
    // 没人登记过、也没有任何成员关系
    env.store
        .ensure_principal(&pid("p-orphan"), ts(T0))
        .unwrap();
    let narrow = narrow_for(&env, ControlOp::RevokePrincipal);
    let who = narrow.who();
    assert_hidden_costs_what_missing_costs(
        &env,
        "revoke_principal",
        |principal| {
            call(&env, T0 + 1, |meta| {
                env.store.control_revoke_principal(
                    &who,
                    meta,
                    &RevokePrincipal {
                        principal: pid(principal),
                        org: aid("teacher"),
                    },
                )
            })
        },
        &["kid2", "kb1", "p-partial", "p-reg", "p-orphan"],
        "p-ghost",
    );
    // 管得到的（每个成员关系的付款账户都在 teacher 子树里，没有登记记录）照常撤销
    assert_eq!(
        Action::RevokePrincipal {
            principal: "kid1",
            org: "teacher"
        }
        .run(&env, &who, T0 + 2),
        Ok(WriteResult::Updated)
    );
}

#[test]
fn freezing_a_hidden_or_missing_account_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    let narrow = narrow_for(&env, ControlOp::SetAccountStatus);
    let who = narrow.who();
    assert_hidden_costs_what_missing_costs(
        &env,
        "set_account_status",
        |account| {
            call(&env, T0 + 1, |meta| {
                env.store.control_set_account_status(
                    &who,
                    meta,
                    &SetAccountStatus {
                        account: aid(account),
                        status: AccountStatus::Frozen,
                    },
                )
            })
        },
        &["s2", "b1", "eduai"],
        "ghost-account",
    );
    assert_eq!(
        Action::SetAccountStatus {
            account: "s1",
            status: AccountStatus::Frozen
        }
        .run(&env, &who, T0 + 2),
        Ok(WriteResult::Updated)
    );
}

#[test]
fn creating_an_account_under_a_hidden_or_missing_parent_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    let narrow = narrow_for(&env, ControlOp::CreateAccount);
    let who = narrow.who();
    assert_hidden_costs_what_missing_costs(
        &env,
        "create_account",
        |parent| {
            call(&env, T0 + 1, |meta| {
                env.store.control_create_account(
                    &who,
                    meta,
                    &CreateAccount {
                        account: Account {
                            id: aid("acct-x"),
                            kind: AccountKind::Member,
                            parent: Some(aid(parent)),
                            unit: Unit::Thb,
                            status: AccountStatus::Active,
                        },
                    },
                )
            })
        },
        &["s2", "b1", "eduai"],
        "ghost-parent",
    );
}

#[test]
fn creating_a_principal_for_a_hidden_or_missing_org_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    let narrow = narrow_for(&env, ControlOp::CreatePrincipal);
    let who = narrow.who();
    assert_hidden_costs_what_missing_costs(
        &env,
        "create_principal",
        |org| {
            call(&env, T0 + 1, |meta| {
                env.store.control_create_principal(
                    &who,
                    meta,
                    &CreatePrincipal {
                        principal: pid("p-x"),
                        org: aid(org),
                    },
                )
            })
        },
        &["eduai", "school-b"],
        "ghost-org",
    );
}

/// 授权方（父）账户管不到 / 不存在：金额是负数时也一样。契约的 `authorize` 对存在的目标先判金额（`invalid_amount`），对不存在的目标先判
/// `out_of_scope`，带负金额的请求能把「账户存在」和「不存在」区分开；现在范围先判，负数留到之后。
#[test]
fn granting_an_allocation_from_a_hidden_or_missing_parent_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let narrow = narrow_for(&env, ControlOp::GrantAllocation);
    let who = narrow.who();
    for micros in [1_000_000, -5] {
        assert_hidden_costs_what_missing_costs(
            &env,
            &format!("grant_allocation ({micros} micros)"),
            |parent| {
                call(&env, T0 + 1, |meta| {
                    env.store.control_grant_allocation(
                        &who,
                        meta,
                        &grant("a-x", parent, "teacher", micros),
                    )
                })
            },
            &["eduai", "pool", "school-b"],
            "ghost-parent",
        );
    }
    // 范围里的父账户：负金额照旧是 `invalid_amount`
    assert_eq!(
        Action::GrantAllocation {
            id: "a-neg",
            parent: "teacher",
            child: "s1",
            micros: -5
        }
        .run(&env, &who, T0 + 2),
        Err(ControlReason::InvalidAmount)
    );
}

#[test]
fn creating_a_membership_on_a_hidden_or_missing_payment_account_runs_the_same_statements() {
    let env = Env::new();
    env.tree();
    let narrow = narrow_for(&env, ControlOp::ManageMembership);
    let who = narrow.who();
    assert_hidden_costs_what_missing_costs(
        &env,
        "create_membership (payment account)",
        |account| {
            call(&env, T0 + 1, |meta| {
                env.store.control_create_membership(
                    &who,
                    meta,
                    &CreateMembership {
                        membership: member_membership("m-x", "kid1", "eduai", account),
                    },
                )
            })
        },
        &["s2", "b1"],
        "ghost-account",
    );
}

/// 限定范围的读（`rows` 里的 `load_*_within`、`principal_within_roots`）本身：范围里的目标读得到，范围外的、不存在的、范围为空的都是「没有」。
/// 范围是账户子树：根自己、它下面的账户、成员关系的付款账户（管理员关系用所属组织）、凭证绑定的账户、额度的授权方（父）账户。
#[test]
fn the_scoped_readers_see_exactly_the_targets_inside_the_given_roots() {
    use super::rows::{
        load_allocation_record_within, load_credential_within, load_membership_within,
        principal_within_roots,
    };
    let env = Env::new();
    env.tree();
    env.allocation_chain();
    let member_in = env.issue_as_operator(Issue::member("m-kid1"));
    let member_sibling = env.issue_as_operator(Issue::member("m-kid2"));
    let service = env.issue_as_operator(Issue::service("eduai", vec![]));
    let conn = env.raw();
    let roots = |names: &[&str]| -> Vec<AccountId> { names.iter().map(|name| aid(name)).collect() };
    let membership = |id: &str, names: &[&str]| {
        load_membership_within(&conn, &mid(id), &roots(names))
            .unwrap()
            .is_some()
    };
    let credential = |minted: &Minted, names: &[&str]| {
        load_credential_within(&conn, &cid(minted.id()), &roots(names))
            .unwrap()
            .is_some()
    };
    let allocation = |id: &str, names: &[&str]| {
        load_allocation_record_within(&conn, &AllocationId::new(id).unwrap(), &roots(names))
            .unwrap()
            .is_some()
    };
    let principal =
        |id: &str, names: &[&str]| principal_within_roots(&conn, &pid(id), &roots(names)).unwrap();
    // 成员关系：付款账户 s1 在 teacher 下、s2 在 eduai 下 teacher 之外、b1 在 school-b；m-tch 的付款账户就是 teacher 自己
    assert!(membership("m-kid1", &["teacher"]));
    assert!(!membership("m-kid2", &["teacher"]));
    assert!(membership("m-tch", &["teacher"]));
    assert!(membership("m-kid2", &["eduai"]));
    assert!(membership("m-kid2", &["pool"]));
    assert!(!membership("m-kb1", &["eduai"]));
    assert!(membership("m-kb1", &["school-b", "teacher"]));
    assert!(!membership("m-ghost", &["eduai"]));
    assert!(!membership("m-kid1", &[]));
    // 凭证：成员凭证看成员关系的付款账户，服务凭证看绑定的账户
    assert!(credential(&member_in, &["teacher"]));
    assert!(!credential(&member_sibling, &["teacher"]));
    assert!(credential(&member_sibling, &["eduai"]));
    assert!(credential(&service, &["eduai"]));
    assert!(credential(&service, &["platform"]));
    assert!(!credential(&service, &["teacher"]));
    assert!(!credential(&service, &[]));
    // 额度：看授权方（父）账户，不看被授权方
    assert!(!allocation("a-teacher", &["teacher"]));
    assert!(allocation("a-teacher", &["eduai"]));
    assert!(allocation("a-teacher", &["pool"]));
    assert!(!allocation("a-b", &["eduai"]));
    assert!(allocation("a-b", &["platform"]));
    assert!(!allocation("a-ghost", &["platform"]));
    assert!(!allocation("a-b", &[]));
    // 主体：每个成员关系的付款账户都要在范围里，并且至少有一个
    assert!(principal("kid1", &["teacher"]));
    assert!(!principal("kid1", &["school-b"]));
    assert!(!principal("kid2", &["teacher"]));
    assert!(principal("kid2", &["eduai"]));
    assert!(!principal("kid1", &[]));
    assert!(!principal("p-ghost", &["platform"]));
    env.store
        .ensure_principal(&pid("p-orphan"), ts(T0))
        .unwrap();
    assert!(!principal("p-orphan", &["platform"]));
    // 足迹一半在 teacher 子树里、一半在外面：要全部落在范围里才算
    env.account("s5", AccountKind::Member, Some("teacher"));
    env.account("s6", AccountKind::Member, Some("eduai"));
    env.account("s7", AccountKind::Member, Some("teacher"));
    let attach = |membership: &str, principal: &str, account: &str| {
        env.store
            .create_membership(
                &member_membership(membership, principal, "eduai", account),
                ts(T0),
            )
            .unwrap();
    };
    env.store
        .ensure_principal(&pid("p-partial"), ts(T0))
        .unwrap();
    attach("m-p1", "p-partial", "s5");
    attach("m-p2", "p-partial", "s6");
    assert!(!principal("p-partial", &["teacher"]));
    assert!(principal("p-partial", &["teacher", "eduai"]));
    // 登记它的组织也要在范围里：登记组织 eduai 在 teacher 子树之外，成员关系在里面
    assert_eq!(
        Action::CreatePrincipal {
            principal: "p-reg",
            org: "eduai"
        }
        .run(&env, &Env::operator(), T0),
        Ok(WriteResult::Created)
    );
    attach("m-r1", "p-reg", "s7");
    assert!(!principal("p-reg", &["teacher"]));
    assert!(principal("p-reg", &["eduai"]));
}

/// `authorize_op` 的金额语义：范围先判、金额后判。范围里的账户：金额不超过权限上限是通过，超过是 `amount_exceeds_authority`，负数是 `invalid_amount`；
/// 范围外的、不存在的账户：金额是什么都一样是 `out_of_scope`（契约的 `authorize` 对存在的目标先判负金额，会把账户存在与否漏出去）。
#[test]
fn authorize_op_judges_the_scope_before_the_amount() {
    use super::control::{authorize_op, resolve_caller};
    let env = Env::new();
    env.tree();
    let narrow = narrow_for(&env, ControlOp::GrantAllocation);
    let conn = env.raw();
    let caller = resolve_caller(&conn, &narrow.who(), ts(T0 + 1))
        .unwrap_or_else(|(_, failure)| panic!("{failure:?}"));
    let check = |target: &str, micros: i64| {
        authorize_op(
            &conn,
            &caller,
            ts(T0 + 1),
            ControlOp::GrantAllocation,
            &aid(target),
            Some(&thb(micros)),
        )
        .map(|_| ())
        .map_err(|failure| failure.reason)
    };
    // 范围里（teacher 子树，单笔上限 1_000_000_000）
    assert_eq!(check("teacher", 1_000_000), Ok(()));
    assert_eq!(
        check("teacher", 2_000_000_000),
        Err(ControlReason::AmountExceedsAuthority)
    );
    assert_eq!(check("teacher", -5), Err(ControlReason::InvalidAmount));
    // 范围外的、不存在的：先判范围
    for target in ["eduai", "school-b", "ghost"] {
        for micros in [1_000_000, -5] {
            assert_eq!(
                check(target, micros),
                Err(ControlReason::OutOfScope),
                "{target} {micros}"
            );
        }
    }
}

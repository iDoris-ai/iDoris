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

    fn account(&self, id: &str, kind: AccountKind, parent: Option<&str>) {
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
    let free = |id: &str, month: &str, micros: i64| {
        let mut request = grant(id, "eduai", "s2", micros);
        request.purpose = Purpose::FreeTier;
        request.month = Some(month.parse::<BillingMonth>().unwrap());
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
    // 别的组织也「登记」同一个 id：只是返回已有的那一个，登记人仍然是 eduai
    assert_eq!(
        Action::CreatePrincipal {
            principal: "fresh",
            org: "school-b"
        }
        .run(&env, &other.who(), T0 + 2),
        Ok(WriteResult::Replayed)
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
    // 一个主体在两个组织都有成员关系：一个组织不能把它整个撤销，只能撤销自己这边的成员关系
    env.account("acct-shared", AccountKind::Member, Some("school-b"));
    assert_eq!(
        Action::CreateMembership {
            id: "m-shared",
            principal: "kid2",
            org: "school-b",
            account: "acct-shared"
        }
        .run(&env, &other.who(), T0 + 7),
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
        Err(ControlReason::InvalidMembership)
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
    assert_eq!(
        set(CredentialStatus::Frozen).run(&env, &who, T0 + 1),
        Ok(WriteResult::Updated)
    );
    assert_eq!(
        set(CredentialStatus::Frozen).run(&env, &who, T0 + 2),
        Ok(WriteResult::Unchanged)
    );
    assert_eq!(
        set(CredentialStatus::Active).run(&env, &who, T0 + 3),
        Ok(WriteResult::Updated)
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

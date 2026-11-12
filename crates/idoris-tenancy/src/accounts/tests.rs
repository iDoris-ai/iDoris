//! 账户存储的测试。三类：
//!
//! 1. **代码层**：通过 `AccountStore` 的公开函数写入，断言每个函数的正例和反例，以及契约层（S0）的错误原样带出来。
//! 2. **数据库层**：绕过 Rust 层，用另一条连接直接写 SQL，证明外键、唯一索引、CHECK、触发器能挡住坏数据。
//!    每个约束都有一条反例，并且有对照组（同样的语句，只改坏那一处，就能写进去）证明失败确实是这条约束造成的。
//! 3. **与 S0 对照**：迁移里写死的常量（深度上限、父子种类矩阵、枚举集合、id 字符集、单位写法）逐项对照 S0 的函数，
//!    S0 改了而迁移没改，这里立刻红。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, AccountTree, Allocation, AllocationError, DirectoryError,
    MAX_ACCOUNT_DEPTH, Membership, MembershipRole, MembershipStatus, Principal, PrincipalStatus,
    TreeError, subsidy_idempotency_key,
};
use idoris_compute_contracts::credential::{
    AdminScope, ControlOp, Credential, CredentialBinding, CredentialError, CredentialStatus,
    ResolveError, resolve_payment_context,
};
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, BillingMonth, CredentialId, IdempotencyKey, MembershipId, NodeId,
    PrincipalId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use rusqlite::Connection;

use super::rows::{from_text, to_text};
use super::{AccountStore, AccountStoreError, AllocationOutcome, PaymentContextError};
use crate::test_support::TempDir;

// ---------------------------------------------------------------- 夹具

const T0: i64 = 1_793_534_400_000; // 2026-11-01T12:00:00Z
const DAY_MS: i64 = 86_400_000;

fn now() -> TimestampMs {
    TimestampMs(T0)
}

struct Env {
    _dir: TempDir,
    path: PathBuf,
    store: AccountStore,
}

fn env() -> Env {
    let dir = TempDir::new("accounts");
    let path = dir.db("accounts.sqlite3");
    let store = AccountStore::open(&path).unwrap();
    Env {
        _dir: dir,
        path,
        store,
    }
}

/// 绕过 Rust 层的第二条连接，外键打开（和生产连接一致），用来直接写 SQL。
fn raw(env: &Env) -> Connection {
    let conn = Connection::open(&env.path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn
}

fn aid(text: &str) -> AccountId {
    AccountId::new(text).unwrap()
}
fn pid(text: &str) -> PrincipalId {
    PrincipalId::new(text).unwrap()
}
fn mid(text: &str) -> MembershipId {
    MembershipId::new(text).unwrap()
}
fn cid(text: &str) -> CredentialId {
    CredentialId::new(text).unwrap()
}
fn alid(text: &str) -> AllocationId {
    AllocationId::new(text).unwrap()
}

fn account(id: &str, kind: AccountKind, parent: Option<&str>) -> Account {
    Account {
        id: aid(id),
        kind,
        parent: parent.map(aid),
        unit: Unit::Thb,
        status: AccountStatus::Active,
    }
}

fn membership(
    id: &str,
    principal: &str,
    org: &str,
    role: MembershipRole,
    acct: Option<&str>,
) -> Membership {
    Membership {
        id: mid(id),
        principal: pid(principal),
        org: aid(org),
        role,
        account: acct.map(aid),
        status: MembershipStatus::Active,
    }
}

fn thb(micros: i64) -> Money {
    Money::new(Unit::Thb, micros)
}

fn grant(micros: i64, purpose: Purpose) -> Grant {
    Grant::new(
        thb(micros),
        purpose,
        TimestampMs(T0),
        TimestampMs(T0 + 30 * DAY_MS),
    )
    .unwrap()
}

fn allocation(id: &str, parent: &str, child: &str, g: Grant, key: Option<&str>) -> Allocation {
    Allocation {
        id: alid(id),
        parent: aid(parent),
        child: aid(child),
        ceiling: g,
        idempotency_key: key.map(|k| IdempotencyKey::new(k).unwrap()),
    }
}

fn credential(id: &str, binding: CredentialBinding) -> Credential {
    Credential::new(cid(id), binding, CredentialStatus::Active, None, Vec::new()).unwrap()
}

/// 夹具里的树和成员（测试名字里会引用）：
///
/// ```text
/// plat (Platform, THB)
/// ├─ pool (SubsidyPool)
/// ├─ org-a (Org)
/// │   ├─ sp-a (Sponsor)        <- m-alice-a（p-alice，担保人）
/// │   │    └─ stu-1 (Member)   <- m-bob-a（p-bob）
/// │   └─ stu-2 (Member)        <- m-multi-a（p-multi）
/// └─ org-b (Org)
///     └─ stu-b (Member)        <- m-multi-b（p-multi：同一个主体属于两个组织）
/// m-admin-a：p-admin 是 org-a 的管理员（没有付款账户）
/// ```
fn seeded() -> Env {
    let env = env();
    let s = &env.store;
    for (id, kind, parent) in [
        ("plat", AccountKind::Platform, None),
        ("pool", AccountKind::SubsidyPool, Some("plat")),
        ("org-a", AccountKind::Org, Some("plat")),
        ("sp-a", AccountKind::Sponsor, Some("org-a")),
        ("stu-1", AccountKind::Member, Some("sp-a")),
        ("stu-2", AccountKind::Member, Some("org-a")),
        ("org-b", AccountKind::Org, Some("plat")),
        ("stu-b", AccountKind::Member, Some("org-b")),
    ] {
        s.create_account(&account(id, kind, parent), now()).unwrap();
    }
    for p in ["p-alice", "p-bob", "p-multi", "p-admin"] {
        s.ensure_principal(&pid(p), now()).unwrap();
    }
    for m in [
        membership(
            "m-alice-a",
            "p-alice",
            "org-a",
            MembershipRole::Sponsor,
            Some("sp-a"),
        ),
        membership(
            "m-bob-a",
            "p-bob",
            "org-a",
            MembershipRole::Member,
            Some("stu-1"),
        ),
        membership(
            "m-multi-a",
            "p-multi",
            "org-a",
            MembershipRole::Member,
            Some("stu-2"),
        ),
        membership(
            "m-multi-b",
            "p-multi",
            "org-b",
            MembershipRole::Member,
            Some("stu-b"),
        ),
        membership("m-admin-a", "p-admin", "org-a", MembershipRole::Admin, None),
    ] {
        s.create_membership(&m, now()).unwrap();
    }
    env
}

/// 原始 SQL 被拒绝，并且错误文本包含 `needle`（约束名 / 触发器里写的说明）。
///
/// `needle` 可以用 `|` 写几个可接受的说法：SQLite 里 BEFORE INSERT 触发器先于 CHECK 约束执行，
/// 同时违反两者的语句报的是触发器的说明；这类语句我们只关心「被挡住了」，两种说法都算。
fn assert_rejected(conn: &Connection, sql: &str, needle: &str) {
    match conn.execute_batch(sql) {
        Ok(()) => panic!("expected the database to reject: {sql}"),
        Err(error) => {
            let text = error.to_string();
            assert!(
                needle
                    .split('|')
                    .any(|alternative| text.contains(alternative)),
                "rejected, but with {text:?} instead of something containing {needle:?}\nsql: {sql}"
            );
        }
    }
}

fn assert_accepted(conn: &Connection, sql: &str) {
    conn.execute_batch(sql)
        .unwrap_or_else(|error| panic!("expected the database to accept {sql}: {error}"));
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

// ================================================================ 打开与迁移

#[test]
fn opening_twice_applies_the_migration_once() {
    let env = env();
    let Env { _dir, path, store } = env;
    drop(store);
    let reopened = AccountStore::open(&path).unwrap();
    drop(reopened);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM account_schema_migrations"),
        1
    );
    assert_eq!(
        count(&conn, "SELECT version FROM account_schema_migrations"),
        1
    );
    for table in [
        "principals",
        "accounts",
        "memberships",
        "credentials",
        "credential_admin_scopes",
        "allocations",
    ] {
        let found: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "{table} must exist");
    }
}

#[test]
fn the_connection_is_wal_with_busy_timeout_and_foreign_keys() {
    let env = env();
    let snap = env.store.connection_pragmas().unwrap();
    assert_eq!(snap.journal_mode, "wal");
    assert_eq!(snap.busy_timeout_ms, 5_000);
    assert!(snap.foreign_keys);
}

#[test]
fn every_new_table_is_strict() {
    let env = env();
    let conn = raw(&env);
    let strict: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_list WHERE schema='main' AND type='table' AND strict=1 \
             AND name IN ('principals','accounts','memberships','credentials','credential_admin_scopes','allocations')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(strict, 6);
}

// ================================================================ 账户（代码层）

#[test]
fn accounts_round_trip_and_the_ancestor_chain_runs_self_to_root() {
    let env = seeded();
    let s = &env.store;
    let stu1 = s.get_account(&aid("stu-1")).unwrap().unwrap();
    assert_eq!(stu1, account("stu-1", AccountKind::Member, Some("sp-a")));
    let chain: Vec<String> = s
        .ancestors(&aid("stu-1"))
        .unwrap()
        .into_iter()
        .map(|a| a.id.to_string())
        .collect();
    assert_eq!(chain, ["stu-1", "sp-a", "org-a", "plat"]);
    let kids: Vec<String> = s
        .children(&aid("org-a"))
        .unwrap()
        .into_iter()
        .map(|a| a.id.to_string())
        .collect();
    assert_eq!(kids, ["sp-a", "stu-2"]);
    // 整棵树读回来通过 S0 的校验，并且链与 `AccountTree::ancestors` 一致。
    let tree = s.load_tree().unwrap().unwrap();
    assert_eq!(tree.len(), 8);
    let from_tree: Vec<String> = tree
        .ancestors(&aid("stu-1"))
        .unwrap()
        .into_iter()
        .map(|a| a.id.to_string())
        .collect();
    assert_eq!(from_tree, chain);
    assert!(s.get_account(&aid("nope")).unwrap().is_none());
    assert!(matches!(
        s.ancestors(&aid("nope")),
        Err(AccountStoreError::NotFound {
            kind: "account",
            ..
        })
    ));
    assert!(env.store.load_tree().unwrap().is_some());
}

#[test]
fn an_empty_store_has_no_tree_and_the_first_account_must_be_the_platform_root() {
    let env = env();
    assert!(env.store.load_tree().unwrap().is_none());
    assert!(env.store.load_directory().unwrap().is_none());
    match env
        .store
        .create_account(&account("orphan", AccountKind::Org, None), now())
    {
        Err(AccountStoreError::Tree(TreeError::NonPlatformRoot(id))) => {
            assert_eq!(id, aid("orphan"))
        }
        other => panic!("{other:?}"),
    }
    match env
        .store
        .create_account(&account("child", AccountKind::Org, Some("ghost")), now())
    {
        Err(AccountStoreError::Tree(TreeError::MissingParent { parent, .. })) => {
            assert_eq!(parent, aid("ghost"));
        }
        other => panic!("{other:?}"),
    }
    env.store
        .create_account(&account("plat", AccountKind::Platform, None), now())
        .unwrap();
    assert!(env.store.load_tree().unwrap().is_some());
}

#[test]
fn a_second_platform_root_is_rejected_by_the_contract_check() {
    let env = seeded();
    match env
        .store
        .create_account(&account("plat-2", AccountKind::Platform, None), now())
    {
        Err(AccountStoreError::Tree(TreeError::MultipleRoots)) => {}
        other => panic!("{other:?}"),
    }
    // 对照：拒绝之后什么都没写进去。
    assert!(env.store.get_account(&aid("plat-2")).unwrap().is_none());
}

#[test]
fn a_duplicate_account_id_is_rejected_and_changes_nothing() {
    let env = seeded();
    match env
        .store
        .create_account(&account("org-a", AccountKind::Org, Some("plat")), now())
    {
        Err(AccountStoreError::AlreadyExists {
            kind: "account",
            id,
        }) => assert_eq!(id, "org-a"),
        other => panic!("{other:?}"),
    }
    assert_eq!(env.store.load_tree().unwrap().unwrap().len(), 8);
}

#[test]
fn parent_child_kinds_and_units_are_checked_by_the_contract() {
    let env = seeded();
    // Sponsor 下面只能挂 Member。
    match env
        .store
        .create_account(&account("bad", AccountKind::Org, Some("sp-a")), now())
    {
        Err(AccountStoreError::Tree(TreeError::KindNotAllowed { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 父子单位必须相同。
    let mut quota = account("quota-kid", AccountKind::Member, Some("org-a"));
    quota.unit = Unit::Quota(NodeId::new("node-1").unwrap());
    match env.store.create_account(&quota, now()) {
        Err(AccountStoreError::Tree(TreeError::UnitMismatch { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 对照：合法的同一个位置能写进去。
    env.store
        .create_account(&account("fine", AccountKind::Member, Some("org-a")), now())
        .unwrap();
}

#[test]
fn the_depth_limit_is_enforced_at_exactly_the_contract_maximum() {
    let env = env();
    let s = &env.store;
    s.create_account(&account("d0", AccountKind::Platform, None), now())
        .unwrap();
    let mut parent = "d0".to_string();
    for depth in 1..=MAX_ACCOUNT_DEPTH {
        let id = format!("d{depth}");
        s.create_account(&account(&id, AccountKind::Org, Some(&parent)), now())
            .unwrap_or_else(|error| panic!("depth {depth} must be allowed: {error}"));
        parent = id;
    }
    match s.create_account(&account("too-deep", AccountKind::Org, Some(&parent)), now()) {
        Err(AccountStoreError::Tree(TreeError::TooDeep { depth, max, .. })) => {
            assert_eq!((depth, max), (MAX_ACCOUNT_DEPTH + 1, MAX_ACCOUNT_DEPTH));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn account_status_changes_follow_the_rules_and_closing_is_final() {
    let env = seeded();
    let s = &env.store;
    s.set_account_status(&aid("org-a"), AccountStatus::Frozen)
        .unwrap();
    assert_eq!(
        s.get_account(&aid("org-a")).unwrap().unwrap().status,
        AccountStatus::Frozen
    );
    s.set_account_status(&aid("org-a"), AccountStatus::Frozen)
        .unwrap(); // 空操作
    s.set_account_status(&aid("org-a"), AccountStatus::Active)
        .unwrap(); // 可以解冻
    s.set_account_status(&aid("org-a"), AccountStatus::Closed)
        .unwrap();
    match s.set_account_status(&aid("org-a"), AccountStatus::Active) {
        Err(AccountStoreError::IllegalTransition {
            kind: "account",
            from,
            to,
            ..
        }) => {
            assert_eq!((from.as_str(), to.as_str()), ("closed", "active"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        s.set_account_status(&aid("ghost"), AccountStatus::Frozen),
        Err(AccountStoreError::NotFound {
            kind: "account",
            ..
        })
    ));
}

// ================================================================ 主体与成员关系（代码层）

#[test]
fn ensure_principal_is_idempotent_and_never_resurrects_a_revoked_principal() {
    let env = env();
    let s = &env.store;
    let first = s.ensure_principal(&pid("p-1"), now()).unwrap();
    assert_eq!(
        first,
        Principal {
            id: pid("p-1"),
            status: PrincipalStatus::Active
        }
    );
    assert_eq!(
        s.ensure_principal(&pid("p-1"), TimestampMs(T0 + 5))
            .unwrap(),
        first
    );
    assert!(s.revoke_principal(&pid("p-1"), now()).unwrap());
    assert!(!s.revoke_principal(&pid("p-1"), now()).unwrap()); // 再撤销一次：没有改动
    let again = s
        .ensure_principal(&pid("p-1"), TimestampMs(T0 + 9))
        .unwrap();
    assert_eq!(
        again.status,
        PrincipalStatus::Revoked,
        "re-provisioning must not resurrect"
    );
    assert!(matches!(
        s.revoke_principal(&pid("ghost"), now()),
        Err(AccountStoreError::NotFound {
            kind: "principal",
            ..
        })
    ));
    assert!(s.get_principal(&pid("ghost")).unwrap().is_none());
}

#[test]
fn memberships_round_trip_and_a_principal_can_belong_to_several_orgs() {
    let env = seeded();
    let s = &env.store;
    let multi = s.memberships_of(&pid("p-multi")).unwrap();
    let ids: Vec<String> = multi.iter().map(|m| m.id.to_string()).collect();
    assert_eq!(ids, ["m-multi-a", "m-multi-b"]);
    assert_eq!(
        s.get_membership(&mid("m-alice-a")).unwrap().unwrap(),
        membership(
            "m-alice-a",
            "p-alice",
            "org-a",
            MembershipRole::Sponsor,
            Some("sp-a")
        )
    );
    let admin = s.get_membership(&mid("m-admin-a")).unwrap().unwrap();
    assert_eq!(admin.account, None);
    assert!(s.get_membership(&mid("ghost")).unwrap().is_none());
    assert!(s.memberships_of(&pid("nobody")).unwrap().is_empty());
}

#[test]
fn membership_writes_are_validated_by_the_contract_directory() {
    let env = seeded();
    let s = &env.store;
    let mk = |id, p, org, role, acct| membership(id, p, org, role, acct);
    // 主体不存在
    match s.create_membership(
        &mk("m-x", "ghost", "org-a", MembershipRole::Admin, None),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::UnknownPrincipal { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 组织不存在
    match s.create_membership(
        &mk("m-x", "p-bob", "ghost", MembershipRole::Admin, None),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::UnknownOrg { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 「组织」不是 org / platform
    match s.create_membership(
        &mk("m-x", "p-bob", "stu-2", MembershipRole::Admin, None),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::NotAnOrg { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 角色与账户种类对不上：担保人角色绑了成员账户
    match s.create_membership(
        &mk(
            "m-x",
            "p-bob",
            "org-a",
            MembershipRole::Sponsor,
            Some("stu-2"),
        ),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::RoleAccountMismatch { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 管理员不能有付款账户；成员必须有
    match s.create_membership(
        &mk(
            "m-x",
            "p-bob",
            "org-a",
            MembershipRole::Admin,
            Some("stu-2"),
        ),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::RoleAccountMismatch { .. })) => {}
        other => panic!("{other:?}"),
    }
    match s.create_membership(
        &mk("m-x", "p-bob", "org-a", MembershipRole::Member, None),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::RoleAccountMismatch { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 账户不在该组织的子树里（stu-b 属于 org-b）
    match s.create_membership(
        &mk(
            "m-x",
            "p-bob",
            "org-a",
            MembershipRole::Member,
            Some("stu-b"),
        ),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::AccountNotUnderOrg { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 账户不存在
    match s.create_membership(
        &mk(
            "m-x",
            "p-bob",
            "org-a",
            MembershipRole::Member,
            Some("ghost"),
        ),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::UnknownAccount { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 一个账户至多属于一个成员关系
    match s.create_membership(
        &mk(
            "m-dup",
            "p-alice",
            "org-a",
            MembershipRole::Member,
            Some("stu-1"),
        ),
        now(),
    ) {
        Err(AccountStoreError::Directory(DirectoryError::AccountAlreadyBound {
            first,
            second,
            ..
        })) => {
            assert_eq!((first, second), (mid("m-bob-a"), mid("m-dup")));
        }
        other => panic!("{other:?}"),
    }
    // id 重复
    match s.create_membership(
        &mk("m-bob-a", "p-bob", "org-a", MembershipRole::Admin, None),
        now(),
    ) {
        Err(AccountStoreError::AlreadyExists {
            kind: "membership", ..
        }) => {}
        other => panic!("{other:?}"),
    }
    // 对照：这些失败都没有留下半条数据；而合法的同类写入能成功。
    assert_eq!(s.memberships_of(&pid("p-bob")).unwrap().len(), 1);
    s.create_membership(
        &mk("m-ok", "p-bob", "org-a", MembershipRole::Admin, None),
        now(),
    )
    .unwrap();
    assert_eq!(s.memberships_of(&pid("p-bob")).unwrap().len(), 2);
}

#[test]
fn revoking_a_membership_is_reported_once_and_is_final() {
    let env = seeded();
    let s = &env.store;
    assert!(s.revoke_membership(&mid("m-bob-a"), now()).unwrap());
    assert!(!s.revoke_membership(&mid("m-bob-a"), now()).unwrap());
    assert_eq!(
        s.get_membership(&mid("m-bob-a")).unwrap().unwrap().status,
        MembershipStatus::Revoked
    );
    assert!(matches!(
        s.revoke_membership(&mid("ghost"), now()),
        Err(AccountStoreError::NotFound {
            kind: "membership",
            ..
        })
    ));
}

// ================================================================ 凭证（代码层）

fn scope(op: ControlOp, subtree: &str, max: Option<Money>) -> AdminScope {
    AdminScope {
        op,
        subtree: aid(subtree),
        max_amount: max,
    }
}

#[test]
fn credentials_round_trip_with_their_admin_scopes_in_order() {
    let env = seeded();
    let s = &env.store;
    let service = Credential::new(
        cid("svc-a"),
        CredentialBinding::Service {
            account: aid("org-a"),
        },
        CredentialStatus::Active,
        Some(TimestampMs(T0 + DAY_MS)),
        vec![
            scope(ControlOp::GrantAllocation, "org-a", Some(thb(5_000_000))),
            scope(ControlOp::IssueCredential, "sp-a", None),
            scope(ControlOp::CreateAccount, "org-a", None),
        ],
    )
    .unwrap();
    s.create_credential(&service, now()).unwrap();
    assert_eq!(s.get_credential(&cid("svc-a")).unwrap().unwrap(), service);

    let node = credential(
        "node-a",
        CredentialBinding::Node {
            account: aid("org-a"),
        },
    );
    s.create_credential(&node, now()).unwrap();
    assert_eq!(s.get_credential(&cid("node-a")).unwrap().unwrap(), node);

    let member = credential(
        "cred-bob",
        CredentialBinding::Member {
            membership: mid("m-bob-a"),
        },
    );
    s.create_credential(&member, now()).unwrap();
    assert_eq!(s.get_credential(&cid("cred-bob")).unwrap().unwrap(), member);
    assert!(s.get_credential(&cid("ghost")).unwrap().is_none());
}

#[test]
fn credential_bindings_are_validated_before_anything_is_written() {
    let env = seeded();
    let s = &env.store;
    // service / node 必须绑 org 账户
    for binding in [
        CredentialBinding::Service {
            account: aid("stu-1"),
        },
        CredentialBinding::Node {
            account: aid("plat"),
        },
    ] {
        match s.create_credential(&credential("bad", binding), now()) {
            Err(AccountStoreError::InvalidBinding(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    // 账户 / 成员关系不存在
    assert!(matches!(
        s.create_credential(
            &credential(
                "bad",
                CredentialBinding::Service {
                    account: aid("ghost")
                }
            ),
            now()
        ),
        Err(AccountStoreError::NotFound {
            kind: "account",
            ..
        })
    ));
    assert!(matches!(
        s.create_credential(
            &credential(
                "bad",
                CredentialBinding::Member {
                    membership: mid("ghost")
                }
            ),
            now()
        ),
        Err(AccountStoreError::NotFound {
            kind: "membership",
            ..
        })
    ));
    // 管理员没有付款账户，不能绑
    match s.create_credential(
        &credential(
            "bad",
            CredentialBinding::Member {
                membership: mid("m-admin-a"),
            },
        ),
        now(),
    ) {
        Err(AccountStoreError::InvalidBinding(_)) => {}
        other => panic!("{other:?}"),
    }
    // 权限范围不能越出自己绑定的账户（org-b 不在 org-a 之内）
    let outside = Credential::new(
        cid("bad"),
        CredentialBinding::Service {
            account: aid("org-a"),
        },
        CredentialStatus::Active,
        None,
        vec![scope(ControlOp::CreateAccount, "org-b", None)],
    )
    .unwrap();
    match s.create_credential(&outside, now()) {
        Err(AccountStoreError::InvalidBinding(_)) => {}
        other => panic!("{other:?}"),
    }
    // 范围里的账户不存在
    let ghost = Credential::new(
        cid("bad"),
        CredentialBinding::Service {
            account: aid("org-a"),
        },
        CredentialStatus::Active,
        None,
        vec![scope(ControlOp::CreateAccount, "ghost", None)],
    )
    .unwrap();
    assert!(matches!(
        s.create_credential(&ghost, now()),
        Err(AccountStoreError::NotFound {
            kind: "account",
            ..
        })
    ));
    // 失败的写入不留痕迹；对照：合法的能写进去。
    assert!(s.get_credential(&cid("bad")).unwrap().is_none());
    s.create_credential(
        &credential(
            "good",
            CredentialBinding::Service {
                account: aid("org-a"),
            },
        ),
        now(),
    )
    .unwrap();
    // id 重复
    assert!(matches!(
        s.create_credential(
            &credential(
                "good",
                CredentialBinding::Node {
                    account: aid("org-b")
                }
            ),
            now()
        ),
        Err(AccountStoreError::AlreadyExists {
            kind: "credential",
            ..
        })
    ));
}

#[test]
fn a_member_credential_cannot_carry_admin_scopes_at_the_contract_level() {
    // 契约层（S0）在构造时就拒绝；存储层不需要、也不可能收到这种凭证。
    let built = Credential::new(
        cid("m"),
        CredentialBinding::Member {
            membership: mid("m-bob-a"),
        },
        CredentialStatus::Active,
        None,
        vec![scope(ControlOp::CreateAccount, "stu-1", None)],
    );
    assert_eq!(
        built.unwrap_err(),
        CredentialError::MemberCredentialWithAdminScopes
    );
}

#[test]
fn credential_status_moves_freely_between_active_and_frozen_and_revocation_is_final() {
    let env = seeded();
    let s = &env.store;
    s.create_credential(
        &credential(
            "c",
            CredentialBinding::Service {
                account: aid("org-a"),
            },
        ),
        now(),
    )
    .unwrap();
    s.set_credential_status(&cid("c"), CredentialStatus::Frozen, now())
        .unwrap();
    assert_eq!(
        s.get_credential(&cid("c")).unwrap().unwrap().status(),
        CredentialStatus::Frozen
    );
    s.set_credential_status(&cid("c"), CredentialStatus::Active, now())
        .unwrap();
    s.set_credential_status(&cid("c"), CredentialStatus::Active, now())
        .unwrap(); // 空操作
    s.set_credential_status(&cid("c"), CredentialStatus::Revoked, now())
        .unwrap();
    for to in [CredentialStatus::Active, CredentialStatus::Frozen] {
        match s.set_credential_status(&cid("c"), to, now()) {
            Err(AccountStoreError::IllegalTransition {
                kind: "credential",
                from,
                ..
            }) => {
                assert_eq!(from, "revoked");
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        s.get_credential(&cid("c")).unwrap().unwrap().status(),
        CredentialStatus::Revoked
    );
    assert!(matches!(
        s.set_credential_status(&cid("ghost"), CredentialStatus::Frozen, now()),
        Err(AccountStoreError::NotFound {
            kind: "credential",
            ..
        })
    ));
}

// ================================================================ 额度（代码层）

#[test]
fn allocations_follow_tree_edges_and_cannot_exceed_the_grantor() {
    let env = seeded();
    let s = &env.store;
    // 平台没有上限：平台 -> org-a
    let top = allocation(
        "a-top",
        "plat",
        "org-a",
        grant(1_000_000_000, Purpose::Prepaid),
        None,
    );
    assert_eq!(
        s.create_allocation(&top, now()).unwrap(),
        AllocationOutcome::Created
    );
    assert_eq!(s.get_allocation(&alid("a-top")).unwrap().unwrap(), top);
    assert_eq!(
        s.allocations_into(&aid("org-a")).unwrap(),
        vec![top.clone()]
    );
    assert_eq!(s.allocations_from(&aid("plat")).unwrap(), vec![top.clone()]);
    // org-a -> sp-a 不超过 org-a 自己的上限：可以
    let mid_alloc = allocation(
        "a-mid",
        "org-a",
        "sp-a",
        grant(600_000_000, Purpose::Prepaid),
        None,
    );
    s.create_allocation(&mid_alloc, now()).unwrap();
    // 超过授权者本人：拒绝
    let too_big = allocation(
        "a-big",
        "org-a",
        "stu-2",
        grant(1_000_000_001, Purpose::Prepaid),
        None,
    );
    match s.create_allocation(&too_big, now()) {
        Err(AccountStoreError::Allocation(AllocationError::ExceedsGrantor { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 各子额度之和可以超过父额度（授权上限，不是转移余额）：对照
    let second = allocation(
        "a-second",
        "org-a",
        "stu-2",
        grant(1_000_000_000, Purpose::Prepaid),
        None,
    );
    s.create_allocation(&second, now()).unwrap();
    // 授权者自己没有额度：sp-a 有（600），stu-1 没有，所以 stu-1 不能再往下发（它是叶子，下发会先因不是父子失败）
    let no_authority = allocation("a-none", "sp-a", "stu-1", grant(10, Purpose::Prepaid), None);
    s.create_allocation(&no_authority, now()).unwrap(); // sp-a 有 600，下发 10 合法
    // 有效期没被授权者的额度包住：拒绝
    let longer = Grant::new(
        thb(1),
        Purpose::Prepaid,
        TimestampMs(T0),
        TimestampMs(T0 + 90 * DAY_MS),
    )
    .unwrap();
    match s.create_allocation(&allocation("a-long", "sp-a", "stu-1", longer, None), now()) {
        Err(AccountStoreError::Allocation(AllocationError::GrantorHasNoAuthority { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 不是父子：org-a 不能直接给 stu-1（stu-1 的父是 sp-a）
    match s.create_allocation(
        &allocation("a-skip", "org-a", "stu-1", grant(1, Purpose::Prepaid), None),
        now(),
    ) {
        Err(AccountStoreError::Allocation(AllocationError::NotParentChild { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 跨枝：org-b 不是 stu-1 的祖先
    match s.create_allocation(
        &allocation(
            "a-cross",
            "org-b",
            "stu-1",
            grant(1, Purpose::Prepaid),
            None,
        ),
        now(),
    ) {
        Err(AccountStoreError::Allocation(AllocationError::NotParentChild { .. })) => {}
        other => panic!("{other:?}"),
    }
    // 账户不存在
    assert!(matches!(
        s.create_allocation(
            &allocation(
                "a-ghost",
                "org-a",
                "ghost",
                grant(1, Purpose::Prepaid),
                None
            ),
            now()
        ),
        Err(AccountStoreError::Allocation(
            AllocationError::UnknownAccount(_)
        ))
    ));
    assert!(matches!(
        s.create_allocation(
            &allocation(
                "a-ghost",
                "ghost",
                "stu-2",
                grant(1, Purpose::Prepaid),
                None
            ),
            now()
        ),
        Err(AccountStoreError::Allocation(
            AllocationError::UnknownAccount(_)
        ))
    ));
    // id 重复
    match s.create_allocation(
        &allocation("a-top", "plat", "org-b", grant(1, Purpose::Prepaid), None),
        now(),
    ) {
        Err(AccountStoreError::Allocation(AllocationError::DuplicateId(_))) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_allocation_unit_must_match_the_child_account_unit() {
    let env = seeded();
    let usd = Grant::new(
        Money::new(Unit::Usd, 1),
        Purpose::Prepaid,
        TimestampMs(T0),
        TimestampMs(T0 + DAY_MS),
    )
    .unwrap();
    match env
        .store
        .create_allocation(&allocation("a-usd", "plat", "org-a", usd, None), now())
    {
        Err(AccountStoreError::Allocation(AllocationError::UnitMismatch { .. })) => {}
        other => panic!("{other:?}"),
    }
}

fn month() -> BillingMonth {
    BillingMonth::new(2026, 11).unwrap()
}

fn subsidy_key_of(principal: &str) -> String {
    subsidy_idempotency_key(&pid(principal), month())
        .unwrap()
        .to_string()
}

/// 补贴池 -> 成员的免费额度（池要先有额度，池下面要挂着成员账户：夹具里 pool 下没有成员，所以建两个）。
fn with_pool_members(env: &Env) {
    let s = &env.store;
    s.create_account(
        &account("pool-stu-1", AccountKind::Member, Some("pool")),
        now(),
    )
    .unwrap();
    s.create_account(
        &account("pool-stu-b", AccountKind::Member, Some("pool")),
        now(),
    )
    .unwrap();
    s.ensure_principal(&pid("p-sub"), now()).unwrap();
    // 同一个主体在两个「组织」里各有成员账户（这里用补贴池下的两个成员账户代表）
    s.create_membership(
        &membership(
            "m-sub-1",
            "p-sub",
            "plat",
            MembershipRole::Member,
            Some("pool-stu-1"),
        ),
        now(),
    )
    .unwrap();
    s.create_membership(
        &membership(
            "m-sub-2",
            "p-sub",
            "plat",
            MembershipRole::Member,
            Some("pool-stu-b"),
        ),
        now(),
    )
    .unwrap();
    // 池自己要先有额度，才有权力往成员账户下发（平台给池的额度不是「某个成员的月度补贴」，所以不是免费额度用途）
    s.create_allocation(
        &allocation(
            "pool-top",
            "plat",
            "pool",
            grant(9_000_000_000, Purpose::Prepaid),
            None,
        ),
        now(),
    )
    .unwrap();
}

#[test]
fn a_subsidy_is_issued_once_per_principal_and_month_even_across_two_member_accounts() {
    let env = seeded();
    with_pool_members(&env);
    let s = &env.store;
    let key = subsidy_key_of("p-sub");
    let first = allocation(
        "sub-1",
        "pool",
        "pool-stu-1",
        grant(5_000_000, Purpose::FreeTier),
        Some(&key),
    );
    assert_eq!(
        s.create_allocation(&first, now()).unwrap(),
        AllocationOutcome::Created
    );
    // 同一个主体、同一个月，换另一个成员账户再领：拒绝（免费额度只该领一份）
    let second = allocation(
        "sub-2",
        "pool",
        "pool-stu-b",
        grant(5_000_000, Purpose::FreeTier),
        Some(&key),
    );
    match s.create_allocation(&second, now()) {
        Err(AccountStoreError::Allocation(AllocationError::DuplicateIdempotencyKey(k))) => {
            assert_eq!(k.as_str(), key);
        }
        other => panic!("{other:?}"),
    }
    assert!(s.get_allocation(&alid("sub-2")).unwrap().is_none());
    // 同一个主体、下个月：可以
    let next_month =
        subsidy_idempotency_key(&pid("p-sub"), BillingMonth::new(2026, 12).unwrap()).unwrap();
    s.create_allocation(
        &allocation(
            "sub-dec",
            "pool",
            "pool-stu-1",
            grant(5_000_000, Purpose::FreeTier),
            Some(next_month.as_str()),
        ),
        now(),
    )
    .unwrap();
}

#[test]
fn replaying_the_same_subsidy_returns_the_existing_allocation_without_writing() {
    let env = seeded();
    with_pool_members(&env);
    let s = &env.store;
    let key = subsidy_key_of("p-sub");
    let original = allocation(
        "sub-1",
        "pool",
        "pool-stu-1",
        grant(5_000_000, Purpose::FreeTier),
        Some(&key),
    );
    s.create_allocation(&original, now()).unwrap();
    // 重试的调用方换了一个新 id，其余内容相同：视为同一次发放
    let retry = allocation(
        "sub-retry",
        "pool",
        "pool-stu-1",
        grant(5_000_000, Purpose::FreeTier),
        Some(&key),
    );
    assert_eq!(
        s.create_allocation(&retry, TimestampMs(T0 + 7)).unwrap(),
        AllocationOutcome::Replayed(original.clone())
    );
    assert!(s.get_allocation(&alid("sub-retry")).unwrap().is_none());
    assert_eq!(s.allocations_into(&aid("pool-stu-1")).unwrap().len(), 1);
    // 同一个键、金额不同：这不是重放，是冲突
    let conflicting = allocation(
        "sub-bigger",
        "pool",
        "pool-stu-1",
        grant(6_000_000, Purpose::FreeTier),
        Some(&key),
    );
    match s.create_allocation(&conflicting, now()) {
        Err(AccountStoreError::Allocation(AllocationError::DuplicateIdempotencyKey(_))) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn free_tier_keys_must_name_the_childs_own_member_and_other_purposes_may_not_use_them() {
    let env = seeded();
    with_pool_members(&env);
    let s = &env.store;
    let ok_key = subsidy_key_of("p-sub");
    let free = |id: &str, child: &str, key: Option<&str>| {
        allocation(id, "pool", child, grant(1_000_000, Purpose::FreeTier), key)
    };
    // 没带键
    match s.create_allocation(&free("f1", "pool-stu-1", None), now()) {
        Err(AccountStoreError::SubsidyKey(_)) => {}
        other => panic!("{other:?}"),
    }
    // 键里的主体不是子账户所属成员的主体
    let other_member = subsidy_key_of("p-alice");
    match s.create_allocation(&free("f2", "pool-stu-1", Some(&other_member)), now()) {
        Err(AccountStoreError::SubsidyKey(_)) => {}
        other => panic!("{other:?}"),
    }
    // 子账户没有绑定任何成员关系（pool 本身）
    match s.create_allocation(&free("f3", "org-a", Some(&ok_key)), now()) {
        Err(AccountStoreError::SubsidyKey(_) | AccountStoreError::Allocation(_)) => {}
        other => panic!("{other:?}"),
    }
    // 键的月份写坏
    match s.create_allocation(
        &free("f4", "pool-stu-1", Some("subsidy:p-sub:2026-13")),
        now(),
    ) {
        Err(AccountStoreError::SubsidyKey(_)) => {}
        other => panic!("{other:?}"),
    }
    // 别的用途不许用 `subsidy:` 键
    let prepaid = allocation(
        "f5",
        "pool",
        "pool-stu-1",
        grant(1, Purpose::Prepaid),
        Some(&ok_key),
    );
    match s.create_allocation(&prepaid, now()) {
        Err(AccountStoreError::SubsidyKey(_)) => {}
        other => panic!("{other:?}"),
    }
    // 对照：键对了就能写进去，并且别的用途可以用不以 subsidy: 开头的幂等键
    s.create_allocation(&free("f6", "pool-stu-1", Some(&ok_key)), now())
        .unwrap();
    let plain = allocation(
        "f7",
        "pool",
        "pool-stu-b",
        grant(1, Purpose::Prepaid),
        Some("topup-2026-11-001"),
    );
    s.create_allocation(&plain, now()).unwrap();
}

// ================================================================ 付款上下文（代码层，对照整棵目录）

/// 用「一小片目录」解析的结果，必须和用「整个目录」解析的结果完全一样。
fn assert_slice_equals_full(env: &Env, credential: &str, asserted: Option<&str>, at: TimestampMs) {
    let asserted = asserted.map(pid);
    let sliced = env
        .store
        .payment_context(&cid(credential), asserted.as_ref(), at);
    let directory = env.store.load_directory().unwrap().unwrap();
    let stored = env.store.get_credential(&cid(credential)).unwrap().unwrap();
    let full = resolve_payment_context(&directory, &stored, asserted.as_ref(), at);
    match (sliced, full) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{credential} {asserted:?}"),
        (Err(PaymentContextError::Resolve(a)), Err(b)) => {
            assert_eq!(a, b, "{credential} {asserted:?}")
        }
        other => {
            panic!("slice and full directory disagree for {credential} {asserted:?}: {other:?}")
        }
    }
}

fn payer(env: &Env, credential: &str, asserted: Option<&str>) -> Result<String, ResolveError> {
    let asserted = asserted.map(pid);
    match env
        .store
        .payment_context(&cid(credential), asserted.as_ref(), now())
    {
        Ok(context) => Ok(context.payer().to_string()),
        Err(PaymentContextError::Resolve(error)) => Err(error),
        other => panic!("{other:?}"),
    }
}

fn payment_env() -> Env {
    let env = seeded();
    let s = &env.store;
    for (id, binding) in [
        (
            "svc-a",
            CredentialBinding::Service {
                account: aid("org-a"),
            },
        ),
        (
            "svc-b",
            CredentialBinding::Service {
                account: aid("org-b"),
            },
        ),
        (
            "node-a",
            CredentialBinding::Node {
                account: aid("org-a"),
            },
        ),
        (
            "cred-bob",
            CredentialBinding::Member {
                membership: mid("m-bob-a"),
            },
        ),
        (
            "cred-alice",
            CredentialBinding::Member {
                membership: mid("m-alice-a"),
            },
        ),
    ] {
        s.create_credential(&credential(id, binding), now())
            .unwrap();
    }
    env
}

#[test]
fn every_request_resolves_to_exactly_one_payer_and_a_principal_in_two_orgs_pays_in_the_right_one() {
    let env = payment_env();
    // 同一个主体属于两个组织：每次请求的付款上下文唯一且正确（设计稿 §13 验收）
    assert_eq!(payer(&env, "svc-a", Some("p-multi")).unwrap(), "stu-2");
    assert_eq!(payer(&env, "svc-b", Some("p-multi")).unwrap(), "stu-b");
    assert_eq!(payer(&env, "svc-a", Some("p-bob")).unwrap(), "stu-1");
    assert_eq!(payer(&env, "svc-a", Some("p-alice")).unwrap(), "sp-a"); // 担保人自己备课也付自己的账户
    assert_eq!(payer(&env, "node-a", None).unwrap(), "org-a");
    assert_eq!(payer(&env, "cred-bob", None).unwrap(), "stu-1");
    assert_eq!(payer(&env, "cred-bob", Some("p-bob")).unwrap(), "stu-1");
    assert_eq!(payer(&env, "cred-alice", None).unwrap(), "sp-a");
    let context = env
        .store
        .payment_context(&cid("svc-a"), Some(&pid("p-multi")), now())
        .unwrap();
    assert_eq!(context.credential(), &cid("svc-a"));
    assert_eq!(context.unit(), &Unit::Thb);
    assert_eq!(context.membership(), Some(&mid("m-multi-a")));
    // 请求不能指定别人付款：越界、缺失、多余、冒名，都是明确的错误
    assert_eq!(
        payer(&env, "svc-b", Some("p-bob")),
        Err(ResolveError::MemberNotInScope)
    );
    assert_eq!(
        payer(&env, "svc-a", Some("nobody")),
        Err(ResolveError::MemberNotInScope)
    );
    assert_eq!(
        payer(&env, "svc-a", None),
        Err(ResolveError::MemberRequired)
    );
    assert_eq!(
        payer(&env, "node-a", Some("p-bob")),
        Err(ResolveError::MemberAssertionNotAllowed)
    );
    assert_eq!(
        payer(&env, "cred-bob", Some("p-alice")),
        Err(ResolveError::MemberMismatch)
    );
    assert!(matches!(
        env.store.payment_context(&cid("ghost"), None, now()),
        Err(PaymentContextError::UnknownCredential(_))
    ));
}

#[test]
fn the_sliced_directory_agrees_with_the_full_directory_in_every_scenario() {
    let env = payment_env();
    let s = &env.store;
    let all_credentials = ["svc-a", "svc-b", "node-a", "cred-bob", "cred-alice"];
    let all_principals = [
        None,
        Some("p-bob"),
        Some("p-alice"),
        Some("p-multi"),
        Some("p-admin"),
        Some("nobody"),
    ];
    let check_all = |env: &Env, at: TimestampMs| {
        for credential in all_credentials {
            for asserted in all_principals {
                assert_slice_equals_full(env, credential, asserted, at);
            }
        }
    };
    check_all(&env, now());

    // 担保人和成员各有一个有效的可付款账户在同一个组织里：歧义，拒绝去猜
    s.create_account(
        &account("sp-a2", AccountKind::Sponsor, Some("org-a")),
        now(),
    )
    .unwrap();
    s.create_membership(
        &membership(
            "m-multi-sp",
            "p-multi",
            "org-a",
            MembershipRole::Sponsor,
            Some("sp-a2"),
        ),
        now(),
    )
    .unwrap();
    assert_eq!(
        payer(&env, "svc-a", Some("p-multi")),
        Err(ResolveError::AmbiguousPaymentContext { count: 2 })
    );
    check_all(&env, now());

    // 撤销成员关系 / 主体 / 冻结账户 / 凭证状态 / 过期：每一种都和整棵目录的结论一致
    s.revoke_membership(&mid("m-multi-sp"), now()).unwrap();
    assert_eq!(payer(&env, "svc-a", Some("p-multi")).unwrap(), "stu-2");
    check_all(&env, now());
    s.revoke_membership(&mid("m-bob-a"), now()).unwrap();
    assert_eq!(
        payer(&env, "svc-a", Some("p-bob")),
        Err(ResolveError::MembershipRevoked(mid("m-bob-a")))
    );
    assert_eq!(
        payer(&env, "cred-bob", None),
        Err(ResolveError::MembershipRevoked(mid("m-bob-a")))
    );
    check_all(&env, now());
    s.revoke_principal(&pid("p-alice"), now()).unwrap();
    assert_eq!(
        payer(&env, "svc-a", Some("p-alice")),
        Err(ResolveError::PrincipalRevoked(pid("p-alice")))
    );
    check_all(&env, now());
    s.set_account_status(&aid("org-b"), AccountStatus::Frozen)
        .unwrap();
    assert!(matches!(
        payer(&env, "svc-b", Some("p-multi")),
        Err(ResolveError::AccountInactive { .. })
    ));
    check_all(&env, now());
    s.set_credential_status(&cid("svc-a"), CredentialStatus::Frozen, now())
        .unwrap();
    assert_eq!(
        payer(&env, "svc-a", Some("p-multi")),
        Err(ResolveError::CredentialFrozen(cid("svc-a")))
    );
    s.set_credential_status(&cid("node-a"), CredentialStatus::Revoked, now())
        .unwrap();
    assert_eq!(
        payer(&env, "node-a", None),
        Err(ResolveError::CredentialRevoked(cid("node-a")))
    );
    check_all(&env, now());
}

#[test]
fn an_expired_credential_stops_resolving_exactly_at_its_expiry() {
    let env = seeded();
    let expiring = Credential::new(
        cid("exp"),
        CredentialBinding::Node {
            account: aid("org-a"),
        },
        CredentialStatus::Active,
        Some(TimestampMs(T0 + 1_000)),
        Vec::new(),
    )
    .unwrap();
    env.store.create_credential(&expiring, now()).unwrap();
    let at = |ms| {
        env.store
            .payment_context(&cid("exp"), None, TimestampMs(ms))
    };
    assert!(at(T0 + 999).is_ok());
    match at(T0 + 1_000) {
        Err(PaymentContextError::Resolve(ResolveError::CredentialExpired { .. })) => {}
        other => panic!("{other:?}"),
    }
    assert_slice_equals_full(&env, "exp", None, TimestampMs(T0 + 999));
    assert_slice_equals_full(&env, "exp", None, TimestampMs(T0 + 1_000));
}

// ================================================================ 读出来的坏数据进不来

#[test]
fn rows_corrupted_behind_the_stores_back_are_errors_on_read_not_silently_accepted() {
    let env = seeded();
    let conn = raw(&env);
    // 绕过触发器的最直接办法：关外键、删掉触发器，然后把枚举改成认不得的值。
    conn.execute_batch("DROP TRIGGER accounts_identity_immutable;")
        .unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    conn.execute_batch("UPDATE accounts SET kind = 'wizard' WHERE account_id = 'stu-2';")
        .unwrap();
    match env.store.get_account(&aid("stu-2")) {
        Err(AccountStoreError::Corrupt(message)) => {
            assert!(message.contains("wizard"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert!(env.store.load_tree().is_err());
    // 其它账户不受影响
    assert!(env.store.get_account(&aid("stu-1")).unwrap().is_some());
}

// ================================================================ 数据库层：账户

fn insert_account_sql(id: &str, kind: &str, parent: Option<&str>, unit: &str) -> String {
    let parent = parent.map_or("NULL".to_string(), |p| format!("'{p}'"));
    format!(
        "INSERT INTO accounts (account_id, kind, parent_id, unit, status, created_at_ms) \
         VALUES ('{id}', '{kind}', {parent}, '{unit}', 'active', 0);"
    )
}

#[test]
fn db_a_second_platform_root_hits_the_unique_index() {
    let env = seeded();
    let conn = raw(&env);
    assert_rejected(
        &conn,
        &insert_account_sql("plat-2", "platform", None, "THB"),
        "UNIQUE constraint failed",
    );
    // 对照：同样的语句在没有根的库里能写进去（见下一个测试的空库），这里证明是索引而不是别的东西拦的
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM accounts WHERE kind = 'platform'"
        ),
        1
    );
}

#[test]
fn db_root_shape_checks_reject_a_parentless_non_platform_and_a_platform_with_a_parent() {
    let env = env();
    let conn = raw(&env);
    assert_rejected(
        &conn,
        &insert_account_sql("orphan", "org", None, "THB"),
        "CHECK constraint failed",
    );
    assert_accepted(&conn, &insert_account_sql("plat", "platform", None, "THB"));
    assert_rejected(
        &conn,
        &insert_account_sql("plat-child", "platform", Some("plat"), "THB"),
        "CHECK constraint failed|kind is not allowed under its parent kind",
    );
    // 自己当自己的父账户
    assert_rejected(
        &conn,
        &insert_account_sql("loop", "org", Some("loop"), "THB"),
        "CHECK constraint failed",
    );
}

#[test]
fn db_a_missing_parent_hits_the_foreign_key() {
    let env = seeded();
    let conn = raw(&env);
    assert_rejected(
        &conn,
        &insert_account_sql("kid", "org", Some("ghost"), "THB"),
        "FOREIGN KEY constraint failed",
    );
    assert_accepted(
        &conn,
        &insert_account_sql("kid", "org", Some("plat"), "THB"),
    ); // 对照
}

#[test]
fn db_parent_and_child_units_must_match() {
    let env = seeded();
    let conn = raw(&env);
    assert_rejected(
        &conn,
        &insert_account_sql("u1", "member", Some("org-a"), "USD"),
        "unit must equal its parent unit",
    );
    assert_rejected(
        &conn,
        &insert_account_sql("u2", "member", Some("org-a"), "QUOTA:node-1"),
        "unit must equal its parent unit",
    );
    assert_accepted(
        &conn,
        &insert_account_sql("u3", "member", Some("org-a"), "THB"),
    );
}

#[test]
fn db_the_parent_child_kind_matrix_matches_the_contract_cell_by_cell() {
    let env = env();
    let conn = raw(&env);
    assert_accepted(&conn, &insert_account_sql("plat", "platform", None, "THB"));
    let kinds = [
        AccountKind::Platform,
        AccountKind::SubsidyPool,
        AccountKind::Org,
        AccountKind::Sponsor,
        AccountKind::Member,
    ];
    // 每种非平台种类各建一个，挂在平台下面（平台可以做它们任何一种的父账户）。
    for kind in kinds.into_iter().filter(|k| *k != AccountKind::Platform) {
        let text = to_text(&kind).unwrap();
        assert_accepted(
            &conn,
            &insert_account_sql(&format!("k-{text}"), &text, Some("plat"), "THB"),
        );
    }
    let parent_id = |kind: AccountKind| {
        if kind == AccountKind::Platform {
            "plat".to_string()
        } else {
            format!("k-{}", to_text(&kind).unwrap())
        }
    };
    let mut cells = 0;
    for parent in kinds {
        for child in kinds {
            cells += 1;
            let child_text = to_text(&child).unwrap();
            let sql = insert_account_sql(
                &format!("c-{cells}"),
                &child_text,
                Some(&parent_id(parent)),
                "THB",
            );
            let accepted = conn.execute_batch(&sql).is_ok();
            assert_eq!(
                accepted,
                parent.may_parent(child),
                "{parent:?} -> {child:?}: the database and AccountKind::may_parent disagree (update migration 0001)"
            );
        }
    }
    assert_eq!(cells, 25);
}

#[test]
fn db_the_depth_limit_matches_the_contract_maximum() {
    let env = env();
    let conn = raw(&env);
    assert_eq!(
        MAX_ACCOUNT_DEPTH, 5,
        "the depth literal in migration 0001's trigger must follow this"
    );
    assert_accepted(&conn, &insert_account_sql("d0", "platform", None, "THB"));
    for depth in 1..=MAX_ACCOUNT_DEPTH {
        assert_accepted(
            &conn,
            &insert_account_sql(
                &format!("d{depth}"),
                "org",
                Some(&format!("d{}", depth - 1)),
                "THB",
            ),
        );
    }
    assert_rejected(
        &conn,
        &insert_account_sql(
            "too-deep",
            "org",
            Some(&format!("d{MAX_ACCOUNT_DEPTH}")),
            "THB",
        ),
        "deeper than the maximum depth",
    );
}

#[test]
fn db_account_identity_is_immutable_so_the_tree_cannot_grow_a_cycle() {
    let env = seeded();
    let conn = raw(&env);
    // 想把 org-a 的父账户改成它自己的后代 stu-1 来造环
    for sql in [
        "UPDATE accounts SET parent_id = 'stu-1' WHERE account_id = 'org-a';",
        "UPDATE accounts SET kind = 'org' WHERE account_id = 'stu-2';",
        "UPDATE accounts SET unit = 'USD' WHERE account_id = 'stu-2';",
        "UPDATE accounts SET account_id = 'renamed' WHERE account_id = 'stu-2';",
    ] {
        assert_rejected(&conn, sql, "immutable");
    }
    // 状态可以改；关闭是终态
    assert_accepted(
        &conn,
        "UPDATE accounts SET status = 'frozen' WHERE account_id = 'stu-2';",
    );
    assert_accepted(
        &conn,
        "UPDATE accounts SET status = 'closed' WHERE account_id = 'stu-2';",
    );
    assert_rejected(
        &conn,
        "UPDATE accounts SET status = 'active' WHERE account_id = 'stu-2';",
        "cannot be reopened",
    );
}

#[test]
fn db_accounts_cannot_be_deleted_while_something_refers_to_them() {
    let env = seeded();
    let conn = raw(&env);
    assert_rejected(
        &conn,
        "DELETE FROM accounts WHERE account_id = 'org-a';",
        "FOREIGN KEY constraint failed",
    );
    // 对照：没有任何东西引用的叶子可以删（库不禁止删除，只保护被引用的行）
    assert_accepted(
        &conn,
        "DELETE FROM accounts WHERE account_id = 'stu-b' AND NOT EXISTS (SELECT 1 FROM memberships WHERE account_id = 'stu-b');",
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM accounts WHERE account_id = 'stu-b'"
        ),
        1,
        "stu-b is bound by a membership, so nothing was deleted"
    );
}

// ================================================================ 数据库层：主体与成员关系

#[test]
fn db_principals_reject_email_shaped_ids_bad_status_and_resurrection() {
    let env = seeded();
    let conn = raw(&env);
    let insert = |id: &str| {
        format!(
            "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) VALUES ('{id}', 'active', 0, NULL);"
        )
    };
    assert_rejected(
        &conn,
        &insert("someone@example.com"),
        "CHECK constraint failed",
    );
    assert_rejected(&conn, &insert("has space"), "CHECK constraint failed");
    assert_rejected(&conn, &insert(""), "CHECK constraint failed");
    assert_accepted(&conn, &insert("eduai:3fa9c0"));
    assert_rejected(
        &conn,
        "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) VALUES ('p-x', 'banned', 0, NULL);",
        "CHECK constraint failed",
    );
    // revoked 必须有撤销时间，active 不能有
    assert_rejected(
        &conn,
        "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) VALUES ('p-y', 'revoked', 0, NULL);",
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) VALUES ('p-z', 'active', 0, 5);",
        "CHECK constraint failed",
    );
    // 撤销不可逆
    assert_accepted(
        &conn,
        "UPDATE principals SET status='revoked', revoked_at_ms=9 WHERE principal_id='p-bob';",
    );
    assert_rejected(
        &conn,
        "UPDATE principals SET status='active', revoked_at_ms=NULL WHERE principal_id='p-bob';",
        "cannot be changed",
    );
    assert_rejected(
        &conn,
        "UPDATE principals SET principal_id='p-renamed' WHERE principal_id='p-alice';",
        "immutable",
    );
}

#[test]
fn db_memberships_hit_foreign_keys_checks_the_unique_binding_and_the_shape_triggers() {
    let env = seeded();
    let conn = raw(&env);
    let insert = |id: &str, principal: &str, org: &str, role: &str, acct: Option<&str>| {
        let acct = acct.map_or("NULL".to_string(), |a| format!("'{a}'"));
        format!(
            "INSERT INTO memberships (membership_id, principal_id, org_id, role, account_id, status, created_at_ms, revoked_at_ms) \
             VALUES ('{id}', '{principal}', '{org}', '{role}', {acct}, 'active', 0, NULL);"
        )
    };
    // 外键
    assert_rejected(
        &conn,
        &insert("m1", "ghost", "org-a", "admin", None),
        "FOREIGN KEY constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "ghost", "admin", None),
        "FOREIGN KEY constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "org-a", "member", Some("ghost")),
        "FOREIGN KEY constraint failed",
    );
    // 角色 / 账户的空与非空
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "org-a", "admin", Some("stu-2")),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "org-a", "member", None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "org-a", "janitor", None),
        "CHECK constraint failed",
    );
    // 组织必须是 org / platform；角色与账户种类对上；账户在组织子树里
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "stu-2", "admin", None),
        "must be an org or platform",
    );
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "org-a", "sponsor", Some("stu-2")),
        "does not match its account kind",
    );
    assert_rejected(
        &conn,
        &insert("m1", "p-bob", "org-a", "member", Some("stu-b")),
        "not under its org",
    );
    // 一个账户至多一个成员关系
    assert_rejected(
        &conn,
        &insert("m1", "p-alice", "org-a", "member", Some("stu-1")),
        "UNIQUE constraint failed",
    );
    // 对照：同样的位置、合法的内容能写进去（账户在组织的更深一层也算子树）
    assert_accepted(&conn, &insert("m-ok", "p-alice", "org-a", "admin", None));
    assert_accepted(
        &conn,
        &insert_account_sql("sp-a3", "sponsor", Some("org-a"), "THB"),
    );
    assert_accepted(
        &conn,
        &insert("m-ok2", "p-admin", "org-a", "sponsor", Some("sp-a3")),
    );
    // 身份列不可改；撤销不可逆
    assert_rejected(
        &conn,
        "UPDATE memberships SET role='admin' WHERE membership_id='m-bob-a';",
        "immutable",
    );
    assert_accepted(
        &conn,
        "UPDATE memberships SET status='revoked', revoked_at_ms=1 WHERE membership_id='m-bob-a';",
    );
    assert_rejected(
        &conn,
        "UPDATE memberships SET status='active', revoked_at_ms=NULL WHERE membership_id='m-bob-a';",
        "cannot be changed",
    );
}

#[test]
fn db_a_principal_with_memberships_cannot_be_deleted() {
    let env = seeded();
    let conn = raw(&env);
    assert_rejected(
        &conn,
        "DELETE FROM principals WHERE principal_id = 'p-bob';",
        "FOREIGN KEY constraint failed",
    );
}

// ================================================================ 数据库层：凭证

#[test]
fn db_credentials_enforce_one_binding_target_kinds_and_final_revocation() {
    let env = seeded();
    let conn = raw(&env);
    let insert = |id: &str, kind: &str, account: Option<&str>, membership: Option<&str>| {
        let a = account.map_or("NULL".to_string(), |v| format!("'{v}'"));
        let m = membership.map_or("NULL".to_string(), |v| format!("'{v}'"));
        format!(
            "INSERT INTO credentials (credential_id, binding_kind, account_id, membership_id, status, expires_at_ms, created_at_ms, revoked_at_ms) \
             VALUES ('{id}', '{kind}', {a}, {m}, 'active', NULL, 0, NULL);"
        )
    };
    // 每张凭证只有一个绑定
    assert_rejected(
        &conn,
        &insert("c1", "service", Some("org-a"), Some("m-bob-a")),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("c1", "service", None, None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("c1", "member", Some("org-a"), None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("c1", "ghost-kind", Some("org-a"), None),
        "CHECK constraint failed",
    );
    // 外键
    assert_rejected(
        &conn,
        &insert("c1", "service", Some("ghost"), None),
        "FOREIGN KEY constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("c1", "member", None, Some("ghost")),
        "FOREIGN KEY constraint failed",
    );
    // 绑定目标的种类
    assert_rejected(
        &conn,
        &insert("c1", "service", Some("stu-1"), None),
        "must be bound to an org account",
    );
    assert_rejected(
        &conn,
        &insert("c1", "node", Some("plat"), None),
        "must be bound to an org account",
    );
    assert_rejected(
        &conn,
        &insert("c1", "member", None, Some("m-admin-a")),
        "member or sponsor membership",
    );
    // 对照
    assert_accepted(&conn, &insert("c-ok-1", "service", Some("org-a"), None));
    assert_accepted(&conn, &insert("c-ok-2", "member", None, Some("m-bob-a")));
    // 付款绑定不能改（改了就是把费用转嫁给别人）
    assert_rejected(
        &conn,
        "UPDATE credentials SET account_id='org-b' WHERE credential_id='c-ok-1';",
        "immutable",
    );
    assert_rejected(
        &conn,
        "UPDATE credentials SET binding_kind='node' WHERE credential_id='c-ok-1';",
        "immutable",
    );
    // 冻结可以解冻；撤销不可逆
    assert_accepted(
        &conn,
        "UPDATE credentials SET status='frozen' WHERE credential_id='c-ok-1';",
    );
    assert_accepted(
        &conn,
        "UPDATE credentials SET status='active' WHERE credential_id='c-ok-1';",
    );
    assert_accepted(
        &conn,
        "UPDATE credentials SET status='revoked', revoked_at_ms=3 WHERE credential_id='c-ok-1';",
    );
    assert_rejected(
        &conn,
        "UPDATE credentials SET status='active', revoked_at_ms=NULL WHERE credential_id='c-ok-1';",
        "cannot be changed",
    );
    assert_rejected(
        &conn,
        "UPDATE credentials SET expires_at_ms=999 WHERE credential_id='c-ok-1';",
        "cannot be changed",
    );
    // revoked 必须带撤销时间
    assert_rejected(
        &conn,
        "UPDATE credentials SET status='revoked' WHERE credential_id='c-ok-2';",
        "CHECK constraint failed",
    );
}

#[test]
fn db_credential_admin_scopes_are_shaped_append_only_and_tied_to_real_accounts() {
    let env = payment_env();
    let conn = raw(&env);
    let insert = |pos: i64, op: &str, subtree: &str, unit: Option<&str>, micros: Option<i64>| {
        let u = unit.map_or("NULL".to_string(), |v| format!("'{v}'"));
        let m = micros.map_or("NULL".to_string(), |v| v.to_string());
        format!(
            "INSERT INTO credential_admin_scopes (credential_id, position, op, subtree_id, max_amount_unit, max_amount_micros) \
             VALUES ('svc-a', {pos}, '{op}', '{subtree}', {u}, {m});"
        )
    };
    assert_accepted(
        &conn,
        &insert(0, "grant_allocation", "org-a", Some("THB"), Some(5_000_000)),
    );
    assert_accepted(&conn, &insert(1, "create_account", "sp-a", None, None));
    // 同一张凭证、同一个位置只能有一条
    assert_rejected(
        &conn,
        &insert(1, "create_account", "stu-2", None, None),
        "UNIQUE constraint failed",
    );
    // 外键：子树必须是真实账户
    assert_rejected(
        &conn,
        &insert(2, "create_account", "ghost", None, None),
        "FOREIGN KEY constraint failed",
    );
    // 额度上限只给 grant_allocation，且不能为负，单位和数值要成对
    assert_rejected(
        &conn,
        &insert(2, "create_account", "org-a", Some("THB"), Some(1)),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert(2, "grant_allocation", "org-a", Some("THB"), Some(-1)),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert(2, "grant_allocation", "org-a", Some("THB"), None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert(2, "grant_allocation", "org-a", Some("EUR"), Some(1)),
        "CHECK constraint failed",
    );
    // 操作名只限制形状，不写死集合（S2 可以新增操作）
    assert_accepted(&conn, &insert(2, "some_future_op", "org-a", None, None));
    assert_rejected(
        &conn,
        &insert(3, "Not-Valid", "org-a", None, None),
        "CHECK constraint failed",
    );
    // 追加式：不能改、不能删
    assert_rejected(
        &conn,
        "UPDATE credential_admin_scopes SET subtree_id='org-b' WHERE position=0;",
        "append-only",
    );
    assert_rejected(
        &conn,
        "DELETE FROM credential_admin_scopes WHERE position=0;",
        "append-only",
    );
    // 读回来：认不得的操作名是错误，不是静默跳过
    match env.store.get_credential(&cid("svc-a")) {
        Err(AccountStoreError::Corrupt(message)) => {
            assert!(message.contains("some_future_op"), "{message}")
        }
        other => panic!("{other:?}"),
    }
}

// ================================================================ 数据库层：额度

#[test]
fn db_allocation_rows_reject_bad_amounts_windows_units_and_transfers() {
    let env = seeded();
    let conn = raw(&env);
    let insert = |id: &str,
                  parent: &str,
                  child: &str,
                  unit: &str,
                  micros: i64,
                  purpose: &str,
                  from: i64,
                  until: i64,
                  transferable: i64,
                  key: Option<&str>| {
        let k = key.map_or("NULL".to_string(), |v| format!("'{v}'"));
        format!(
            "INSERT INTO allocations (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose, valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms) \
             VALUES ('{id}', '{parent}', '{child}', '{unit}', {micros}, '{purpose}', {from}, {until}, {transferable}, {k}, 0);"
        )
    };
    let base = |id: &str| insert(id, "plat", "org-a", "THB", 100, "prepaid", 10, 20, 0, None);
    assert_accepted(&conn, &base("ok-1")); // 对照
    // 负数、空窗口、反向窗口
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "THB", -1, "prepaid", 10, 20, 0, None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "THB", 1, "prepaid", 10, 10, 0, None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "THB", 1, "prepaid", 20, 10, 0, None),
        "CHECK constraint failed",
    );
    // 不可转让：`transferable` 只能是 0
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "THB", 1, "prepaid", 10, 20, 1, None),
        "CHECK constraint failed",
    );
    // 单位写法、用途与单位相容
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "EUR", 1, "prepaid", 10, 20, 0, None),
        "CHECK constraint failed|must equal the child account unit",
    );
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "THB", 1, "org_quota", 10, 20, 0, None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert(
            "x", "plat", "org-a", "QUOTA:n1", 1, "prepaid", 10, 20, 0, None,
        ),
        "CHECK constraint failed|must equal the child account unit",
    );
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "THB", 1, "gift", 10, 20, 0, None),
        "CHECK constraint failed",
    );
    // 自己给自己
    assert_rejected(
        &conn,
        &insert("x", "org-a", "org-a", "THB", 1, "prepaid", 10, 20, 0, None),
        "CHECK constraint failed|direct child of its parent",
    );
    // 外键
    assert_rejected(
        &conn,
        &insert("x", "ghost", "org-a", "THB", 1, "prepaid", 10, 20, 0, None),
        "FOREIGN KEY constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("x", "plat", "ghost", "THB", 1, "prepaid", 10, 20, 0, None),
        "FOREIGN KEY constraint failed",
    );
    // 只沿树边、单位等于子账户的单位
    assert_rejected(
        &conn,
        &insert("x", "plat", "stu-1", "THB", 1, "prepaid", 10, 20, 0, None),
        "direct child of its parent",
    );
    assert_rejected(
        &conn,
        &insert("x", "plat", "org-a", "USD", 1, "prepaid", 10, 20, 0, None),
        "must equal the child account unit",
    );
    // id 重复
    assert_rejected(&conn, &base("ok-1"), "UNIQUE constraint failed");
    // 追加式
    assert_rejected(
        &conn,
        "UPDATE allocations SET ceiling_micros = 1 WHERE allocation_id='ok-1';",
        "append-only",
    );
    assert_rejected(
        &conn,
        "DELETE FROM allocations WHERE allocation_id='ok-1';",
        "append-only",
    );
}

#[test]
fn db_the_subsidy_idempotency_key_is_unique_and_free_tier_requires_it() {
    let env = seeded();
    with_pool_members(&env);
    let conn = raw(&env);
    let insert = |id: &str, child: &str, purpose: &str, key: Option<&str>| {
        let k = key.map_or("NULL".to_string(), |v| format!("'{v}'"));
        format!(
            "INSERT INTO allocations (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose, valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms) \
             VALUES ('{id}', 'pool', '{child}', 'THB', 5, '{purpose}', 10, 20, 0, {k}, 0);"
        )
    };
    let key = subsidy_key_of("p-sub");
    assert_accepted(&conn, &insert("s1", "pool-stu-1", "free_tier", Some(&key)));
    // 同一个 (成员, 月份) 的键重复：唯一索引拦下，换账户、换 id 都一样
    assert_rejected(
        &conn,
        &insert("s2", "pool-stu-b", "free_tier", Some(&key)),
        "UNIQUE constraint failed: allocations.idempotency_key",
    );
    assert_rejected(
        &conn,
        &insert("s3", "pool-stu-1", "free_tier", Some(&key)),
        "UNIQUE constraint failed: allocations.idempotency_key",
    );
    // 免费额度必须带 `subsidy:` 键（CHECK 对 NULL 放行，所以这条是显式写了 IS NOT NULL 的）
    assert_rejected(
        &conn,
        &insert("s4", "pool-stu-b", "free_tier", None),
        "CHECK constraint failed",
    );
    assert_rejected(
        &conn,
        &insert("s5", "pool-stu-b", "free_tier", Some("monthly-grant-1")),
        "CHECK constraint failed",
    );
    // `subsidy:` 键只给免费额度
    assert_rejected(
        &conn,
        &insert("s6", "pool-stu-b", "prepaid", Some("subsidy:p-sub:2026-12")),
        "CHECK constraint failed",
    );
    // 没有键的非免费额度可以有任意多条（唯一索引只管有键的）
    assert_accepted(&conn, &insert("s7", "pool-stu-b", "prepaid", None));
    assert_accepted(&conn, &insert("s8", "pool-stu-b", "prepaid", None));
    // 不同的月份是不同的键
    let december =
        subsidy_idempotency_key(&pid("p-sub"), BillingMonth::new(2026, 12).unwrap()).unwrap();
    assert_accepted(
        &conn,
        &insert("s9", "pool-stu-1", "free_tier", Some(december.as_str())),
    );
}

// ================================================================ 与 S0 对照

/// 根账户用 QUOTA 单位的库（组织形态的配额树），用来测 `org_quota` 用途。
fn env_with_quota_root() -> Env {
    let env = env();
    let conn = raw(&env);
    for (id, kind, parent) in [
        ("q-plat", "platform", None),
        ("q-org", "org", Some("q-plat")),
    ] {
        assert_accepted(&conn, &insert_account_sql(id, kind, parent, "QUOTA:n1"));
    }
    drop(conn);
    env
}

/// 编译期的穷尽性检查：S0 给枚举新增变体时这里编译不过，提醒同时更新迁移里的 CHECK 集合和下面的列表。
fn account_kinds() -> [AccountKind; 5] {
    fn exhaustive(kind: AccountKind) {
        match kind {
            AccountKind::Platform
            | AccountKind::SubsidyPool
            | AccountKind::Org
            | AccountKind::Sponsor
            | AccountKind::Member => {}
        }
    }
    exhaustive(AccountKind::Org);
    [
        AccountKind::Platform,
        AccountKind::SubsidyPool,
        AccountKind::Org,
        AccountKind::Sponsor,
        AccountKind::Member,
    ]
}

fn purposes() -> [Purpose; 4] {
    fn exhaustive(purpose: Purpose) {
        match purpose {
            Purpose::FreeTier | Purpose::Prepaid | Purpose::CreditLine | Purpose::OrgQuota => {}
        }
    }
    exhaustive(Purpose::Prepaid);
    [
        Purpose::FreeTier,
        Purpose::Prepaid,
        Purpose::CreditLine,
        Purpose::OrgQuota,
    ]
}

#[test]
fn the_check_sets_in_the_migration_accept_exactly_the_contracts_wire_strings() {
    // 账户种类：S0 的每个线上字符串都能写进库（再读出来相等），随便一个别的字符串写不进去。
    let env = env();
    let conn = raw(&env);
    assert_accepted(&conn, &insert_account_sql("plat", "platform", None, "THB"));
    for (index, kind) in account_kinds()
        .into_iter()
        .enumerate()
        .filter(|(_, k)| *k != AccountKind::Platform)
    {
        let text = to_text(&kind).unwrap();
        assert_accepted(
            &conn,
            &insert_account_sql(&format!("k{index}"), &text, Some("plat"), "THB"),
        );
        assert_eq!(from_text::<AccountKind>("kind", &text).unwrap(), kind);
    }
    assert_rejected(
        &conn,
        &insert_account_sql("kx", "wizard", Some("plat"), "THB"),
        "CHECK constraint failed|kind is not allowed under its parent kind",
    );
    // 状态
    for (index, status) in [
        AccountStatus::Active,
        AccountStatus::Frozen,
        AccountStatus::Closed,
    ]
    .into_iter()
    .enumerate()
    {
        let text = to_text(&status).unwrap();
        assert_accepted(
            &conn,
            &format!(
                "INSERT INTO accounts (account_id, kind, parent_id, unit, status, created_at_ms) VALUES ('s{index}', 'org', 'plat', 'THB', '{text}', 0);"
            ),
        );
    }
    // 用途：每个线上字符串都能写进库。`org_quota` 要求 QUOTA 单位，所以另起一棵 QUOTA 的树来放它。
    let quota_env = env_with_quota_root();
    let quota = raw(&quota_env);
    for (index, purpose) in purposes().into_iter().enumerate() {
        let text = to_text(&purpose).unwrap();
        assert_eq!(text, purpose.as_str(), "serde name and as_str must agree");
        let key = if purpose == Purpose::FreeTier {
            "'subsidy:p:2026-11'"
        } else {
            "NULL"
        };
        let (target, unit) = if purpose == Purpose::OrgQuota {
            (&quota, "QUOTA:n1")
        } else {
            (&conn, "THB")
        };
        let child = if purpose == Purpose::OrgQuota {
            "q-org"
        } else {
            "k2"
        };
        assert_accepted(
            target,
            &format!(
                "INSERT INTO allocations (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose, valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms) \
             VALUES ('al{index}', '{}', '{child}', '{unit}', 1, '{text}', 1, 2, 0, {key}, 0);",
                if purpose == Purpose::OrgQuota {
                    "q-plat"
                } else {
                    "plat"
                }
            ),
        );
    }
    // 成员关系角色、凭证状态
    for role in [
        MembershipRole::Member,
        MembershipRole::Sponsor,
        MembershipRole::Admin,
    ] {
        let _ = to_text(&role).unwrap();
    }
    assert_eq!(to_text(&MembershipRole::Member).unwrap(), "member");
    assert_eq!(to_text(&MembershipRole::Sponsor).unwrap(), "sponsor");
    assert_eq!(to_text(&MembershipRole::Admin).unwrap(), "admin");
    assert_eq!(to_text(&CredentialStatus::Active).unwrap(), "active");
    assert_eq!(to_text(&CredentialStatus::Frozen).unwrap(), "frozen");
    assert_eq!(to_text(&CredentialStatus::Revoked).unwrap(), "revoked");
    assert_eq!(to_text(&PrincipalStatus::Revoked).unwrap(), "revoked");
    assert_eq!(to_text(&MembershipStatus::Revoked).unwrap(), "revoked");
}

#[test]
fn the_id_check_in_the_migration_matches_the_contracts_id_validation_for_every_ascii_character() {
    let env = env();
    let conn = raw(&env);
    for code in 1_u8..=127 {
        let ch = char::from(code);
        let candidate = format!("a{ch}b");
        let rust_ok = PrincipalId::new(candidate.clone()).is_ok();
        let tx = conn.unchecked_transaction().unwrap();
        let db_ok = tx
            .execute(
                "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) VALUES (?1, 'active', 0, NULL)",
                [&candidate],
            )
            .is_ok();
        tx.rollback().unwrap();
        assert_eq!(
            db_ok, rust_ok,
            "character {ch:?} (0x{code:02x}): the database and PrincipalId::new disagree"
        );
    }
    // 长度边界：128 字节可以，129 不行；非 ASCII 一律不行
    for (candidate, expected) in [
        ("x".repeat(128), true),
        ("x".repeat(129), false),
        ("学生".to_string(), false),
        (String::new(), false),
    ] {
        let rust_ok = PrincipalId::new(candidate.clone()).is_ok();
        assert_eq!(rust_ok, expected, "{candidate:?}");
        let tx = conn.unchecked_transaction().unwrap();
        let db_ok = tx
            .execute(
                "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) VALUES (?1, 'active', 0, NULL)",
                [&candidate],
            )
            .is_ok();
        tx.rollback().unwrap();
        assert_eq!(db_ok, expected, "{candidate:?}");
    }
}

#[test]
fn the_unit_check_in_the_migration_matches_the_contracts_unit_parser() {
    let env = env();
    let conn = raw(&env);
    let long_node = format!("QUOTA:{}", "n".repeat(64));
    let too_long_node = format!("QUOTA:{}", "n".repeat(65));
    let samples = [
        "THB",
        "USD",
        "thb",
        "EUR",
        "",
        "QUOTA:",
        "QUOTA:node-1",
        "QUOTA:a.b_c-9",
        "QUOTA:a:b",
        "QUOTA:a~b",
        "QUOTA:a b",
        "quota:node-1",
        "QUOTA:学",
        long_node.as_str(),
        too_long_node.as_str(),
    ];
    for sample in samples {
        let rust_ok = sample.parse::<Unit>().is_ok();
        let tx = conn.unchecked_transaction().unwrap();
        let db_ok = tx
            .execute(
                "INSERT INTO accounts (account_id, kind, parent_id, unit, status, created_at_ms) VALUES ('u', 'platform', NULL, ?1, 'active', 0)",
                [sample],
            )
            .is_ok();
        tx.rollback().unwrap();
        assert_eq!(
            db_ok, rust_ok,
            "unit {sample:?}: the database and Unit::from_str disagree"
        );
    }
}

// ================================================================ 重新校验读路径的整棵树

#[test]
fn the_full_directory_reads_back_through_the_contract_validation() {
    let env = payment_env();
    let directory = env.store.load_directory().unwrap().unwrap();
    assert_eq!(directory.tree().len(), 8);
    assert_eq!(directory.memberships_of(&pid("p-multi")).count(), 2);
    assert!(directory.principal(&pid("p-admin")).is_some());
    // 整棵树和单独读出来的账户一致
    let tree: AccountTree = env.store.load_tree().unwrap().unwrap();
    assert_eq!(tree.root().id, aid("plat"));
}

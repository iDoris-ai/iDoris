//! 账户存储（S1）和尝试记录存储（S4）住在同一个数据库文件里：各自的迁移用各自的版本表，
//! 表、索引、触发器的名字不重叠，谁先打开都行，重复打开互不影响。
//!
//! 这是两个切片分别开发、最后合到一起的接缝：任何一边以后改名、加对象撞了对方的名字，这里立刻红。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::attempts::AttemptStore;
use rusqlite::Connection;

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "idoris-coexist-{tag}-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        )))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

fn open_accounts(db: &Path) {
    drop(AccountStore::open(db).unwrap());
}

fn open_attempts(db: &Path) {
    drop(AttemptStore::open(db).unwrap());
}

/// 库里的每个模式对象（表、索引、触发器），不含 SQLite 自己的内部对象。
fn objects(db: &Path) -> BTreeSet<(String, String)> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn versions(db: &Path, table: &str) -> Vec<i64> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(&format!("SELECT version FROM {table} ORDER BY version"))
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn the_two_stores_share_one_database_without_touching_each_others_schema() {
    // 各自单独建在一个新库里：两边的对象名字不能重叠。
    let accounts_only = TempDb::new("accounts-only");
    open_accounts(accounts_only.path());
    let attempts_only = TempDb::new("attempts-only");
    open_attempts(attempts_only.path());
    let account_objects = objects(accounts_only.path());
    let attempt_objects = objects(attempts_only.path());
    assert!(!account_objects.is_empty() && !attempt_objects.is_empty());
    let clashes: Vec<_> = account_objects.intersection(&attempt_objects).collect();
    assert!(
        clashes.is_empty(),
        "both stores created the same schema object: {clashes:?}"
    );
    let union: BTreeSet<_> = account_objects.union(&attempt_objects).cloned().collect();

    // 同一个库里，谁先谁后都一样：得到的就是两边对象的并集，各自的版本表各有一行。
    for accounts_first in [true, false] {
        let db = TempDb::new(if accounts_first {
            "accounts-first"
        } else {
            "attempts-first"
        });
        if accounts_first {
            open_accounts(db.path());
            open_attempts(db.path());
        } else {
            open_attempts(db.path());
            open_accounts(db.path());
        }
        assert_eq!(objects(db.path()), union, "accounts_first={accounts_first}");
        assert_eq!(versions(db.path(), "account_schema_migrations"), [1, 2]);
        assert_eq!(
            versions(db.path(), "compute_attempt_schema_migrations"),
            [1, 2]
        );

        // 再打开几次：什么都不变，也不重复应用迁移。
        for _ in 0..2 {
            open_accounts(db.path());
            open_attempts(db.path());
        }
        assert_eq!(objects(db.path()), union, "accounts_first={accounts_first}");
        assert_eq!(versions(db.path(), "account_schema_migrations"), [1, 2]);
        assert_eq!(
            versions(db.path(), "compute_attempt_schema_migrations"),
            [1, 2]
        );
    }
}

#[test]
fn a_database_with_only_one_of_the_stores_never_gets_the_others_tables() {
    // 个人和存量 tenant 部署两个都不打开；只开了尝试记录的库不会冒出账户表，反过来也一样。
    let attempts_only = TempDb::new("no-accounts");
    open_attempts(attempts_only.path());
    assert!(
        !objects(attempts_only.path())
            .iter()
            .any(|(_, name)| name == "accounts" || name == "account_schema_migrations")
    );
    let accounts_only = TempDb::new("no-attempts");
    open_accounts(accounts_only.path());
    assert!(
        !objects(accounts_only.path())
            .iter()
            .any(|(_, name)| name.starts_with("compute_"))
    );
}

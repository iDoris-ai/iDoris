//! 计算服务会用到的 SQLite 连接：统一的 PRAGMA 设置（S1，设计稿 §10、`docs/design/compute-service-ops.md` §3）。
//!
//! # 背景
//!
//! 四个存储共用同一个库文件，各开各的连接：租户记录（`TenantStore`）、预算账本（`BudgetLedger`）、
//! 虚拟 key（`VirtualKeyStore`）、事件日志（`EventLogStore`）。S1 又新增了账户存储（`AccountStore`）。
//! 过去只有账本连接显式设了 `journal_mode=WAL` 和 `busy_timeout`，其余三个靠 rusqlite 的默认值
//! （`Connection::open` 内部会设 5 秒的 busy_timeout），而且谁也没有检查 `journal_mode` 的返回值。
//!
//! # 两种档位，由部署形态开关选择
//!
//! - [`ConnectionProfile::Legacy`]：**存量行为，一个字符不动**。个人形态和没设 `IDORIS_FORM` 的存量 tenant
//!   部署用它：账本照旧自己设 WAL 和 busy_timeout，其余三个存储照旧什么都不设。
//!   统一设置会让个人形态的行为发生变化（见下），所以不对它们生效。
//! - [`ConnectionProfile::Standard`]：组织、云形态用。三件事都显式做，并且**检查结果**：
//!   1. `journal_mode=WAL`，SQLite 回答的不是 `wal` 就启动失败（不是只调用然后丢掉返回值）；
//!   2. `busy_timeout` 显式写成 [`STANDARD_BUSY_TIMEOUT`]，不依赖依赖库的默认；
//!   3. `foreign_keys=ON`，回读确认。`foreign_keys` 是每条连接各自的开关、不持久，账户存储的完整性
//!      （见 `accounts/migrations/0001_accounts.sql`）靠它。
//!
//! # 为什么不对个人形态统一设置
//!
//! - `journal_mode=WAL` 是库文件的持久属性。离线的 `idoris key issue|list|revoke` 只打开虚拟 key 存储；
//!   如果它在一个全新文件上设 WAL，文件头就比今天更早变成 WAL，并且在不支持共享内存的文件系统上，
//!   这些离线命令会从「能用」变成「报错」。守护进程路径上账本本来就会设 WAL，所以那条路径没有差别，
//!   差别只在离线命令和不常见的文件系统上。
//! - `busy_timeout` 显式写 5 秒与 rusqlite 的默认值相同，没有行为差别；`foreign_keys=ON` 在既有四个
//!   存储的 schema 里没有外键，同样没有行为差别（而且 libsqlite3-sys 的 bundled 构建带
//!   `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`，既有连接本来就是开的）。但这两条单独设置没有收益，所以也不动。
//!   Standard 的价值是把「依赖库的编译默认」变成「显式设置并回读确认」，让账户存储的外键不依赖一个编译选项。
//!
//! 不设 `synchronous`：运维文档建议钉成 `FULL`，那就是 SQLite 的默认值，设不设没有区别，也不在 S1 的范围里。

use std::time::Duration;

use rusqlite::Connection;

/// 显式写出的 busy_timeout，与 rusqlite `Connection::open` 内部的默认值相同（5 秒），也是账本一直在用的值。
pub const STANDARD_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// 连接的设置档位。见模块文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionProfile {
    /// 存量行为，不额外设置任何 PRAGMA。
    #[default]
    Legacy,
    /// 组织、云形态：WAL（已验证）、显式 busy_timeout、`foreign_keys=ON`（已回读）。
    Standard,
}

/// 一条连接当前的三项设置，用来做断言（测试、诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PragmaSnapshot {
    /// SQLite 回答的 `journal_mode`，小写（`wal`、`delete`、`memory`……）。
    pub journal_mode: String,
    pub busy_timeout_ms: i64,
    pub foreign_keys: bool,
}

/// 设置连接失败。
#[derive(Debug, thiserror::Error)]
pub enum ConnectionSetupError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// `PRAGMA journal_mode = WAL` 的回答不是 `wal`（例如内存库、不支持共享内存的文件系统）。
    #[error("journal_mode must be wal but SQLite answered {0:?}")]
    NotWal(String),
    /// `PRAGMA foreign_keys = ON` 之后回读不是 1（SQLite 编译时关掉了外键，或在事务里调用）。
    #[error("foreign_keys could not be enabled on this connection")]
    ForeignKeysUnavailable,
}

/// 对一条刚打开、还没有开事务的连接应用 [`ConnectionProfile::Standard`]。
///
/// 顺序固定：先 WAL（改变日志模式不能在事务里），再 busy_timeout，最后 `foreign_keys`。
/// 任何一步的结果不符合预期都返回错误，调用方应当让启动失败。
pub fn apply_standard(
    conn: &Connection,
    busy_timeout: Duration,
) -> Result<(), ConnectionSetupError> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(ConnectionSetupError::NotWal(mode));
    }
    conn.busy_timeout(busy_timeout)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let enabled: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if enabled != 1 {
        return Err(ConnectionSetupError::ForeignKeysUnavailable);
    }
    Ok(())
}

/// 读出连接当前的三项设置。
pub fn snapshot(conn: &Connection) -> Result<PragmaSnapshot, rusqlite::Error> {
    let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    let busy_timeout_ms: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    Ok(PragmaSnapshot {
        journal_mode: journal_mode.to_ascii_lowercase(),
        busy_timeout_ms,
        foreign_keys: foreign_keys == 1,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::test_support::TempDir;

    fn temp_db(tag: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new(tag);
        let path = dir.db("pragma.sqlite3");
        (dir, path)
    }

    #[test]
    fn a_plain_connection_is_not_wal_and_the_bundled_sqlite_defaults_foreign_keys_on() {
        // 对照组：证明下面的断言确实是 apply_standard 造成的，而不是默认值碰巧如此。
        // 事实：libsqlite3-sys 的 bundled 构建带 `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`，所以不设也是开的；
        // 既有四个存储的连接因此一直是 foreign_keys=ON（它们的 schema 里没有外键，所以没有效果）。
        // Standard 把它从「依赖库的编译默认」变成「显式设置并回读确认」。
        let (_dir, path) = temp_db("plain");
        let conn = Connection::open(&path).unwrap();
        let snap = snapshot(&conn).unwrap();
        assert_eq!(snap.journal_mode, "delete");
        assert!(
            snap.foreign_keys,
            "bundled default; if this ever flips, Standard still sets it"
        );
    }

    #[test]
    fn standard_turns_foreign_keys_on_even_when_the_connection_had_them_off() {
        let (_dir, path) = temp_db("fk-off");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        assert!(!snapshot(&conn).unwrap().foreign_keys);
        apply_standard(&conn, STANDARD_BUSY_TIMEOUT).unwrap();
        assert!(snapshot(&conn).unwrap().foreign_keys);
    }

    #[test]
    fn standard_sets_wal_busy_timeout_and_foreign_keys_and_verifies_them() {
        let (_dir, path) = temp_db("standard");
        let conn = Connection::open(&path).unwrap();
        apply_standard(&conn, STANDARD_BUSY_TIMEOUT).unwrap();
        assert_eq!(
            snapshot(&conn).unwrap(),
            PragmaSnapshot {
                journal_mode: "wal".into(),
                busy_timeout_ms: 5_000,
                foreign_keys: true,
            }
        );
    }

    #[test]
    fn standard_honours_an_explicit_busy_timeout() {
        let (_dir, path) = temp_db("busy");
        let conn = Connection::open(&path).unwrap();
        apply_standard(&conn, Duration::from_millis(1_234)).unwrap();
        assert_eq!(snapshot(&conn).unwrap().busy_timeout_ms, 1_234);
    }

    #[test]
    fn standard_refuses_a_connection_that_cannot_be_wal() {
        // 内存库的 journal_mode 永远是 memory：必须报错，不能当成功。
        let conn = Connection::open_in_memory().unwrap();
        match apply_standard(&conn, STANDARD_BUSY_TIMEOUT) {
            Err(ConnectionSetupError::NotWal(mode)) => assert_eq!(mode, "memory"),
            other => panic!("expected NotWal, got {other:?}"),
        }
    }

    #[test]
    fn standard_is_idempotent_on_an_already_wal_file() {
        let (_dir, path) = temp_db("again");
        let first = Connection::open(&path).unwrap();
        apply_standard(&first, STANDARD_BUSY_TIMEOUT).unwrap();
        let second = Connection::open(&path).unwrap();
        apply_standard(&second, STANDARD_BUSY_TIMEOUT).unwrap();
        assert_eq!(snapshot(&second).unwrap(), snapshot(&first).unwrap());
    }

    #[test]
    fn the_default_profile_is_legacy() {
        assert_eq!(ConnectionProfile::default(), ConnectionProfile::Legacy);
    }
}

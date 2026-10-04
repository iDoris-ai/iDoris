//! SQLite usage/audit storage. Every record access requires tenant scope;
//! schema initialization alone does not expose a tenant CRUD interface.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Usage,
    Audit,
}

impl RecordKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Audit => "audit",
        }
    }

    fn from_str(value: &str) -> rusqlite::Result<Self> {
        match value {
            "usage" => Ok(Self::Usage),
            "audit" => Ok(Self::Audit),
            _ => Err(rusqlite::Error::InvalidColumnType(
                1,
                "kind".to_owned(),
                rusqlite::types::Type::Text,
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TenantRecord {
    pub tenant_id: String,
    pub kind: RecordKind,
    pub record_id: String,
    pub request_id: String,
    pub origin_record_id: Option<String>,
    pub payload: Map<String, Value>,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("tenant scope is required")]
    ScopeRequired,
    #[error("record tenant does not match scope")]
    TenantMismatch,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Owns the connection so callers cannot bypass scope checks through this store.
pub struct TenantStore {
    conn: Connection,
}

impl TenantStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::new(Connection::open(path)?)
    }

    pub fn new(mut conn: Connection) -> Result<Self, StoreError> {
        initialize_record_schema(&mut conn)?;
        Ok(Self { conn })
    }

    /// Reject mismatched tenants and duplicate keys without overwriting records.
    pub fn put(&self, scope: Option<&str>, record: &TenantRecord) -> Result<(), StoreError> {
        let scope = required_scope(scope)?;
        if record.tenant_id != scope {
            return Err(StoreError::TenantMismatch);
        }
        let payload = serde_json::to_string(&record.payload)?;
        self.conn.execute(
            "INSERT INTO tenant_records (tenant_id, kind, record_id, request_id, origin_record_id, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![scope, record.kind.as_str(), record.record_id, record.request_id, record.origin_record_id, payload],
        )?;
        Ok(())
    }

    pub fn list(
        &self,
        scope: Option<&str>,
        kind: Option<RecordKind>,
    ) -> Result<Vec<TenantRecord>, StoreError> {
        let scope = required_scope(scope)?;
        let mut stmt = self.conn.prepare(
            "SELECT tenant_id, kind, record_id, request_id, origin_record_id, payload FROM tenant_records WHERE tenant_id = ?1 AND (?2 IS NULL OR kind = ?2) ORDER BY kind, record_id",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![scope, kind.map(RecordKind::as_str)],
            row_to_raw,
        )?;
        rows.map(|row| raw_to_record(row?)).collect()
    }

    pub fn get(
        &self,
        scope: Option<&str>,
        kind: RecordKind,
        record_id: &str,
    ) -> Result<Option<TenantRecord>, StoreError> {
        let scope = required_scope(scope)?;
        self.conn.query_row(
            "SELECT tenant_id, kind, record_id, request_id, origin_record_id, payload FROM tenant_records WHERE tenant_id = ?1 AND kind = ?2 AND record_id = ?3",
            rusqlite::params![scope, kind.as_str(), record_id],
            row_to_raw,
        ).optional()?.map(raw_to_record).transpose()
    }
}

fn required_scope(scope: Option<&str>) -> Result<&str, StoreError> {
    scope
        .filter(|value| !value.trim().is_empty())
        .ok_or(StoreError::ScopeRequired)
}

type RawRecord = (String, String, String, String, Option<String>, String);

fn row_to_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecord> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn raw_to_record(raw: RawRecord) -> Result<TenantRecord, StoreError> {
    Ok(TenantRecord {
        tenant_id: raw.0,
        kind: RecordKind::from_str(&raw.1)?,
        record_id: raw.2,
        request_id: raw.3,
        origin_record_id: raw.4,
        payload: serde_json::from_str(&raw.5)?,
    })
}

/// Initialize the tenant record schema on an existing SQLite connection.
pub fn initialize_record_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS record_schema_migrations (\
            version INTEGER PRIMARY KEY)",
    )?;
    let applied: Option<i64> = tx
        .query_row(
            "SELECT version FROM record_schema_migrations WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if applied.is_none() {
        tx.execute_batch(include_str!("store/migrations/0001_records.sql"))?;
        tx.execute(
            "INSERT INTO record_schema_migrations (version) VALUES (1)",
            [],
        )?;
    }
    tx.commit()
}

#[cfg(test)]
#[path = "store/tests.rs"]
mod tests;

//! Tenant record storage schema. The raw-connection migration function is
//! for later `TenantStore` initialization, not a tenant CRUD interface.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

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

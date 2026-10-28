use std::path::Path;

use idoris_contracts::common::PrivacyClass;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{VirtualKeyHash, VirtualKeySecret};

const OPEN_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualKeyScope {
    pub owner: String,
    /// Explicit tenant authority. Legacy keys have no tenant authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    pub allowed_privacy: Vec<PrivacyClass>,
    pub allowed_roles: Vec<String>,
    pub budget_ref: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub admin_scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedVirtualKey {
    pub key_id: String,
    pub scope: VirtualKeyScope,
}

#[derive(Debug, Error)]
pub enum VirtualKeyStoreError {
    #[error("invalid virtual-key metadata")]
    InvalidMetadata,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct VirtualKeyStore {
    conn: Connection,
}

impl VirtualKeyStore {
    /// Opens the shared database file. Offline key commands open it while
    /// `idoris serve` may be writing, so wait (like the budget ledger) instead
    /// of failing at once with `database is locked`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, VirtualKeyStoreError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(OPEN_BUSY_TIMEOUT)?;
        Self::new(conn)
    }

    pub fn new(mut conn: Connection) -> Result<Self, VirtualKeyStoreError> {
        initialize_virtual_key_schema(&mut conn)?;
        Ok(Self { conn })
    }

    /// Inserts an active virtual-key verifier. Plaintext key material never
    /// crosses this API boundary.
    pub fn insert_active(
        &self,
        key_id: &str,
        hash: VirtualKeyHash,
        scope: &VirtualKeyScope,
    ) -> Result<(), VirtualKeyStoreError> {
        validate_metadata(key_id, scope)?;
        self.conn.execute(
            "INSERT INTO virtual_keys (
                key_id, key_hash, owner, tenant_id, allowed_privacy, allowed_roles,
                budget_ref, expires_at_ms, admin_scopes, status, revoked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', NULL)",
            rusqlite::params![
                key_id,
                &hash.0[..],
                scope.owner.as_str(),
                scope.tenant_id.as_deref(),
                serde_json::to_string(&scope.allowed_privacy)?,
                serde_json::to_string(&scope.allowed_roles)?,
                scope.budget_ref.as_deref(),
                scope.expires_at_ms,
                serde_json::to_string(&scope.admin_scopes)?,
            ],
        )?;
        Ok(())
    }

    /// Authentication deliberately folds unknown, revoked and expired
    /// verifiers into None. HTTP callers must not learn which case occurred.
    pub fn authenticate(
        &self,
        secret: &VirtualKeySecret,
        now_ms: i64,
    ) -> Result<Option<AuthenticatedVirtualKey>, VirtualKeyStoreError> {
        if now_ms < 0 {
            return Err(VirtualKeyStoreError::InvalidMetadata);
        }
        let hash = secret.hash();
        let row: Option<RawVirtualKey> = self
            .conn
            .query_row(
                "SELECT key_id, owner, tenant_id, allowed_privacy, allowed_roles, budget_ref,
                            expires_at_ms, admin_scopes
                     FROM virtual_keys
                     WHERE key_hash = ?1
                       AND status = 'active'
                       AND (expires_at_ms IS NULL OR expires_at_ms > ?2)",
                rusqlite::params![&hash.0[..], now_ms],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(raw_to_identity).transpose()
    }

    /// Startup authority check for exposing the data-plane listener beyond
    /// loopback. Revoked/expired rows do not count as active authority.
    pub fn has_active_key(&self, now_ms: i64) -> Result<bool, VirtualKeyStoreError> {
        if now_ms < 0 {
            return Err(VirtualKeyStoreError::InvalidMetadata);
        }
        let present: i64 = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM virtual_keys
                 WHERE status = 'active'
                   AND (expires_at_ms IS NULL OR expires_at_ms > ?1)
             )",
            [now_ms],
            |row| row.get(0),
        )?;
        Ok(present == 1)
    }

    /// Revocation is durable and idempotent. Management code may observe the
    /// boolean; authentication still exposes only active-vs-not-active.
    pub fn revoke(&self, key_id: &str, revoked_at_ms: i64) -> Result<bool, VirtualKeyStoreError> {
        if key_id.trim().is_empty() || revoked_at_ms <= 0 {
            return Err(VirtualKeyStoreError::InvalidMetadata);
        }
        let changed = self.conn.execute(
            "UPDATE virtual_keys
             SET status = 'revoked', revoked_at_ms = ?2
             WHERE key_id = ?1 AND status = 'active'",
            rusqlite::params![key_id, revoked_at_ms],
        )?;
        Ok(changed == 1)
    }
}

/// Management view of one stored key. Never carries the verifier hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualKeyRecord {
    pub key_id: String,
    pub scope: VirtualKeyScope,
    pub revoked_at_ms: Option<i64>,
}

impl VirtualKeyStore {
    /// Lists all keys in insertion order (rowid; creation time is not stored).
    pub fn list(&self) -> Result<Vec<VirtualKeyRecord>, VirtualKeyStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT key_id, owner, tenant_id, allowed_privacy, allowed_roles, budget_ref,
                    expires_at_ms, admin_scopes, revoked_at_ms
             FROM virtual_keys ORDER BY rowid, key_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                (
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ),
                row.get::<_, Option<i64>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (raw, revoked_at_ms) = row?;
            let key = raw_to_identity(raw)?;
            out.push(VirtualKeyRecord {
                key_id: key.key_id,
                scope: key.scope,
                revoked_at_ms,
            });
        }
        Ok(out)
    }
}

type RawVirtualKey = (
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<i64>,
    String,
);

fn raw_to_identity(row: RawVirtualKey) -> Result<AuthenticatedVirtualKey, VirtualKeyStoreError> {
    Ok(AuthenticatedVirtualKey {
        key_id: row.0,
        scope: VirtualKeyScope {
            owner: row.1,
            tenant_id: row.2,
            allowed_privacy: serde_json::from_str(&row.3)?,
            allowed_roles: serde_json::from_str(&row.4)?,
            budget_ref: row.5,
            expires_at_ms: row.6,
            admin_scopes: serde_json::from_str(&row.7)?,
        },
    })
}

fn validate_metadata(key_id: &str, scope: &VirtualKeyScope) -> Result<(), VirtualKeyStoreError> {
    if key_id.trim().is_empty()
        || scope.owner.trim().is_empty()
        || scope.tenant_id.as_deref().is_some_and(|tenant| {
            tenant.is_empty()
                || tenant.len() > 128
                || tenant.trim() != tenant
                || tenant.chars().any(char::is_control)
        })
        || scope.allowed_privacy.is_empty()
        || scope
            .allowed_roles
            .iter()
            .chain(scope.admin_scopes.iter())
            .any(|value| value.trim().is_empty())
        || scope
            .budget_ref
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        || scope.expires_at_ms.is_some_and(|value| value <= 0)
    {
        return Err(VirtualKeyStoreError::InvalidMetadata);
    }
    Ok(())
}

pub fn initialize_virtual_key_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS virtual_key_schema_migrations (
            version INTEGER PRIMARY KEY
        )",
    )?;
    let applied: Option<i64> = tx
        .query_row(
            "SELECT version FROM virtual_key_schema_migrations WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if applied.is_none() {
        tx.execute_batch(include_str!("store/0001_virtual_keys.sql"))?;
        tx.execute(
            "INSERT INTO virtual_key_schema_migrations (version) VALUES (1)",
            [],
        )?;
    }
    let tenant_migration: Option<i64> = tx
        .query_row(
            "SELECT version FROM virtual_key_schema_migrations WHERE version = 2",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if tenant_migration.is_none() {
        tx.execute_batch(include_str!("store/0002_tenant_binding.sql"))?;
        tx.execute(
            "INSERT INTO virtual_key_schema_migrations (version) VALUES (2)",
            [],
        )?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::virtual_key::MintedVirtualKey;

    fn scope(expires_at_ms: Option<i64>) -> VirtualKeyScope {
        VirtualKeyScope {
            owner: "agent24-instance".into(),
            tenant_id: None,
            allowed_privacy: vec![PrivacyClass::LocalOnly],
            allowed_roles: vec!["fast".into(), "daily".into()],
            budget_ref: Some("default".into()),
            expires_at_ms,
            admin_scopes: Vec::new(),
        }
    }

    #[test]
    fn opened_store_waits_for_a_concurrent_writer_instead_of_failing() {
        let path = std::env::temp_dir().join(format!(
            "idoris-virtual-key-busy-{}.sqlite3",
            uuid::Uuid::new_v4().simple()
        ));
        drop(VirtualKeyStore::open(&path).unwrap());
        let mut holder = Connection::open(&path).unwrap();
        let tx = holder
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                let store = VirtualKeyStore::open(&path).unwrap();
                let minted = MintedVirtualKey::mint();
                store.insert_active(&minted.key_id, minted.hash, &scope(None))
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        tx.commit().unwrap();
        writer.join().unwrap().unwrap();
        drop(holder);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persists_only_hash_and_authenticates_active_unexpired_key() {
        let path = std::env::temp_dir().join(format!(
            "idoris-virtual-key-{}.sqlite3",
            uuid::Uuid::new_v4().simple()
        ));
        let minted = MintedVirtualKey::mint();
        let plaintext = minted.secret.expose_secret().as_bytes().to_vec();
        {
            let store = VirtualKeyStore::open(&path).unwrap();
            store
                .insert_active(&minted.key_id, minted.hash, &scope(Some(2_000)))
                .unwrap();
            let identity = store.authenticate(&minted.secret, 1_000).unwrap().unwrap();
            assert_eq!(identity.key_id, minted.key_id);
            assert_eq!(
                identity.scope.allowed_privacy,
                vec![PrivacyClass::LocalOnly]
            );
        }

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(plaintext.len())
                .any(|window| window == plaintext.as_slice()),
            "plaintext bearer token leaked to SQLite"
        );
        let reopened = VirtualKeyStore::open(&path).unwrap();
        assert!(
            reopened
                .authenticate(&minted.secret, 1_000)
                .unwrap()
                .is_some()
        );
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn migrates_legacy_keys_without_granting_tenant_authority() {
        let path = std::env::temp_dir().join(format!(
            "idoris-tenant-migration-{}.sqlite3",
            uuid::Uuid::new_v4().simple()
        ));
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE virtual_key_schema_migrations (version INTEGER PRIMARY KEY);",
        )
        .unwrap();
        conn.execute_batch(include_str!("store/0001_virtual_keys.sql"))
            .unwrap();
        conn.execute(
            "INSERT INTO virtual_key_schema_migrations (version) VALUES (1)",
            [],
        )
        .unwrap();
        let legacy = MintedVirtualKey::mint();
        conn.execute(
            "INSERT INTO virtual_keys (
                key_id, key_hash, owner, allowed_privacy, allowed_roles,
                budget_ref, expires_at_ms, admin_scopes, status, revoked_at_ms
             ) VALUES (?1, ?2, 'legacy', '[\"local_only\"]', '[\"fast\"]',
                       NULL, NULL, '[]', 'active', NULL)",
            rusqlite::params![legacy.key_id, &legacy.hash.0[..]],
        )
        .unwrap();
        initialize_virtual_key_schema(&mut conn).unwrap();
        initialize_virtual_key_schema(&mut conn).unwrap();
        let store = VirtualKeyStore::new(conn).unwrap();
        let legacy_identity = store.authenticate(&legacy.secret, 1).unwrap().unwrap();
        assert_eq!(legacy_identity.scope.tenant_id, None);

        let bound = MintedVirtualKey::mint();
        let mut bound_scope = scope(None);
        bound_scope.tenant_id = Some("victim".into());
        store
            .insert_active(&bound.key_id, bound.hash, &bound_scope)
            .unwrap();
        let bound_identity = store.authenticate(&bound.secret, 1).unwrap().unwrap();
        assert_eq!(bound_identity.scope.tenant_id.as_deref(), Some("victim"));
        drop(store);
        let reopened = VirtualKeyStore::open(&path).unwrap();
        assert_eq!(
            reopened.authenticate(&bound.secret, 1).unwrap(),
            Some(bound_identity)
        );
        assert_eq!(
            reopened.authenticate(&legacy.secret, 1).unwrap(),
            Some(legacy_identity)
        );
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn tenant_binding_rejects_unusable_identifiers_without_activating_key() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        for tenant in [
            "".into(),
            " tenant".into(),
            "tenant\n".into(),
            "tenant\0suffix".into(),
            "x".repeat(129),
            "é".repeat(65),
        ] {
            let key = MintedVirtualKey::mint();
            let mut bound = scope(None);
            bound.tenant_id = Some(tenant);
            assert!(matches!(
                store.insert_active(&key.key_id, key.hash, &bound),
                Err(VirtualKeyStoreError::InvalidMetadata)
            ));
            assert!(store.authenticate(&key.secret, 1).unwrap().is_none());
            assert!(!store.has_active_key(1).unwrap());
        }
        let key = MintedVirtualKey::mint();
        let mut bound = scope(None);
        bound.tenant_id = Some("é".repeat(64));
        store.insert_active(&key.key_id, key.hash, &bound).unwrap();
        assert_eq!(
            store.authenticate(&key.secret, 1).unwrap().unwrap().scope,
            bound
        );
    }

    #[test]
    fn unknown_revoked_and_expired_keys_all_authenticate_as_none() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let active = MintedVirtualKey::mint();
        let revoked = MintedVirtualKey::mint();
        let expired = MintedVirtualKey::mint();
        store
            .insert_active(&active.key_id, active.hash, &scope(None))
            .unwrap();
        store
            .insert_active(&revoked.key_id, revoked.hash, &scope(None))
            .unwrap();
        store
            .insert_active(&expired.key_id, expired.hash, &scope(Some(100)))
            .unwrap();
        assert!(store.revoke(&revoked.key_id, 50).unwrap());
        assert!(!store.revoke(&revoked.key_id, 60).unwrap());

        assert!(
            store
                .authenticate(&MintedVirtualKey::mint().secret, 200)
                .unwrap()
                .is_none()
        );
        assert!(store.authenticate(&revoked.secret, 200).unwrap().is_none());
        assert!(store.authenticate(&expired.secret, 200).unwrap().is_none());
        assert!(store.authenticate(&active.secret, 200).unwrap().is_some());
        assert!(store.has_active_key(200).unwrap());
        assert!(store.revoke(&active.key_id, 201).unwrap());
        assert!(!store.has_active_key(202).unwrap());
    }

    #[test]
    fn active_key_presence_ignores_revoked_and_expired_rows() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let revoked = MintedVirtualKey::mint();
        let expired = MintedVirtualKey::mint();
        store
            .insert_active(&revoked.key_id, revoked.hash, &scope(None))
            .unwrap();
        store
            .insert_active(&expired.key_id, expired.hash, &scope(Some(100)))
            .unwrap();
        assert!(store.revoke(&revoked.key_id, 50).unwrap());
        assert!(!store.has_active_key(200).unwrap());
        assert!(matches!(
            store.has_active_key(-1),
            Err(VirtualKeyStoreError::InvalidMetadata)
        ));
    }

    #[test]
    fn invalid_scope_is_rejected_before_any_row_is_written() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let minted = MintedVirtualKey::mint();
        let mut invalid = scope(None);
        invalid.allowed_privacy.clear();
        assert!(matches!(
            store.insert_active(&minted.key_id, minted.hash, &invalid),
            Err(VirtualKeyStoreError::InvalidMetadata)
        ));
        assert!(store.authenticate(&minted.secret, 1).unwrap().is_none());
    }
}

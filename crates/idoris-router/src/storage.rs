//! Persistent tenancy bootstrap (B1 task18).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use idoris_contracts::DeployMode;
use idoris_tenancy::budget::{BudgetLedger, SpendGate};
use idoris_tenancy::event_log::EventLogStore;
use idoris_tenancy::store::TenantStore;
use idoris_tenancy::virtual_key::store::VirtualKeyStore;
use serde::Deserialize;

use crate::budget::PERSONAL_TENANT_ID;

const DEFAULT_TENANTS_RELATIVE: &str = "config/tenants.yaml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantFile {
    tenants: Vec<TenantConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantConfig {
    tenant_id: String,
    limit_minor: i64,
    billing_timezone: String,
    #[serde(default)]
    scope: ConfigSpendGate,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfigSpendGate {
    #[default]
    PaidOnly,
    All,
}

impl From<ConfigSpendGate> for SpendGate {
    fn from(value: ConfigSpendGate) -> Self {
        match value {
            ConfigSpendGate::PaidOnly => Self::PaidOnly,
            ConfigSpendGate::All => Self::All,
        }
    }
}

pub struct StorageBootstrap {
    pub records: Arc<Mutex<TenantStore>>,
    pub budget: Arc<BudgetLedger>,
    /// Persistent verifier storage only; request authentication is wired separately.
    pub virtual_keys: Arc<Mutex<VirtualKeyStore>>,
    pub event_log: Arc<EventLogStore>,
}

pub fn bootstrap_process(deploy_mode: DeployMode) -> Result<StorageBootstrap, String> {
    let executable =
        std::env::current_exe().map_err(|err| format!("无法定位当前可执行文件：{err}"))?;
    let parent = executable
        .parent()
        .ok_or_else(|| "当前可执行文件没有父目录".to_string())?;
    let db_path = process_db_path()?;
    let config_path = match env_path("IDORIS_TENANTS_CONFIG")? {
        Some(path) => Some(path),
        None if deploy_mode == DeployMode::Tenant => Some(parent.join(DEFAULT_TENANTS_RELATIVE)),
        None => None,
    };
    bootstrap(&db_path, config_path.as_deref(), deploy_mode)
}

/// Opens only the persistent virtual-key verifier store using the same
/// process database resolution as daemon startup. This intentionally avoids
/// tenant/budget bootstrap side effects for offline key-management commands.
pub fn open_virtual_key_store_process() -> Result<VirtualKeyStore, String> {
    let db_path = process_db_path()?;
    if let Some(parent) = db_path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("无法创建数据库目录 \"{}\"：{err}", parent.display()))?;
    }
    VirtualKeyStore::open(&db_path)
        .map_err(|err| format!("无法打开虚拟 key 数据库 \"{}\"：{err}", db_path.display()))
}

fn process_db_path() -> Result<PathBuf, String> {
    match env_path("IDORIS_DB_PATH")? {
        Some(path) => Ok(path),
        None => default_db_path(),
    }
}

pub fn bootstrap(
    db_path: &Path,
    tenant_config_path: Option<&Path>,
    deploy_mode: DeployMode,
) -> Result<StorageBootstrap, String> {
    if let Some(parent) = db_path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("无法创建数据库目录 \"{}\"：{err}", parent.display()))?;
    }

    let configs = load_configs(tenant_config_path, deploy_mode)?;
    let records = TenantStore::open(db_path)
        .map_err(|err| format!("无法打开租户记录数据库 \"{}\"：{err}", db_path.display()))?;
    let budget = BudgetLedger::open(db_path)
        .map_err(|err| format!("无法打开预算数据库 \"{}\"：{err}", db_path.display()))?;
    // Open the verifier schema during startup so a broken key store fails closed.
    let virtual_keys = VirtualKeyStore::open(db_path)
        .map_err(|err| format!("无法打开虚拟 key 数据库 \"{}\"：{err}", db_path.display()))?;
    let event_log = EventLogStore::open(db_path)
        .map_err(|err| format!("无法打开事件日志数据库 \"{}\"：{err}", db_path.display()))?;

    let mut seen = BTreeSet::new();
    for config in configs {
        let tenant_id = config.tenant_id.trim();
        if tenant_id.is_empty() {
            return Err("租户配置 tenant_id 不能为空".to_string());
        }
        if !seen.insert(tenant_id.to_string()) {
            return Err(format!("租户配置重复 tenant_id {tenant_id:?}"));
        }
        budget
            .configure_tenant(
                tenant_id,
                config.limit_minor,
                &config.billing_timezone,
                config.scope.into(),
            )
            .map_err(|err| format!("租户 {tenant_id:?} 配置无效：{err}"))?;
    }
    if deploy_mode == DeployMode::Personal && !seen.contains(PERSONAL_TENANT_ID) {
        return Err(format!(
            "personal 模式必须配置固定内部租户 {PERSONAL_TENANT_ID:?}"
        ));
    }
    let revoked = budget
        .retain_tenants(&seen)
        .map_err(|err| format!("无法对账可信租户清单：{err}"))?;
    if revoked > 0 {
        eprintln!("idoris: 已撤销 {revoked} 个不再存在于可信配置中的租户");
    }

    Ok(StorageBootstrap {
        records: Arc::new(Mutex::new(records)),
        budget: Arc::new(budget),
        virtual_keys: Arc::new(Mutex::new(virtual_keys)),
        event_log: Arc::new(event_log),
    })
}

fn load_configs(path: Option<&Path>, mode: DeployMode) -> Result<Vec<TenantConfig>, String> {
    let Some(path) = path else {
        if mode == DeployMode::Personal {
            return Ok(vec![TenantConfig {
                tenant_id: PERSONAL_TENANT_ID.to_string(),
                limit_minor: 0,
                billing_timezone: "UTC".to_string(),
                scope: ConfigSpendGate::PaidOnly,
            }]);
        }
        return Err("tenant 模式必须提供可信租户配置".to_string());
    };
    let raw = std::fs::read_to_string(path)
        .map_err(|err| format!("无法读取租户配置 \"{}\"：{err}", path.display()))?;
    let file: TenantFile = serde_yaml::from_str(&raw)
        .map_err(|err| format!("租户配置 \"{}\" 无效：{err}", path.display()))?;
    if file.tenants.is_empty() {
        return Err("租户配置 tenants 不能为空".to_string());
    }
    Ok(file.tenants)
}

fn env_path(name: &str) -> Result<Option<PathBuf>, String> {
    std::env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map(|value| {
                    if value.trim().is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    }
                })
                .map_err(|_| format!("环境变量 {name} 不是有效的 Unicode 路径"))
        })
        .transpose()
        .map(Option::flatten)
}

fn default_db_path() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "未设置 LOCALAPPDATA；请显式设置 IDORIS_DB_PATH".to_string())?;
        Ok(PathBuf::from(root).join("iDoris").join("idoris.sqlite3"))
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "未设置 HOME；请显式设置 IDORIS_DB_PATH".to_string())?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("iDoris")
            .join("idoris.sqlite3"))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(root).join("idoris").join("idoris.sqlite3"));
        }
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "未设置 XDG_DATA_HOME/HOME；请显式设置 IDORIS_DB_PATH".to_string())?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("idoris")
            .join("idoris.sqlite3"))
    }

    #[cfg(not(any(target_os = "windows", unix)))]
    Err("当前平台没有默认数据目录；请显式设置 IDORIS_DB_PATH".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use idoris_contracts::common::PrivacyClass;
    use idoris_tenancy::budget::{BudgetError, BudgetScope, Price};
    use idoris_tenancy::event_log::{EventType, NewEvent};
    use idoris_tenancy::store::{RecordKind, TenantRecord};
    use idoris_tenancy::virtual_key::MintedVirtualKey;
    use idoris_tenancy::virtual_key::store::VirtualKeyScope;
    use rusqlite::Connection;
    use serde_json::Map;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn virtual_key_scope() -> VirtualKeyScope {
        VirtualKeyScope {
            owner: "agent24-instance".into(),
            allowed_privacy: vec![PrivacyClass::LocalOnly],
            allowed_roles: vec!["fast".into()],
            budget_ref: None,
            expires_at_ms: None,
            admin_scopes: Vec::new(),
        }
    }

    #[test]
    fn personal_defaults_are_persistent_and_unknown_tenant_is_not_fake_zero() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("state.sqlite3");
        let key = MintedVirtualKey::mint();
        let event_id = Uuid::new_v4().to_string();
        {
            let storage = bootstrap(&db, None, DeployMode::Personal).unwrap();
            let view = storage.budget.tenant_readview(PERSONAL_TENANT_ID).unwrap();
            assert_eq!(view.limit_minor, 0);
            storage
                .virtual_keys
                .lock()
                .unwrap()
                .insert_active(&key.key_id, key.hash, &virtual_key_scope())
                .unwrap();
            storage
                .event_log
                .append(
                    Some(PERSONAL_TENANT_ID),
                    &NewEvent {
                        event_id: event_id.clone(),
                        tenant_id: PERSONAL_TENANT_ID.into(),
                        record_id: "event-record".into(),
                        event_type: EventType::RequestReceived,
                        ts_utc_ms: 1,
                        request_id: None,
                        session_id: None,
                        trace_id: None,
                        parent_id: None,
                        origin_record_id: None,
                        metadata: BTreeMap::new(),
                    },
                )
                .unwrap();
            storage
                .records
                .lock()
                .unwrap()
                .put(
                    Some(PERSONAL_TENANT_ID),
                    &TenantRecord {
                        tenant_id: PERSONAL_TENANT_ID.into(),
                        kind: RecordKind::Audit,
                        record_id: "r1".into(),
                        request_id: "q1".into(),
                        origin_record_id: None,
                        payload: Map::new(),
                    },
                )
                .unwrap();
        }
        let reopened = bootstrap(&db, None, DeployMode::Personal).unwrap();
        assert!(
            reopened
                .records
                .lock()
                .unwrap()
                .get(Some(PERSONAL_TENANT_ID), RecordKind::Audit, "r1")
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            reopened.budget.tenant_readview("missing"),
            Err(BudgetError::TenantNotConfigured { .. })
        ));
        assert_eq!(
            reopened
                .virtual_keys
                .lock()
                .unwrap()
                .authenticate(&key.secret, 1)
                .unwrap()
                .unwrap()
                .key_id,
            key.key_id
        );
        let events = reopened
            .event_log
            .events_for_record(Some(PERSONAL_TENANT_ID), "event-record")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.event_id, event_id);
    }

    #[test]
    fn virtual_key_store_open_failure_fails_bootstrap_closed() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("state.sqlite3");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE virtual_key_schema_migrations (broken INTEGER);")
            .unwrap();
        drop(conn);

        let err = bootstrap(&db, None, DeployMode::Personal).err().unwrap();
        assert!(err.contains("无法打开虚拟 key 数据库"), "{err}");
    }

    #[test]
    fn broken_event_log_schema_fails_storage_bootstrap() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("broken-event-log.sqlite3");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY); \
             INSERT INTO event_log_schema_migrations VALUES(1); \
             CREATE TABLE event_log_events(sequence INTEGER PRIMARY KEY AUTOINCREMENT);",
        )
        .unwrap();
        drop(conn);

        let err = bootstrap(&db, None, DeployMode::Personal).err().unwrap();
        assert!(err.contains("无法打开事件日志数据库"), "{err}");
        let ledger = BudgetLedger::open(&db).unwrap();
        assert!(matches!(
            ledger.tenant_readview(PERSONAL_TENANT_ID),
            Err(BudgetError::TenantNotConfigured { .. })
        ));
    }

    #[test]
    fn trusted_file_controls_tenant_budget_and_bad_db_path_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("tenants.yaml");
        std::fs::write(
            &config,
            "tenants:\n  - tenant_id: acme\n    limit_minor: 1234\n    billing_timezone: Asia/Bangkok\n    scope: all\n",
        )
        .unwrap();
        let storage = bootstrap(
            &dir.path().join("tenant.sqlite3"),
            Some(&config),
            DeployMode::Tenant,
        )
        .unwrap();
        let view = storage.budget.tenant_readview("acme").unwrap();
        assert_eq!(view.limit_minor, 1234);
        assert_eq!(view.billing_timezone, "Asia/Bangkok");
        assert_eq!(view.scope, SpendGate::All);

        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "file").unwrap();
        let err = bootstrap(
            &blocker.join("db.sqlite3"),
            Some(&config),
            DeployMode::Tenant,
        )
        .err()
        .unwrap();
        assert!(err.contains("无法创建数据库目录"), "{err}");
    }

    #[test]
    fn trusted_tenant_list_revokes_removed_tenant_on_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("tenant.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(
            &config,
            "tenants:\n  - tenant_id: acme\n    limit_minor: 100\n    billing_timezone: UTC\n    scope: all\n  - tenant_id: beta\n    limit_minor: 100\n    billing_timezone: UTC\n    scope: all\n",
        )
        .unwrap();
        let first = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
        let beta_scope = BudgetScope::new("beta", "key", "provider", "model");
        first.budget.reserve(&beta_scope, Price::Known(1)).unwrap();
        drop(first);

        std::fs::write(
            &config,
            "tenants:\n  - tenant_id: acme\n    limit_minor: 100\n    billing_timezone: UTC\n    scope: all\n",
        )
        .unwrap();
        let reopened = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
        assert!(matches!(
            reopened.budget.tenant_readview("beta"),
            Err(BudgetError::TenantNotConfigured { .. })
        ));
        assert!(matches!(
            reopened.budget.reserve(&beta_scope, Price::Known(1)),
            Err(BudgetError::NotConfigured { .. })
        ));
        assert!(reopened.budget.tenant_readview("acme").is_ok());
    }
}

//! Persistent tenancy bootstrap (B1 task18).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use idoris_contracts::DeployMode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::budget::{BudgetLedger, SeedDrift, SeedOutcome, SpendGate, TenantSeed};
use idoris_tenancy::event_log::{EventLogError, EventLogStore, EventType, NewEvent};
use idoris_tenancy::sqlite_conn::{ConnectionProfile, open_standard};
use idoris_tenancy::store::{TenantRecord, TenantStore};
use idoris_tenancy::virtual_key::store::VirtualKeyStore;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::budget::PERSONAL_TENANT_ID;
use crate::startup_form::StartupForm;

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
    /// Compute-service account storage (principals, memberships, payment
    /// accounts, credentials, allocations). Opened **only** when
    /// `IDORIS_FORM` declares the organization or cloud form; `None` for the
    /// personal form and for every legacy tenant deployment, whose database
    /// then never gets these tables.
    pub accounts: Option<Arc<AccountStore>>,
}

pub fn bootstrap_process(deploy_mode: DeployMode) -> Result<StorageBootstrap, String> {
    bootstrap_process_with_form(StartupForm::legacy(deploy_mode))
}

/// As [`bootstrap_process`], with the deployment form resolved from
/// `IDORIS_FORM` (see [`StartupForm`]).
pub fn bootstrap_process_with_form(startup: StartupForm) -> Result<StorageBootstrap, String> {
    let deploy_mode = startup.deploy_mode();
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
    bootstrap_with_form(&db_path, config_path.as_deref(), startup)
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

/// Why [`acquire_instance_lock`] failed.
#[derive(Debug)]
pub enum InstanceLockError {
    /// Another `idoris serve` already holds the lock for this database.
    Held(PathBuf),
    /// DB path/dir problem; storage bootstrap reports the same failure with
    /// its established error precedence, so the caller may defer to it.
    Deferred(String),
    /// Lock file unusable: startup must fail.
    Other(String),
}

/// Takes an exclusive OS advisory lock on `<db_path>.lock` for the whole
/// process lifetime (the caller keeps the returned file alive). The kernel
/// releases it on any exit, including a crash, so there is no stale-lock
/// state. `File` is opened close-on-exec, so spawned runtimes never inherit it.
pub fn acquire_instance_lock() -> Result<std::fs::File, InstanceLockError> {
    let db_path = process_db_path().map_err(InstanceLockError::Deferred)?;
    if let Some(parent) = db_path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|err| {
            InstanceLockError::Deferred(format!(
                "无法创建数据库目录 \"{}\"：{err}",
                parent.display()
            ))
        })?;
    }
    let mut lock_path = canonical_lock_base(&db_path).into_os_string();
    lock_path.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|err| {
            InstanceLockError::Other(format!(
                "无法获取实例锁（锁文件不可用） \"{}\"：{err}",
                Path::new(&lock_path).display()
            ))
        })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(InstanceLockError::Held(db_path)),
        Err(std::fs::TryLockError::Error(err)) => Err(InstanceLockError::Other(format!(
            "无法获取实例锁 \"{}\"：{err}",
            Path::new(&lock_path).display()
        ))),
    }
}

/// Path the lock file is derived from, so every alias of one database maps to
/// one lock. An existing DB file is canonicalized (resolves a symlinked file
/// and symlinked directories). A not-yet-created file is `canonical(parent)`
/// joined with its name; a dangling symlink is first followed to the file it
/// will create, so the lock is the same before and after the first start
/// creates it. If the parent cannot be canonicalized either, the literal path
/// is used (best effort). Odd names such as `:memory:` just resolve against
/// the working directory.
fn canonical_lock_base(db_path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(db_path) {
        return real;
    }
    // Bounded like the kernel's own symlink limit, so a loop cannot hang.
    let mut target = db_path.to_path_buf();
    for _ in 0..40 {
        let Ok(next) = std::fs::read_link(&target) else {
            break;
        };
        target = match target.parent() {
            Some(dir) => dir.join(next),
            None => next,
        };
    }
    let db_path = target.as_path();
    let parent = db_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match (std::fs::canonicalize(parent), db_path.file_name()) {
        (Ok(parent), Some(name)) => parent.join(name),
        _ => db_path.to_path_buf(),
    }
}

fn process_db_path() -> Result<PathBuf, String> {
    match env_path("IDORIS_DB_PATH")? {
        Some(path) => Ok(path),
        None => default_db_path(),
    }
}

/// Opens the persistent stores with the pre-S1 behaviour (no deployment form
/// declared). Identical to [`bootstrap_with_form`] with
/// [`StartupForm::legacy`]: the configuration file is the authority for
/// tenants and every start prunes tenants it no longer lists.
pub fn bootstrap(
    db_path: &Path,
    tenant_config_path: Option<&Path>,
    deploy_mode: DeployMode,
) -> Result<StorageBootstrap, String> {
    bootstrap_with_form(
        db_path,
        tenant_config_path,
        StartupForm::legacy(deploy_mode),
    )
}

/// The four stores, opened either exactly as before S1 ([`ConnectionProfile::Legacy`])
/// or with the compute-service connection settings ([`ConnectionProfile::Standard`]).
type OpenedStores = (TenantStore, BudgetLedger, VirtualKeyStore, EventLogStore);

fn open_stores(db_path: &Path, profile: ConnectionProfile) -> Result<OpenedStores, String> {
    match profile {
        ConnectionProfile::Legacy => {
            let records = TenantStore::open(db_path).map_err(|err| {
                format!("无法打开租户记录数据库 \"{}\"：{err}", db_path.display())
            })?;
            let budget = BudgetLedger::open(db_path)
                .map_err(|err| format!("无法打开预算数据库 \"{}\"：{err}", db_path.display()))?;
            // Open the verifier schema during startup so a broken key store fails closed.
            let virtual_keys = VirtualKeyStore::open(db_path).map_err(|err| {
                format!("无法打开虚拟 key 数据库 \"{}\"：{err}", db_path.display())
            })?;
            let event_log = EventLogStore::open(db_path).map_err(|err| {
                format!("无法打开事件日志数据库 \"{}\"：{err}", db_path.display())
            })?;
            Ok((records, budget, virtual_keys, event_log))
        }
        ConnectionProfile::Standard => {
            let standard = || {
                open_standard(db_path)
                    .map_err(|err| format!("无法设置数据库连接 \"{}\"：{err}", db_path.display()))
            };
            let records = TenantStore::new(standard()?).map_err(|err| {
                format!("无法打开租户记录数据库 \"{}\"：{err}", db_path.display())
            })?;
            let budget = BudgetLedger::open_with_profile(db_path, ConnectionProfile::Standard)
                .map_err(|err| format!("无法打开预算数据库 \"{}\"：{err}", db_path.display()))?;
            let virtual_keys = VirtualKeyStore::new(standard()?).map_err(|err| {
                format!("无法打开虚拟 key 数据库 \"{}\"：{err}", db_path.display())
            })?;
            let event_log = EventLogStore::new(standard()?).map_err(|err| {
                format!("无法打开事件日志数据库 \"{}\"：{err}", db_path.display())
            })?;
            Ok((records, budget, virtual_keys, event_log))
        }
    }
}

/// What reconciling the tenant list with the database did, for one summary
/// line at start-up (database-authoritative forms only).
#[derive(Default)]
struct SeedSummary {
    seeded: usize,
    adopted: usize,
    stale: Vec<(String, SeedDrift)>,
}

/// Opens every persistent store and reconciles the tenant list.
///
/// - **No `IDORIS_FORM`** (legacy, and the explicit personal form): the
///   trusted configuration file is the authority. Every listed tenant is
///   upserted with `configure_tenant`, then `retain_tenants` revokes the
///   configuration of tenants the file no longer lists. Byte-for-byte what
///   start-up did before S1.
/// - **Organization / cloud** (design §9 item 2): the database is the
///   authority. The file only *seeds* tenants it lists that the database has
///   never seen (`BudgetLedger::seed_tenant`); nothing is overwritten and
///   `retain_tenants` is **not** called, so removing a tenant from the file
///   neither deletes it nor its data. These forms also open the account store
///   and use the standard connection settings.
pub fn bootstrap_with_form(
    db_path: &Path,
    tenant_config_path: Option<&Path>,
    startup: StartupForm,
) -> Result<StorageBootstrap, String> {
    let deploy_mode = startup.deploy_mode();
    if let Some(parent) = db_path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("无法创建数据库目录 \"{}\"：{err}", parent.display()))?;
    }

    let configs = load_configs(tenant_config_path, deploy_mode)?;
    let (records, budget, virtual_keys, event_log) =
        open_stores(db_path, startup.connection_profile())?;
    let accounts = if startup.opens_account_store() {
        Some(Arc::new(AccountStore::open(db_path).map_err(|err| {
            format!("无法打开账户数据库 \"{}\"：{err}", db_path.display())
        })?))
    } else {
        None
    };

    let mut seen = BTreeSet::new();
    let mut summary = SeedSummary::default();
    for config in configs {
        let tenant_id = config.tenant_id.trim();
        if tenant_id.is_empty() {
            return Err("租户配置 tenant_id 不能为空".to_string());
        }
        if !seen.insert(tenant_id.to_string()) {
            return Err(format!("租户配置重复 tenant_id {tenant_id:?}"));
        }
        if startup.tenants_are_database_authoritative() {
            let report = budget
                .seed_tenant(&TenantSeed {
                    tenant_id: tenant_id.to_string(),
                    limit_minor: config.limit_minor,
                    billing_timezone: config.billing_timezone.clone(),
                    gate: config.scope.into(),
                })
                .map_err(|err| format!("租户 {tenant_id:?} 配置无效：{err}"))?;
            match report.outcome {
                SeedOutcome::Seeded => summary.seeded += 1,
                SeedOutcome::Adopted => summary.adopted += 1,
                SeedOutcome::Kept => {}
            }
            if report.drift != SeedDrift::InSync {
                summary.stale.push((tenant_id.to_string(), report.drift));
            }
        } else {
            budget
                .configure_tenant(
                    tenant_id,
                    config.limit_minor,
                    &config.billing_timezone,
                    config.scope.into(),
                )
                .map_err(|err| format!("租户 {tenant_id:?} 配置无效：{err}"))?;
        }
    }
    if deploy_mode == DeployMode::Personal && !seen.contains(PERSONAL_TENANT_ID) {
        return Err(format!(
            "personal 模式必须配置固定内部租户 {PERSONAL_TENANT_ID:?}"
        ));
    }
    if startup.prunes_unlisted_tenants_on_startup() {
        let revoked = budget
            .retain_tenants(&seen)
            .map_err(|err| format!("无法对账可信租户清单：{err}"))?;
        if revoked > 0 {
            eprintln!("idoris: 已撤销 {revoked} 个不再存在于可信配置中的租户");
        }
    } else {
        report_seed_summary(&summary);
    }

    let migration = backfill_legacy_audits(&records, &event_log)?;
    if migration.retained_incompatible > 0 {
        eprintln!(
            "idoris: 已保留 {} 条无法迁移的旧审计记录；这些记录不会出现在 Event Log 审计查询中",
            migration.retained_incompatible
        );
    }

    Ok(StorageBootstrap {
        records: Arc::new(Mutex::new(records)),
        budget: Arc::new(budget),
        virtual_keys: Arc::new(Mutex::new(virtual_keys)),
        event_log: Arc::new(event_log),
        accounts,
    })
}

/// One line when this start wrote anything, and one warning per tenant whose
/// database row differs from (or is missing relative to) the file. The usual
/// restart, where nothing was seeded and everything agrees, prints nothing.
fn report_seed_summary(summary: &SeedSummary) {
    if summary.seeded > 0 || summary.adopted > 0 {
        eprintln!(
            "idoris: 租户初始化：从配置文件新写入 {} 个，沿用数据库里已有配置的 {} 个（之后以数据库为准，不再按配置文件删除租户）",
            summary.seeded, summary.adopted
        );
    }
    for (tenant_id, drift) in &summary.stale {
        match drift {
            SeedDrift::Changed => eprintln!(
                "idoris: 警告：租户 {tenant_id:?} 在数据库里的配置与配置文件不同，以数据库为准（配置文件只在第一次初始化时使用）"
            ),
            SeedDrift::MissingInDb => eprintln!(
                "idoris: 警告：租户 {tenant_id:?} 在配置文件里，但数据库里已没有它的配置；配置文件不会把它加回来"
            ),
            SeedDrift::InSync => {}
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct AuditMigrationReport {
    migrated: usize,
    retained_incompatible: usize,
}

fn backfill_legacy_audits(
    records: &TenantStore,
    event_log: &EventLogStore,
) -> Result<AuditMigrationReport, String> {
    let mut report = AuditMigrationReport::default();
    let tenants = records
        .audit_tenants_for_migration()
        .map_err(|error| format!("无法枚举旧审计租户：{error}"))?;
    for tenant in tenants {
        let compatible_tenant = crate::event_identifier::valid(&tenant);
        let mut after: Option<String> = None;
        loop {
            let batch = records
                .audit_migration_batch(Some(&tenant), after.as_deref(), 256)
                .map_err(|error| format!("无法读取租户 {tenant:?} 的旧审计记录：{error}"))?;
            if batch.is_empty() {
                break;
            }
            for record in &batch {
                // Older audit writes accepted identifiers that Event Log rejects.
                // Keep those rows in the legacy store without blocking startup.
                if !compatible_tenant
                    || !crate::event_identifier::valid(&record.record_id)
                    || record
                        .origin_record_id
                        .as_deref()
                        .is_some_and(|id| !crate::event_identifier::valid(id))
                {
                    report.retained_incompatible += 1;
                    continue;
                }
                let event = match migrated_audit_event(record) {
                    Ok(event) => event,
                    Err(_) => {
                        report.retained_incompatible += 1;
                        continue;
                    }
                };
                let existing = event_log
                    .events_for_record(Some(&tenant), &record.record_id)
                    .map_err(|error| {
                        format!(
                            "无法检查旧审计记录 {tenant:?}/{:?}：{error}",
                            record.record_id
                        )
                    })?;
                if existing
                    .iter()
                    .any(|event| event.event.event_type == EventType::AuditFinalized)
                {
                    continue;
                }
                match event_log.append(Some(&tenant), &event) {
                    Ok(_) => report.migrated += 1,
                    Err(
                        EventLogError::InvalidIdentifier(_)
                        | EventLogError::InvalidRecordId
                        | EventLogError::InvalidTimestamp
                        | EventLogError::InvalidMetadata(_),
                    ) => report.retained_incompatible += 1,
                    Err(error) => {
                        return Err(format!(
                            "无法迁移旧审计记录 {tenant:?}/{:?}：{error}",
                            record.record_id
                        ));
                    }
                }
            }
            after = batch.last().map(|record| record.record_id.clone());
        }
    }
    Ok(report)
}

fn migrated_audit_event(record: &TenantRecord) -> Result<NewEvent, String> {
    let label = format!("{:?}/{:?}", record.tenant_id, record.record_id);
    let timestamp = record
        .payload
        .get("ts_utc")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0 && *value < i64::MAX as f64)
        .ok_or_else(|| format!("旧审计记录 {label} 的 ts_utc 无法迁移"))?
        .floor() as i64;
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("component".into(), Value::String("router".into()));
    for (source, target) in [
        ("component", "component"),
        ("intent", "intent"),
        ("privacy", "privacy"),
        ("tier", "tier"),
        ("provider_id", "provider_id"),
        ("model_id", "model_id"),
        ("tokens_in", "tokens_in"),
        ("tokens_out", "tokens_out"),
        ("cost_minor", "cost_minor"),
        ("latency_ms", "latency_ms"),
        ("status", "http_status"),
        ("reason", "reason"),
    ] {
        if let Some(value) = record.payload.get(source)
            && value.as_str().is_none_or(|text| {
                text.encode_utf16().count() <= 500 && !text.chars().any(char::is_control)
            })
        {
            metadata.insert(target.into(), value.clone());
        }
    }
    let request_id = if crate::event_identifier::valid(&record.request_id) {
        &record.request_id
    } else {
        &record.record_id
    };
    Ok(NewEvent {
        event_id: Uuid::new_v4().to_string(),
        tenant_id: record.tenant_id.clone(),
        record_id: record.record_id.clone(),
        event_type: EventType::AuditFinalized,
        ts_utc_ms: timestamp,
        request_id: Some(request_id.clone()),
        session_id: None,
        trace_id: None,
        parent_id: None,
        origin_record_id: record.origin_record_id.clone(),
        metadata,
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
    use serde_json::{Map, json};
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[cfg(unix)]
    #[test]
    fn dangling_symlinked_db_keeps_its_lock_after_the_first_start() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("data").join("real.db");
        std::fs::create_dir(real.parent().unwrap()).unwrap();
        let alias = tmp.path().join("alias.db");
        std::os::unix::fs::symlink("data/real.db", &alias).unwrap();
        let before = canonical_lock_base(&alias);
        std::fs::write(&real, "").unwrap();
        let after = canonical_lock_base(&alias);
        assert_eq!(before, after);
        assert_eq!(after, std::fs::canonicalize(&real).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_falls_back_without_hanging() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.db");
        let b = tmp.path().join("b.db");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        let base = canonical_lock_base(&a);
        assert_eq!(
            base.parent(),
            Some(std::fs::canonicalize(tmp.path()).unwrap().as_path())
        );
    }

    fn virtual_key_scope() -> VirtualKeyScope {
        VirtualKeyScope {
            owner: "agent24-instance".into(),
            tenant_id: None,
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
                        payload: Map::from_iter([
                            ("ts_utc".into(), json!(1)),
                            ("reason".into(), json!("intent_match")),
                        ]),
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
    fn startup_backfills_legacy_audit_history_once_for_each_tenant() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("state.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(
            &config,
            "tenants:\n  - tenant_id: acme\n    limit_minor: 0\n    billing_timezone: UTC\n",
        )
        .unwrap();
        let records = TenantStore::open(&db).unwrap();
        for (tenant, record_id, ts) in [("acme", "old-a", 1000), ("beta", "old-b", 2000)] {
            records
                .put(
                    Some(tenant),
                    &TenantRecord {
                        tenant_id: tenant.into(),
                        kind: RecordKind::Audit,
                        record_id: record_id.into(),
                        request_id: format!("request-{record_id}"),
                        origin_record_id: None,
                        payload: Map::from_iter([
                            ("ts_utc".into(), json!(ts)),
                            ("reason".into(), json!("intent_match")),
                            ("status".into(), json!(200)),
                            ("cost_minor".into(), json!(3)),
                            ("intent".into(), json!("left\tright")),
                        ]),
                    },
                )
                .unwrap();
        }
        drop(records);
        let old_event_log = EventLogStore::open(&db).unwrap();
        old_event_log
            .append(
                Some("acme"),
                &NewEvent {
                    event_id: Uuid::new_v4().to_string(),
                    tenant_id: "acme".into(),
                    record_id: "old-a".into(),
                    event_type: EventType::RequestReceived,
                    ts_utc_ms: 900,
                    request_id: None,
                    session_id: None,
                    trace_id: None,
                    parent_id: None,
                    origin_record_id: None,
                    metadata: BTreeMap::new(),
                },
            )
            .unwrap();
        drop(old_event_log);

        for _ in 0..2 {
            let storage = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
            for (tenant, record_id, ts) in [("acme", "old-a", 1000), ("beta", "old-b", 2000)] {
                let events = storage
                    .event_log
                    .events_for_record(Some(tenant), record_id)
                    .unwrap();
                let expected_len = if record_id == "old-a" { 2 } else { 1 };
                assert_eq!(events.len(), expected_len, "migration must be idempotent");
                assert_eq!(
                    events.last().unwrap().event.event_type,
                    EventType::AuditFinalized
                );
                let audit = crate::queries::audit::query_event_audit(
                    &storage.event_log,
                    tenant,
                    Some(tenant),
                    &crate::queries::audit::AuditQuery {
                        from: Some(ts),
                        to: Some(ts + 1),
                        limit: Some(1),
                        record_id: Some(record_id.into()),
                    },
                )
                .unwrap();
                assert_eq!(audit.records.len(), 1);
                assert_eq!(audit.records[0].request_id, format!("request-{record_id}"));
                assert_eq!(
                    audit.records[0].payload["request_id"],
                    json!(format!("request-{record_id}"))
                );
                assert_eq!(audit.records[0].payload["status"], json!(200));
                assert_eq!(audit.records[0].payload["cost_minor"], json!(3));
                assert!(!audit.records[0].payload.contains_key("intent"));
            }
        }
    }

    #[test]
    fn startup_retains_incompatible_legacy_audits_and_migrates_valid_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("state.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(
            &config,
            "tenants:\n  - tenant_id: acme\n    limit_minor: 0\n    billing_timezone: UTC\n",
        )
        .unwrap();
        let long_id = "x".repeat(129);
        let records = TenantStore::open(&db).unwrap();
        let add = |tenant: &str, record_id: &str, origin: Option<&str>, ts: i64| {
            records
                .put(
                    Some(tenant),
                    &TenantRecord {
                        tenant_id: tenant.into(),
                        kind: RecordKind::Audit,
                        record_id: record_id.into(),
                        request_id: "legacy-request".into(),
                        origin_record_id: origin.map(str::to_owned),
                        payload: Map::from_iter([
                            ("ts_utc".into(), json!(ts)),
                            ("status".into(), json!(400)),
                        ]),
                    },
                )
                .unwrap();
        };
        // The old audit store accepted all of these identifiers and values.
        add(&long_id, "bad-tenant", None, 1);
        add("acme", &long_id, None, 2);
        add("acme", "bad-origin", Some(&long_id), 3);
        add("acme", "bad-timestamp", None, -1);
        add("acme", "good", None, 4);
        drop(records);

        let storage = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
        let legacy = storage.records.lock().unwrap();
        for (tenant, record_id) in [
            (long_id.as_str(), "bad-tenant"),
            ("acme", long_id.as_str()),
            ("acme", "bad-origin"),
            ("acme", "bad-timestamp"),
        ] {
            assert!(
                legacy
                    .get(Some(tenant), RecordKind::Audit, record_id)
                    .unwrap()
                    .is_some(),
                "incompatible legacy audit must be retained"
            );
        }
        assert_eq!(
            storage
                .event_log
                .events_for_record(Some("acme"), "good")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            backfill_legacy_audits(&legacy, &storage.event_log).unwrap(),
            AuditMigrationReport {
                migrated: 0,
                retained_incompatible: 4,
            }
        );
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

    // ------------------------------------------------------------------
    // S1: deployment form (`IDORIS_FORM`) and database-authoritative tenants.
    // Every test below has a control that runs the same sequence through the
    // legacy path, so a pass proves the *form switch* made the difference.
    // ------------------------------------------------------------------

    use crate::startup_form::StartupForm;
    use idoris_compute_contracts::form::DeploymentForm;

    fn tenants_yaml(entries: &[(&str, i64)]) -> String {
        let mut out = String::from("tenants:\n");
        for (tenant, limit) in entries {
            out.push_str(&format!(
                "  - tenant_id: {tenant}\n    limit_minor: {limit}\n    billing_timezone: UTC\n    scope: all\n"
            ));
        }
        out
    }

    fn form(deploy_mode: DeployMode, form: DeploymentForm) -> StartupForm {
        StartupForm::declared(deploy_mode, form).unwrap()
    }

    fn settle_some_spend(storage: &StorageBootstrap, tenant: &str, key: &str, spent: i64) {
        let scope = BudgetScope::new(tenant, key, "provider", "model");
        let id = storage.budget.reserve(&scope, Price::Known(spent)).unwrap();
        storage.budget.settle(tenant, &id, spent).unwrap();
    }

    #[test]
    fn organization_and_cloud_keep_a_tenant_removed_from_the_file_together_with_its_data() {
        for target in [DeploymentForm::Organization, DeploymentForm::Cloud] {
            let dir = tempfile::TempDir::new().unwrap();
            let db = dir.path().join("tenant.sqlite3");
            let config = dir.path().join("tenants.yaml");
            std::fs::write(&config, tenants_yaml(&[("acme", 1_000), ("beta", 1_000)])).unwrap();
            let startup = form(DeployMode::Tenant, target);

            let first = bootstrap_with_form(&db, Some(&config), startup).unwrap();
            settle_some_spend(&first, "beta", "vk_beta", 250);
            drop(first);

            // The file no longer lists beta; restart twice.
            std::fs::write(&config, tenants_yaml(&[("acme", 1_000)])).unwrap();
            for round in 0..2 {
                let reopened = bootstrap_with_form(&db, Some(&config), startup).unwrap();
                let view = reopened
                    .budget
                    .tenant_readview("beta")
                    .unwrap_or_else(|err| {
                        panic!("{target:?} round {round}: beta must survive a restart: {err}")
                    });
                assert_eq!((view.limit_minor, view.spent_minor), (1_000, 250));
                // beta can still spend; the data and the configuration are both still there.
                let scope = BudgetScope::new("beta", "vk_beta", "provider", "model");
                reopened.budget.reserve(&scope, Price::Known(1)).unwrap();
                assert!(reopened.budget.tenant_readview("acme").is_ok());
            }
        }
    }

    #[test]
    fn the_same_sequence_through_the_legacy_path_still_revokes_the_removed_tenant() {
        // Control for the test above: only the form switch differs.
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("tenant.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(&config, tenants_yaml(&[("acme", 1_000), ("beta", 1_000)])).unwrap();
        let first = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
        settle_some_spend(&first, "beta", "vk_beta", 250);
        drop(first);
        std::fs::write(&config, tenants_yaml(&[("acme", 1_000)])).unwrap();
        let reopened = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
        assert!(matches!(
            reopened.budget.tenant_readview("beta"),
            Err(BudgetError::TenantNotConfigured { .. })
        ));
        // `StartupForm::legacy` is the same thing spelled out.
        drop(reopened);
        let legacy =
            bootstrap_with_form(&db, Some(&config), StartupForm::legacy(DeployMode::Tenant))
                .unwrap();
        assert!(legacy.budget.tenant_readview("beta").is_err());
    }

    #[test]
    fn an_explicit_personal_form_still_prunes_like_the_default_personal_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("personal.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(
            &config,
            tenants_yaml(&[(PERSONAL_TENANT_ID, 1_000), ("stray", 1)]),
        )
        .unwrap();
        let startup = form(DeployMode::Personal, DeploymentForm::Personal);
        drop(bootstrap_with_form(&db, Some(&config), startup).unwrap());
        std::fs::write(&config, tenants_yaml(&[(PERSONAL_TENANT_ID, 1_000)])).unwrap();
        let reopened = bootstrap_with_form(&db, Some(&config), startup).unwrap();
        assert!(matches!(
            reopened.budget.tenant_readview("stray"),
            Err(BudgetError::TenantNotConfigured { .. })
        ));
        assert!(reopened.accounts.is_none());
    }

    #[test]
    fn the_database_is_the_authority_after_the_first_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("tenant.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(&config, tenants_yaml(&[("acme", 100)])).unwrap();
        let startup = form(DeployMode::Tenant, DeploymentForm::Organization);
        let first = bootstrap_with_form(&db, Some(&config), startup).unwrap();
        // An administrator raises the limit in the database.
        first
            .budget
            .configure_tenant("acme", 999, "UTC", SpendGate::All)
            .unwrap();
        drop(first);
        // The file still says 100; the restart must not take it back.
        let reopened = bootstrap_with_form(&db, Some(&config), startup).unwrap();
        assert_eq!(
            reopened.budget.tenant_readview("acme").unwrap().limit_minor,
            999
        );
        drop(reopened);
        // Control: the legacy path lets the file win on every start.
        let legacy = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
        assert_eq!(
            legacy.budget.tenant_readview("acme").unwrap().limit_minor,
            100
        );
    }

    #[test]
    fn a_tenant_deleted_from_the_database_is_not_brought_back_by_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("tenant.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(&config, tenants_yaml(&[("acme", 100), ("beta", 100)])).unwrap();
        let startup = form(DeployMode::Tenant, DeploymentForm::Cloud);
        drop(bootstrap_with_form(&db, Some(&config), startup).unwrap());
        Connection::open(&db)
            .unwrap()
            .execute("DELETE FROM tenant_config WHERE tenant_id = 'beta'", [])
            .unwrap();
        let reopened = bootstrap_with_form(&db, Some(&config), startup).unwrap();
        assert!(matches!(
            reopened.budget.tenant_readview("beta"),
            Err(BudgetError::TenantNotConfigured { .. })
        ));
        assert!(reopened.budget.tenant_readview("acme").is_ok());
    }

    #[test]
    fn switching_an_existing_tenant_database_to_the_organization_form_adopts_it_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("tenant.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(&config, tenants_yaml(&[("acme", 100), ("beta", 100)])).unwrap();
        // Written by the pre-S1 start-up (no IDORIS_FORM).
        let legacy = bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
        settle_some_spend(&legacy, "acme", "default", 40);
        drop(legacy);
        // Now the operator declares the organization form; the file disagrees about acme and
        // no longer lists beta.
        std::fs::write(&config, tenants_yaml(&[("acme", 555), ("gamma", 77)])).unwrap();
        let startup = form(DeployMode::Tenant, DeploymentForm::Organization);
        let adopted = bootstrap_with_form(&db, Some(&config), startup).unwrap();
        let acme = adopted.budget.tenant_readview("acme").unwrap();
        assert_eq!(
            (acme.limit_minor, acme.spent_minor),
            (100, 40),
            "the database row and its history stay"
        );
        assert!(
            adopted.budget.tenant_readview("beta").is_ok(),
            "beta is not pruned"
        );
        assert_eq!(
            adopted.budget.tenant_readview("gamma").unwrap().limit_minor,
            77
        );
    }

    #[test]
    fn organization_start_uses_standard_connections_and_opens_the_account_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("tenant.sqlite3");
        let config = dir.path().join("tenants.yaml");
        std::fs::write(&config, tenants_yaml(&[("acme", 100)])).unwrap();
        let startup = form(DeployMode::Tenant, DeploymentForm::Organization);
        let opened = bootstrap_with_form(&db, Some(&config), startup).unwrap();
        let standard = idoris_tenancy::sqlite_conn::PragmaSnapshot {
            journal_mode: "wal".into(),
            busy_timeout_ms: 5_000,
            foreign_keys: true,
        };
        assert_eq!(
            opened.records.lock().unwrap().connection_pragmas().unwrap(),
            standard
        );
        assert_eq!(opened.budget.connection_pragmas().unwrap(), standard);
        assert_eq!(
            opened
                .virtual_keys
                .lock()
                .unwrap()
                .connection_pragmas()
                .unwrap(),
            standard
        );
        assert_eq!(opened.event_log.connection_pragmas().unwrap(), standard);
        // The organization form opens the account store.
        let accounts = opened.accounts.as_ref().unwrap();
        assert_eq!(accounts.connection_pragmas().unwrap(), standard);
        let tables: i64 = Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('accounts','principals','memberships','credentials','allocations')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 5);
    }

    #[test]
    fn legacy_starts_never_open_the_account_store_or_create_its_tables() {
        for (deploy_mode, with_config) in
            [(DeployMode::Personal, false), (DeployMode::Tenant, true)]
        {
            let dir = tempfile::TempDir::new().unwrap();
            let db = dir.path().join("legacy.sqlite3");
            let config = dir.path().join("tenants.yaml");
            std::fs::write(&config, tenants_yaml(&[("acme", 100)])).unwrap();
            let opened =
                bootstrap(&db, with_config.then_some(config.as_path()), deploy_mode).unwrap();
            assert!(opened.accounts.is_none(), "{deploy_mode:?}");
            let conn = Connection::open(&db).unwrap();
            let account_tables: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name IN \
                     ('accounts','principals','memberships','credentials','credential_admin_scopes','allocations','account_schema_migrations')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                account_tables, 0,
                "{deploy_mode:?}: no compute-service tables in a legacy database"
            );
            // The additive ledger migration exists but the legacy path never writes to it.
            let markers: i64 = conn
                .query_row("SELECT count(*) FROM tenant_bootstrap", [], |r| r.get(0))
                .unwrap();
            assert_eq!(markers, 0, "{deploy_mode:?}");
            // And it keeps exactly the pre-S1 connection settings: WAL and the 5 s busy timeout.
            let ledger = opened.budget.connection_pragmas().unwrap();
            assert_eq!(
                (ledger.journal_mode.as_str(), ledger.busy_timeout_ms),
                ("wal", 5_000)
            );
        }
    }

    #[test]
    fn a_broken_tenant_file_stops_the_organization_form_too() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("tenant.sqlite3");
        let config = dir.path().join("tenants.yaml");
        let startup = form(DeployMode::Tenant, DeploymentForm::Organization);
        // missing file
        let err = bootstrap_with_form(&db, Some(&config), startup)
            .err()
            .unwrap();
        assert!(err.contains("无法读取租户配置"), "{err}");
        // typo in a time zone: fails even for a tenant that is already seeded
        std::fs::write(&config, tenants_yaml(&[("acme", 100)])).unwrap();
        drop(bootstrap_with_form(&db, Some(&config), startup).unwrap());
        std::fs::write(
            &config,
            "tenants:\n  - tenant_id: acme\n    limit_minor: 100\n    billing_timezone: Not/AZone\n",
        )
        .unwrap();
        let err = bootstrap_with_form(&db, Some(&config), startup)
            .err()
            .unwrap();
        assert!(err.contains("配置无效"), "{err}");
        // duplicates are rejected as before
        std::fs::write(&config, tenants_yaml(&[("acme", 1), ("acme", 2)])).unwrap();
        let err = bootstrap_with_form(&db, Some(&config), startup)
            .err()
            .unwrap();
        assert!(err.contains("租户配置重复"), "{err}");
    }
}

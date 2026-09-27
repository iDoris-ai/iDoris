//! SQLite-backed budget ledger. See `budget/mod.rs`/`README.md` for how
//! this relates to `packages/tenancy/src/budget.ts`. Storage plumbing
//! (`open`, migrations, `configure`, `balance`) lands here first; atomic
//! `reserve`/`settle`/`release` follow in later changes.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::clock::{Clock, SystemClock};
use super::error::BudgetError;
use super::period::billing_period_key;
use super::scope::BudgetScope;

const SCHEMA_MIGRATIONS: &[&str] = &[include_str!("migrations/0001_init.sql")];

/// SQLite-backed budget ledger: atomic reserve/settle/release scoped to
/// `(tenant, key, provider, model)`, bucketed per billing period.
///
/// Concurrency: every write happens inside one `BEGIN IMMEDIATE` transaction,
/// so "check balance" and "deduct reservation" stay atomic across separate
/// connections/processes sharing the same file — WAL mode plus a 5s
/// `busy_timeout` (see [`open_with`]) make writers wait instead of failing.
/// This is the LiteLLM #32614 bug class: non-atomic check-then-deduct lets N
/// concurrent requests all pass the check before any commits, over-spending.
pub struct BudgetLedger {
    conn: Mutex<Connection>,
    clock: Arc<dyn Clock>,
}

/// Per-scope configuration: the period limit and the explicit IANA zone its
/// periods are resolved in.
struct ScopeConfig {
    limit_minor: i64,
    billing_timezone: String,
}

impl BudgetLedger {
    /// Open (creating if needed) a ledger at `path`, with the real system
    /// clock. Trust boundary: `path` is trusted verbatim — source it from
    /// trusted config, never tenant/request input.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BudgetError> {
        Self::open_with(path, Arc::new(SystemClock))
    }

    /// Full control for tests: inject a [`Clock`] so tests don't sleep.
    pub fn open_with(path: impl AsRef<Path>, clock: Arc<dyn Clock>) -> Result<Self, BudgetError> {
        let mut conn = Connection::open(path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        run_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// A poisoned mutex only happens if a previous call panicked mid-method.
    /// Safe for the *database*: every write is a single statement or an
    /// explicit transaction, either commits or drops un-committed on
    /// unwind — never torn. Not a full guarantee for the panicking *caller*:
    /// a panic between `commit()` and returning (nothing here triggers that
    /// today, but a future change might) leaves that caller unsure it
    /// succeeded — not corruption, at worst one orphan reservation that
    /// self-heals via its TTL, but a real ambiguity.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set (or update) the per-scope period limit and billing time zone.
    /// Takes effect for the *current* and future periods; already-settled
    /// spend in past periods is untouched.
    pub fn configure(
        &self,
        scope: &BudgetScope,
        limit_minor: i64,
        billing_timezone: &str,
    ) -> Result<(), BudgetError> {
        validate_scope(scope)?;
        if limit_minor < 0 {
            return Err(BudgetError::InvalidLimit { limit_minor });
        }
        if !idoris_contracts::tenant::is_iana_time_zone(billing_timezone) {
            return Err(BudgetError::InvalidTimeZone {
                billing_timezone: billing_timezone.to_string(),
            });
        }
        let conn = self.lock();
        conn.execute(
            "INSERT INTO budget_config \
                (tenant_id, key_id, provider_id, model_id, limit_minor, billing_timezone) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (tenant_id, key_id, provider_id, model_id) \
             DO UPDATE SET limit_minor = excluded.limit_minor, \
                           billing_timezone = excluded.billing_timezone",
            rusqlite::params![
                scope.tenant_id,
                scope.key_id,
                scope.provider_id,
                scope.model_id,
                limit_minor,
                billing_timezone,
            ],
        )?;
        Ok(())
    }

    /// Read-only remaining balance for `scope`: `limit - settled_spend -
    /// active_reservations`. All three reads share one `BEGIN DEFERRED`
    /// transaction for a consistent snapshot — otherwise a racing `settle()`
    /// could move an amount from "reserved" to "spent" between the two
    /// reads, silently overstating the balance. Still not a decision-time
    /// lock like `reserve` takes, so the result can be stale the instant
    /// it's returned — for observability/tests, not for deciding whether a
    /// spend can proceed.
    pub fn balance(&self, scope: &BudgetScope) -> Result<i64, BudgetError> {
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let config = load_config(&tx, scope)?.ok_or_else(|| BudgetError::NotConfigured {
            scope: scope.clone(),
        })?;
        let period = billing_period_key(now_ms, &config.billing_timezone)?;
        let spent = spent_for(&tx, scope, &period)?;
        let reserved = active_reserved_for(&tx, scope, &period, now_ms)?;
        // Pure read; roll back explicitly rather than relying on drop.
        tx.rollback()?;
        Ok(config.limit_minor - spent - reserved)
    }
}

fn run_migrations(conn: &mut Connection) -> Result<(), BudgetError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\
            version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL)",
    )?;
    for (idx, sql) in SCHEMA_MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        let already: Option<i64> = tx
            .query_row(
                "SELECT version FROM schema_migrations WHERE version = ?1",
                [version],
                |row| row.get(0),
            )
            .optional()?;
        if already.is_some() {
            continue;
        }
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT OR IGNORE INTO schema_migrations (version, applied_at_ms) VALUES (?1, ?2)",
            rusqlite::params![version, SystemClock.now_ms()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Reject a blank scope field: it would still satisfy the SQL composite-key
/// columns, silently merging unrelated calls onto one budget row. Only
/// guards the "empty string" case; case/whitespace normalization is left to
/// the caller.
fn validate_scope(scope: &BudgetScope) -> Result<(), BudgetError> {
    let fields: [(&'static str, &str); 4] = [
        ("tenant_id", &scope.tenant_id),
        ("key_id", &scope.key_id),
        ("provider_id", &scope.provider_id),
        ("model_id", &scope.model_id),
    ];
    for (field, value) in fields {
        if value.trim().is_empty() {
            return Err(BudgetError::InvalidScope { field });
        }
    }
    Ok(())
}

fn load_config(conn: &Connection, scope: &BudgetScope) -> Result<Option<ScopeConfig>, BudgetError> {
    conn.query_row(
        "SELECT limit_minor, billing_timezone FROM budget_config \
         WHERE tenant_id = ?1 AND key_id = ?2 AND provider_id = ?3 AND model_id = ?4",
        rusqlite::params![
            scope.tenant_id,
            scope.key_id,
            scope.provider_id,
            scope.model_id
        ],
        |row| {
            Ok(ScopeConfig {
                limit_minor: row.get(0)?,
                billing_timezone: row.get(1)?,
            })
        },
    )
    .optional()
    .map_err(BudgetError::from)
}

fn spent_for(conn: &Connection, scope: &BudgetScope, period: &str) -> Result<i64, BudgetError> {
    let spent: Option<i64> = conn
        .query_row(
            "SELECT spent_minor FROM budget_periods \
             WHERE tenant_id=?1 AND key_id=?2 AND provider_id=?3 AND model_id=?4 AND period=?5",
            rusqlite::params![
                scope.tenant_id,
                scope.key_id,
                scope.provider_id,
                scope.model_id,
                period
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(spent.unwrap_or(0))
}

fn active_reserved_for(
    conn: &Connection,
    scope: &BudgetScope,
    period: &str,
    now_ms: i64,
) -> Result<i64, BudgetError> {
    conn.query_row(
        "SELECT COALESCE(SUM(reserved_minor), 0) FROM reservations \
         WHERE tenant_id=?1 AND key_id=?2 AND provider_id=?3 AND model_id=?4 AND period=?5 \
           AND status='active' AND expires_at_ms > ?6",
        rusqlite::params![
            scope.tenant_id,
            scope.key_id,
            scope.provider_id,
            scope.model_id,
            period,
            now_ms
        ],
        |row| row.get(0),
    )
    .map_err(BudgetError::from)
}

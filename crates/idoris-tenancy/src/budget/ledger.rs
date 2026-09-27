//! SQLite-backed budget ledger. See `budget/mod.rs`/`README.md` for how
//! this relates to `packages/tenancy/src/budget.ts`.
//!
//! `reserve` only ever answers "can this scope still spend `estimated_cost`
//! minor units in the current period" — it does not decide whether the call
//! itself may proceed (privacy/intent/admission are the router's job, ahead
//! of this in the chain per 总体规划 §2 不变式 #1). Conversely, being
//! allowed to call never implies being charged: LoopX's lesson
//! (总体规划 §4.6) is to keep "may I call" (`reserve`) and "charge me"
//! (`settle`) independently callable, independently testable operations —
//! which is why they're two methods here, not one.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use uuid::Uuid;

use super::clock::{Clock, SystemClock};
use super::error::BudgetError;
use super::period::billing_period_key;
use super::scope::BudgetScope;

const SCHEMA_MIGRATIONS: &[&str] = &[include_str!("migrations/0001_init.sql")];

/// Default reservation TTL: long enough to cover a slow upstream call,
/// short enough that an abandoned reservation (a caller that crashes before
/// `settle`/`release`) doesn't lock up budget for long. Override via
/// [`BudgetLedger::open_with`] — e.g. a short TTL in TTL-expiry tests.
pub const DEFAULT_RESERVATION_TTL_MS: i64 = 5 * 60 * 1000;

/// A caller-supplied cost estimate, or `Unknown`. `reserve` refuses
/// `Unknown` outright (总体规划 §4.6 / 不变式 #3: "价格未知 ≠ 免费") instead
/// of silently treating it as `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Price {
    Known(i64),
    Unknown,
}

/// Opaque reservation handle returned by [`BudgetLedger::reserve`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReservationId(pub String);

impl std::fmt::Display for ReservationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Result of [`BudgetLedger::settle`]: what was reserved vs. actually
/// charged, and how much of the reservation was refunded back to the
/// scope's balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleReceipt {
    pub reserved_minor: i64,
    pub actual_cost_minor: i64,
    /// `reserved_minor - actual_cost_minor`, clamped to `>= 0` — settling
    /// for *more* than was reserved still charges the real amount (billing
    /// truth, not a cap), it just refunds nothing.
    pub refunded_minor: i64,
}

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
    ttl_ms: i64,
}

/// Per-scope configuration: the period limit and the explicit IANA zone its
/// periods are resolved in.
struct ScopeConfig {
    limit_minor: i64,
    billing_timezone: String,
}

impl BudgetLedger {
    /// Open (creating if needed) a ledger at `path`, with the real system
    /// clock and [`DEFAULT_RESERVATION_TTL_MS`]. Trust boundary: `path` is
    /// trusted verbatim — source it from trusted config, never
    /// tenant/request input.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BudgetError> {
        Self::open_with(path, Arc::new(SystemClock), DEFAULT_RESERVATION_TTL_MS)
    }

    /// Full control for tests: inject a [`Clock`] (so TTL/billing-period-
    /// boundary tests don't need to sleep real wall-clock time) and a
    /// reservation TTL. `ttl_ms <= 0` is rejected: a reservation already
    /// expired at creation would never count against the balance.
    pub fn open_with(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        ttl_ms: i64,
    ) -> Result<Self, BudgetError> {
        if ttl_ms <= 0 {
            return Err(BudgetError::InvalidTtl { ttl_ms });
        }
        let mut conn = Connection::open(path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        run_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
            ttl_ms,
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

    /// Atomically check-and-deduct: inside one `BEGIN IMMEDIATE` transaction,
    /// compute the scope's remaining balance for the current period and, if
    /// `estimated_cost` fits, insert an `active` reservation holding that
    /// amount until it's settled, released, or its TTL expires. Two
    /// concurrent callers can never both succeed past the same last unit of
    /// budget, because the second one's `BEGIN IMMEDIATE` blocks (up to the
    /// `busy_timeout`) until the first commits or rolls back.
    pub fn reserve(
        &self,
        scope: &BudgetScope,
        estimated_cost: Price,
    ) -> Result<ReservationId, BudgetError> {
        let estimated_cost_minor = match estimated_cost {
            Price::Unknown => return Err(BudgetError::PriceUnknown),
            Price::Known(v) if v < 0 => {
                return Err(BudgetError::InvalidEstimate {
                    estimated_cost_minor: v,
                });
            }
            Price::Known(v) => v,
        };

        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let config = load_config(&tx, scope)?.ok_or_else(|| BudgetError::NotConfigured {
            scope: scope.clone(),
        })?;
        let period = billing_period_key(now_ms, &config.billing_timezone)?;

        // Expired reservations already stop counting toward the balance sum
        // below (it filters `expires_at_ms > now_ms`); flipping their status
        // here too means a later `settle`/`release` on the same id gets a
        // clear `ReservationNotActive` instead of silently succeeding.
        sweep_expired_scope(&tx, scope, &period, now_ms)?;

        let spent = spent_for(&tx, scope, &period)?;
        let reserved = active_reserved_for(&tx, scope, &period, now_ms)?;
        let balance_minor = config.limit_minor - spent - reserved;

        if estimated_cost_minor > balance_minor {
            tx.commit()?; // nothing written yet, but keep the sweep above.
            return Err(BudgetError::exceeded(balance_minor, estimated_cost_minor));
        }

        // Overflow would wrap to a past instant (release) and make the
        // reservation invisible to the balance sum — reject instead.
        let expires_at_ms = now_ms
            .checked_add(self.ttl_ms)
            .ok_or(BudgetError::InvalidTimestamp { now_ms })?;
        let id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO reservations \
                (id, tenant_id, key_id, provider_id, model_id, period, reserved_minor, \
                 status, created_at_ms, expires_at_ms, actual_cost_minor) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?9, NULL)",
            rusqlite::params![
                id,
                scope.tenant_id,
                scope.key_id,
                scope.provider_id,
                scope.model_id,
                period,
                estimated_cost_minor,
                now_ms,
                expires_at_ms,
            ],
        )?;
        tx.commit()?;
        Ok(ReservationId(id))
    }

    /// Sweep every scope/period for expired-but-still-`active` reservations
    /// and flip them to `expired`. `reserve` already excludes expired rows
    /// from its balance check via the `expires_at_ms` filter, so nothing
    /// over-reserves even if this is never called; it exists for hygiene
    /// (bounded table growth) and so tests can assert TTL rows actually
    /// transition rather than merely becoming invisible to the sum.
    pub fn sweep_expired(&self) -> Result<usize, BudgetError> {
        let now_ms = self.clock.now_ms();
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE reservations SET status='expired' WHERE status='active' AND expires_at_ms <= ?1",
            [now_ms],
        )?;
        Ok(n)
    }
}

fn sweep_expired_scope(
    conn: &Connection,
    scope: &BudgetScope,
    period: &str,
    now_ms: i64,
) -> Result<usize, BudgetError> {
    let n = conn.execute(
        "UPDATE reservations SET status='expired' \
         WHERE tenant_id=?1 AND key_id=?2 AND provider_id=?3 AND model_id=?4 AND period=?5 \
           AND status='active' AND expires_at_ms <= ?6",
        rusqlite::params![
            scope.tenant_id,
            scope.key_id,
            scope.provider_id,
            scope.model_id,
            period,
            now_ms
        ],
    )?;
    Ok(n)
}

impl BudgetLedger {
    /// Finalize a reservation with the *actual* cost: adds `actual_cost_minor`
    /// to the scope's settled spend for the reservation's period and marks it
    /// `settled`. Only call this for calls that actually completed — a
    /// failed or fallback call must call [`release`](Self::release) instead
    /// (LoopX's lesson: "may I call" and "charge me" are separate, so a
    /// caller can never accidentally charge for a call it didn't make).
    pub fn settle(
        &self,
        reservation_id: &ReservationId,
        actual_cost_minor: i64,
    ) -> Result<SettleReceipt, BudgetError> {
        if actual_cost_minor < 0 {
            return Err(BudgetError::InvalidActualCost { actual_cost_minor });
        }
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let row = find_reservation(&tx, reservation_id)?;

        if row.status != "active" {
            return Err(BudgetError::ReservationNotActive {
                reservation_id: reservation_id.0.clone(),
                status: row.status,
            });
        }
        if row.expires_at_ms <= now_ms {
            tx.execute(
                "UPDATE reservations SET status='expired' WHERE id=?1",
                [&reservation_id.0],
            )?;
            tx.commit()?;
            return Err(BudgetError::ReservationNotActive {
                reservation_id: reservation_id.0.clone(),
                status: "expired".to_string(),
            });
        }

        tx.execute(
            "INSERT INTO budget_periods (tenant_id, key_id, provider_id, model_id, period, spent_minor) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (tenant_id, key_id, provider_id, model_id, period) \
             DO UPDATE SET spent_minor = spent_minor + excluded.spent_minor",
            rusqlite::params![
                row.tenant_id,
                row.key_id,
                row.provider_id,
                row.model_id,
                row.period,
                actual_cost_minor
            ],
        )?;
        tx.execute(
            "UPDATE reservations SET status='settled', actual_cost_minor=?2 WHERE id=?1",
            rusqlite::params![reservation_id.0, actual_cost_minor],
        )?;
        tx.commit()?;

        let refunded_minor = (row.reserved_minor - actual_cost_minor).max(0);
        Ok(SettleReceipt {
            reserved_minor: row.reserved_minor,
            actual_cost_minor,
            refunded_minor,
        })
    }

    /// Fully release a reservation without charging anything — for calls
    /// that failed or that fell back to a different (separately reserved)
    /// candidate. Idempotent when the reservation is already `released`;
    /// erroring on `settled`/`expired` prevents un-settling a completed
    /// charge.
    pub fn release(&self, reservation_id: &ReservationId) -> Result<(), BudgetError> {
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let row = find_reservation(&tx, reservation_id)?;

        match row.status.as_str() {
            "released" => {
                tx.commit()?;
                Ok(())
            }
            "active" if row.expires_at_ms > now_ms => {
                tx.execute(
                    "UPDATE reservations SET status='released' WHERE id=?1",
                    [&reservation_id.0],
                )?;
                tx.commit()?;
                Ok(())
            }
            "active" => {
                tx.execute(
                    "UPDATE reservations SET status='expired' WHERE id=?1",
                    [&reservation_id.0],
                )?;
                tx.commit()?;
                Err(BudgetError::ReservationNotActive {
                    reservation_id: reservation_id.0.clone(),
                    status: "expired".to_string(),
                })
            }
            other => Err(BudgetError::ReservationNotActive {
                reservation_id: reservation_id.0.clone(),
                status: other.to_string(),
            }),
        }
    }
}

/// A reservation row looked up by id — named fields instead of a tuple so a
/// column reorder in the `SELECT` can't silently swap two same-typed values
/// (e.g. `tenant_id`/`key_id`) without the compiler noticing (Codex review).
struct ReservationRow {
    tenant_id: String,
    key_id: String,
    provider_id: String,
    model_id: String,
    period: String,
    reserved_minor: i64,
    status: String,
    expires_at_ms: i64,
}

fn find_reservation(
    conn: &Connection,
    reservation_id: &ReservationId,
) -> Result<ReservationRow, BudgetError> {
    conn.query_row(
        "SELECT tenant_id, key_id, provider_id, model_id, period, reserved_minor, status, expires_at_ms \
         FROM reservations WHERE id = ?1",
        [&reservation_id.0],
        |r| {
            Ok(ReservationRow {
                tenant_id: r.get(0)?,
                key_id: r.get(1)?,
                provider_id: r.get(2)?,
                model_id: r.get(3)?,
                period: r.get(4)?,
                reserved_minor: r.get(5)?,
                status: r.get(6)?,
                expires_at_ms: r.get(7)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| BudgetError::ReservationNotFound {
        reservation_id: reservation_id.0.clone(),
    })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Temp SQLite path that deletes the database and its `-wal`/`-shm`
    /// sidecars on drop — including when the test panics. Bind it *before*
    /// the ledger so the ledger (and its open connection) drops first.
    struct TempDb(std::path::PathBuf);

    impl AsRef<Path> for TempDb {
        fn as_ref(&self) -> &Path {
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

    fn temp_db_path(tag: &str) -> TempDb {
        TempDb(std::env::temp_dir().join(format!(
            "idoris-tenancy-budget-{tag}-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4()
        )))
    }

    #[test]
    fn configure_then_balance_reports_full_limit() {
        let path = temp_db_path("configure-balance");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger
            .configure(&scope, 1_000, "Asia/Bangkok")
            .expect("configure");
        assert_eq!(ledger.balance(&scope).expect("balance"), 1_000);
    }

    /// Negative control: querying balance for an unconfigured scope must
    /// error, not silently report a limit of zero (which would be
    /// indistinguishable from "budget deliberately set to zero").
    #[test]
    fn balance_on_unconfigured_scope_errors() {
        let path = temp_db_path("unconfigured");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        assert!(matches!(
            ledger.balance(&scope),
            Err(BudgetError::NotConfigured { .. })
        ));
    }

    /// Negative control: an invalid time zone name must be rejected at
    /// `configure` time, not accepted and silently misinterpreted later.
    #[test]
    fn configure_rejects_unknown_time_zone() {
        let path = temp_db_path("bad-tz");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        assert!(matches!(
            ledger.configure(&scope, 1_000, "Not/AZone"),
            Err(BudgetError::InvalidTimeZone { .. })
        ));
    }

    #[test]
    fn configure_rejects_negative_limit() {
        let path = temp_db_path("neg-limit");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        assert!(matches!(
            ledger.configure(&scope, -1, "Asia/Bangkok"),
            Err(BudgetError::InvalidLimit { .. })
        ));
    }

    #[test]
    fn reopening_the_same_file_reuses_existing_config() {
        let path = temp_db_path("reopen");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        {
            let ledger = BudgetLedger::open(&path).expect("open");
            ledger.configure(&scope, 500, "UTC").expect("configure");
        }
        let reopened = BudgetLedger::open(&path).expect("reopen");
        assert_eq!(reopened.balance(&scope).expect("balance"), 500);
    }

    /// Negative control: a blank scope field is rejected at `configure`
    /// time, not silently accepted into the SQL composite key (Codex
    /// review — see `validate_scope`).
    #[test]
    fn configure_rejects_blank_scope_field() {
        let path = temp_db_path("blank-scope");
        let ledger = BudgetLedger::open(&path).expect("open");
        // One case per field (empty and whitespace-only alternate), so a
        // field accidentally dropped from `validate_scope` fails here.
        let cases = [
            (BudgetScope::new("", "k", "p", "m"), "tenant_id"),
            (BudgetScope::new("t", "  ", "p", "m"), "key_id"),
            (BudgetScope::new("t", "k", "", "m"), "provider_id"),
            (BudgetScope::new("t", "k", "p", "\t"), "model_id"),
        ];
        for (scope, expected) in cases {
            match ledger.configure(&scope, 100, "UTC") {
                Err(BudgetError::InvalidScope { field }) => assert_eq!(field, expected),
                other => panic!("{expected}: expected InvalidScope, got {other:?}"),
            }
        }
    }

    /// Negative control: a non-positive TTL would create reservations that
    /// are already expired and never count against the balance.
    #[test]
    fn open_with_rejects_non_positive_ttl() {
        for ttl_ms in [0, -1, i64::MIN] {
            let path = temp_db_path("bad-ttl");
            assert!(matches!(
                BudgetLedger::open_with(&path, Arc::new(SystemClock), ttl_ms),
                Err(BudgetError::InvalidTtl { .. })
            ));
        }
        let path = temp_db_path("good-ttl");
        assert!(BudgetLedger::open_with(&path, Arc::new(SystemClock), 1).is_ok());
    }
}

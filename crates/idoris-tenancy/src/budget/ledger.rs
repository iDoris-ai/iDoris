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

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use uuid::Uuid;

use super::clock::{Clock, SystemClock};
use super::error::{BudgetError, checked_add_i64, checked_sub_i64};
use super::period::billing_period_key;
use super::scope::BudgetScope;

/// Opus Tier-2 re-review B-8: a deterministic way to prove the concurrency
/// tests in `tests/budget_concurrency.rs` would actually catch a "split the
/// atomic check-and-deduct transaction" regression class — without hand-
/// editing `reserve()` and adding a sleep to widen the race window (which
/// only proves the bug is catchable *with help*, not that the test suite
/// reliably catches an unmitigated real mutation). Gated behind the
/// `mutation-test-hooks` feature, off by default: with the feature disabled
/// this module doesn't exist and `reserve()`'s check is a no-op `cfg`
/// branch that compiles away entirely, so there is no production cost or
/// risk from this existing.
#[cfg(feature = "mutation-test-hooks")]
pub mod test_hooks {
    use std::sync::{Barrier, OnceLock};

    /// When armed (via [`arm`]), `reserve` commits its balance-check
    /// transaction and rendezvous on this barrier before opening a *new*
    /// transaction for the insert — reproducing the split-transaction bug
    /// class on demand. Every participating thread blocks here until all of
    /// them have arrived, i.e. until all of them have passed their own
    /// balance check, which is what forces the over-spend deterministically
    /// (no thread can "get lucky" and slip through before the others catch
    /// up, and no thread can proceed to its insert before the others have
    /// all committed their checks).
    static BARRIER: OnceLock<Barrier> = OnceLock::new();

    /// Arm the hook for `thread_count` participants. Call once before
    /// spawning the threads that will call `reserve`; each of them must
    /// actually call `reserve` exactly once for the barrier to release.
    pub fn arm(thread_count: usize) {
        BARRIER
            .set(Barrier::new(thread_count))
            .unwrap_or_else(|_| panic!("test_hooks::arm called more than once per process"));
    }

    /// `None` when not armed — `reserve` skips the hook entirely.
    pub(super) fn barrier() -> Option<&'static Barrier> {
        BARRIER.get()
    }
}

/// Reservation lifecycle state. Opus Tier-2 acceptance L3: this used to be
/// raw `&str`/`String` comparisons against the SQL `status` column's TEXT
/// values scattered across `settle`/`release`, which is exactly the kind of
/// thing a typo (`"Active"` vs `"active"`) slips through silently. The SQL
/// column itself stays `TEXT` (with a `CHECK` constraint — see
/// `migrations/0001_init.sql`) since that's what every existing row already
/// is; this enum is the single place that maps to/from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationStatus {
    Active,
    Settled,
    Released,
    Expired,
}

impl ReservationStatus {
    fn as_sql(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Settled => "settled",
            Self::Released => "released",
            Self::Expired => "expired",
        }
    }

    fn parse(s: &str) -> Result<Self, BudgetError> {
        match s {
            "active" => Ok(Self::Active),
            "settled" => Ok(Self::Settled),
            "released" => Ok(Self::Released),
            "expired" => Ok(Self::Expired),
            other => Err(BudgetError::Storage(format!(
                "unknown reservations.status value {other:?}"
            ))),
        }
    }
}

const SCHEMA_MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_init.sql"),
    include_str!("migrations/0002_overage_events.sql"),
    include_str!("migrations/0003_tenant_scope.sql"),
    include_str!("migrations/0004_tenant_period_column.sql"),
    include_str!("migrations/0005_dispatch_hold.sql"),
];

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
    /// `true` if this reservation's TTL had already lapsed by the time
    /// `settle` ran (Opus Tier-2 acceptance H1) — the call still gets
    /// charged (money doesn't become fictional just because a slow upstream
    /// call took longer than the TTL anticipated), but callers/observability
    /// may want to know a call ran unusually long, or that its TTL sizing
    /// needs revisiting.
    pub late: bool,
    /// `actual_cost_minor - reserved_minor`, clamped to `>= 0` (Opus Tier-2
    /// acceptance M1) — the mirror of `refunded_minor` for the overspend
    /// case. A non-zero value here also writes a `budget_overage_events`
    /// row (see `migrations/0002_overage_events.sql`) for independent
    /// auditing.
    pub overage_minor: i64,
}

/// Whether a tenant's check gates every candidate or only priced ones (H2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendGate {
    /// Default: a zero-cost candidate bypasses the tenant-level check.
    PaidOnly,
    /// Every candidate is checked, including zero-cost ones.
    All,
}

impl SpendGate {
    fn as_sql(self) -> &'static str {
        match self {
            Self::PaidOnly => "paid_only",
            Self::All => "all",
        }
    }

    fn parse(s: &str) -> Result<Self, BudgetError> {
        match s {
            "paid_only" => Ok(Self::PaidOnly),
            "all" => Ok(Self::All),
            other => Err(BudgetError::Storage(format!(
                "unknown tenant_config.scope value {other:?}"
            ))),
        }
    }
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
    pub(super) conn: Mutex<Connection>,
    pub(super) settlements: Mutex<Connection>,
    pub(super) settlement_outcomes: Mutex<HashMap<String, (String, i64)>>,
    pub(super) unverified_settlements: Mutex<HashMap<(ReservationId, String), i64>>,
    pub(super) live_intents: Mutex<HashMap<String, String>>,
    pub(super) dispatch_locks: Mutex<HashMap<String, super::settlement::DispatchLease>>,
    pub(super) dispatch_lock_dir: PathBuf,
    pub(super) release_outcomes: Mutex<HashMap<String, String>>,
    #[cfg(test)]
    pub(super) begin_settlement_test_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    pub(super) clock: Arc<dyn Clock>,
    ttl_ms: i64,
}

/// Per-scope configuration: the period limit and the explicit IANA zone its
/// periods are resolved in.
struct ScopeConfig {
    limit_minor: i64,
    billing_timezone: String,
}

/// Per-tenant configuration (H2), independent of any sub-scope's config.
struct TenantConfig {
    limit_minor: i64,
    billing_timezone: String,
    gate: SpendGate,
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
    /// expired at creation would never count against the balance. Uses a 5s
    /// `busy_timeout`; see
    /// [`open_with_busy_timeout`](Self::open_with_busy_timeout) to override
    /// that too (e.g. a short one in a busy-timeout test).
    pub fn open_with(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        ttl_ms: i64,
    ) -> Result<Self, BudgetError> {
        Self::open_with_busy_timeout(path, clock, ttl_ms, Duration::from_secs(5))
    }

    /// As [`open_with`](Self::open_with), with an explicit `busy_timeout`
    /// (Opus Tier-2 acceptance L4's busy-timeout test needs this shorter
    /// than the 5s default so it doesn't take 5 real seconds to run).
    pub fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        ttl_ms: i64,
        busy_timeout: Duration,
    ) -> Result<Self, BudgetError> {
        if ttl_ms <= 0 {
            return Err(BudgetError::InvalidTtl { ttl_ms });
        }
        if !path.as_ref().exists()
            && matches!(
                std::fs::symlink_metadata(path.as_ref()),
                Ok(metadata) if metadata.file_type().is_symlink()
            )
        {
            return Err(BudgetError::Storage(
                "database symlink target does not exist".into(),
            ));
        }
        let storage_path = if path.as_ref() == Path::new(":memory:") {
            path.as_ref().to_path_buf()
        } else if path.as_ref().exists() {
            std::fs::canonicalize(path.as_ref()).map_err(|e| BudgetError::Storage(e.to_string()))?
        } else {
            let parent = path
                .as_ref()
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let name = path
                .as_ref()
                .file_name()
                .ok_or_else(|| BudgetError::Storage("database path has no file name".into()))?;
            std::fs::canonicalize(parent)
                .map_err(|e| BudgetError::Storage(e.to_string()))?
                .join(name)
        };
        let mut conn = Connection::open(&storage_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(busy_timeout)?;
        run_migrations(&mut conn)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS budget_emergency_settlements (
                reservation_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL,
                actual_cost_minor INTEGER NOT NULL CHECK(actual_cost_minor >= 0)
            );",
        )?;
        let settlements =
            super::settlement::open(&storage_path, path.as_ref(), &conn, busy_timeout)?;
        let ledger = Self {
            conn: Mutex::new(conn),
            settlements: Mutex::new(settlements),
            settlement_outcomes: Mutex::new(HashMap::new()),
            unverified_settlements: Mutex::new(HashMap::new()),
            live_intents: Mutex::new(HashMap::new()),
            dispatch_locks: Mutex::new(HashMap::new()),
            dispatch_lock_dir: if path.as_ref() == Path::new(":memory:") {
                std::env::temp_dir().join("idoris-dispatch-memory")
            } else {
                storage_path.with_added_extension("dispatch-locks")
            },
            release_outcomes: Mutex::new(HashMap::new()),
            #[cfg(test)]
            begin_settlement_test_hook: Mutex::new(None),
            clock,
            ttl_ms,
        };
        if let Err(err) = ledger.retry_settlements() {
            eprintln!("budget settlement recovery pending: {err}");
        }
        Ok(ledger)
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
    /// L2: rejects a `billing_timezone` change while this scope has active
    /// reservations — their `period` was fixed under the *old* zone.
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
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // B-2: every configure/configure_tenant call for one tenant must
        // agree on a single billing_timezone. Checked against the *other*
        // dimensions only (this scope's own prior value is what the L2
        // check right below governs, not this one).
        if let Some(tenant_cfg) = load_tenant_config(&tx, &scope.tenant_id)?
            && tenant_cfg.billing_timezone != billing_timezone
        {
            tx.rollback().ok();
            return Err(BudgetError::InvalidConfig {
                tenant_id: scope.tenant_id.clone(),
                existing: tenant_cfg.billing_timezone,
                requested: billing_timezone.to_string(),
            });
        }
        if let Some(sibling_tz) = sibling_sub_scope_timezone(&tx, &scope.tenant_id, Some(scope))?
            && sibling_tz != billing_timezone
        {
            tx.rollback().ok();
            return Err(BudgetError::InvalidConfig {
                tenant_id: scope.tenant_id.clone(),
                existing: sibling_tz,
                requested: billing_timezone.to_string(),
            });
        }
        if let Some(existing) = load_config(&tx, scope)?
            && existing.billing_timezone != billing_timezone
            && has_active_reservations_for_scope(&tx, scope, now_ms)?
        {
            tx.rollback().ok();
            return Err(BudgetError::TimeZoneChangeWithActiveReservations);
        }
        tx.execute(
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
        tx.commit()?;
        Ok(())
    }

    /// Tenant-level total and billing zone (H2), optional layer on top of
    /// `configure`'s sub-scope limits. Same L2 timezone rule.
    pub fn configure_tenant(
        &self,
        tenant_id: &str,
        limit_minor: i64,
        billing_timezone: &str,
        gate: SpendGate,
    ) -> Result<(), BudgetError> {
        if tenant_id.trim().is_empty() {
            return Err(BudgetError::InvalidScope { field: "tenant_id" });
        }
        if limit_minor < 0 {
            return Err(BudgetError::InvalidLimit { limit_minor });
        }
        if !idoris_contracts::tenant::is_iana_time_zone(billing_timezone) {
            return Err(BudgetError::InvalidTimeZone {
                billing_timezone: billing_timezone.to_string(),
            });
        }
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // B-2: same cross-dimension timezone agreement as `configure`,
        // checked against sub-scope configs only — this tenant_config's own
        // prior value is what the L2 check right below governs, not this
        // one (changing your own value can't conflict with itself).
        if let Some(sibling_tz) = sibling_sub_scope_timezone(&tx, tenant_id, None)?
            && sibling_tz != billing_timezone
        {
            tx.rollback().ok();
            return Err(BudgetError::InvalidConfig {
                tenant_id: tenant_id.to_string(),
                existing: sibling_tz,
                requested: billing_timezone.to_string(),
            });
        }
        if let Some(existing) = load_tenant_config(&tx, tenant_id)?
            && existing.billing_timezone != billing_timezone
            && has_active_reservations_for_tenant(&tx, tenant_id, now_ms)?
        {
            tx.rollback().ok();
            return Err(BudgetError::TimeZoneChangeWithActiveReservations);
        }
        tx.execute(
            "INSERT INTO tenant_config (tenant_id, limit_minor, billing_timezone, scope) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (tenant_id) \
             DO UPDATE SET limit_minor = excluded.limit_minor, \
                           billing_timezone = excluded.billing_timezone, \
                           scope = excluded.scope",
            rusqlite::params![tenant_id, limit_minor, billing_timezone, gate.as_sql()],
        )?;
        tx.commit()?;
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
        Ok(checked_sub_i64(
            config.limit_minor,
            checked_add_i64(spent, reserved),
        ))
    }

    /// As `balance`, but for the tenant-level total (H2).
    pub fn tenant_balance(&self, tenant_id: &str) -> Result<i64, BudgetError> {
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let config = load_tenant_config(&tx, tenant_id)?.ok_or_else(|| {
            BudgetError::TenantNotConfigured {
                tenant_id: tenant_id.to_string(),
            }
        })?;
        let period = billing_period_key(now_ms, &config.billing_timezone)?;
        let spent = tenant_spent_for(&tx, tenant_id, &period)?;
        let reserved = tenant_active_reserved_for(&tx, tenant_id, &period, now_ms)?;
        tx.rollback()?;
        Ok(checked_sub_i64(
            config.limit_minor,
            checked_add_i64(spent, reserved),
        ))
    }

    /// Atomically check-and-deduct: inside one `BEGIN IMMEDIATE` transaction,
    /// compute the scope's remaining balance for the current period and, if
    /// `estimated_cost` fits, insert an `active` reservation holding that
    /// amount until it's settled, released, or its TTL expires. A dispatch
    /// hold keeps an in-flight request reserved beyond its TTL. Two
    /// concurrent callers can never both succeed past the same last unit of
    /// budget, because the second one's `BEGIN IMMEDIATE` blocks (up to the
    /// `busy_timeout`) until the first commits or rolls back.
    pub fn reserve(
        &self,
        scope: &BudgetScope,
        estimated_cost: Price,
    ) -> Result<ReservationId, BudgetError> {
        // prdaemon review on #48: since B-3 made the sub-scope `configure`
        // call optional, `reserve` could be called for a tenant that only
        // ever called `configure_tenant` — with `scope.key_id`/
        // `provider_id`/`model_id` left blank. Without this check, a blank
        // field is a valid composite-key value, so every caller who left a
        // field blank (accidentally or otherwise) would silently share the
        // same `(tenant_id, "", "", "")` sub-scope row instead of erroring.
        // `configure`'s own `validate_scope` call no longer guards this
        // path once configuring the sub-scope became optional, so `reserve`
        // must validate it directly instead of relying on `configure`
        // having been called first.
        validate_scope(scope)?;

        let estimated_cost_minor = match estimated_cost {
            Price::Unknown => return Err(BudgetError::PriceUnknown),
            Price::Known(v) if v < 0 => {
                return Err(BudgetError::InvalidEstimate {
                    estimated_cost_minor: v,
                });
            }
            Price::Known(v) => v,
        };

        self.retry_settlements_for_tenant(Some(&scope.tenant_id))?;
        // Live dispatches keep their reservations. Only an intent whose
        // caller is no longer live blocks this tenant pending recovery.
        let _outcomes = self
            .settlement_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let live = self.live_intents.lock().unwrap_or_else(|p| p.into_inner());
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        // Keep intent and owner reads in one snapshot; a healthy completion
        // cannot delete the claim between its enumeration and the lock probe.
        let snapshot = journal.transaction()?;
        let mut intent_stmt =
            snapshot.prepare("SELECT reservation_id FROM settlement_intents WHERE tenant_id=?1")?;
        let intents = intent_stmt
            .query_map([&scope.tenant_id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(intent_stmt);
        for id in &intents {
            if live.get(id) == Some(&scope.tenant_id) {
                continue;
            }
            let token: Option<String> = snapshot
                .query_row(
                    "SELECT token FROM dispatch_owners WHERE reservation_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(token) = token else {
                return Err(BudgetError::Storage(
                    "tenant has an ownerless settlement intent".into(),
                ));
            };
            let path = self.dispatch_lock_dir.join(format!("{token}.lock"));
            let file = File::options()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|e| BudgetError::Storage(e.to_string()))?;
            match file.try_lock() {
                Err(std::fs::TryLockError::WouldBlock) => continue,
                Err(error) => {
                    return Err(BudgetError::Storage(format!(
                        "dispatch owner lock probe failed: {error:?}"
                    )));
                }
                Ok(()) => {
                    return Err(BudgetError::Storage(
                        "tenant has an orphaned settlement intent".into(),
                    ));
                }
            }
        }
        drop(snapshot);
        drop(journal);
        drop(live);
        // Keep this process-local gate until the reservation transaction is
        // finished. Otherwise begin_settlement could insert an intent after
        // this check but before this request commits its reservation.
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // B-3 (Opus Tier-2 re-review): the sub-scope four-tuple config is
        // the *optional* finer layer, the tenant-level total is the
        // (contract-tenancy §3) primary dimension — not the other way
        // around. `NotConfigured` only fires when *neither* is set; a
        // tenant with no sub-scope config at all can still `reserve` purely
        // against its tenant-level total.
        let sub_config = load_config(&tx, scope)?;
        let tenant_cfg = load_tenant_config(&tx, &scope.tenant_id)?;
        if sub_config.is_none() && tenant_cfg.is_none() {
            return Err(BudgetError::NotConfigured {
                scope: scope.clone(),
            });
        }

        // Period bucketing prefers the sub-scope's own zone when it exists
        // (unchanged from before); with no sub-scope config, falls back to
        // the tenant's zone. Both `None` was already rejected above, so one
        // of the two branches below always has a `billing_timezone` to use;
        // the `NotConfigured` fallback in the `None`/`None` arm is
        // unreachable in practice but keeps this total instead of relying
        // on a panic-capable `expect`.
        let period = match (&sub_config, &tenant_cfg) {
            (Some(cfg), _) => billing_period_key(now_ms, &cfg.billing_timezone)?,
            (None, Some(tenant)) => billing_period_key(now_ms, &tenant.billing_timezone)?,
            (None, None) => {
                return Err(BudgetError::NotConfigured {
                    scope: scope.clone(),
                });
            }
        };

        // B-2/B-4: the tenant-level period bucket, computed independently of
        // `period` above and stored on the row (`tenant_period`) so tenant
        // aggregation never has to reuse a value computed under a
        // *different* zone. Prefers the tenant's own zone; falls back to
        // the sub-scope's if `configure_tenant` hasn't happened yet (B-4 —
        // spend recorded before a tenant is configured must still land in a
        // real bucket, so it's visible once `configure_tenant` does happen).
        let tenant_period = match (&tenant_cfg, &sub_config) {
            (Some(tenant), _) => billing_period_key(now_ms, &tenant.billing_timezone)?,
            (None, Some(_)) => period.clone(),
            (None, None) => {
                return Err(BudgetError::NotConfigured {
                    scope: scope.clone(),
                });
            }
        };

        // Expired reservations already stop counting toward the balance sum
        // below (it filters `expires_at_ms > now_ms`); flipping their status
        // here too means a later `settle`/`release` on the same id gets a
        // clear `ReservationNotActive` instead of silently succeeding.
        sweep_expired_scope(&tx, scope, &period, now_ms)?;

        // B-6: the `SpendGate` is a tenant-level setting, but its free-
        // request bypass applies uniformly to *both* dimensions — an
        // unconfigured tenant defaults to `PaidOnly` (matching
        // `SpendGate::PaidOnly`'s own "default" doc comment), so the
        // sub-scope check also skips a free request unless a tenant has
        // explicitly opted into `All`.
        let effective_gate = tenant_cfg
            .as_ref()
            .map(|t| t.gate)
            .unwrap_or(SpendGate::PaidOnly);
        let skip_zero_cost = effective_gate == SpendGate::PaidOnly && estimated_cost_minor == 0;

        if !skip_zero_cost && let Some(sub_cfg) = &sub_config {
            let sub_spent = spent_for(&tx, scope, &period)?;
            let sub_reserved = active_reserved_for(&tx, scope, &period, now_ms)?;
            let sub_committed = checked_add_i64(sub_spent, sub_reserved);
            if exceeds_limit(
                estimated_cost_minor,
                sub_cfg.limit_minor,
                sub_committed,
                effective_gate,
            ) {
                tx.commit()?; // nothing written yet, but keep the sweep above.
                return Err(BudgetError::exceeded(
                    scope.tenant_id.clone(),
                    sub_cfg.limit_minor,
                    sub_committed,
                    estimated_cost_minor,
                ));
            }
        }

        // H2: the tenant-level dimension, checked in the same transaction as
        // the sub-scope one above, so either insufficient rejects the whole
        // `reserve` atomically with the other.
        if !skip_zero_cost && let Some(tenant_cfg) = &tenant_cfg {
            let tenant_spent = tenant_spent_for(&tx, &scope.tenant_id, &tenant_period)?;
            let tenant_reserved =
                tenant_active_reserved_for(&tx, &scope.tenant_id, &tenant_period, now_ms)?;
            let tenant_committed = checked_add_i64(tenant_spent, tenant_reserved);
            if exceeds_limit(
                estimated_cost_minor,
                tenant_cfg.limit_minor,
                tenant_committed,
                effective_gate,
            ) {
                tx.commit()?;
                return Err(BudgetError::exceeded(
                    scope.tenant_id.clone(),
                    tenant_cfg.limit_minor,
                    tenant_committed,
                    estimated_cost_minor,
                ));
            }
        }

        // B-8 mutation-testing hook (see `test_hooks` doc comment) — a
        // no-op unless a test has explicitly armed it. When armed, this
        // reproduces the split-transaction bug class on purpose: commit the
        // check we just did, rendezvous with every other participating
        // thread, then open a *new* transaction for the insert below —
        // exactly the non-atomic check-then-deduct this crate exists to
        // prevent.
        #[cfg(feature = "mutation-test-hooks")]
        let mut tx = tx;
        #[cfg(feature = "mutation-test-hooks")]
        if let Some(barrier) = test_hooks::barrier() {
            tx.commit()?;
            barrier.wait();
            tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
                 status, created_at_ms, expires_at_ms, actual_cost_minor, tenant_period) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11)",
            rusqlite::params![
                id,
                scope.tenant_id,
                scope.key_id,
                scope.provider_id,
                scope.model_id,
                period,
                estimated_cost_minor,
                ReservationStatus::Active.as_sql(),
                now_ms,
                expires_at_ms,
                tenant_period,
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
            "UPDATE reservations SET status=?1 WHERE status=?2 AND expires_at_ms <= ?3 AND dispatch_hold=0",
            rusqlite::params![
                ReservationStatus::Expired.as_sql(),
                ReservationStatus::Active.as_sql(),
                now_ms
            ],
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
        "UPDATE reservations SET status=?7 \
         WHERE tenant_id=?1 AND key_id=?2 AND provider_id=?3 AND model_id=?4 AND period=?5 \
           AND status=?6 AND expires_at_ms <= ?8 AND dispatch_hold=0",
        rusqlite::params![
            scope.tenant_id,
            scope.key_id,
            scope.provider_id,
            scope.model_id,
            period,
            ReservationStatus::Active.as_sql(),
            ReservationStatus::Expired.as_sql(),
            now_ms,
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
    ///
    /// H1 (Opus Tier-2 acceptance): an *expired* reservation (its TTL lapsed
    /// before `settle` ran) still gets charged here — `SettleReceipt::late`
    /// is `true` in that case — because a slow upstream call may have
    /// genuinely spent real money by the time it returns; the TTL exists to
    /// free budget held by *abandoned* calls, not to make a late-but-real
    /// charge disappear. Only an already-`settled` or already-`released`
    /// reservation is rejected, since settling either again would
    /// double-charge or un-release a completed outcome. See
    /// [`extend`](Self::extend) for renewing a reservation before its TTL
    /// lapses, for a caller that anticipates running long.
    pub fn settle(
        &self,
        tenant_id: &str,
        reservation_id: &ReservationId,
        actual_cost_minor: i64,
    ) -> Result<SettleReceipt, BudgetError> {
        self.settle_public(tenant_id, reservation_id, actual_cost_minor)
    }

    /// Commit a settlement after the sidecar has serialized dispatch ownership.
    /// This is internal so callers cannot bypass that ownership protocol.
    pub(super) fn settle_primary(
        &self,
        tenant_id: &str,
        reservation_id: &ReservationId,
        actual_cost_minor: i64,
    ) -> Result<SettleReceipt, BudgetError> {
        if actual_cost_minor < 0 {
            return Err(BudgetError::InvalidActualCost { actual_cost_minor });
        }
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let row = find_reservation(&tx, tenant_id, reservation_id)?;
        let status = ReservationStatus::parse(&row.status)?;

        let late = match status {
            ReservationStatus::Active => row.expires_at_ms <= now_ms,
            ReservationStatus::Expired => true,
            ReservationStatus::Settled | ReservationStatus::Released => {
                tx.rollback().ok();
                return Err(BudgetError::ReservationNotActive {
                    reservation_id: reservation_id.0.clone(),
                    status: row.status,
                });
            }
        };

        // M1: track overage independently of the receipt returned below —
        // `settle` can return `Err(OverageTooLarge)` further down, and the
        // charge must still be recorded/auditable even on that path.
        let overage_minor = checked_sub_i64(actual_cost_minor, row.reserved_minor).max(0);
        // i128 to avoid the multiply overflowing i64 for a maliciously (or
        // just very wrongly) large `actual_cost_minor`.
        let too_large = (actual_cost_minor as i128) > (row.reserved_minor as i128) * 4;

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
            "UPDATE reservations SET status=?2, actual_cost_minor=?3 WHERE id=?1",
            rusqlite::params![
                reservation_id.0,
                ReservationStatus::Settled.as_sql(),
                actual_cost_minor
            ],
        )?;

        // H2/B-4: also credit the tenant-level total — *unconditionally*,
        // not only when `configure_tenant` has already been called. Spend
        // that happens before a tenant is configured must still land in a
        // real `tenant_period` bucket (populated by `reserve`, see B-4's
        // doc comment there) so it's already accounted for the moment
        // `configure_tenant` does happen, rather than silently missing.
        // Same transaction as the writes above, so `reserve`'s
        // dual-dimension check never sees a half-updated state.
        tx.execute(
            "INSERT INTO tenant_periods (tenant_id, period, spent_minor) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT (tenant_id, period) \
             DO UPDATE SET spent_minor = spent_minor + excluded.spent_minor",
            rusqlite::params![tenant_id, row.tenant_period, actual_cost_minor],
        )?;

        if overage_minor > 0 {
            tx.execute(
                "INSERT INTO budget_overage_events \
                    (id, tenant_id, reservation_id, reserved_minor, actual_cost_minor, \
                     overage_minor, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    tenant_id,
                    reservation_id.0,
                    row.reserved_minor,
                    actual_cost_minor,
                    overage_minor,
                    now_ms,
                ],
            )?;
        }

        tx.commit()?;

        // M1: the charge above is already committed at this point — an
        // extreme overage (>4x reserved) is a loud "something upstream is
        // very wrong" signal, but the money owed is real and must not
        // vanish just because this call returns `Err` instead of the
        // receipt.
        if too_large {
            return Err(BudgetError::OverageTooLarge {
                reservation_id: reservation_id.0.clone(),
                reserved_minor: row.reserved_minor,
                actual_cost_minor,
            });
        }

        let refunded_minor = checked_sub_i64(row.reserved_minor, actual_cost_minor).max(0);
        Ok(SettleReceipt {
            reserved_minor: row.reserved_minor,
            actual_cost_minor,
            refunded_minor,
            late,
            overage_minor,
        })
    }

    /// Renew a still-`active` reservation's TTL by `additional_ttl_ms` from
    /// now (H1) — for a caller whose upstream call is running longer than
    /// anticipated and wants to keep holding the budget rather than risk a
    /// concurrent `sweep_expired`/another `reserve` treating it as freed.
    /// Size `additional_ttl_ms` from the upstream's own timeout plus margin,
    /// the same way the original TTL should be sized. Only valid on a
    /// reservation still in `active` status; a reservation already
    /// `settled`/`released` can't be extended, and an already-swept
    /// `expired` one should be re-reserved instead (extending it would
    /// resurrect a reservation other code may have already treated as
    /// freed).
    /// B-5 (Opus Tier-2 re-review) hardening:
    /// - `additional_ttl_ms` may not exceed this ledger's own `ttl_ms` — one
    ///   `extend` call can renew for at most as long as a fresh `reserve`
    ///   would have granted, not an arbitrary caller-chosen amount.
    /// - the reservation's total lifetime (`created_at_ms` to the *new*
    ///   `expires_at_ms`) may not exceed 4x `ttl_ms` — bounds how many times
    ///   `extend` can be chained, so a caller can't keep a reservation (and
    ///   the budget it holds) alive indefinitely.
    /// - a reservation whose TTL has already lapsed (`expires_at_ms <=
    ///   now_ms`) is rejected even if its DB `status` is still `active`
    ///   (lazily not yet swept) — extending a reservation other code may
    ///   already be treating as freed would resurrect it unexpectedly.
    pub fn extend(
        &self,
        tenant_id: &str,
        reservation_id: &ReservationId,
        additional_ttl_ms: i64,
    ) -> Result<(), BudgetError> {
        if additional_ttl_ms <= 0 || additional_ttl_ms > self.ttl_ms {
            return Err(BudgetError::InvalidTtl {
                ttl_ms: additional_ttl_ms,
            });
        }
        let now_ms = self.clock.now_ms();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let row = find_reservation(&tx, tenant_id, reservation_id)?;
        let status = ReservationStatus::parse(&row.status)?;
        if status != ReservationStatus::Active || row.expires_at_ms <= now_ms {
            tx.rollback().ok();
            return Err(BudgetError::ReservationNotActive {
                reservation_id: reservation_id.0.clone(),
                status: row.status,
            });
        }

        let new_expires_at_ms = now_ms
            .checked_add(additional_ttl_ms)
            .ok_or(BudgetError::InvalidTimestamp { now_ms })?;
        let max_lifetime_ms = self.ttl_ms.checked_mul(4).ok_or(BudgetError::InvalidTtl {
            ttl_ms: additional_ttl_ms,
        })?;
        let max_expires_at_ms = row
            .created_at_ms
            .checked_add(max_lifetime_ms)
            .ok_or(BudgetError::InvalidTimestamp { now_ms })?;
        if new_expires_at_ms > max_expires_at_ms {
            tx.rollback().ok();
            return Err(BudgetError::InvalidTtl {
                ttl_ms: additional_ttl_ms,
            });
        }

        tx.execute(
            "UPDATE reservations SET expires_at_ms=?2 WHERE id=?1",
            rusqlite::params![reservation_id.0, new_expires_at_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stop advertising a live dispatch when its final cost is unknown.
    /// Keep the durable claim, intent and hold for reconciliation; this is
    /// not confirmation that the upstream did no work and must not refund.
    pub fn abandon_dispatch(
        &self,
        tenant_id: &str,
        reservation_id: &ReservationId,
    ) -> Result<(), BudgetError> {
        let mut live = self.live_intents.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(owner) = live.get(&reservation_id.0) {
            if owner != tenant_id {
                return Err(BudgetError::TenantMismatch {
                    reservation_id: reservation_id.0.clone(),
                });
            }
            live.remove(&reservation_id.0);
            self.dispatch_locks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&reservation_id.0);
        }
        Ok(())
    }

    /// Confirm that the upstream call did not execute, using the same
    /// dispatch ownership checks and durable cancellation retry as release.
    pub fn release_confirmed_unexecuted(
        &self,
        tenant_id: &str,
        reservation_id: &ReservationId,
    ) -> Result<(), BudgetError> {
        self.release(tenant_id, reservation_id)
    }

    /// Fully release a reservation without charging anything — for calls
    /// that failed or that fell back to a different (separately reserved)
    /// candidate. Idempotent when the reservation is already `released`;
    /// erroring on `settled` prevents un-settling a completed charge.
    /// The dispatch owner must only call this after confirming no charge is
    /// due (including cancellation before chat submission). Ownership alone
    /// or a cancelled request future is not proof; unknown outcomes must use
    /// [`Self::abandon_dispatch`] and retain their hold.
    ///
    /// L3 (Opus Tier-2 acceptance): releasing an already-`expired` (but not
    /// yet settled/released) reservation succeeds (`Ok(())`) instead of
    /// erroring — no charge was ever recorded against it, so "release" (no
    /// charge is due) is already true when no dispatch intent or completed
    /// outcome is awaiting recovery.
    ///
    /// Every success path here writes `Released`, **never** leaves a row as
    /// `Expired` — this matters as of H1 (a follow-up PR makes `settle`
    /// still charge an `Expired`-but-not-yet-finalized reservation): if
    /// `release` left an expired row's status as `Expired` instead of
    /// explicitly finalizing it to `Released`, a caller could `release()` a
    /// reservation (declaring "no charge is due") and then still have a
    /// racing/late `settle()` on the same id succeed and charge it anyway.
    /// `Released` is the one status H1's `settle` always rejects, so it's
    /// the only correct terminal state for this method to leave behind.
    pub fn release(
        &self,
        tenant_id: &str,
        reservation_id: &ReservationId,
    ) -> Result<(), BudgetError> {
        let outcomes = self
            .settlement_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if self
            .unverified_settlements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&(reservation_id.clone(), tenant_id.to_owned()))
        {
            return Err(BudgetError::SettlementConflict {
                reservation_id: reservation_id.0.clone(),
                reason: "completed settlement is awaiting ownership verification".into(),
            });
        }
        let mut live = self.live_intents.lock().unwrap_or_else(|p| p.into_inner());
        if outcomes.contains_key(&reservation_id.0) {
            return Err(BudgetError::Storage(
                "cannot release reservation with a known actual outcome".into(),
            ));
        }
        let deferred_primary_error = {
            let conn = self.lock();
            match find_reservation(&conn, tenant_id, reservation_id) {
                Ok(row) => {
                    if row.status == ReservationStatus::Settled.as_sql() {
                        return Err(BudgetError::ReservationNotActive {
                            reservation_id: reservation_id.0.clone(),
                            status: row.status,
                        });
                    }
                    if row.actual_cost_minor.is_some() {
                        return Err(BudgetError::Storage(
                            "cannot release reservation with a recorded actual outcome".into(),
                        ));
                    }
                    let emergency: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM budget_emergency_settlements WHERE reservation_id=?1)",
                        [&reservation_id.0], |r| r.get(0),
                    )?;
                    if emergency {
                        return Err(BudgetError::SettlementConflict {
                            reservation_id: reservation_id.0.clone(),
                            reason: "completed settlement is pending emergency recovery".into(),
                        });
                    }
                    None
                }
                Err(err @ (BudgetError::Busy | BudgetError::Storage(_)))
                    if live
                        .get(&reservation_id.0)
                        .is_some_and(|owner| owner == tenant_id) =>
                {
                    // begin_settlement already validated this owner. If the
                    // primary lookup is temporarily unavailable, keep the
                    // cancellation retryable instead of orphaning its intent.
                    Some(err)
                }
                Err(err) => return Err(err),
            }
        };
        // The caller explicitly confirms no charge is due; ownership only
        // authorizes that confirmation, it cannot establish cancellation.
        // Preserve that acknowledgement even when the journal is unavailable;
        // a foreign caller must first prove a durable acknowledgement exists.
        let _dispatch_lease = if live
            .get(&reservation_id.0)
            .is_some_and(|owner| owner == tenant_id)
        {
            self.release_outcomes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(reservation_id.0.clone(), tenant_id.to_string());
            live.remove(&reservation_id.0);
            self.dispatch_locks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&reservation_id.0)
        } else {
            None
        };
        let local_confirmation = self
            .release_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&reservation_id.0)
            .is_some_and(|owner| owner == tenant_id);
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let confirmation = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_intent: bool = confirmation.query_row(
            "SELECT EXISTS(SELECT 1 FROM settlement_intents WHERE reservation_id=?1)",
            [&reservation_id.0],
            |r| r.get(0),
        )?;
        let confirmed_release: Option<String> = confirmation
            .query_row(
                "SELECT tenant_id FROM pending_releases WHERE reservation_id=?1",
                [&reservation_id.0],
                |r| r.get(0),
            )
            .optional()?;
        if confirmed_release
            .as_deref()
            .is_some_and(|owner| owner != tenant_id)
        {
            return Err(BudgetError::TenantMismatch {
                reservation_id: reservation_id.0.clone(),
            });
        }
        if has_intent && !local_confirmation && confirmed_release.is_none() {
            return Err(BudgetError::Storage(
                "dispatch cancellation requires its owner's acknowledgement".into(),
            ));
        }
        confirmation.execute(
            "INSERT INTO pending_releases VALUES (?1, ?2) ON CONFLICT(reservation_id) DO NOTHING",
            rusqlite::params![reservation_id.0, tenant_id],
        )?;
        // Commit confirmation before attempting the primary write: restart
        // may replay confirmed cancellation, but never infer it from silence.
        confirmation.commit()?;
        self.release_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(reservation_id.0.clone(), tenant_id.to_string());
        if let Some(err) = deferred_primary_error {
            return Err(err);
        }
        let journal_tx = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending_actual: i64 = journal_tx.query_row(
            "SELECT count(*) FROM pending_settlements WHERE reservation_id=?1",
            [&reservation_id.0],
            |r| r.get(0),
        )?;
        if pending_actual > 0 {
            journal_tx.execute(
                "DELETE FROM pending_releases WHERE reservation_id=?1 AND tenant_id=?2",
                rusqlite::params![reservation_id.0, tenant_id],
            )?;
            journal_tx.commit()?;
            self.release_outcomes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&reservation_id.0);
            return Err(BudgetError::Storage(
                "cannot release reservation with a pending actual outcome".into(),
            ));
        }
        // Keep a retryable outcome even when either database is Busy. After
        // a crash, the durable pre-call intent still fails closed.
        // The terminal commit itself also retires the intent. A concurrent
        // durable settlement that starts after this transaction commits must
        // validate the primary row and cannot revive a released reservation.
        journal_tx.execute(
            "DELETE FROM settlement_intents WHERE reservation_id=?1 AND tenant_id=?2",
            rusqlite::params![reservation_id.0, tenant_id],
        )?;
        journal_tx.execute(
            "DELETE FROM dispatch_owners WHERE reservation_id=?1 AND tenant_id=?2",
            rusqlite::params![reservation_id.0, tenant_id],
        )?;
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let row = find_reservation(&tx, tenant_id, reservation_id)?;
        let status = ReservationStatus::parse(&row.status)?;
        if row.actual_cost_minor.is_some() && status != ReservationStatus::Settled {
            return Err(BudgetError::Storage(
                "cannot release reservation with a recorded actual outcome".into(),
            ));
        }
        match status {
            ReservationStatus::Released => {}
            ReservationStatus::Active | ReservationStatus::Expired => {
                tx.execute(
                    "UPDATE reservations SET status=?2 WHERE id=?1",
                    rusqlite::params![reservation_id.0, ReservationStatus::Released.as_sql()],
                )?;
            }
            ReservationStatus::Settled => {
                return Err(BudgetError::ReservationNotActive {
                    reservation_id: reservation_id.0.clone(),
                    status: row.status,
                });
            }
        }
        tx.commit()?;
        drop(conn);
        journal_tx.commit()?;
        drop(journal);
        live.remove(&reservation_id.0);
        self.dispatch_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&reservation_id.0);
        drop(live);
        drop(outcomes);
        self.cancel_settlement(tenant_id, reservation_id)?;
        self.settlements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execute(
                "DELETE FROM pending_releases WHERE reservation_id=?1 AND tenant_id=?2",
                rusqlite::params![reservation_id.0, tenant_id],
            )?;
        self.release_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&reservation_id.0);
        Ok(())
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
    /// B-2: the period bucket for *tenant-level* aggregation, computed
    /// independently of `period` (see `migrations/0004_tenant_period_column.sql`).
    /// Always populated by `reserve` (falls back to the sub-scope's own
    /// zone if no tenant config existed yet at reserve time — B-4).
    tenant_period: String,
    reserved_minor: i64,
    status: String,
    expires_at_ms: i64,
    /// B-5: needed to cap a reservation's total lifetime across repeated
    /// `extend` calls.
    created_at_ms: i64,
    actual_cost_minor: Option<i64>,
}

/// M2 (Opus Tier-2 acceptance): filters by `tenant_id` in the `WHERE`
/// clause, not just by `id` — a caller from a different tenant guessing a
/// valid reservation id must not be able to settle/release/extend it. When
/// the row exists but under a different tenant, this returns
/// [`BudgetError::TenantMismatch`] rather than `ReservationNotFound`, purely
/// so this crate's own tests can assert isolation held; the two variants
/// render identical error text (see `TenantMismatch`'s doc comment), so
/// nothing observable leaks cross-tenant existence to an external caller.
fn find_reservation(
    conn: &Connection,
    tenant_id: &str,
    reservation_id: &ReservationId,
) -> Result<ReservationRow, BudgetError> {
    let found = conn
        .query_row(
            "SELECT tenant_id, key_id, provider_id, model_id, period, reserved_minor, status, \
                    expires_at_ms, tenant_period, created_at_ms, actual_cost_minor \
             FROM reservations WHERE id = ?1 AND tenant_id = ?2",
            rusqlite::params![reservation_id.0, tenant_id],
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
                    tenant_period: r.get(8)?,
                    created_at_ms: r.get(9)?,
                    actual_cost_minor: r.get(10)?,
                })
            },
        )
        .optional()?;
    if let Some(row) = found {
        return Ok(row);
    }
    let exists_elsewhere: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM reservations WHERE id = ?1",
            [&reservation_id.0],
            |r| r.get(0),
        )
        .optional()?;
    match exists_elsewhere {
        Some(_) => Err(BudgetError::TenantMismatch {
            reservation_id: reservation_id.0.clone(),
        }),
        None => Err(BudgetError::ReservationNotFound {
            reservation_id: reservation_id.0.clone(),
        }),
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

/// B-6/B-7: whether `estimated_cost_minor` leaves no room against
/// `limit_minor - committed`, honoring `gate`. A priced request
/// (`estimated_cost_minor > 0`) is unaffected by `gate` — plain
/// `cost > balance`. A *free* request under `SpendGate::All` instead
/// matches policy's `is_over()` semantics: `committed >= limit` (not just
/// strictly over) counts as "no room left" too. `SpendGate::PaidOnly`'s
/// free-request bypass is handled by the caller *before* this is invoked
/// (see `reserve`'s `skip_zero_cost`), so in practice `gate` here is only
/// ever `All` on the `estimated_cost_minor == 0` path.
fn exceeds_limit(
    estimated_cost_minor: i64,
    limit_minor: i64,
    committed: i64,
    gate: SpendGate,
) -> bool {
    let balance = checked_sub_i64(limit_minor, committed);
    if estimated_cost_minor == 0 && gate == SpendGate::All {
        balance <= 0
    } else {
        estimated_cost_minor > balance
    }
}

/// B-2: the `billing_timezone` any *other* `(key, provider, model)`
/// sub-scope already uses for this tenant, excluding `exclude` if given
/// (the scope currently being configured, which can't conflict with
/// itself). Assumes at most one distinct value exists among the remaining
/// rows — an invariant this same check enforces on every write, so there's
/// nothing to reconcile between multiple disagreeing siblings.
fn sibling_sub_scope_timezone(
    conn: &Connection,
    tenant_id: &str,
    exclude: Option<&BudgetScope>,
) -> Result<Option<String>, BudgetError> {
    let mut stmt = conn.prepare(
        "SELECT key_id, provider_id, model_id, billing_timezone \
         FROM budget_config WHERE tenant_id = ?1",
    )?;
    let rows = stmt.query_map([tenant_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (key_id, provider_id, model_id, billing_timezone) = row?;
        if let Some(ex) = exclude
            && ex.key_id == key_id
            && ex.provider_id == provider_id
            && ex.model_id == model_id
        {
            continue;
        }
        return Ok(Some(billing_timezone));
    }
    Ok(None)
}

fn has_active_reservations_for_scope(
    conn: &Connection,
    scope: &BudgetScope,
    now_ms: i64,
) -> Result<bool, BudgetError> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM reservations \
         WHERE tenant_id=?1 AND key_id=?2 AND provider_id=?3 AND model_id=?4 \
           AND status=?5 AND (expires_at_ms > ?6 OR dispatch_hold=1))",
        rusqlite::params![
            scope.tenant_id,
            scope.key_id,
            scope.provider_id,
            scope.model_id,
            ReservationStatus::Active.as_sql(),
            now_ms
        ],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn load_tenant_config(
    conn: &Connection,
    tenant_id: &str,
) -> Result<Option<TenantConfig>, BudgetError> {
    let row: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT limit_minor, billing_timezone, scope FROM tenant_config WHERE tenant_id = ?1",
            [tenant_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(limit_minor, billing_timezone, gate_text)| {
        Ok(TenantConfig {
            limit_minor,
            billing_timezone,
            gate: SpendGate::parse(&gate_text)?,
        })
    })
    .transpose()
}

fn tenant_spent_for(conn: &Connection, tenant_id: &str, period: &str) -> Result<i64, BudgetError> {
    let spent: Option<i64> = conn
        .query_row(
            "SELECT spent_minor FROM tenant_periods WHERE tenant_id=?1 AND period=?2",
            rusqlite::params![tenant_id, period],
            |row| row.get(0),
        )
        .optional()?;
    Ok(spent.unwrap_or(0))
}

/// Tenant-level active-reservation sum (H2): aggregates across every
/// sub-scope for `tenant_id`, matched by `period`.
fn tenant_active_reserved_for(
    conn: &Connection,
    tenant_id: &str,
    tenant_period: &str,
    now_ms: i64,
) -> Result<i64, BudgetError> {
    conn.query_row(
        "SELECT COALESCE(SUM(reserved_minor), 0) FROM reservations \
         WHERE tenant_id=?1 AND tenant_period=?2 AND status=?3 AND (expires_at_ms > ?4 OR dispatch_hold=1)",
        rusqlite::params![
            tenant_id,
            tenant_period,
            ReservationStatus::Active.as_sql(),
            now_ms
        ],
        |row| row.get(0),
    )
    .map_err(BudgetError::from)
}

fn has_active_reservations_for_tenant(
    conn: &Connection,
    tenant_id: &str,
    now_ms: i64,
) -> Result<bool, BudgetError> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM reservations \
         WHERE tenant_id=?1 AND status=?2 AND (expires_at_ms > ?3 OR dispatch_hold=1))",
        rusqlite::params![tenant_id, ReservationStatus::Active.as_sql(), now_ms],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
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
           AND status='active' AND (expires_at_ms > ?6 OR dispatch_hold=1)",
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

    struct TestClock(std::sync::atomic::AtomicI64);

    impl Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

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
            let sidecar = self.0.with_added_extension("settlements.sqlite3");
            for suffix in ["", "-wal", "-shm"] {
                let mut p = sidecar.clone().into_os_string();
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
    fn unknown_dispatch_retains_hold_and_blocks_new_spend_after_ttl() {
        let path = temp_db_path("unknown-dispatch");
        let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(0)));
        let ledger = BudgetLedger::open_with(&path, clock.clone(), 100).unwrap();
        let scope = BudgetScope::new("tenant", "key", "provider", "model");
        ledger.configure(&scope, 100, "UTC").unwrap();
        let id = ledger.reserve(&scope, Price::Known(30)).unwrap();
        ledger.begin_settlement("tenant", &id).unwrap();
        assert!(matches!(
            ledger.abandon_dispatch("other", &id),
            Err(BudgetError::TenantMismatch { .. })
        ));
        ledger.abandon_dispatch("tenant", &id).unwrap();
        clock.0.store(101, std::sync::atomic::Ordering::SeqCst);
        ledger.retry_settlements().unwrap();
        assert_eq!(ledger.sweep_expired().unwrap(), 0);
        assert_eq!(ledger.balance(&scope).unwrap(), 70);
        assert!(ledger.release("tenant", &id).is_err());
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(1)),
            Err(BudgetError::Storage(_))
        ));
        drop(ledger);
        let reopened = BudgetLedger::open_with(&path, clock, 100).unwrap();
        assert_eq!(reopened.balance(&scope).unwrap(), 70);
        assert!(reopened.release("tenant", &id).is_err());
        assert!(reopened.reserve(&scope, Price::Known(1)).is_err());
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

    /// H2: `configure_tenant`/`tenant_balance` exercised directly.
    #[test]
    fn configure_tenant_then_tenant_balance_reports_full_limit() {
        let path = temp_db_path("h2-configure-tenant");
        let ledger = BudgetLedger::open(&path).expect("open");
        ledger
            .configure_tenant("acme-co", 500, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        assert_eq!(
            ledger.tenant_balance("acme-co").expect("tenant_balance"),
            500
        );
    }

    /// Negative control: an unconfigured tenant errors, not a silent zero.
    #[test]
    fn tenant_balance_on_unconfigured_tenant_errors() {
        let path = temp_db_path("h2-tenant-unconfigured");
        let ledger = BudgetLedger::open(&path).expect("open");
        assert!(matches!(
            ledger.tenant_balance("acme-co"),
            Err(BudgetError::TenantNotConfigured { .. })
        ));
    }

    #[test]
    fn configure_tenant_rejects_invalid_input() {
        let path = temp_db_path("h2-tenant-invalid");
        let ledger = BudgetLedger::open(&path).expect("open");
        assert!(matches!(
            ledger.configure_tenant("  ", 100, "UTC", SpendGate::PaidOnly),
            Err(BudgetError::InvalidScope { field: "tenant_id" })
        ));
        assert!(matches!(
            ledger.configure_tenant("acme-co", -1, "UTC", SpendGate::PaidOnly),
            Err(BudgetError::InvalidLimit { .. })
        ));
        assert!(matches!(
            ledger.configure_tenant("acme-co", 100, "Not/AZone", SpendGate::PaidOnly),
            Err(BudgetError::InvalidTimeZone { .. })
        ));
    }

    /// H2: an unconfigured tenant has no tenant-level check at all —
    /// `reserve` behaves exactly as before `configure_tenant` existed.
    #[test]
    fn reserve_without_tenant_config_only_checks_sub_scope() {
        let path = temp_db_path("h2-no-tenant-config");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        assert!(ledger.reserve(&scope, Price::Known(1_000)).is_ok());
    }

    /// B-3 (Opus Tier-2 re-review): the reverse of the test above — a
    /// tenant with *only* `configure_tenant` called (no sub-scope
    /// `configure` at all) can still `reserve`. `NotConfigured` must only
    /// fire when neither dimension is set up.
    #[test]
    fn reserve_with_only_tenant_config_succeeds() {
        let path = temp_db_path("b3-only-tenant-config");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger
            .configure_tenant("acme-co", 1_000, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        assert!(ledger.reserve(&scope, Price::Known(400)).is_ok());
        assert_eq!(
            ledger.tenant_balance("acme-co").expect("tenant_balance"),
            600
        );
    }

    /// prdaemon review on #48: since B-3 made the sub-scope `configure`
    /// call optional, a caller could `reserve` against a tenant-only setup
    /// with a blank `key_id`/`provider_id`/`model_id` — without
    /// `reserve` validating the scope itself, that blank field would
    /// silently become part of the composite key (merging unrelated calls
    /// onto the same sub-scope row) instead of being rejected the way
    /// `configure` has always rejected it.
    #[test]
    fn reserve_rejects_blank_scope_field_even_with_only_tenant_configured() {
        let path = temp_db_path("prdaemon-reserve-blank-scope");
        let ledger = BudgetLedger::open(&path).expect("open");
        ledger
            .configure_tenant("acme-co", 1_000, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        let cases = [
            (BudgetScope::new("acme-co", "", "openai", "gpt-5"), "key_id"),
            (
                BudgetScope::new("acme-co", "key-1", "  ", "gpt-5"),
                "provider_id",
            ),
            (
                BudgetScope::new("acme-co", "key-1", "openai", "\t"),
                "model_id",
            ),
        ];
        for (scope, expected) in cases {
            match ledger.reserve(&scope, Price::Known(1)) {
                Err(BudgetError::InvalidScope { field }) => assert_eq!(field, expected),
                other => panic!("{expected}: expected InvalidScope, got {other:?}"),
            }
        }
    }

    /// H2: a tight tenant-level limit rejects even though the sub-scope
    /// alone would allow it — both dimensions must pass.
    #[test]
    fn reserve_rejected_by_tenant_level_limit_even_when_sub_scope_allows() {
        let path = temp_db_path("h2-tenant-rejects");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger
            .configure_tenant("acme-co", 50, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(100)),
            Err(BudgetError::Exceeded {
                limit_minor: 50,
                spent_minor: 0,
                ..
            })
        ));
    }

    /// H2: the reverse — a tight sub-scope limit rejects even though the
    /// tenant-level total alone would allow it.
    #[test]
    fn reserve_rejected_by_sub_scope_limit_even_when_tenant_level_allows() {
        let path = temp_db_path("h2-sub-scope-rejects");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 50, "UTC").expect("configure");
        ledger
            .configure_tenant("acme-co", 10_000, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(100)),
            Err(BudgetError::Exceeded { .. })
        ));
    }

    /// H2: the tenant-level total aggregates across multiple sub-scopes.
    #[test]
    fn tenant_level_limit_aggregates_across_sub_scopes() {
        let path = temp_db_path("h2-aggregate");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope_a = BudgetScope::new("acme-co", "key-a", "openai", "gpt-5");
        let scope_b = BudgetScope::new("acme-co", "key-b", "openai", "gpt-5");
        ledger
            .configure(&scope_a, 1_000, "UTC")
            .expect("configure a");
        ledger
            .configure(&scope_b, 1_000, "UTC")
            .expect("configure b");
        ledger
            .configure_tenant("acme-co", 150, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        ledger
            .reserve(&scope_a, Price::Known(100))
            .expect("first reserve fits tenant total");
        assert!(matches!(
            ledger.reserve(&scope_b, Price::Known(100)),
            Err(BudgetError::Exceeded { .. })
        ));
    }

    /// B-2: two sub-scopes for the same tenant must agree on a
    /// `billing_timezone` — this is what actually prevents the bug the
    /// probe found (a tenant limit silently not enforced because one
    /// sub-scope's period bucket disagreed with another's), by making the
    /// disagreement impossible to configure in the first place rather than
    /// papering over it after the fact.
    #[test]
    fn configure_rejects_mismatched_timezone_against_sibling_sub_scope() {
        let path = temp_db_path("b2-sibling-mismatch");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope_a = BudgetScope::new("acme-co", "key-a", "openai", "gpt-5");
        let scope_b = BudgetScope::new("acme-co", "key-b", "openai", "gpt-5");
        ledger
            .configure(&scope_a, 1_000, "UTC")
            .expect("configure a");
        let err = ledger
            .configure(&scope_b, 1_000, "Asia/Bangkok")
            .expect_err("mismatched sibling timezone must be rejected");
        assert!(matches!(err, BudgetError::InvalidConfig { .. }));
        // Same tenant, same zone as the sibling: still fine.
        assert!(ledger.configure(&scope_b, 1_000, "UTC").is_ok());
    }

    /// B-2 mirror: a sub-scope's timezone must also agree with the
    /// tenant-level config, in both directions.
    #[test]
    fn configure_and_configure_tenant_reject_mismatched_timezones_both_directions() {
        let path = temp_db_path("b2-tenant-sub-mismatch");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");

        ledger
            .configure_tenant("acme-co", 1_000, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        assert!(matches!(
            ledger.configure(&scope, 1_000, "Asia/Bangkok"),
            Err(BudgetError::InvalidConfig { .. })
        ));
        ledger
            .configure(&scope, 1_000, "UTC")
            .expect("configure matching zone");

        let scope2 = BudgetScope::new("acme-co", "key-2", "openai", "gpt-5");
        ledger.configure(&scope2, 1_000, "UTC").expect("configure");
        assert!(matches!(
            ledger.configure_tenant("acme-co", 1_000, "Asia/Bangkok", SpendGate::PaidOnly),
            Err(BudgetError::InvalidConfig { .. })
        ));
    }

    /// B-4: spend recorded *before* a tenant is ever configured still lands
    /// in a real tenant-level bucket (via the row's `tenant_period`,
    /// falling back to the sub-scope's own zone — B-2), so it's already
    /// counted the moment `configure_tenant` happens, rather than silently
    /// missing.
    #[test]
    fn settle_before_configure_tenant_is_still_counted_once_tenant_is_configured() {
        let path = temp_db_path("b4-spend-before-tenant-config");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");

        let id = ledger.reserve(&scope, Price::Known(400)).expect("reserve");
        ledger
            .settle(&scope.tenant_id, &id, 400)
            .expect("settle before any configure_tenant call");

        ledger
            .configure_tenant("acme-co", 500, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant after the fact");
        assert_eq!(
            ledger.tenant_balance("acme-co").expect("tenant_balance"),
            100,
            "the earlier 400 spend must already be reflected"
        );
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(200)),
            Err(BudgetError::Exceeded { .. })
        ));
    }

    /// H2: `paid_only` (the default) lets a zero-cost candidate through even
    /// when the tenant-level balance is already exhausted.
    #[test]
    fn paid_only_gate_bypasses_zero_cost_even_when_tenant_exhausted() {
        let path = temp_db_path("h2-paid-only");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger
            .configure_tenant("acme-co", 0, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        assert!(ledger.reserve(&scope, Price::Known(0)).is_ok());
    }

    /// H2 negative control: `all` gates every candidate, including
    /// zero-cost ones, once the tenant-level balance actually goes
    /// negative — resolves the ambiguity `paid_only` leaves open. A
    /// `settle` overage (M1) is the way to drive the tenant balance
    /// negative in the first place, since `reserve` itself never lets a
    /// reservation through that would push it there.
    #[test]
    fn all_gate_rejects_zero_cost_once_tenant_balance_is_negative() {
        let path = temp_db_path("h2-all-gate");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger
            .configure_tenant("acme-co", 100, "UTC", SpendGate::All)
            .expect("configure_tenant");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        ledger
            .settle(&scope.tenant_id, &id, 150)
            .expect("settle overage");
        assert!(matches!(
            ledger.tenant_balance("acme-co"),
            Ok(balance) if balance < 0
        ));
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(0)),
            Err(BudgetError::Exceeded { .. })
        ));
    }

    /// L2: changing `billing_timezone` while a reservation is still active
    /// is rejected — the active reservation's period was already fixed
    /// under the old zone.
    #[test]
    fn configure_rejects_timezone_change_with_active_reservations() {
        let path = temp_db_path("l2-tz-change");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        assert!(matches!(
            ledger.configure(&scope, 1_000, "Asia/Bangkok"),
            Err(BudgetError::TimeZoneChangeWithActiveReservations)
        ));
        assert!(ledger.configure(&scope, 2_000, "UTC").is_ok());
    }

    #[test]
    fn configure_tenant_rejects_timezone_change_with_active_reservations() {
        // No `configure()` sub-scope at all here (relies on B-3's "tenant
        // dimension alone is enough to reserve"), so B-2's cross-dimension
        // timezone-agreement check has nothing to compare against and this
        // isolates the L2 active-reservations guard specifically.
        let path = temp_db_path("l2-tenant-tz-change");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger
            .configure_tenant("acme-co", 1_000, "UTC", SpendGate::PaidOnly)
            .expect("configure_tenant");
        ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        assert!(matches!(
            ledger.configure_tenant("acme-co", 1_000, "Asia/Bangkok", SpendGate::PaidOnly),
            Err(BudgetError::TimeZoneChangeWithActiveReservations)
        ));
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

    #[test]
    fn reserve_deducts_from_balance() {
        let path = temp_db_path("reserve-deducts");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger.reserve(&scope, Price::Known(400)).expect("reserve");
        assert_eq!(ledger.balance(&scope).expect("balance"), 600);
    }

    #[test]
    fn reserve_over_balance_is_rejected_with_structured_error() {
        let path = temp_db_path("reserve-exceeded");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let err = ledger
            .reserve(&scope, Price::Known(150))
            .expect_err("must reject over-balance reserve");
        match err {
            BudgetError::Exceeded {
                tenant_id,
                balance_minor,
                estimated_cost_minor,
                limit_minor,
                spent_minor,
                ..
            } => {
                assert_eq!(tenant_id, "acme-co");
                assert_eq!(balance_minor, 100);
                assert_eq!(estimated_cost_minor, 150);
                assert_eq!(limit_minor, 100);
                assert_eq!(spent_minor, 0);
            }
            other => panic!("expected Exceeded, got {other:?}"),
        }
        // Negative control: the rejected reserve must not have touched the
        // balance — a naive "deduct on error path" bug would leave it < 100.
        assert_eq!(ledger.balance(&scope).expect("balance"), 100);
    }

    /// Negative control: `Price::Unknown` must never be treated as free —
    /// checked before the scope even needs to be configured.
    #[test]
    fn reserve_rejects_unknown_price_even_when_unconfigured() {
        let path = temp_db_path("reserve-price-unknown");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        assert!(matches!(
            ledger.reserve(&scope, Price::Unknown),
            Err(BudgetError::PriceUnknown)
        ));
    }

    /// Negative control: a zero-cost candidate is *not* "unknown price" —
    /// don't conflate free with unpriced.
    #[test]
    fn reserve_allows_zero_cost() {
        let path = temp_db_path("reserve-zero-cost");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        assert!(ledger.reserve(&scope, Price::Known(0)).is_ok());
        assert_eq!(ledger.balance(&scope).expect("balance"), 100);
    }

    /// B-6: `paid_only` (the implicit default with no tenant configured at
    /// all) bypasses the *sub-scope* check for a free request too, not just
    /// the tenant-level one — even once the sub-scope's own balance has
    /// gone negative (via an M1 overage).
    #[test]
    fn paid_only_default_bypasses_zero_cost_at_the_sub_scope_even_with_negative_balance() {
        let path = temp_db_path("b6-sub-scope-paid-only");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        ledger
            .settle(&scope.tenant_id, &id, 150)
            .expect("settle overage pushes sub-scope balance negative");
        assert!(ledger.balance(&scope).expect("balance") < 0);
        assert!(ledger.reserve(&scope, Price::Known(0)).is_ok());
    }

    /// B-7: under `SpendGate::All`, a free request is rejected once
    /// `committed` reaches the limit *exactly* — matching policy's
    /// `is_over()` (`committed >= limit`), not just `committed > limit`.
    #[test]
    fn all_gate_rejects_zero_cost_when_committed_exactly_equals_limit() {
        let path = temp_db_path("b7-all-gate-exact-limit");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger
            .configure_tenant("acme-co", 100, "UTC", SpendGate::All)
            .expect("configure_tenant");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        ledger
            .settle(&scope.tenant_id, &id, 100)
            .expect("settle exactly at the tenant limit");
        assert_eq!(ledger.tenant_balance("acme-co").expect("tenant_balance"), 0);
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(0)),
            Err(BudgetError::Exceeded { .. })
        ));
    }

    /// Negative control mirroring B-7: with balance strictly positive
    /// (committed < limit), a free request under `All` still succeeds.
    #[test]
    fn all_gate_allows_zero_cost_when_balance_is_still_positive() {
        let path = temp_db_path("b7-all-gate-positive-balance");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger
            .configure_tenant("acme-co", 100, "UTC", SpendGate::All)
            .expect("configure_tenant");
        let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
        ledger.settle(&scope.tenant_id, &id, 50).expect("settle");
        assert!(ledger.reserve(&scope, Price::Known(0)).is_ok());
    }

    #[test]
    fn reserve_rejects_negative_estimate() {
        let path = temp_db_path("reserve-negative");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(-1)),
            Err(BudgetError::InvalidEstimate { .. })
        ));
    }

    #[test]
    fn reserve_on_unconfigured_scope_errors() {
        let path = temp_db_path("reserve-unconfigured");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(1)),
            Err(BudgetError::NotConfigured { .. })
        ));
    }

    #[test]
    fn settle_for_less_than_reserved_refunds_the_difference() {
        let path = temp_db_path("settle-refund");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(500)).expect("reserve");
        let receipt = ledger.settle(&scope.tenant_id, &id, 300).expect("settle");
        assert_eq!(receipt.reserved_minor, 500);
        assert_eq!(receipt.actual_cost_minor, 300);
        assert_eq!(receipt.refunded_minor, 200);
        // 1000 limit - 300 settled spend - 0 active reservations left.
        assert_eq!(ledger.balance(&scope).expect("balance"), 700);
    }

    /// Negative control: settling for *more* than was reserved still charges
    /// the real amount — this is billing truth, not a cap — and refunds
    /// nothing.
    #[test]
    fn settle_for_more_than_reserved_charges_the_real_amount() {
        let path = temp_db_path("settle-overage");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        let receipt = ledger.settle(&scope.tenant_id, &id, 250).expect("settle");
        assert_eq!(receipt.refunded_minor, 0);
        assert_eq!(receipt.overage_minor, 150);
        assert_eq!(ledger.balance(&scope).expect("balance"), 750);
    }

    /// M1 negative control: overage beyond 4x the reserved amount still
    /// records the charge (balance reflects it) but reports
    /// `OverageTooLarge` instead of a receipt.
    #[test]
    fn settle_more_than_four_times_reserved_records_charge_but_errors() {
        let path = temp_db_path("settle-overage-too-large");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 10_000, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        let err = ledger
            .settle(&scope.tenant_id, &id, 500)
            .expect_err("4x+ overage must error");
        assert!(matches!(err, BudgetError::OverageTooLarge { .. }));
        // The charge was still recorded despite the Err.
        assert_eq!(ledger.balance(&scope).expect("balance"), 9_500);
        // And the reservation is finalized, not left dangling as `active`.
        assert!(matches!(
            ledger.settle(&scope.tenant_id, &id, 1),
            Err(BudgetError::ReservationNotActive { .. })
        ));
    }

    /// Negative control: settling the same reservation twice must fail, not
    /// double-charge.
    #[test]
    fn settling_twice_is_rejected() {
        let path = temp_db_path("settle-twice");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        ledger
            .settle(&scope.tenant_id, &id, 100)
            .expect("first settle");
        assert!(matches!(
            ledger.settle(&scope.tenant_id, &id, 100),
            Err(BudgetError::ReservationNotActive { .. })
        ));
    }

    #[test]
    fn settle_unknown_reservation_errors() {
        let path = temp_db_path("settle-unknown");
        let ledger = BudgetLedger::open(&path).expect("open");
        assert!(matches!(
            ledger.settle("acme-co", &ReservationId("does-not-exist".to_string()), 1),
            Err(BudgetError::ReservationNotFound { .. })
        ));
    }

    /// M2: a reservation belongs to the tenant that created it — a
    /// different tenant guessing the id must not be able to settle it, and
    /// must see the same error shape as a truly unknown id (no leak).
    #[test]
    fn settle_with_wrong_tenant_is_rejected_like_not_found() {
        let path = temp_db_path("settle-wrong-tenant");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
        assert!(matches!(
            ledger.settle("someone-else", &id, 50),
            Err(BudgetError::TenantMismatch { .. })
        ));
        // The reservation is untouched — still settleable by its own tenant.
        assert!(ledger.settle(&scope.tenant_id, &id, 50).is_ok());
    }

    /// M2 mirror: `release`/`extend` apply the same tenant filter.
    #[test]
    fn release_and_extend_with_wrong_tenant_are_rejected_like_not_found() {
        let path = temp_db_path("release-extend-wrong-tenant");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
        assert!(matches!(
            ledger.release("someone-else", &id),
            Err(BudgetError::TenantMismatch { .. })
        ));
        assert!(matches!(
            ledger.extend("someone-else", &id, 1_000),
            Err(BudgetError::TenantMismatch { .. })
        ));
        // Untouched by the rejected cross-tenant attempts.
        assert!(ledger.release(&scope.tenant_id, &id).is_ok());
    }

    #[test]
    fn release_fully_frees_the_reservation() {
        let path = temp_db_path("release-frees");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        // Negative control: while the reservation is active it really does
        // hold the budget — a second reserve must fail.
        assert!(ledger.reserve(&scope, Price::Known(1)).is_err());
        ledger.release(&scope.tenant_id, &id).expect("release");
        assert_eq!(ledger.balance(&scope).expect("balance"), 100);
        assert!(ledger.reserve(&scope, Price::Known(100)).is_ok());
    }

    #[test]
    fn release_and_cross_instance_settlement_serialize_without_losing_actual() {
        for primary_locked in [true, false] {
            let path = temp_db_path("release-settlement-cross-instance");
            let open = || {
                BudgetLedger::open_with_busy_timeout(
                    &path,
                    Arc::new(SystemClock),
                    60_000,
                    Duration::ZERO,
                )
                .unwrap()
            };
            let releaser = open();
            let owner = open();
            let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
            owner.configure(&scope, 1_000, "UTC").unwrap();
            let id = owner.reserve(&scope, Price::Known(100)).unwrap();
            owner.begin_settlement(&scope.tenant_id, &id).unwrap();
            assert!(releaser.begin_settlement(&scope.tenant_id, &id).is_err());
            let primary = Connection::open(&path).unwrap();
            if primary_locked {
                primary.execute_batch("BEGIN IMMEDIATE").unwrap();
            }
            let journal =
                Connection::open(path.as_ref().with_added_extension("settlements.sqlite3"))
                    .unwrap();
            journal.execute_batch("BEGIN IMMEDIATE").unwrap();
            assert!(releaser.release(&scope.tenant_id, &id).is_err());
            assert!(releaser.release_outcomes.lock().unwrap().is_empty());
            assert!(matches!(
                owner.settle_durable(&scope.tenant_id, &id, 20),
                Err(BudgetError::Busy) | Ok(None)
            ));
            journal.execute_batch("ROLLBACK").unwrap();
            if primary_locked {
                primary.execute_batch("ROLLBACK").unwrap();
            }
            // An unpersisted completed cost must survive a foreign release,
            // then be charged exactly once by retry and after restart.
            assert!(releaser.release(&scope.tenant_id, &id).is_err());
            owner.retry_settlements().unwrap();
            releaser.retry_settlements().unwrap();
            let row: (String, Option<i64>) = releaser
                .lock()
                .query_row(
                    "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
                    [&id.0],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(row, ("settled".to_string(), Some(20)));
            assert_eq!(releaser.balance(&scope).unwrap(), 980);
            drop(owner);
            drop(releaser);
            let recovered = open();
            recovered.retry_settlements().unwrap();
            assert_eq!(recovered.balance(&scope).unwrap(), 980);
        }
    }

    #[test]
    fn releasing_twice_is_idempotent() {
        let path = temp_db_path("release-twice");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
        ledger
            .release(&scope.tenant_id, &id)
            .expect("first release");
        assert!(ledger.release(&scope.tenant_id, &id).is_ok());
    }

    #[test]
    fn cancellation_does_not_release_pending_actual_after_exclusive_read_failure() {
        let path = temp_db_path("release-sidecar-exclusive");
        let ledger = BudgetLedger::open_with_busy_timeout(
            &path,
            Arc::new(SystemClock),
            60_000,
            Duration::ZERO,
        )
        .expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(100)).expect("reserve");
        ledger
            .begin_settlement(&scope.tenant_id, &id)
            .expect("begin settlement");
        ledger
            .settlements
            .lock()
            .expect("settlement journal")
            .execute(
                "INSERT INTO pending_settlements VALUES (?1, ?2, 20)",
                rusqlite::params![id.0, scope.tenant_id],
            )
            .expect("record actual outcome");

        let blocker = Connection::open(path.as_ref().with_added_extension("settlements.sqlite3"))
            .expect("open sidecar blocker");
        blocker.busy_timeout(Duration::ZERO).expect("busy timeout");
        blocker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("lock sidecar");

        assert!(matches!(
            ledger.release(&scope.tenant_id, &id),
            Err(BudgetError::Busy)
        ));
        assert!(matches!(ledger.retry_settlements(), Err(BudgetError::Busy)));
        assert_eq!(ledger.balance(&scope).expect("balance while pending"), 900);

        blocker.execute_batch("ROLLBACK").expect("unlock sidecar");
        ledger.retry_settlements().expect("recover actual outcome");
        assert_eq!(ledger.balance(&scope).expect("recovered balance"), 980);
        assert!(
            !ledger
                .release_outcomes
                .lock()
                .expect("release outcomes")
                .contains_key(&id.0)
        );
        assert!(
            !ledger
                .live_intents
                .lock()
                .expect("live intents")
                .contains_key(&id.0)
        );
        let (status, actual): (String, Option<i64>) = ledger
            .conn
            .lock()
            .expect("ledger connection")
            .query_row(
                "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("reservation outcome");
        assert_eq!((status.as_str(), actual), ("settled", Some(20)));
    }

    #[test]
    fn cancellation_primary_read_failure_recovers_after_ttl_and_restart() {
        let path = temp_db_path("release-primary-read-failure");
        let ttl_ms = 25;
        let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(
            SystemClock.now_ms(),
        )));
        let ledger = BudgetLedger::open_with(&path, clock.clone(), ttl_ms).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
        ledger
            .begin_settlement(&scope.tenant_id, &id)
            .expect("begin settlement");

        Connection::open(path.as_ref())
            .expect("open primary database")
            .execute_batch("ALTER TABLE reservations RENAME TO reservations_unavailable")
            .expect("hide reservations table");
        assert!(ledger.release("someone-else", &id).is_err());
        assert_eq!(
            ledger.live_intents.lock().expect("live intents").get(&id.0),
            Some(&scope.tenant_id)
        );
        let no_wrong_tenant_release: i64 = ledger
            .settlements
            .lock()
            .expect("settlement journal")
            .query_row("SELECT count(*) FROM pending_releases", [], |r| r.get(0))
            .expect("pending release count");
        assert_eq!(no_wrong_tenant_release, 0);
        assert!(matches!(
            ledger.release(&scope.tenant_id, &id),
            Err(BudgetError::Storage(_))
        ));
        assert!(
            !ledger
                .live_intents
                .lock()
                .expect("live intents")
                .contains_key(&id.0)
        );
        let pending_release: Option<String> = ledger
            .settlements
            .lock()
            .expect("settlement journal")
            .query_row(
                "SELECT tenant_id FROM pending_releases WHERE reservation_id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .optional()
            .expect("pending release query");
        assert_eq!(pending_release.as_deref(), Some(scope.tenant_id.as_str()));

        drop(ledger);
        clock
            .0
            .fetch_add(ttl_ms + 1, std::sync::atomic::Ordering::SeqCst);
        Connection::open(path.as_ref())
            .expect("open primary database")
            .execute_batch("ALTER TABLE reservations_unavailable RENAME TO reservations")
            .expect("restore reservations table");

        // Opening the worker retries its durable cancellation even though
        // the reservation's dispatch hold outlived its normal TTL.
        let recovered = BudgetLedger::open_with(&path, clock, ttl_ms).expect("restart ledger");
        let (status, actual): (String, Option<i64>) = recovered
            .lock()
            .query_row(
                "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("recovered reservation");
        assert_eq!((status.as_str(), actual), ("released", None));
        assert_eq!(recovered.balance(&scope).expect("released balance"), 100);
        let intent_count: i64 = recovered
            .settlements
            .lock()
            .expect("settlement journal")
            .query_row(
                "SELECT count(*) FROM settlement_intents WHERE reservation_id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .expect("intent count");
        assert_eq!(intent_count, 0);
    }

    /// Negative control: a settled reservation can't be released — that
    /// would un-settle a completed charge.
    #[test]
    fn releasing_a_settled_reservation_errors() {
        let path = temp_db_path("release-settled");
        let ledger = BudgetLedger::open(&path).expect("open");
        let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
        ledger.configure(&scope, 100, "UTC").expect("configure");
        let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
        ledger.settle(&scope.tenant_id, &id, 50).expect("settle");
        assert!(matches!(
            ledger.release(&scope.tenant_id, &id),
            Err(BudgetError::ReservationNotActive { .. })
        ));
    }
}

//! Independent durable outbox: a writer locking the ledger cannot block
//! recording completed usage. Keep this sidecar with the ledger on backup.
use std::{path::Path, time::Duration};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{BudgetError, BudgetLedger, ReservationId};

pub(super) fn open(path: &Path, timeout: Duration) -> Result<Connection, BudgetError> {
    let conn = if path == Path::new(":memory:") {
        Connection::open_in_memory()?
    } else {
        Connection::open(path.with_added_extension("settlements.sqlite3"))?
    };
    conn.busy_timeout(timeout)?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pending_settlements (
        reservation_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL,
        actual_cost_minor INTEGER NOT NULL CHECK(actual_cost_minor >= 0));
        CREATE TABLE IF NOT EXISTS settlement_intents (
        reservation_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS pending_releases (
        reservation_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL)",
    )?;
    Ok(conn)
}

impl BudgetLedger {
    /// Durably mark a paid dispatch before calling its upstream. An intent
    /// owned by a live caller keeps its budget reserved without blocking
    /// concurrent requests. An orphan intent blocks only its tenant.
    pub fn begin_settlement(&self, tenant: &str, id: &ReservationId) -> Result<(), BudgetError> {
        // Match reserve's local serialization gate. reserve holds this gate
        // from its intent check through its ledger commit, so a dispatch
        // intent cannot race past an already-admitted reservation.
        let _outcomes = self
            .settlement_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut live = self.live_intents.lock().unwrap_or_else(|p| p.into_inner());
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let (owner, status, expires_at, actual): (String, String, i64, Option<i64>) = conn.query_row(
            "SELECT tenant_id, status, expires_at_ms, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if owner != tenant {
            return Err(BudgetError::TenantMismatch {
                reservation_id: id.0.clone(),
            });
        }
        if status != "active" || expires_at <= self.clock.now_ms() || actual.is_some() {
            return Err(BudgetError::Storage(
                "reservation is not eligible for dispatch".into(),
            ));
        }
        drop(conn);
        let journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        journal.execute(
            "INSERT INTO settlement_intents VALUES (?1, ?2)
             ON CONFLICT(reservation_id) DO NOTHING",
            params![id.0, tenant],
        )?;
        let recorded: String = journal.query_row(
            "SELECT tenant_id FROM settlement_intents WHERE reservation_id=?1",
            [&id.0],
            |r| r.get(0),
        )?;
        if recorded != tenant {
            return Err(BudgetError::Storage(
                "settlement intent tenant mismatch".into(),
            ));
        }
        drop(journal);
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let held = conn.execute(
            "UPDATE reservations SET dispatch_hold=1 WHERE id=?1 AND tenant_id=?2 AND status='active' AND actual_cost_minor IS NULL AND (expires_at_ms>?3 OR dispatch_hold=1)",
            params![id.0, tenant, self.clock.now_ms()],
        )?;
        if held != 1 {
            return Err(BudgetError::Storage(
                "reservation could not be held for dispatch".into(),
            ));
        }
        live.insert(id.0.clone(), tenant.to_string());
        Ok(())
    }

    /// Remove an intent only after the corresponding reservation was
    /// successfully released by the caller.
    pub(super) fn cancel_settlement(
        &self,
        tenant: &str,
        id: &ReservationId,
    ) -> Result<(), BudgetError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM reservations WHERE id=?1 AND tenant_id=?2",
                params![id.0, tenant],
                |r| r.get(0),
            )
            .optional()?;
        if status.as_deref() != Some("released") {
            return Err(BudgetError::Storage(
                "cannot cancel settlement intent before release".into(),
            ));
        }
        drop(conn);
        let mut outcomes = self
            .settlement_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let owner: Option<String> = journal
            .query_row(
                "SELECT tenant_id FROM settlement_intents WHERE reservation_id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .optional()?;
        if owner.as_deref().is_some_and(|owner| owner != tenant) {
            return Err(BudgetError::TenantMismatch {
                reservation_id: id.0.clone(),
            });
        }
        journal.execute(
            "DELETE FROM settlement_intents WHERE reservation_id=?1",
            [&id.0],
        )?;
        outcomes.remove(&id.0);
        Ok(())
    }

    /// Persist before attempting settlement. None means durable retry is
    /// pending. If the journal write fails, retain the outcome in memory and
    /// store it on the reservation when the primary ledger remains writable.
    pub fn settle_durable(
        &self,
        tenant: &str,
        id: &ReservationId,
        actual: i64,
    ) -> Result<Option<i64>, BudgetError> {
        if actual < 0 {
            return Err(BudgetError::InvalidActualCost {
                actual_cost_minor: actual,
            });
        }
        let mut outcomes = self
            .settlement_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut live = self.live_intents.lock().unwrap_or_else(|p| p.into_inner());
        let verified_owner = live.get(&id.0).cloned();
        if verified_owner
            .as_deref()
            .is_some_and(|owner| owner != tenant)
        {
            return Err(BudgetError::TenantMismatch {
                reservation_id: id.0.clone(),
            });
        }
        // The upstream operation has completed. Keep the mutex guard until
        // the outcome is durable so same-instance release cannot pass us, but
        // remove the live marker on every subsequent storage-error path.
        if verified_owner.is_some() {
            live.remove(&id.0);
        }
        if verified_owner.is_none() {
            // Non-dispatch callers do not have the owner proof captured by
            // begin_settlement, so they must still validate against primary.
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let (owner, primary_actual): (String, Option<i64>) = conn.query_row(
                "SELECT tenant_id, actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if owner != tenant {
                return Err(BudgetError::TenantMismatch {
                    reservation_id: id.0.clone(),
                });
            }
            if primary_actual.is_some_and(|recorded| recorded != actual) {
                return Err(BudgetError::Storage(
                    "primary settlement outcome mismatch".into(),
                ));
            }
        }
        if let Some((recorded_tenant, recorded_actual)) = outcomes.get(&id.0)
            && (recorded_tenant != tenant || *recorded_actual != actual)
        {
            live.remove(&id.0);
            return Err(BudgetError::Storage(
                "in-memory settlement outcome mismatch".into(),
            ));
        }

        // Release uses the same cross-process sidecar writer lock and keeps it
        // until its primary terminal transition commits. This makes the
        // release/settlement decision and the durable outcome registration a
        // single ordered protocol across ledger instances.
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let tx = match journal.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(tx) => tx,
            Err(error) => {
                live.remove(&id.0);
                drop(live);
                let journal_error = BudgetError::from(error);
                // Preserve a completed cost on primary when the independent
                // journal cannot be written. If begin_settlement supplied a
                // verified owner, a failed primary lookup must not discard it.
                outcomes.insert(id.0.clone(), (tenant.to_string(), actual));
                let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                let fallback = store_primary_fallback(&conn, &id.0, tenant, actual);
                if matches!(fallback, Ok(false)) {
                    outcomes.remove(&id.0);
                }
                eprintln!(
                    "budget settlement fallback failed: reservation={} journal={journal_error} fallback={fallback:?}",
                    id.0
                );
                return Err(journal_error);
            }
        };

        // Check the primary terminal state while holding the same sidecar
        // transaction used by release. A verified dispatch owner lets us
        // record the completed outcome if this read alone fails.
        let intent_owner: Result<Option<String>, rusqlite::Error> = tx
            .query_row(
                "SELECT tenant_id FROM settlement_intents WHERE reservation_id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .optional();
        match intent_owner {
            Ok(Some(owner)) if owner == tenant => {}
            Ok(Some(_)) => {
                live.remove(&id.0);
                return Err(BudgetError::TenantMismatch {
                    reservation_id: id.0.clone(),
                });
            }
            Ok(None) => {}
            Err(error) if verified_owner.as_deref() == Some(tenant) => {
                eprintln!(
                    "budget settlement intent check deferred: reservation={} error={error}",
                    id.0
                );
            }
            Err(error) => return Err(error.into()),
        }
        let primary = {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            conn.query_row(
                "SELECT tenant_id, status, actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
        };
        match primary {
            Ok((owner, status, primary_actual)) => {
                if owner != tenant {
                    return Err(BudgetError::TenantMismatch {
                        reservation_id: id.0.clone(),
                    });
                }
                if status == "released" {
                    return Err(BudgetError::ReservationNotActive {
                        reservation_id: id.0.clone(),
                        status,
                    });
                }
                if primary_actual.is_some_and(|recorded| recorded != actual) {
                    return Err(BudgetError::Storage(
                        "primary settlement outcome mismatch".into(),
                    ));
                }
            }
            Err(error) if verified_owner.as_deref() == Some(tenant) => {
                // `begin_settlement` checked and held this tenant's row before
                // dispatch. Preserve its completed amount in the sidecar even
                // while a transient primary read fault prevents replay.
                eprintln!(
                    "budget settlement primary check deferred: reservation={} error={error}",
                    id.0
                );
            }
            Err(error) => return Err(error.into()),
        }

        let journal_result = insert_pending(&tx, &id.0, tenant, actual);
        match journal_result {
            Err(PendingInsertError::Conflict) => {
                live.remove(&id.0);
                return Err(BudgetError::Storage(
                    "settlement journal outcome mismatch".into(),
                ));
            }
            Err(PendingInsertError::Storage(journal_error)) => {
                drop(tx);
                // The independent journal may be Busy. Persist the known actual
                // on the reservation as a second recovery source before returning
                // the journal error. Keep the in-memory copy too until a journal
                // write succeeds in this process.
                live.remove(&id.0);
                drop(live);
                outcomes.insert(id.0.clone(), (tenant.to_string(), actual));
                let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                let fallback = store_primary_fallback(&conn, &id.0, tenant, actual);
                if matches!(fallback, Ok(false)) {
                    outcomes.remove(&id.0);
                }
                if matches!(fallback, Ok(true)) {
                    drop(conn);
                    drop(outcomes);
                    return Err(journal_error);
                }
                eprintln!(
                    "budget settlement fallback failed: reservation={} journal={journal_error} fallback={fallback:?}",
                    id.0
                );
                return Err(journal_error);
            }
            Ok(()) => {}
        }
        live.remove(&id.0);
        if let Err(error) = tx.commit() {
            let journal_error = BudgetError::from(error);
            outcomes.insert(id.0.clone(), (tenant.to_string(), actual));
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let fallback = store_primary_fallback(&conn, &id.0, tenant, actual);
            if matches!(fallback, Ok(false)) {
                outcomes.remove(&id.0);
            }
            eprintln!(
                "budget settlement journal commit failed: reservation={} error={journal_error} fallback={fallback:?}",
                id.0
            );
            return Err(journal_error);
        }
        drop(journal);
        drop(live);
        self.release_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id.0);
        outcomes.remove(&id.0);
        drop(outcomes);
        match self.retry_settlements() {
            Ok(()) => Ok(Some(actual)),
            Err(err @ (BudgetError::Busy | BudgetError::Storage(_))) => {
                eprintln!("budget settlement queued: reservation={} error={err}", id.0);
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Retry on startup, periodically, and before admitting new spending.
    /// A journal write transaction serializes recovery across processes.
    pub fn retry_settlements(&self) -> Result<(), BudgetError> {
        let mut releases = {
            let journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
            let mut stmt =
                journal.prepare("SELECT reservation_id, tenant_id FROM pending_releases")?;
            stmt.query_map([], |r| {
                Ok((ReservationId(r.get(0)?), r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        // An unavailable journal must not lose a known cancellation in
        // this process; release will persist it before retrying the ledger.
        for (id, tenant) in self
            .release_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
        {
            if !releases.iter().any(|(queued, _)| &queued.0 == id) {
                releases.push((ReservationId(id.clone()), tenant.clone()));
            }
        }
        let mut release_error = None;
        for (id, tenant) in releases {
            let pending_actual = {
                let journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
                journal.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pending_settlements WHERE reservation_id=?1)",
                    [&id.0],
                    |r| r.get::<_, bool>(0),
                )?
            };
            let recorded_actual = self.conn.lock().unwrap_or_else(|p| p.into_inner()).query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE id=?1 AND tenant_id=?2 AND actual_cost_minor IS NOT NULL)",
                params![id.0, tenant], |r| r.get::<_, bool>(0),
            )?;
            let known_actual = pending_actual
                || recorded_actual
                || self
                    .settlement_outcomes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains_key(&id.0);
            if known_actual {
                eprintln!(
                    "budget release conflicts with pending settlement: reservation={}",
                    id.0
                );
                let journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
                journal.execute(
                    "DELETE FROM pending_releases WHERE reservation_id=?1",
                    [&id.0],
                )?;
                self.release_outcomes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id.0);
                continue;
            }
            if let Err(err) = self.release(&tenant, &id) {
                eprintln!(
                    "budget release retry pending: reservation={} error={err}",
                    id.0
                );
                release_error.get_or_insert(err);
            }
        }
        if let Some(err) = release_error {
            return Err(err);
        }
        let mut outcomes = self
            .settlement_outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let tx = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A failed journal write may have stored the known outcome on the
        // reservation. Rehydrate it before replay so an immediate restart
        // after journal Busy can still recover the actual charge.
        let fallback_rows = {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let mut stmt = conn.prepare(
                "SELECT id, tenant_id, actual_cost_minor FROM reservations
                 WHERE status IN ('active','expired') AND actual_cost_minor IS NOT NULL",
            )?;
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        for (id, tenant, actual) in fallback_rows {
            if self.discard_conflicting_fallback(&tx, &id, &tenant, actual)? {
                outcomes.remove(&id);
                continue;
            }
            if let Some((known_tenant, known_actual)) = outcomes.get(&id)
                && (known_tenant != &tenant || *known_actual != actual)
            {
                return Err(BudgetError::Storage(
                    "fallback settlement outcome mismatch".into(),
                ));
            }
            outcomes.insert(id, (tenant, actual));
        }
        // A fallback can be written while another instance owns the sidecar
        // lock for release. Before rehydrating that in-memory outcome, inspect
        // primary under our journal transaction: if release won first, discard
        // the stale fallback instead of creating an unreplayable pending row.
        let mut obsolete_outcomes = Vec::new();
        {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            for (id, (tenant, actual)) in outcomes.iter() {
                let primary: Option<(String, String, Option<i64>)> = conn
                    .query_row(
                        "SELECT tenant_id, status, actual_cost_minor FROM reservations WHERE id=?1",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                match primary {
                    Some((owner, _, _)) if owner != *tenant => {
                        return Err(BudgetError::TenantMismatch {
                            reservation_id: id.clone(),
                        });
                    }
                    Some((_, status, _)) if status == "released" => {
                        obsolete_outcomes.push(id.clone());
                    }
                    Some((_, status, Some(recorded)))
                        if status == "settled" && recorded == *actual =>
                    {
                        obsolete_outcomes.push(id.clone());
                    }
                    Some((_, status, Some(recorded))) if status == "settled" => {
                        // Another instance may have recovered the journal's
                        // authoritative amount after this instance cached a
                        // conflicting Busy fallback. The terminal primary row
                        // decides the outcome; quarantine this stale cache so
                        // it cannot make every later reserve fail.
                        eprintln!(
                            "discarded conflicting terminal settlement cache: reservation={id} cached={actual} settled={recorded}"
                        );
                        obsolete_outcomes.push(id.clone());
                    }
                    Some((_, status, _)) if matches!(status.as_str(), "active" | "expired") => {}
                    Some(_) => {
                        return Err(BudgetError::Storage(
                            "in-memory settlement outcome conflicts with primary".into(),
                        ));
                    }
                    None => {
                        return Err(BudgetError::Storage(
                            "in-memory settlement reservation is missing".into(),
                        ));
                    }
                }
            }
        }
        for id in obsolete_outcomes {
            outcomes.remove(&id);
        }
        // A terminal outcome may have committed while intent deletion hit
        // journal Busy. Complete that deletion on the next retry.
        let intents = {
            let mut stmt =
                tx.prepare("SELECT reservation_id, tenant_id FROM settlement_intents")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let finished_intents = {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let mut released = Vec::new();
            for (id, tenant) in intents {
                let status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM reservations WHERE id=?1 AND tenant_id=?2",
                        params![id, tenant],
                        |r| r.get(0),
                    )
                    .optional()?;
                if matches!(status.as_deref(), Some("released" | "settled")) {
                    released.push((id, tenant));
                }
            }
            released
        };
        for (id, tenant) in finished_intents {
            tx.execute(
                "DELETE FROM settlement_intents WHERE reservation_id=?1 AND tenant_id=?2",
                params![id, tenant],
            )?;
        }
        for (id, (tenant, actual)) in outcomes.iter() {
            match insert_pending(&tx, id, tenant, *actual) {
                Ok(()) => {}
                Err(PendingInsertError::Conflict) => {
                    if !self.discard_conflicting_fallback(&tx, id, tenant, *actual)? {
                        return Err(BudgetError::Storage(
                            "settlement journal outcome mismatch".into(),
                        ));
                    }
                }
                Err(error) => return Err(error.into_budget_error()),
            }
        }
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT reservation_id, tenant_id, actual_cost_minor
                FROM pending_settlements",
            )?;
            stmt.query_map([], |r| {
                Ok((
                    ReservationId(r.get(0)?),
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        for (id, tenant, actual) in rows {
            match self.settle(&tenant, &id, actual) {
                Ok(_) => {}
                Err(err @ BudgetError::OverageTooLarge { .. }) => {
                    // Already committed; retrying must not double charge.
                    eprintln!("budget settlement overage: {err}");
                }
                Err(BudgetError::ReservationNotActive { status, .. }) if status == "settled" => {
                    // Crash after ledger commit but before journal deletion.
                    let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                    let recorded: i64 = conn.query_row(
                        "SELECT actual_cost_minor FROM reservations
                        WHERE id=?1 AND tenant_id=?2",
                        params![id.0, tenant],
                        |r| r.get(0),
                    )?;
                    if recorded != actual {
                        return Err(BudgetError::Storage("settlement cost mismatch".into()));
                    }
                }
                Err(err) => return Err(err),
            }
            tx.execute(
                "DELETE FROM pending_settlements WHERE reservation_id=?1",
                [&id.0],
            )?;
            tx.execute(
                "DELETE FROM settlement_intents WHERE reservation_id=?1 AND tenant_id=?2",
                params![id.0, tenant],
            )?;
        }
        tx.commit()?;
        outcomes.clear();
        Ok(())
    }

    /// A durable journal entry wins over a stale in-memory or primary-ledger
    /// fallback after a conflicting submission could not read the journal.
    /// Never rewrite a terminal reservation or accept a tenant mismatch.
    fn discard_conflicting_fallback(
        &self,
        journal: &Connection,
        id: &str,
        tenant: &str,
        fallback_actual: i64,
    ) -> Result<bool, BudgetError> {
        let recorded: Option<(String, i64)> = journal
            .query_row(
                "SELECT tenant_id, actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((recorded_tenant, recorded_actual)) = recorded else {
            return Ok(false);
        };
        if recorded_tenant != tenant {
            return Err(BudgetError::TenantMismatch {
                reservation_id: id.to_string(),
            });
        }
        if recorded_actual == fallback_actual {
            return Ok(false);
        }
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let cleared = conn.execute(
            "UPDATE reservations SET actual_cost_minor=NULL
             WHERE id=?1 AND tenant_id=?2 AND status IN ('active','expired')
               AND actual_cost_minor=?3",
            params![id, tenant, fallback_actual],
        )?;
        if cleared == 1 {
            eprintln!(
                "discarded conflicting settlement fallback: reservation={id} fallback={fallback_actual} journal={recorded_actual}"
            );
            return Ok(true);
        }
        let primary: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT status, actual_cost_minor FROM reservations WHERE id=?1 AND tenant_id=?2",
                params![id, tenant],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let disposable = match primary {
            Some((status, actual)) if matches!(status.as_str(), "active" | "expired") => {
                actual.is_none() || actual == Some(recorded_actual)
            }
            Some((status, actual)) => status == "settled" && actual == Some(recorded_actual),
            None => false,
        };
        if !disposable {
            return Err(BudgetError::Storage(
                "conflicting settlement fallback could not be cleared".into(),
            ));
        }
        eprintln!(
            "discarded stale settlement outcome: reservation={id} fallback={fallback_actual} journal={recorded_actual}"
        );
        Ok(true)
    }
}

enum PendingInsertError {
    Conflict,
    Storage(BudgetError),
}

/// Store a known amount on the primary row if the independent journal is
/// unavailable. `Ok(false)` means a release already reached its terminal state.
fn store_primary_fallback(
    conn: &Connection,
    id: &str,
    tenant: &str,
    actual: i64,
) -> Result<bool, BudgetError> {
    let updated = conn.execute(
        "UPDATE reservations SET actual_cost_minor=?3
         WHERE id=?1 AND tenant_id=?2 AND status IN ('active','expired')
           AND (actual_cost_minor IS NULL OR actual_cost_minor=?3)",
        params![id, tenant, actual],
    )?;
    if updated == 1 {
        return Ok(true);
    }
    let primary: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1 AND tenant_id=?2",
            params![id, tenant],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match primary {
        Some((status, _)) if status == "released" => Ok(false),
        Some((status, Some(recorded))) if recorded == actual && status == "settled" => Ok(true),
        Some(_) => Err(BudgetError::Storage(
            "primary settlement fallback did not match an active outcome".into(),
        )),
        None => Err(BudgetError::Storage(
            "primary settlement fallback reservation is missing".into(),
        )),
    }
}

impl PendingInsertError {
    fn into_budget_error(self) -> BudgetError {
        match self {
            Self::Conflict => BudgetError::Storage("settlement journal outcome mismatch".into()),
            Self::Storage(error) => error,
        }
    }
}

fn insert_pending(
    journal: &Connection,
    id: &str,
    tenant: &str,
    actual: i64,
) -> Result<(), PendingInsertError> {
    let existing: Option<(String, i64)> = journal
        .query_row(
            "SELECT tenant_id, actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|error| PendingInsertError::Storage(error.into()))?;
    if let Some(recorded) = existing
        && recorded != (tenant.to_string(), actual)
    {
        return Err(PendingInsertError::Conflict);
    }
    journal
        .execute(
            "INSERT INTO pending_settlements VALUES (?1, ?2, ?3)
         ON CONFLICT(reservation_id) DO NOTHING",
            params![id, tenant, actual],
        )
        .map_err(|error| PendingInsertError::Storage(error.into()))?;
    let recorded: (String, i64) = journal
        .query_row(
            "SELECT tenant_id, actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|error| PendingInsertError::Storage(error.into()))?;
    if recorded != (tenant.to_string(), actual) {
        return Err(PendingInsertError::Conflict);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::budget::{BudgetScope, Price, SpendGate};
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };

    struct TestClock(AtomicI64);

    impl crate::budget::clock::Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct TestDb(std::path::PathBuf);

    impl TestDb {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("idoris-release-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> std::path::PathBuf {
            self.0.join("budget.sqlite3")
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn reserved() -> (BudgetLedger, ReservationId) {
        let ledger = BudgetLedger::open(":memory:").unwrap();
        ledger
            .configure_tenant("t", 1000, "UTC", SpendGate::All)
            .unwrap();
        let id = ledger
            .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
            .unwrap();
        (ledger, id)
    }

    fn pending(ledger: &BudgetLedger) -> i64 {
        ledger
            .settlements
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM pending_settlements", [], |r| r.get(0))
            .unwrap()
    }

    fn intents(ledger: &BudgetLedger) -> i64 {
        ledger
            .settlements
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM settlement_intents", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn live_intents_allow_same_and_other_tenant_reservations_while_holding_budget() {
        let (ledger, id) = reserved();
        ledger
            .configure_tenant("other", 1000, "UTC", SpendGate::All)
            .unwrap();
        ledger.begin_settlement("t", &id).unwrap();
        assert!(
            ledger
                .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
                .is_ok()
        );
        assert!(
            ledger
                .reserve(&BudgetScope::new("other", "k", "p", "m"), Price::Known(10))
                .is_ok()
        );
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
    }

    #[test]
    fn intent_is_reserved_after_ttl_and_unknown_intent_only_blocks_its_tenant() {
        let clock = Arc::new(TestClock(AtomicI64::new(100)));
        let ledger = BudgetLedger::open_with(":memory:", clock.clone(), 10).unwrap();
        ledger
            .configure_tenant("t", 1000, "UTC", SpendGate::All)
            .unwrap();
        ledger
            .configure_tenant("other", 1000, "UTC", SpendGate::All)
            .unwrap();
        let id = ledger
            .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
            .unwrap();
        ledger.begin_settlement("t", &id).unwrap();
        clock.0.store(120, Ordering::SeqCst);
        assert_eq!(ledger.sweep_expired().unwrap(), 0);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 990);
        let scope = BudgetScope::new("t", "k", "p", "m");
        assert!(matches!(
            ledger.reserve(&scope, Price::Known(991)),
            Err(BudgetError::Exceeded { .. })
        ));
        let concurrent = ledger.reserve(&scope, Price::Known(1)).unwrap();
        ledger.release("t", &concurrent).unwrap();
        ledger.live_intents.lock().unwrap().remove(&id.0);
        assert!(matches!(
            ledger.reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(1)),
            Err(BudgetError::Storage(_))
        ));
        assert!(
            ledger
                .reserve(&BudgetScope::new("other", "k", "p", "m"), Price::Known(1))
                .is_ok()
        );
    }

    #[test]
    fn storage_rollback_keeps_actual_cost_and_blocks_new_spending_until_recovery() {
        let (ledger, id) = reserved();
        ledger
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_settle
            BEFORE UPDATE OF status ON reservations WHEN NEW.status='settled'
            BEGIN SELECT RAISE(ABORT, 'injected storage error'); END;",
            )
            .unwrap();
        assert_eq!(ledger.settle_durable("t", &id, 20).unwrap(), None);
        assert_eq!(pending(&ledger), 1);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 990);
        assert!(matches!(
            ledger.reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(1)),
            Err(BudgetError::Storage(_))
        ));
        ledger
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_settle")
            .unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(pending(&ledger), 0);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
    }

    #[test]
    fn primary_read_failure_keeps_verified_dispatch_cost_for_recovery() {
        let (ledger, id) = reserved();
        ledger
            .configure_tenant("other", 1000, "UTC", SpendGate::All)
            .unwrap();
        ledger.begin_settlement("t", &id).unwrap();

        // Make the ownership/status SELECT fail while leaving the independent
        // settlement journal available for the completed upstream result.
        ledger
            .conn
            .lock()
            .unwrap()
            .execute_batch("ALTER TABLE reservations RENAME TO reservations_unavailable")
            .unwrap();
        assert!(matches!(
            ledger.settle_durable("other", &id, 20),
            Err(BudgetError::TenantMismatch { .. })
        ));
        assert_eq!(ledger.settle_durable("t", &id, 20).unwrap(), None);
        assert_eq!(pending(&ledger), 1);
        assert!(!ledger.live_intents.lock().unwrap().contains_key(&id.0));

        ledger
            .conn
            .lock()
            .unwrap()
            .execute_batch("ALTER TABLE reservations_unavailable RENAME TO reservations")
            .unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(pending(&ledger), 0);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
        assert_eq!(ledger.tenant_balance("other").unwrap(), 1000);
    }

    #[test]
    fn cancelled_release_survives_busy_ttl_and_restart_without_charging() {
        for restart in [false, true] {
            let db = TestDb::new();
            let clock = Arc::new(TestClock(AtomicI64::new(100)));
            let ledger =
                BudgetLedger::open_with_busy_timeout(db.path(), clock.clone(), 10, Duration::ZERO)
                    .unwrap();
            ledger
                .configure_tenant("t", 1000, "UTC", SpendGate::All)
                .unwrap();
            let scope = BudgetScope::new("t", "k", "p", "m");
            let id = ledger.reserve(&scope, Price::Known(100)).unwrap();
            ledger.begin_settlement("t", &id).unwrap();
            let blocker = Connection::open(db.path()).unwrap();
            blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
            assert!(matches!(ledger.release("t", &id), Err(BudgetError::Busy)));
            let queued: i64 = ledger
                .settlements
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM pending_releases", [], |r| r.get(0))
                .unwrap();
            assert_eq!(queued, 1);
            assert!(matches!(ledger.retry_settlements(), Err(BudgetError::Busy)));
            assert_eq!(ledger.tenant_balance("t").unwrap(), 900);
            clock.0.store(120, Ordering::SeqCst);
            blocker.execute_batch("ROLLBACK").unwrap();
            let recovered = if restart {
                drop(ledger);
                BudgetLedger::open_with(db.path(), clock.clone(), 10).unwrap()
            } else {
                ledger
            };
            for _ in 0..2 {
                recovered.retry_settlements().unwrap();
                assert_eq!(recovered.tenant_balance("t").unwrap(), 1000);
                assert_eq!((pending(&recovered), intents(&recovered)), (0, 0));
            }
            let (status, actual): (String, Option<i64>) = recovered
                .conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
                    [&id.0],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((status.as_str(), actual), ("released", None));
            let queued: i64 = recovered
                .settlements
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM pending_releases", [], |r| r.get(0))
                .unwrap();
            assert_eq!(queued, 0);
            let next = recovered.reserve(&scope, Price::Known(1000)).unwrap();
            recovered.release("t", &next).unwrap();
        }
    }

    #[test]
    fn cancellation_retries_when_both_outcome_stores_were_busy() {
        let db = TestDb::new();
        let ledger = BudgetLedger::open_with_busy_timeout(
            db.path(),
            Arc::new(TestClock(AtomicI64::new(100))),
            10,
            Duration::ZERO,
        )
        .unwrap();
        ledger
            .configure_tenant("t", 1000, "UTC", SpendGate::All)
            .unwrap();
        let id = ledger
            .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(100))
            .unwrap();
        ledger.begin_settlement("t", &id).unwrap();
        let main = Connection::open(db.path()).unwrap();
        let journal =
            Connection::open(db.path().with_added_extension("settlements.sqlite3")).unwrap();
        main.execute_batch("BEGIN IMMEDIATE").unwrap();
        journal.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(matches!(ledger.release("t", &id), Err(BudgetError::Busy)));
        journal.execute_batch("ROLLBACK").unwrap();
        main.execute_batch("ROLLBACK").unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(ledger.tenant_balance("t").unwrap(), 1000);
        assert_eq!(intents(&ledger), 0);
    }

    #[test]
    fn release_rejects_foreign_tenants_and_preserves_completed_usage() {
        let (ledger, id) = reserved();
        ledger.begin_settlement("t", &id).unwrap();
        assert!(matches!(
            ledger.release("other", &id),
            Err(BudgetError::TenantMismatch { .. })
        ));
        ledger.settlements.lock().unwrap().execute_batch("CREATE TRIGGER fail_journal BEFORE INSERT ON pending_settlements BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END;").unwrap();
        assert!(ledger.settle_durable("t", &id, 20).is_err());
        assert!(ledger.release("t", &id).is_err());
        let queued: i64 = ledger
            .settlements
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM pending_releases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(queued, 0);
        ledger
            .settlements
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_journal")
            .unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
    }

    #[test]
    fn settlement_winning_a_release_race_does_not_leave_a_stuck_release() {
        let (ledger, id) = reserved();
        ledger.begin_settlement("t", &id).unwrap();
        // Another connection can settle after release queued its outcome
        // but before release acquires the primary write transaction.
        ledger
            .settlements
            .lock()
            .unwrap()
            .execute("INSERT INTO pending_releases VALUES (?1, 't')", [&id.0])
            .unwrap();
        ledger.settle("t", &id, 20).unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
        assert_eq!(intents(&ledger), 0);
        let queued: i64 = ledger
            .settlements
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM pending_releases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(queued, 0);
        assert!(
            ledger
                .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(1))
                .is_ok()
        );
    }

    #[test]
    fn committed_overage_is_observable_but_never_queued_for_another_charge() {
        let (ledger, id) = reserved();
        assert_eq!(ledger.settle_durable("t", &id, 50).unwrap(), Some(50));
        assert_eq!(pending(&ledger), 0);
        ledger.retry_settlements().unwrap();
        assert_eq!(ledger.tenant_balance("t").unwrap(), 950);
    }

    #[test]
    fn replay_after_commit_only_deletes_matching_results() {
        let (ledger, id) = reserved();
        ledger.settle("t", &id, 20).unwrap();
        for actual in [21, 20] {
            ledger
                .settlements
                .lock()
                .unwrap()
                .execute(
                    "INSERT OR REPLACE INTO pending_settlements VALUES (?1, 't', ?2)",
                    params![id.0, actual],
                )
                .unwrap();
            let result = ledger.retry_settlements();
            if actual == 21 {
                assert!(matches!(result, Err(BudgetError::Storage(_))));
                assert_eq!(pending(&ledger), 1);
            } else {
                result.unwrap();
                assert_eq!(pending(&ledger), 0);
            }
            assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
        }
    }

    #[test]
    fn journal_failure_is_an_error_and_does_not_release_completed_usage() {
        let (ledger, id) = reserved();
        ledger.begin_settlement("t", &id).unwrap();
        ledger
            .settlements
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_journal
            BEFORE INSERT ON pending_settlements
            BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END;",
            )
            .unwrap();
        assert!(matches!(
            ledger.settle_durable("t", &id, 20),
            Err(BudgetError::Storage(_))
        ));
        assert_eq!(ledger.tenant_balance("t").unwrap(), 990);
        let fallback_actual: i64 = ledger
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fallback_actual, 20);
        ledger
            .settlements
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_journal")
            .unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
        assert_eq!(intents(&ledger), 0);
    }

    #[test]
    fn releasing_clears_intent_and_retry_finishes_cleanup_after_journal_busy() {
        let (ledger, id) = reserved();
        ledger.begin_settlement("t", &id).unwrap();
        ledger
            .settlements
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_intent_delete BEFORE DELETE ON settlement_intents
                 BEGIN SELECT RAISE(ABORT, 'injected journal error'); END;",
            )
            .unwrap();
        assert!(matches!(
            ledger.release("t", &id),
            Err(BudgetError::Storage(_))
        ));
        assert_eq!(intents(&ledger), 1);
        ledger
            .settlements
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_intent_delete")
            .unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(intents(&ledger), 0);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 1000);
    }

    #[test]
    fn one_completed_intent_is_committed_while_another_unknown_intent_stays_blocked() {
        let (ledger, first) = reserved();
        let second = ledger
            .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
            .unwrap();
        ledger.begin_settlement("t", &first).unwrap();
        ledger.begin_settlement("t", &second).unwrap();
        assert_eq!(ledger.settle_durable("t", &first, 20).unwrap(), Some(20));
        assert_eq!(ledger.tenant_balance("t").unwrap(), 970);
        assert_eq!(intents(&ledger), 1);
        ledger.live_intents.lock().unwrap().remove(&second.0);
        assert!(matches!(
            ledger.reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(1)),
            Err(BudgetError::Storage(_))
        ));
        ledger.release("t", &second).unwrap();
        ledger.retry_settlements().unwrap();
        assert_eq!(intents(&ledger), 0);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
    }

    #[test]
    fn conflicting_outcome_never_overwrites_existing_journal_record() {
        let (ledger, id) = reserved();
        ledger
            .settlements
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO pending_settlements VALUES (?1, 't', 20)",
                [&id.0],
            )
            .unwrap();
        assert!(matches!(
            ledger.settle_durable("t", &id, 21),
            Err(BudgetError::Storage(_))
        ));
        assert_eq!(pending(&ledger), 1);
        let recorded: i64 = ledger
            .settlements
            .lock()
            .unwrap()
            .query_row(
                "SELECT actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 20);
    }

    #[test]
    fn conflicting_outcome_does_not_poison_recovery_or_other_tenants() {
        for retry_before_restart in [false, true] {
            let db = TestDb::new();
            let path = db.path();
            let ledger = BudgetLedger::open(&path).unwrap();
            ledger
                .configure_tenant("t", 1000, "UTC", SpendGate::All)
                .unwrap();
            ledger
                .configure_tenant("other", 1000, "UTC", SpendGate::All)
                .unwrap();
            let id = ledger
                .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
                .unwrap();
            ledger.begin_settlement("t", &id).unwrap();
            ledger
                .settlements
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO pending_settlements VALUES (?1, 't', 20)",
                    [&id.0],
                )
                .unwrap();

            assert!(matches!(
                ledger.settle_durable("t", &id, 21),
                Err(BudgetError::Storage(_))
            ));
            assert!(
                !ledger
                    .settlement_outcomes
                    .lock()
                    .unwrap()
                    .contains_key(&id.0)
            );
            let primary: Option<i64> = ledger
                .conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT actual_cost_minor FROM reservations WHERE id=?1",
                    [&id.0],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(primary, None);
            if retry_before_restart {
                assert!(
                    ledger
                        .reserve(&BudgetScope::new("other", "k", "p", "m"), Price::Known(10))
                        .is_ok()
                );
                assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
            }
            drop(ledger);

            let recovered = BudgetLedger::open(&path).unwrap();
            assert_eq!(recovered.tenant_balance("t").unwrap(), 980);
            assert_eq!(
                recovered.tenant_balance("other").unwrap(),
                if retry_before_restart { 990 } else { 1000 }
            );
            if !retry_before_restart {
                assert!(
                    recovered
                        .reserve(&BudgetScope::new("other", "k", "p", "m"), Price::Known(10))
                        .is_ok()
                );
            }
            assert_eq!(recovered.tenant_balance("t").unwrap(), 980);
            // A settled primary record must reject conflicting resubmissions too.
            assert!(recovered.settle_durable("t", &id, 21).is_err());
            assert_eq!(pending(&recovered), 0);
            assert_eq!(recovered.settle_durable("t", &id, 20).unwrap(), Some(20));
            drop(recovered);

            let recovered_again = BudgetLedger::open(&path).unwrap();
            assert_eq!(recovered_again.tenant_balance("t").unwrap(), 980);
            assert_eq!(recovered_again.tenant_balance("other").unwrap(), 990);
        }
    }

    #[test]
    fn exclusive_journal_lock_conflict_recovers_authoritative_cost() {
        for restart_first in [false, true] {
            for primary_locked in [false, true] {
                let db = TestDb::new();
                let path = db.path();
                let timeout = Duration::from_millis(10);
                let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
                let ledger =
                    BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 10, timeout)
                        .unwrap();
                ledger
                    .configure_tenant("t", 1000, "UTC", SpendGate::All)
                    .unwrap();
                ledger
                    .configure_tenant("other", 1000, "UTC", SpendGate::All)
                    .unwrap();
                let id = ledger
                    .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
                    .unwrap();
                ledger.begin_settlement("t", &id).unwrap();
                ledger
                    .settlements
                    .lock()
                    .unwrap()
                    .execute(
                        "INSERT INTO pending_settlements VALUES (?1, 't', 20)",
                        [&id.0],
                    )
                    .unwrap();

                let journal_path = path.with_added_extension("settlements.sqlite3");
                let blocker = Connection::open(journal_path).unwrap();
                blocker.busy_timeout(timeout).unwrap();
                blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
                let primary_blocker = if primary_locked {
                    let conn = Connection::open(&path).unwrap();
                    conn.busy_timeout(timeout).unwrap();
                    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                    Some(conn)
                } else {
                    None
                };
                assert!(matches!(
                    ledger.settle_durable("t", &id, 21),
                    Err(BudgetError::Busy)
                ));
                if let Some(primary_blocker) = primary_blocker {
                    primary_blocker.execute_batch("ROLLBACK").unwrap();
                }
                blocker.execute_batch("ROLLBACK").unwrap();
                drop(blocker);

                if restart_first {
                    drop(ledger);
                } else {
                    ledger.retry_settlements().unwrap();
                    assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
                    drop(ledger);
                }
                let recovered =
                    BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 10, timeout)
                        .unwrap();
                assert_eq!(recovered.tenant_balance("t").unwrap(), 980);
                assert!(
                    recovered
                        .reserve(&BudgetScope::new("other", "k", "p", "m"), Price::Known(10))
                        .is_ok()
                );
                assert_eq!(recovered.tenant_balance("t").unwrap(), 980);
                drop(recovered);
            }
        }
    }

    #[test]
    fn another_instance_recovery_quarantines_conflicting_busy_cache() {
        let db = TestDb::new();
        let path = db.path();
        let timeout = Duration::from_millis(10);
        let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
        let a = BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 10, timeout).unwrap();
        a.configure_tenant("t", 1000, "UTC", SpendGate::All)
            .unwrap();
        a.configure_tenant("other", 1000, "UTC", SpendGate::All)
            .unwrap();
        let id = a
            .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
            .unwrap();
        a.begin_settlement("t", &id).unwrap();
        a.settlements
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO pending_settlements VALUES (?1, 't', 20)",
                [&id.0],
            )
            .unwrap();

        let journal_path = path.with_added_extension("settlements.sqlite3");
        let blocker = Connection::open(journal_path).unwrap();
        blocker.busy_timeout(timeout).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        assert!(matches!(
            a.settle_durable("t", &id, 21),
            Err(BudgetError::Busy)
        ));
        assert_eq!(
            a.settlement_outcomes.lock().unwrap().get(&id.0),
            Some(&("t".to_string(), 21))
        );
        blocker.execute_batch("ROLLBACK").unwrap();
        drop(blocker);

        // Instance B recovers the authoritative journal amount before A gets
        // another chance to retry its stale in-memory Busy fallback.
        let b = BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 10, timeout).unwrap();
        b.retry_settlements().unwrap();
        assert_eq!(b.tenant_balance("t").unwrap(), 980);
        assert_eq!(pending(&b), 0);
        let authoritative: (String, Option<i64>) = b
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(authoritative, ("settled".to_string(), Some(20)));

        a.retry_settlements().unwrap();
        assert!(!a.settlement_outcomes.lock().unwrap().contains_key(&id.0));
        assert_eq!(a.tenant_balance("t").unwrap(), 980);
        assert!(
            a.reserve(&BudgetScope::new("other", "k", "p", "m"), Price::Known(10))
                .is_ok()
        );
        a.retry_settlements().unwrap();
        b.retry_settlements().unwrap();
        assert_eq!(a.tenant_balance("t").unwrap(), 980);
        assert_eq!(b.tenant_balance("t").unwrap(), 980);
        drop(a);
        drop(b);

        let recovered = BudgetLedger::open_with_busy_timeout(&path, clock, 10, timeout).unwrap();
        recovered.retry_settlements().unwrap();
        assert_eq!(recovered.tenant_balance("t").unwrap(), 980);
        assert_eq!(recovered.tenant_balance("other").unwrap(), 990);
    }

    #[test]
    fn foreign_tenant_cannot_poison_the_journal() {
        let (ledger, id) = reserved();
        assert!(matches!(
            ledger.settle_durable("other", &id, 20),
            Err(BudgetError::TenantMismatch { .. })
        ));
        assert_eq!(pending(&ledger), 0);
    }

    #[test]
    fn journal_busy_fallback_survives_ttl_and_restart() {
        let path = std::env::temp_dir().join(format!(
            "idoris-settlement-recovery-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let clock = std::sync::Arc::new(TestClock(AtomicI64::new(1_000)));
        let ledger = BudgetLedger::open_with_busy_timeout(
            &path,
            clock.clone(),
            10,
            Duration::from_millis(10),
        )
        .unwrap();
        ledger
            .configure_tenant("t", 1000, "UTC", SpendGate::All)
            .unwrap();
        let id = ledger
            .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
            .unwrap();
        ledger.begin_settlement("t", &id).unwrap();

        let journal_path = path.with_added_extension("settlements.sqlite3");
        let blocker = Connection::open(journal_path).unwrap();
        blocker.busy_timeout(Duration::from_millis(10)).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(matches!(
            ledger.settle_durable("t", &id, 20),
            Err(BudgetError::Busy)
        ));
        drop(blocker);
        drop(ledger);

        // The primary ledger now contains the real cost even though the
        // journal could not accept it. Recovery runs at startup after TTL.
        clock.0.store(1_100, Ordering::SeqCst);
        let recovered =
            BudgetLedger::open_with_busy_timeout(&path, clock, 10, Duration::from_millis(10))
                .unwrap();
        assert_eq!(recovered.tenant_balance("t").unwrap(), 980);
        assert_eq!(pending(&recovered), 0);
        assert_eq!(intents(&recovered), 0);

        drop(recovered);
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.clone().into_os_string();
            file.push(suffix);
            let _ = std::fs::remove_file(file);
            let mut file = path
                .with_added_extension("settlements.sqlite3")
                .into_os_string();
            file.push(suffix);
            let _ = std::fs::remove_file(file);
        }
    }
}

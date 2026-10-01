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
        CREATE TABLE IF NOT EXISTS quarantined_settlements (
        reservation_id TEXT NOT NULL, tenant_id TEXT NOT NULL,
        actual_cost_minor INTEGER NOT NULL, reason TEXT NOT NULL,
        quarantined_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        PRIMARY KEY (reservation_id, tenant_id));
        CREATE TABLE IF NOT EXISTS settlement_intents (
            reservation_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL
        );",
    )?;
    Ok(conn)
}

impl BudgetLedger {
    fn persist_emergency_settlement(
        &self,
        id: &ReservationId,
        tenant: &str,
        actual: i64,
    ) -> Result<(), BudgetError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let changed = conn.execute(
            "INSERT INTO budget_emergency_settlements VALUES (?1, ?2, ?3)
             ON CONFLICT(reservation_id) DO UPDATE SET actual_cost_minor=excluded.actual_cost_minor
             WHERE tenant_id=excluded.tenant_id AND actual_cost_minor=excluded.actual_cost_minor",
            params![id.0, tenant, actual],
        )?;
        if changed != 1 {
            return Err(BudgetError::SettlementConflict {
                reservation_id: id.0.clone(),
                reason: "primary recovery already contains a different tenant or cost".into(),
            });
        }
        Ok(())
    }

    fn emergency_cost(&self, id: &ReservationId, tenant: &str) -> Result<Option<i64>, BudgetError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row(
            "SELECT actual_cost_minor FROM budget_emergency_settlements WHERE reservation_id=?1 AND tenant_id=?2",
            params![id.0, tenant], |row| row.get(0),
        ).optional().map_err(BudgetError::from)
    }

    /// Persist before attempting settlement. None means durable retry is
    /// pending. Errors report failed persistence or an invalid outcome, except
    /// `OverageTooLarge`, which confirms that the actual charge committed.
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
        if let Some((owner, queued)) = self
            .emergency_settlements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
            && (owner != tenant || queued != actual)
        {
            if owner != tenant {
                return Err(BudgetError::TenantMismatch {
                    reservation_id: id.0.clone(),
                });
            }
            return Err(BudgetError::SettlementConflict {
                reservation_id: id.0.clone(),
                reason: "in-memory recovery already contains a different tenant or cost".into(),
            });
        }
        if self
            .unverified_settlements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(id.clone(), tenant.to_owned()))
            .is_some_and(|queued| *queued != actual)
        {
            return Err(BudgetError::SettlementConflict {
                reservation_id: id.0.clone(),
                reason: "ownership verification is pending for a different cost".into(),
            });
        }
        // Validate ownership before the sidecar transaction can return Busy.
        // In that path the caller's cost is kept in memory for recovery, so
        // only a tenant already verified against the primary ledger may reach
        // that fallback. Drop the primary connection guard before acquiring
        // the sidecar lock below to preserve the sidecar -> primary lock order.
        let state = {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            conn.query_row(
                "SELECT tenant_id FROM reservations WHERE id=?1",
                [&id.0],
                |row| row.get::<_, String>(0),
            )
        };
        match state {
            Ok(owner) if owner == tenant => {}
            Ok(_) => {
                return Err(BudgetError::TenantMismatch {
                    reservation_id: id.0.clone(),
                });
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(BudgetError::ReservationNotFound {
                    reservation_id: id.0.clone(),
                });
            }
            Err(err) => {
                // The upstream call already completed. Keep its cost without
                // trusting the caller's tenant until ownership can be read.
                // Dropping the live marker makes the durable intent fail
                // closed until the result can be verified and recovered.
                let mut pending = self
                    .unverified_settlements
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                pending
                    .entry((id.clone(), tenant.to_owned()))
                    .or_insert(actual);
                drop(pending);
                self.finish_settlement_intent(id);
                return Err(err.into());
            }
        }
        // The caller's upstream request has completed. Once ownership is
        // verified, stop treating it as live before any sidecar operation can
        // fail (for example, BEGIN IMMEDIATE returning Busy). The durable
        // intent remains as the fail-closed admission fence until recovery.
        self.finish_settlement_intent(id);
        let result = self.settle_durable_with_hook(tenant, id, actual, || {});
        if matches!(result, Err(BudgetError::Busy | BudgetError::Storage(_))) && actual >= 0 {
            let mut memory = self
                .emergency_settlements
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some((queued_tenant, queued_actual)) = memory.get(id) {
                if queued_tenant != tenant || *queued_actual != actual {
                    return Err(BudgetError::SettlementConflict {
                        reservation_id: id.0.clone(),
                        reason: format!(
                            "in-memory recovery cost is {queued_tenant}/{queued_actual}, requested {tenant}/{actual}"
                        ),
                    });
                }
            } else {
                memory.insert(id.clone(), (tenant.to_owned(), actual));
            }
        }
        result
    }

    /// Test seam for deterministically coordinating release against the
    /// active-state read while the sidecar's cross-instance writer lock is
    /// held. The callback is instance-local and absent from production paths.
    pub(super) fn settle_durable_with_hook<F: FnOnce()>(
        &self,
        tenant: &str,
        id: &ReservationId,
        actual: i64,
        after_active_read: F,
    ) -> Result<Option<i64>, BudgetError> {
        if actual < 0 {
            return Err(BudgetError::InvalidActualCost {
                actual_cost_minor: actual,
            });
        }
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let tx = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // The sidecar writer lock is acquired before touching the ledger in
        // every settlement/release path, coordinating independent processes.
        let state = {
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
        let (owner, status, recorded) = match state {
            Ok(state) => state,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(BudgetError::ReservationNotFound {
                    reservation_id: id.0.clone(),
                });
            }
            Err(err) => return Err(err.into()),
        };
        if owner != tenant {
            return Err(BudgetError::TenantMismatch {
                reservation_id: id.0.clone(),
            });
        }
        if let Some(queued) = self.emergency_cost(id, tenant)?
            && queued != actual
        {
            return Err(BudgetError::SettlementConflict {
                reservation_id: id.0.clone(),
                reason: format!("emergency recovery cost is {queued}, requested {actual}"),
            });
        }
        self.finish_settlement_intent(id);
        match status.as_str() {
            "released" => {
                return Err(BudgetError::ReservationNotActive {
                    reservation_id: id.0.clone(),
                    status,
                });
            }
            "settled" if recorded == Some(actual) => {
                // Recovery will remove or quarantine any stale journal row.
                // Its cleanup cannot change this already committed outcome.
                return Ok(Some(actual));
            }
            "settled" => {
                return Err(BudgetError::SettlementConflict {
                    reservation_id: id.0.clone(),
                    reason: format!("already settled at {:?}, requested {actual}", recorded),
                });
            }
            "active" | "expired" => {}
            _ => {
                return Err(BudgetError::Storage(format!(
                    "unknown reservations.status value {status:?}"
                )));
            }
        }
        after_active_read();
        if let Some((queued_tenant, queued)) = tx.query_row(
            "SELECT tenant_id, actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
            [&id.0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        ).optional()? {
            if queued_tenant != tenant || queued != actual {
                return Err(BudgetError::SettlementConflict { reservation_id: id.0.clone(), reason: format!("pending tenant/cost is {queued_tenant}/{queued}, requested {tenant}/{actual}") });
            }
        } else {
            if let Err(err) = tx.execute(
                "INSERT INTO pending_settlements VALUES (?1, ?2, ?3)",
                params![id.0, tenant, actual],
            ) {
                // Keep the coordinator transaction active while applying the
                // primary fallback, so another process cannot release first.
                // SQLITE_FULL/IOERR or a RAISE(ROLLBACK) trigger may have
                // rolled back implicitly; a Rust Transaction alone is no lock.
                if tx.is_autocommit() {
                    return Err(err.into());
                }
                return match self.settle_uncoordinated(tenant, id, actual) {
                    Ok(_) => Ok(Some(actual)),
                    Err(overage @ BudgetError::OverageTooLarge { .. }) => Err(overage),
                    Err(primary_err) => {
                        let persisted = self.persist_emergency_settlement(id, tenant, actual);
                        let _ = tx.rollback();
                        match persisted {
                            Ok(()) => {
                                eprintln!("budget settlement stored in primary recovery journal: reservation={} sidecar={err} primary={primary_err}", id.0);
                                Ok(None)
                            }
                            Err(persist_err) => Err(BudgetError::Storage(format!(
                                "settlement sidecar enqueue failed ({err}); primary fallback failed ({primary_err}); emergency journal failed ({})",
                                persist_err
                            ))),
                        }
                    }
                };
            }
        }
        if let Err(err) = tx.commit() {
            // The transaction is consumed and the cross-process lock is gone.
            // Keep the intent active and retain the actual in memory; reserve
            // retries durable persistence before admitting more spend.
            return Err(BudgetError::Storage(format!(
                "settlement sidecar commit failed: {err}"
            )));
        } // pending is durable before touching the primary ledger
        drop(journal);
        match self.retry_one(id, tenant, actual) {
            Ok(()) => Ok(Some(actual)),
            Err(err @ (BudgetError::Busy | BudgetError::Storage(_))) => {
                eprintln!("budget settlement queued: reservation={} error={err}", id.0);
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Recover every tenant on startup or when called explicitly. Reserve
    /// uses the tenant-scoped variant before admitting new spending.
    /// A journal write transaction serializes recovery across processes.
    pub fn retry_settlements(&self) -> Result<(), BudgetError> {
        self.retry_settlements_for_tenant(None)
    }

    pub(super) fn retry_settlements_for_tenant(
        &self,
        tenant_scope: Option<&str>,
    ) -> Result<(), BudgetError> {
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let tx = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Recheck ownership before promoting any in-memory result into the
        // durable emergency journal. A failed read leaves the result here and
        // aborts recovery, keeping reserve fail-closed through the intent.
        let unverified_rows: Vec<_> = self
            .unverified_settlements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|((_, tenant), _)| tenant_scope.is_none_or(|scope| tenant == scope))
            .map(|((id, tenant), actual)| (id.clone(), tenant.clone(), *actual))
            .collect();
        for (id, tenant, actual) in unverified_rows {
            let owner: Option<String> = {
                let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                conn.query_row(
                    "SELECT tenant_id FROM reservations WHERE id=?1",
                    [&id.0],
                    |r| r.get(0),
                )
                .optional()?
            };
            match owner {
                Some(owner) if owner == tenant => {
                    self.persist_emergency_settlement(&id, &tenant, actual)?;
                }
                Some(owner) => {
                    quarantine(
                        &tx,
                        &id,
                        &tenant,
                        actual,
                        &format!("unverified tenant does not own reservation (owner {owner})"),
                    )?;
                }
                None => {
                    quarantine(&tx, &id, &tenant, actual, "reservation does not exist")?;
                }
            }
            self.unverified_settlements
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&(id, tenant));
        }
        let memory_rows: Vec<_> = self
            .emergency_settlements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, (tenant, _))| tenant_scope.is_none_or(|scope| tenant == scope))
            .map(|(id, (tenant, actual))| (id.clone(), tenant.clone(), *actual))
            .collect();
        for (id, tenant, actual) in memory_rows {
            if let Some((queued_tenant, queued_actual)) = self
                .emergency_settlements
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&id)
                .cloned()
                && (queued_tenant != tenant || queued_actual != actual)
            {
                return Err(BudgetError::SettlementConflict {
                    reservation_id: id.0.clone(),
                    reason: "in-memory emergency recovery changed during retry".into(),
                });
            }
            self.persist_emergency_settlement(&id, &tenant, actual)?;
            self.emergency_settlements
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
        }
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT reservation_id, tenant_id, actual_cost_minor
                FROM pending_settlements WHERE (?1 IS NULL OR tenant_id=?1)",
            )?;
            stmt.query_map([tenant_scope], |r| {
                Ok((
                    ReservationId(r.get(0)?),
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        for (id, tenant, actual) in rows {
            match self.apply_pending(&tx, &id, &tenant, actual) {
                Ok(()) => {}
                Err(err @ BudgetError::OverageTooLarge { .. }) => {
                    // The primary ledger committed; allow remaining rows to recover.
                    eprintln!("budget settlement overage: {err}");
                }
                Err(
                    err @ (BudgetError::ReservationNotActive { .. }
                    | BudgetError::SettlementConflict { .. }
                    | BudgetError::ReservationNotFound { .. }
                    | BudgetError::TenantMismatch { .. }
                    | BudgetError::InvalidActualCost { .. }),
                ) => {
                    eprintln!("budget settlement quarantined: {err}");
                }
                Err(err) => return Err(err),
            }
            tx.execute(
                "DELETE FROM pending_settlements WHERE reservation_id=?1",
                [&id.0],
            )?;
        }
        let emergencies = {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let mut stmt = conn.prepare(
                "SELECT reservation_id, tenant_id, actual_cost_minor FROM budget_emergency_settlements WHERE (?1 IS NULL OR tenant_id=?1)",
            )?;
            stmt.query_map([tenant_scope], |r| {
                Ok((
                    ReservationId(r.get(0)?),
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut completed_emergencies = Vec::new();
        for (id, tenant, actual) in emergencies {
            match self.apply_pending(&tx, &id, &tenant, actual) {
                Ok(()) | Err(BudgetError::OverageTooLarge { .. }) => {
                    completed_emergencies.push(id);
                }
                Err(
                    err @ (BudgetError::ReservationNotActive { .. }
                    | BudgetError::SettlementConflict { .. }
                    | BudgetError::ReservationNotFound { .. }
                    | BudgetError::TenantMismatch { .. }
                    | BudgetError::InvalidActualCost { .. }),
                ) => {
                    eprintln!("emergency settlement quarantined: {err}");
                    completed_emergencies.push(id);
                }
                Err(err) => return Err(err),
            }
        }
        self.check_settlement_intents(&tx, tenant_scope)?;
        tx.commit()?;
        for id in completed_emergencies {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            conn.execute(
                "DELETE FROM budget_emergency_settlements WHERE reservation_id=?1",
                [&id.0],
            )?;
        }
        Ok(())
    }

    fn retry_one(
        &self,
        id: &ReservationId,
        expected_tenant: &str,
        expected_actual: i64,
    ) -> Result<(), BudgetError> {
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let tx = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, i64)> = tx.query_row(
            "SELECT tenant_id, actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
            [&id.0], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((tenant, actual)) = row else {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let recorded: Option<(String, String, Option<i64>)> = conn
                .query_row(
                    "SELECT tenant_id, status, actual_cost_minor FROM reservations WHERE id=?1",
                    [&id.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            tx.commit()?;
            return if recorded
                == Some((
                    expected_tenant.to_string(),
                    "settled".into(),
                    Some(expected_actual),
                )) {
                Ok(())
            } else {
                Err(BudgetError::ReservationNotFound {
                    reservation_id: id.0.clone(),
                })
            };
        };
        let applied = self.apply_pending(&tx, id, &tenant, actual);
        let permanent = matches!(
            &applied,
            Err(BudgetError::ReservationNotActive { .. }
                | BudgetError::SettlementConflict { .. }
                | BudgetError::ReservationNotFound { .. }
                | BudgetError::TenantMismatch { .. }
                | BudgetError::InvalidActualCost { .. })
        );
        if !permanent
            && !matches!(&applied, Err(BudgetError::OverageTooLarge { .. }))
            && applied.is_err()
        {
            return applied;
        }
        let cleanup = tx
            .execute(
                "DELETE FROM pending_settlements WHERE reservation_id=?1",
                [&id.0],
            )
            .and_then(|_| tx.commit());
        if let Err(err) = cleanup {
            if applied.is_ok() || matches!(&applied, Err(BudgetError::OverageTooLarge { .. })) {
                eprintln!(
                    "committed settlement sidecar cleanup failed: reservation={} error={err}",
                    id.0
                );
                return applied;
            }
            return Err(err.into());
        }
        applied
    }

    fn apply_pending(
        &self,
        tx: &rusqlite::Transaction<'_>,
        id: &ReservationId,
        tenant: &str,
        actual: i64,
    ) -> Result<(), BudgetError> {
        match self.settle_uncoordinated(tenant, id, actual) {
            Ok(_) => Ok(()),
            Err(err @ BudgetError::OverageTooLarge { .. }) => Err(err),
            Err(BudgetError::ReservationNotActive { status, .. }) if status == "settled" => {
                let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                let recorded: i64 = conn.query_row(
                    "SELECT actual_cost_minor FROM reservations WHERE id=?1 AND tenant_id=?2",
                    params![id.0, tenant],
                    |r| r.get(0),
                )?;
                if recorded == actual {
                    Ok(())
                } else {
                    let err = BudgetError::SettlementConflict {
                        reservation_id: id.0.clone(),
                        reason: format!("already settled at {recorded}, queued {actual}"),
                    };
                    quarantine(tx, id, tenant, actual, &err.to_string())?;
                    Err(err)
                }
            }
            Err(
                err @ (BudgetError::ReservationNotActive { .. }
                | BudgetError::SettlementConflict { .. }
                | BudgetError::ReservationNotFound { .. }
                | BudgetError::TenantMismatch { .. }
                | BudgetError::InvalidActualCost { .. }),
            ) => {
                quarantine(tx, id, tenant, actual, &err.to_string())?;
                Err(err)
            }
            Err(err) => Err(err),
        }
    }
}

fn quarantine(
    tx: &rusqlite::Transaction<'_>,
    id: &ReservationId,
    tenant: &str,
    actual: i64,
    reason: &str,
) -> Result<(), BudgetError> {
    tx.execute("INSERT OR REPLACE INTO quarantined_settlements (reservation_id, tenant_id, actual_cost_minor, reason) VALUES (?1, ?2, ?3, ?4)", params![id.0, tenant, actual, reason])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::budget::{BudgetScope, Price, SpendGate};

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
        assert_eq!(ledger.settle_durable("t", &id, 20).unwrap(), None);
        assert!(matches!(
            ledger.settle_durable("t", &id, 21),
            Err(BudgetError::SettlementConflict { .. })
        ));
        assert!(matches!(
            ledger.settle("t", &id, 21),
            Err(BudgetError::SettlementConflict { .. })
        ));
        assert!(matches!(
            ledger.release("t", &id),
            Err(BudgetError::SettlementConflict { .. })
        ));
        assert!(matches!(
            ledger.release("other", &id),
            Err(BudgetError::TenantMismatch { .. })
        ));
        assert_eq!(pending(&ledger), 1);
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
    fn committed_overage_is_observable_but_never_queued_for_another_charge() {
        let (ledger, id) = reserved();
        assert!(matches!(
            ledger.settle_durable("t", &id, 50),
            Err(BudgetError::OverageTooLarge { .. })
        ));
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
                result.unwrap();
                assert_eq!(pending(&ledger), 0);
                let isolated: i64 = ledger
                    .settlements
                    .lock()
                    .unwrap()
                    .query_row("SELECT count(*) FROM quarantined_settlements", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(isolated, 1);
            } else {
                result.unwrap();
                assert_eq!(pending(&ledger), 0);
            }
            assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
        }
    }

    #[test]
    fn journal_failure_falls_back_to_primary_settlement() {
        let (ledger, id) = reserved();
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
        assert_eq!(ledger.settle_durable("t", &id, 20).unwrap(), Some(20));
        assert_eq!(pending(&ledger), 0);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 980);
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
    fn terminal_and_conflicting_costs_are_rejected_without_blocking_other_tenants() {
        let (ledger, released) = reserved();
        ledger.release("t", &released).unwrap();
        assert!(matches!(
            ledger.settle_durable("t", &released, 20),
            Err(BudgetError::ReservationNotActive { .. })
        ));
        assert_eq!(pending(&ledger), 0);
        ledger
            .configure_tenant("other", 100, "UTC", SpendGate::All)
            .unwrap();
        assert!(
            ledger
                .reserve(&BudgetScope::new("other", "k", "p", "m"), Price::Known(1))
                .is_ok()
        );

        let settled = ledger
            .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
            .unwrap();
        ledger.settle("t", &settled, 10).unwrap();
        assert!(matches!(
            ledger.settle_durable("t", &settled, 11),
            Err(BudgetError::SettlementConflict { .. })
        ));
        assert_eq!(pending(&ledger), 0);
    }

    #[test]
    fn expired_reservation_can_be_durably_settled() {
        let (ledger, id) = reserved();
        ledger
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE reservations SET status='expired' WHERE id=?1",
                [&id.0],
            )
            .unwrap();
        assert_eq!(ledger.settle_durable("t", &id, 8).unwrap(), Some(8));
        assert_eq!(pending(&ledger), 0);
        assert_eq!(ledger.tenant_balance("t").unwrap(), 992);
    }

    #[test]
    fn file_recovery_quarantines_cross_tenant_row_and_recovers_valid_row_after_reopen() {
        let path = std::env::temp_dir().join(format!(
            "idoris-settlement-recovery-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let ledger = BudgetLedger::open(&path).unwrap();
        for tenant in ["a", "b"] {
            ledger
                .configure_tenant(tenant, 100, "UTC", SpendGate::All)
                .unwrap();
        }
        let a = ledger
            .reserve(&BudgetScope::new("a", "k", "p", "m"), Price::Known(10))
            .unwrap();
        let b = ledger
            .reserve(&BudgetScope::new("b", "k", "p", "m"), Price::Known(10))
            .unwrap();
        let scope_a = BudgetScope::new("a", "k", "p", "m");
        let released = ledger.reserve(&scope_a, Price::Known(10)).unwrap();
        ledger.release("a", &released).unwrap();
        let settled = ledger.reserve(&scope_a, Price::Known(10)).unwrap();
        ledger.settle("a", &settled, 5).unwrap();
        {
            let journal = ledger.settlements.lock().unwrap();
            journal.execute(
                "INSERT INTO pending_settlements VALUES (?1, 'b', 7), (?2, 'b', 6), (?3, 'a', 8), (?4, 'a', 9)",
                params![a.0, b.0, released.0, settled.0],
            ).unwrap();
        }
        drop(ledger);

        let reopened = BudgetLedger::open(&path).unwrap();
        assert_eq!(pending(&reopened), 0);
        assert_eq!(reopened.tenant_balance("a").unwrap(), 85);
        assert_eq!(reopened.tenant_balance("b").unwrap(), 94);
        assert!(
            reopened
                .reserve(&BudgetScope::new("b", "k", "p", "m"), Price::Known(1))
                .is_ok()
        );
        let quarantined: (String, String) = reopened
            .settlements
            .lock()
            .unwrap()
            .query_row(
                "SELECT tenant_id, reason FROM quarantined_settlements WHERE reservation_id=?1",
                [&a.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(quarantined.0, "b");
        assert!(quarantined.1.contains("reservation not found"));
        let quarantine_count: i64 = reopened
            .settlements
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM quarantined_settlements", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(quarantine_count, 3);
        // B's admission leaves A's poison row for A's retry or global maintenance.
        reopened
            .settlements
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO pending_settlements VALUES (?1, 'a', 8)",
                [&released.0],
            )
            .unwrap();
        assert!(
            reopened
                .reserve(&BudgetScope::new("b", "k", "p", "m"), Price::Known(1))
                .is_ok()
        );
        assert_eq!(pending(&reopened), 1);
        reopened.retry_settlements().unwrap();
        assert_eq!(pending(&reopened), 0);
        drop(reopened);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_added_extension("settlements.sqlite3"));
    }

    #[test]
    fn sidecar_cleanup_failure_does_not_mask_committed_charge() {
        for actual in [20, 50] {
            let (ledger, id) = reserved();
            ledger
                .settlements
                .lock()
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER fail_pending_delete BEFORE DELETE ON pending_settlements
             BEGIN SELECT RAISE(ABORT, 'injected cleanup error'); END;",
                )
                .unwrap();
            let result = ledger.settle_durable("t", &id, actual);
            if actual == 50 {
                assert!(matches!(result, Err(BudgetError::OverageTooLarge { .. })));
            } else {
                assert_eq!(result.unwrap(), Some(actual));
            }
            assert_eq!(
                ledger.settle_durable("t", &id, actual).unwrap(),
                Some(actual)
            );
            assert_eq!(ledger.tenant_balance("t").unwrap(), 1000 - actual);
            assert_eq!(pending(&ledger), 1);
            ledger
                .settlements
                .lock()
                .unwrap()
                .execute_batch("DROP TRIGGER fail_pending_delete")
                .unwrap();
            ledger.retry_settlements().unwrap();
            assert_eq!(pending(&ledger), 0);
            assert_eq!(ledger.tenant_balance("t").unwrap(), 1000 - actual);
        }
    }
}

#[cfg(test)]
#[path = "settlement/race_tests.rs"]
mod race_tests;

//! Independent durable outbox: a writer locking the ledger cannot block
//! recording completed usage. Keep this sidecar with the ledger on backup.
use std::{path::Path, time::Duration};

use rusqlite::{Connection, TransactionBehavior, params};

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
        actual_cost_minor INTEGER NOT NULL CHECK(actual_cost_minor >= 0))",
    )?;
    Ok(conn)
}

impl BudgetLedger {
    /// Persist before attempting settlement. None means durable retry is
    /// pending; an error means persistence failed or an invalid outcome.
    pub fn settle_durable(
        &self,
        tenant: &str,
        id: &ReservationId,
        actual: i64,
    ) -> Result<Option<i64>, BudgetError> {
        // Validate ownership before persisting; reads work under a WAL writer.
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let owner: String = conn.query_row(
            "SELECT tenant_id FROM reservations WHERE id=?1",
            [&id.0],
            |r| r.get(0),
        )?;
        if owner != tenant {
            return Err(BudgetError::TenantMismatch {
                reservation_id: id.0.clone(),
            });
        }
        drop(conn);
        let journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        journal.execute(
            "INSERT INTO pending_settlements VALUES (?1, ?2, ?3)",
            params![id.0, tenant, actual],
        )?;
        drop(journal);
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
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let tx = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        }
        tx.commit()?;
        Ok(())
    }
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
}

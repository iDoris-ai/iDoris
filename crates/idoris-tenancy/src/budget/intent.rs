//! A pre-dispatch fence survives losing both stores before the actual cost
//! can be recorded. Unknown outcomes require reconciliation, never TTL expiry.
//! Callers may use `release_confirmed_unexecuted` only after independently
//! verifying that the upstream operation did not execute.
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{BudgetError, BudgetLedger, ReservationId};

impl BudgetLedger {
    /// Persist before starting a paid upstream call. After an unclean restart,
    /// this tenant's admission stays closed until its unknown outcomes are reconciled using
    /// `settle_durable`, or `release_confirmed_unexecuted` after confirming
    /// that the upstream call did not execute.
    pub fn begin_settlement(&self, tenant: &str, id: &ReservationId) -> Result<(), BudgetError> {
        let mut journal = self.settlements.lock().unwrap_or_else(|p| p.into_inner());
        let tx = journal.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.check_settlement_intents(&tx, Some(tenant))?;
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let row: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT tenant_id, status, expires_at_ms FROM reservations WHERE id=?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((owner, status, expires)) = row else {
            return Err(BudgetError::ReservationNotFound {
                reservation_id: id.0.clone(),
            });
        };
        if owner != tenant {
            return Err(BudgetError::TenantMismatch {
                reservation_id: id.0.clone(),
            });
        }
        if status != "active" || expires <= self.clock.now_ms() {
            return Err(BudgetError::ReservationNotActive {
                reservation_id: id.0.clone(),
                status,
            });
        }
        tx.execute(
            "INSERT INTO settlement_intents VALUES (?1, ?2)",
            params![id.0, tenant],
        )?;
        tx.commit()?;
        self.live_settlement_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.0.clone(), tenant.to_owned());
        Ok(())
    }

    pub(super) fn finish_settlement_intent(&self, id: &ReservationId) {
        self.live_settlement_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id.0);
    }

    pub(super) fn finish_settlement_intent_for_tenant(&self, tenant: &str, id: &ReservationId) {
        let mut live = self
            .live_settlement_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if live.get(&id.0).is_some_and(|owner| owner == tenant) {
            live.remove(&id.0);
        }
    }

    pub(super) fn check_settlement_intents(
        &self,
        tx: &rusqlite::Transaction<'_>,
        tenant_scope: Option<&str>,
    ) -> Result<(), BudgetError> {
        let rows = tx
            .prepare("SELECT reservation_id, tenant_id FROM settlement_intents WHERE (?1 IS NULL OR tenant_id=?1)")?
            .query_map([tenant_scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut live = self
            .live_settlement_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for (id, tenant) in rows {
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM reservations WHERE id=?1 AND tenant_id=?2",
                    params![id, tenant],
                    |r| r.get(0),
                )
                .optional()?;
            if matches!(status.as_deref(), Some("settled" | "released")) {
                tx.execute(
                    "DELETE FROM settlement_intents WHERE reservation_id=?1",
                    [&id],
                )?;
                live.remove(&id);
            } else if !live.contains_key(&id) {
                return Err(BudgetError::Storage(
                    "unconfirmed dispatch outcome; reconcile before admitting spending".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::budget::{BudgetScope, Clock, Price, SpendGate};
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    use std::time::Duration;

    struct TestClock(AtomicI64);
    impl Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn lost_volatile_outcome_keeps_dispatch_fenced_after_ttl_and_restart() {
        let path =
            std::env::temp_dir().join(format!("idoris-intent-{}.sqlite3", uuid::Uuid::new_v4()));
        let clock = Arc::new(TestClock(AtomicI64::new(0)));
        let ledger =
            BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 100, Duration::ZERO)
                .unwrap();
        ledger
            .configure_tenant("t", 100, "UTC", SpendGate::All)
            .unwrap();
        let scope = BudgetScope::new("t", "k", "p", "m");
        let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
        ledger.begin_settlement("t", &id).unwrap();
        let sidecar =
            rusqlite::Connection::open(path.with_added_extension("settlements.sqlite3")).unwrap();
        sidecar.execute_batch("CREATE TRIGGER fail_pending BEFORE INSERT ON pending_settlements BEGIN SELECT RAISE(ABORT, 'sidecar failed'); END;").unwrap();
        let mut primary = rusqlite::Connection::open(&path).unwrap();
        let tx = primary
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(ledger.settle_durable("t", &id, 31).is_err());
        clock.0.store(101, Ordering::SeqCst);
        assert!(ledger.reserve(&scope, Price::Known(1)).is_err());
        // Lose even the in-memory actual-cost copy, while retaining the fence
        // that was committed before the upstream was allowed to run.
        drop(ledger);
        tx.rollback().unwrap();
        let restarted = BudgetLedger::open_with(&path, clock, 100).unwrap();
        assert_eq!(restarted.tenant_balance("t").unwrap(), 100);
        assert!(restarted.reserve(&scope, Price::Known(1)).is_err());
        assert!(restarted.retry_settlements().is_err());
        // Actual usage must now come from upstream records/operator logs.
        // The sidecar INSERT fault is still present: primary fallback settles.
        assert_eq!(restarted.settle_durable("t", &id, 31).unwrap(), Some(31));
        restarted.retry_settlements().unwrap();
        assert_eq!(restarted.tenant_balance("t").unwrap(), 69);
        let next = restarted.reserve(&scope, Price::Known(1)).unwrap();
        restarted.release("t", &next).unwrap();
        restarted.retry_settlements().unwrap();
        assert_eq!(restarted.tenant_balance("t").unwrap(), 69);
        drop(restarted);
        drop(primary);
        drop(sidecar);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_added_extension("settlements.sqlite3"));
    }

    #[test]
    fn busy_release_drops_live_exemption_but_keeps_intent_fenced_after_ttl() {
        let path = std::env::temp_dir().join(format!(
            "idoris-intent-busy-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let clock = Arc::new(TestClock(AtomicI64::new(0)));
        let ledger =
            BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 100, Duration::ZERO)
                .unwrap();
        ledger
            .configure_tenant("t", 100, "UTC", SpendGate::All)
            .unwrap();
        let scope = BudgetScope::new("t", "k", "p", "m");
        let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
        ledger.begin_settlement("t", &id).unwrap();

        let mut sidecar =
            rusqlite::Connection::open(path.with_added_extension("settlements.sqlite3")).unwrap();
        let tx = sidecar
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();

        // A wrong tenant cannot clear the live exemption, even if the sidecar
        // is currently busy and cannot confirm ownership from durable state.
        assert!(matches!(
            ledger.release("other", &id),
            Err(BudgetError::Busy)
        ));
        assert!(
            ledger
                .live_settlement_intents
                .lock()
                .unwrap()
                .contains_key(&id.0)
        );

        // The real request ending during Busy clears only its volatile
        // exemption. Its durable intent remains for outcome reconciliation.
        assert!(matches!(ledger.release("t", &id), Err(BudgetError::Busy)));
        assert!(
            !ledger
                .live_settlement_intents
                .lock()
                .unwrap()
                .contains_key(&id.0)
        );
        let durable_intent: i64 = ledger
            .settlements
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM settlement_intents WHERE reservation_id=?1 AND tenant_id=?2",
                rusqlite::params![id.0, "t"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(durable_intent, 1);
        clock.0.store(101, Ordering::SeqCst);
        tx.rollback().unwrap();

        assert!(matches!(
            ledger.reserve(&scope, Price::Known(1)),
            Err(BudgetError::Storage(message))
                if message.contains("unconfirmed dispatch outcome")
        ));
        drop(ledger);
        drop(sidecar);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_added_extension("settlements.sqlite3"));
    }
}

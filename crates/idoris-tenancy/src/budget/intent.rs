#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price, SpendGate};
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
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();

        assert!(ledger.settle_durable("t", &id, 31).is_err());
        clock.0.store(101, Ordering::SeqCst);
        assert!(ledger.reserve(&scope, Price::Known(1)).is_err());
        ledger.abandon_dispatch("t", &id).unwrap();
        drop(ledger);
        tx.rollback().unwrap();

        let restarted = BudgetLedger::open_with(&path, clock, 100).unwrap();
        assert_eq!(restarted.tenant_balance("t").unwrap(), 60);
        assert!(restarted.reserve(&scope, Price::Known(1)).is_err());
        assert!(restarted.retry_settlements().is_ok());
        let terminal_actual: Option<i64> = restarted
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminal_actual, None);
        let durable_intent: i64 = restarted
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
        assert!(restarted.reserve(&scope, Price::Known(1)).is_err());
        drop(restarted);
        drop(primary);
        drop(sidecar);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_added_extension("settlements.sqlite3"));
    }

    #[test]
    fn unknown_dispatch_abandonment_keeps_intent_fenced_after_ttl() {
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

        assert!(matches!(
            ledger.release("other", &id),
            Err(BudgetError::TenantMismatch { .. })
        ));
        assert_eq!(
            ledger
                .live_intents
                .lock()
                .unwrap()
                .get(&id.0)
                .map(String::as_str),
            Some("t")
        );
        ledger.abandon_dispatch("t", &id).unwrap();
        assert!(!ledger.live_intents.lock().unwrap().contains_key(&id.0));
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
        assert!(ledger.reserve(&scope, Price::Known(1)).is_err());
        let still_fenced: i64 = ledger
            .settlements
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM settlement_intents WHERE reservation_id=?1 AND tenant_id=?2",
                rusqlite::params![id.0, "t"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(still_fenced, 1);
        drop(ledger);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_added_extension("settlements.sqlite3"));
    }

    #[test]
    fn confirmed_unexecuted_dispatch_releases_hold() {
        let ledger = BudgetLedger::open(":memory:").unwrap();
        ledger
            .configure_tenant("t", 100, "UTC", SpendGate::All)
            .unwrap();
        let scope = BudgetScope::new("t", "k", "p", "m");
        let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
        ledger.begin_settlement("t", &id).unwrap();
        ledger.release_confirmed_unexecuted("t", &id).unwrap();
        assert_eq!(ledger.tenant_balance("t").unwrap(), 100);
        assert_eq!(
            ledger
                .settlements
                .lock()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM settlement_intents WHERE reservation_id=?1",
                    [&id.0],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}

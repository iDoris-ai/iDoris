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

fn reserve_pair(ledger: &BudgetLedger) -> [ReservationId; 2] {
    let scope = BudgetScope::new("t", "k", "p", "m");
    [
        ledger.reserve(&scope, Price::Known(10)).unwrap(),
        ledger.reserve(&scope, Price::Known(10)).unwrap(),
    ]
}

fn fail_both_stores(ledger: &BudgetLedger, ids: &[ReservationId; 2]) {
    for id in ids {
        ledger.begin_settlement("t", id).unwrap();
    }
    ledger
        .settlements
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_pending BEFORE INSERT ON pending_settlements
             BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END;",
        )
        .unwrap();
    ledger
        .conn
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_formal BEFORE UPDATE OF status ON reservations
             WHEN NEW.status='settled' BEGIN SELECT RAISE(ABORT, 'injected settle failure'); END;
             CREATE TRIGGER fail_fallback BEFORE UPDATE OF actual_cost_minor ON reservations
             BEGIN SELECT RAISE(ABORT, 'injected fallback failure'); END;",
        )
        .unwrap();
    for (id, actual) in ids.iter().zip([20, 30]) {
        assert!(ledger.settle_durable("t", id, actual).is_err());
    }
}

#[test]
fn one_fallback_failure_does_not_block_the_other_cost() {
    let (ledger, _) = reserved();
    let ids = reserve_pair(&ledger);
    fail_both_stores(&ledger, &ids);
    let failing_id = ledger
        .settlement_outcomes
        .lock()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let successful_id = ids.iter().find(|id| id.0 != failing_id).unwrap().0.clone();
    let successful_actual = if ids[0].0 == successful_id { 20 } else { 30 };
    ledger
        .conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_fallback")
        .unwrap();
    let fail_one = format!(
        "CREATE TRIGGER fail_fallback BEFORE UPDATE OF actual_cost_minor ON reservations
         WHEN OLD.id='{failing_id}' BEGIN SELECT RAISE(ABORT, 'injected fallback failure'); END;"
    );
    ledger
        .conn
        .lock()
        .unwrap()
        .execute_batch(&fail_one)
        .unwrap();
    let before_retry: Option<i64> = ledger
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1",
            [&successful_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before_retry, None);

    assert!(ledger.retry_settlements().is_err());
    let outcomes = ledger.settlement_outcomes.lock().unwrap();
    assert!(outcomes.contains_key(&failing_id));
    assert!(!outcomes.contains_key(&successful_id));
    let actual: i64 = ledger
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1",
            [&successful_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actual, successful_actual);
}

#[test]
fn retry_falls_back_for_all_costs_and_restart_charges_each_once() {
    let dir = std::env::temp_dir().join(format!(
        "idoris-settlement-fallback-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("budget.sqlite3");
    let ledger = BudgetLedger::open(&path).unwrap();
    ledger
        .configure_tenant("t", 1000, "UTC", SpendGate::All)
        .unwrap();
    let ids = reserve_pair(&ledger);
    fail_both_stores(&ledger, &ids);
    ledger
        .conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_fallback")
        .unwrap();

    assert!(ledger.retry_settlements().is_err());
    assert!(ledger.settlement_outcomes.lock().unwrap().is_empty());
    let actuals: Vec<i64> = ids
        .iter()
        .map(|id| {
            ledger
                .conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT actual_cost_minor FROM reservations WHERE id=?1",
                    [&id.0],
                    |row| row.get(0),
                )
                .unwrap()
        })
        .collect();
    assert_eq!(actuals, [20, 30]);
    drop(ledger);

    Connection::open(path.with_added_extension("settlements.sqlite3"))
        .unwrap()
        .execute_batch("DROP TRIGGER fail_pending")
        .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TRIGGER fail_formal")
        .unwrap();
    let reopened = BudgetLedger::open(&path).unwrap();
    reopened.retry_settlements().unwrap();
    assert_eq!(reopened.tenant_balance("t").unwrap(), 950);
    for (id, expected_actual) in ids.iter().zip([20, 30]) {
        let (status, actual): (String, i64) = reopened
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "settled");
        assert_eq!(actual, expected_actual);
    }
    reopened.retry_settlements().unwrap();
    assert_eq!(reopened.tenant_balance("t").unwrap(), 950);
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
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
        Err(BudgetError::SettlementConflict { .. }) | Err(BudgetError::Storage(_))
    ));
    assert!(matches!(
        ledger.settle("t", &id, 21),
        Err(BudgetError::SettlementConflict { .. }) | Err(BudgetError::Storage(_))
    ));
    assert!(matches!(
        ledger.release("t", &id),
        Err(BudgetError::SettlementConflict { .. }) | Err(BudgetError::Storage(_))
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
        Err(BudgetError::SettlementConflict { .. }) | Err(BudgetError::Storage(_))
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

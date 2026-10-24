//! Leader-only paid reservation state; transport wiring follows separately.
use crate::budget::{self, UsageEvidence};
use crate::dispatch::ReservationGuard;
use crate::proxy::{ExecutionDisposition, ForwardOutcome, SuccessFinalizeError};
use idoris_contracts::provider::Cost;
use idoris_tenancy::budget::{BudgetError, BudgetLedger};
use std::sync::{Mutex, MutexGuard};

#[derive(Debug)]
pub(crate) enum ProxyBudgetError {
    Ledger(BudgetError),
    StateUnavailable,
}
impl From<BudgetError> for ProxyBudgetError {
    fn from(error: BudgetError) -> Self {
        Self::Ledger(error)
    }
}
struct State<'a> {
    admission: ReservationGuard<'a>,
    paid: Option<ReservationGuard<'a>>,
    settlement: Option<(i64, UsageEvidence)>,
}
pub(crate) struct ProxyBudget<'a> {
    ledger: Option<&'a BudgetLedger>,
    tenant_id: Option<&'a str>,
    provider_id: &'a str,
    estimate: i64,
    state: Mutex<State<'a>>,
}
impl<'a> ProxyBudget<'a> {
    pub(crate) fn new(
        ledger: Option<&'a BudgetLedger>,
        tenant_id: Option<&'a str>,
        provider_id: &'a str,
        estimate: i64,
    ) -> Result<Self, BudgetError> {
        if budget::is_paid(Some(estimate)) && ledger.is_none() {
            return Err(budget::ledger_unavailable_error(tenant_id, provider_id));
        }
        let admission = if budget::is_paid(Some(estimate)) {
            if let Some(ledger) = ledger {
                budget::check_admission(ledger, tenant_id, provider_id, 0)?;
            }
            ReservationGuard::vacant(ledger, tenant_id)
        } else {
            ReservationGuard::reserve(ledger, tenant_id, provider_id, 0)?
        };
        Ok(Self {
            ledger,
            tenant_id,
            provider_id,
            estimate,
            state: Mutex::new(State {
                admission,
                paid: None,
                settlement: None,
            }),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, State<'a>>, ProxyBudgetError> {
        self.state
            .lock()
            .map_err(|_| ProxyBudgetError::StateUnavailable)
    }
    pub(crate) fn prepare_for_post(&self) -> Result<(), ProxyBudgetError> {
        let mut state = self.lock()?;
        if state.paid.is_some() {
            return Err(ProxyBudgetError::StateUnavailable);
        }
        state.admission.release_now()?;
        state.paid = Some(ReservationGuard::reserve(
            self.ledger,
            self.tenant_id,
            self.provider_id,
            self.estimate,
        )?);
        Ok(())
    }
    /// Arm only after every async pre-send append has succeeded.
    pub(crate) fn arm(&self) -> Result<(), ProxyBudgetError> {
        self.lock()?
            .paid
            .as_mut()
            .ok_or(ProxyBudgetError::StateUnavailable)?
            .retain_on_drop();
        Ok(())
    }
    pub(crate) fn settle_response(
        &self,
        outcome: &ForwardOutcome,
        cost: &Cost,
    ) -> Result<(), SuccessFinalizeError> {
        let usage = budget::parse_openai_usage(&outcome.body).map_err(|_| {
            (
                502,
                "upstream_usage_invalid",
                "付费上游成功响应缺少可验证的 usage 证据",
            )
        })?;
        let mut state = self
            .lock()
            .map_err(|_| (500, "internal_error", "付费上游预算状态不可用"))?;
        let paid =
            state
                .paid
                .as_mut()
                .ok_or((500, "internal_error", "付费上游缺少可结算的预算预留"))?;
        let actual = budget::actual_cost_from_usage(cost, usage).map_err(|_| {
            paid.retain_until_expiry();
            (
                500,
                "internal_error",
                "付费上游 usage 无法转换为安全结算金额",
            )
        })?;
        let charged = paid
            .settle_replayable(actual)
            .map_err(|_| {
                (
                    500,
                    "internal_error",
                    "付费上游结算未能确认，请稍后查询预算账本",
                )
            })?
            .ok_or((500, "internal_error", "付费上游缺少可结算的预算预留"))?;
        state.settlement = Some((charged, usage));
        Ok(())
    }
    pub(crate) fn release_unsent(&self) -> Result<(), ProxyBudgetError> {
        let mut state = self.lock()?;
        state.admission.release_now()?;
        if let Some(paid) = state.paid.as_mut() {
            paid.release_now()?;
        }
        Ok(())
    }
    pub(crate) fn finish(&self, outcome: &ForwardOutcome) -> Result<(), ProxyBudgetError> {
        match outcome.execution {
            ExecutionDisposition::NotExecuted | ExecutionDisposition::Replay => {
                self.release_unsent()
            }
            ExecutionDisposition::Uncertain => {
                if let Some(paid) = self.lock()?.paid.as_mut() {
                    paid.retain_until_expiry();
                }
                Ok(())
            }
            ExecutionDisposition::Executed if !(200..300).contains(&outcome.status) => {
                self.release_unsent()
            }
            ExecutionDisposition::Executed => Ok(()),
        }
    }
    pub(crate) fn settlement(&self) -> Result<Option<(i64, UsageEvidence)>, ProxyBudgetError> {
        Ok(self.lock()?.settlement)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn outcome(execution: ExecutionDisposition) -> ForwardOutcome {
        ForwardOutcome {
            status: 200,
            body: r#"{"usage":{"prompt_tokens":1,"completion_tokens":4}}"#.into(),
            content_type: None,
            cached: execution == ExecutionDisposition::Replay,
            origin_record_id: None,
            replayed_served_locality: None,
            retries: 0,
            execution,
        }
    }
    #[test]
    fn cached_admission_does_not_require_a_second_paid_reservation() {
        let (_dir, ledger) = crate::tests::configured_budget_ledger(2052);
        let cost = crate::tests::paid_component_card("paid").provider.cost;
        let quote = budget::estimate_cost_minor(&cost, "headroom").unwrap();
        let first = ProxyBudget::new(Some(&ledger), None, "paid", quote).unwrap();
        first.prepare_for_post().unwrap();
        first.arm().unwrap();
        first
            .settle_response(&outcome(ExecutionDisposition::Executed), &cost)
            .unwrap();
        first
            .finish(&outcome(ExecutionDisposition::Executed))
            .unwrap();
        assert_eq!(first.settlement().unwrap().unwrap().0, 9);
        let cached = ProxyBudget::new(Some(&ledger), None, "paid", quote).unwrap();
        cached
            .finish(&outcome(ExecutionDisposition::Replay))
            .unwrap();
        assert!(cached.settlement().unwrap().is_none());
        let denied = ProxyBudget::new(Some(&ledger), None, "paid", quote).unwrap();
        match denied.prepare_for_post().unwrap_err() {
            ProxyBudgetError::Ledger(error) => assert!(error.to_402_body().is_some()),
            ProxyBudgetError::StateUnavailable => panic!("expected ledger denial"),
        }
        let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(view.spent_minor, 9);
        assert_eq!(view.reserved_minor, 0);
    }

    #[test]
    fn paid_replay_admission_is_denied_without_inserting_a_row() {
        let (dir, ledger) = crate::tests::configured_budget_ledger(0);
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                0,
                "UTC",
                idoris_tenancy::budget::SpendGate::All,
            )
            .unwrap();
        let quote = budget::estimate_cost_minor(
            &crate::tests::paid_component_card("paid").provider.cost,
            "replay",
        )
        .unwrap();
        let db = dir.path().join("b.sqlite3");
        let count = || {
            rusqlite::Connection::open(&db)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM reservations", [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        let before = count();
        assert!(ProxyBudget::new(Some(&ledger), None, "paid", quote).is_err());
        assert_eq!(count(), before);
    }
    #[test]
    fn cancelled_before_send_releases_but_armed_execution_retains() {
        let (_dir, ledger) = crate::tests::configured_budget_ledger(1_000_000);
        {
            let pending = ProxyBudget::new(Some(&ledger), None, "paid", 2051).unwrap();
            pending.prepare_for_post().unwrap();
            assert_eq!(
                ledger
                    .tenant_readview(budget::PERSONAL_TENANT_ID)
                    .unwrap()
                    .reserved_minor,
                2051
            );
        }
        assert_eq!(
            ledger
                .tenant_readview(budget::PERSONAL_TENANT_ID)
                .unwrap()
                .reserved_minor,
            0
        );
        {
            let sent = ProxyBudget::new(Some(&ledger), None, "paid", 2051).unwrap();
            sent.prepare_for_post().unwrap();
            sent.arm().unwrap();
            // Dropping the armed request models cancellation before a terminal result.
        }
        assert_eq!(
            ledger
                .tenant_readview(budget::PERSONAL_TENANT_ID)
                .unwrap()
                .reserved_minor,
            2051
        );
        assert_eq!(
            ledger
                .tenant_readview(budget::PERSONAL_TENANT_ID)
                .unwrap()
                .spent_minor,
            0
        );
    }
}

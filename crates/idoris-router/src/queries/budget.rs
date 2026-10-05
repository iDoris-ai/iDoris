use idoris_tenancy::budget::{BudgetError, BudgetLedger, SpendGate};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetResponse {
    pub tenant_id: String,
    pub billing_timezone: String,
    pub limit_minor: i64,
    pub spent_minor: i64,
    pub reserved_minor: i64,
    pub remaining_minor: i64,
    pub available_minor: i64,
    pub scope: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum BudgetQueryError {
    #[error("X-iDoris-Tenant is required for tenant budget queries")]
    ScopeRequired,
    #[error("path tenant does not match X-iDoris-Tenant")]
    ScopeMismatch,
    #[error(transparent)]
    Budget(#[from] BudgetError),
}

pub fn query_budget(
    ledger: &BudgetLedger,
    path_tenant: &str,
    scope_tenant: Option<&str>,
) -> Result<BudgetResponse, BudgetQueryError> {
    let scope_tenant = scope_tenant
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(BudgetQueryError::ScopeRequired)?;
    if scope_tenant != path_tenant {
        return Err(BudgetQueryError::ScopeMismatch);
    }
    let view = ledger.tenant_readview(path_tenant)?;
    Ok(BudgetResponse {
        tenant_id: view.tenant_id,
        billing_timezone: view.billing_timezone,
        limit_minor: view.limit_minor,
        spent_minor: view.spent_minor,
        reserved_minor: view.reserved_minor,
        remaining_minor: view.remaining_minor,
        available_minor: view.available_minor,
        scope: match view.scope {
            SpendGate::PaidOnly => "paid_only",
            SpendGate::All => "all",
        },
    })
}

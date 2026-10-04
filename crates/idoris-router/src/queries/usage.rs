use idoris_contracts::tenant::{Budget, BudgetScope, TenantContext};
use idoris_tenancy::billing::{BillingAggregateError, MonthlyUsage, query_monthly_usage};
use idoris_tenancy::budget::{BudgetError, BudgetLedger, SpendGate};
use idoris_tenancy::store::TenantStore;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    pub period: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UsageQueryError {
    #[error("X-iDoris-Tenant is required for tenant usage queries")]
    ScopeRequired,
    #[error("path tenant does not match X-iDoris-Tenant")]
    ScopeMismatch,
    #[error("tenant record store lock is poisoned")]
    StorePoisoned,
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Billing(#[from] BillingAggregateError),
}

pub fn query_usage(
    store: &TenantStore,
    ledger: &BudgetLedger,
    path_tenant: &str,
    scope_tenant: Option<&str>,
    query: &UsageQuery,
) -> Result<MonthlyUsage, UsageQueryError> {
    let scope_tenant = scope_tenant
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(UsageQueryError::ScopeRequired)?;
    if scope_tenant != path_tenant {
        return Err(UsageQueryError::ScopeMismatch);
    }
    let view = ledger.tenant_readview(path_tenant)?;
    let context = TenantContext {
        tenant_id: path_tenant.to_string(),
        budget: Budget {
            limit_minor: view.limit_minor,
            spent_minor: view.spent_minor,
            scope: match view.scope {
                SpendGate::PaidOnly => BudgetScope::PaidOnly,
                SpendGate::All => BudgetScope::All,
            },
        },
        billing_timezone: view.billing_timezone,
        quota: None,
    };
    Ok(query_monthly_usage(
        store,
        Some(&context),
        &query.period,
        None,
    )?)
}

//! The unit every budget check/spend is bucketed under.
//!
//! 总体规划 §4.6 ("维度" row) says the hard limit is tenant × env × provider.
//! This crate refines that to a four-tuple that also includes `model_id`
//! (price and rate limits are per-model in practice, not just per-provider)
//! and `key_id` (the caller's virtual API key — one tenant may run several
//! independently budgeted keys, e.g. one per downstream product).

/// `(tenant, key, provider, model)` — the account a reservation/spend is
/// tracked against. Two scopes are equal only if all four fields match, so
/// two different models under the same tenant+key+provider never share a
/// budget.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BudgetScope {
    pub tenant_id: String,
    pub key_id: String,
    pub provider_id: String,
    pub model_id: String,
}

impl BudgetScope {
    pub fn new(
        tenant_id: impl Into<String>,
        key_id: impl Into<String>,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            key_id: key_id.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All four constructor parameters are the same type (`impl
    /// Into<String>`), so a future refactor could silently swap two
    /// positions without a type error — pin the field mapping down
    /// explicitly (Codex review suggestion).
    #[test]
    fn new_maps_positional_arguments_to_the_matching_field() {
        let scope = BudgetScope::new("t", "k", "p", "m");
        assert_eq!(scope.tenant_id, "t");
        assert_eq!(scope.key_id, "k");
        assert_eq!(scope.provider_id, "p");
        assert_eq!(scope.model_id, "m");
    }
}

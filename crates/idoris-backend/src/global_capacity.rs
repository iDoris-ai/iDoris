//! Process-wide memory reservations shared by otherwise independent runtimes.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::BackendError;

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalCapacitySnapshot {
    pub budget_gb: f64,
    pub reserved_gb: f64,
    pub allocations: BTreeMap<String, f64>,
}

#[derive(Debug)]
pub struct GlobalCapacityLedger {
    budget_gb: f64,
    allocations: Mutex<BTreeMap<String, f64>>,
}

impl GlobalCapacityLedger {
    pub fn new(budget_gb: f64) -> Result<Self, BackendError> {
        validate("budget_gb", budget_gb)?;
        Ok(Self {
            budget_gb,
            allocations: Mutex::new(BTreeMap::new()),
        })
    }

    /// Reserve one runtime/model allocation. Repeating the exact same key and
    /// amount is idempotent; changing an existing amount is an invariant error
    /// so two owners cannot silently rewrite each other's accounting.
    pub fn reserve(&self, key: &str, memory_gb: f64) -> Result<(), BackendError> {
        validate("memory_gb", memory_gb)?;
        if key.trim().is_empty() {
            return Err(BackendError::invalid_request(
                "global capacity allocation key must not be empty",
            ));
        }
        let mut allocations = self
            .allocations
            .lock()
            .map_err(|_| BackendError::lock_poisoned("global capacity ledger lock poisoned"))?;
        if let Some(existing) = allocations.get(key) {
            if *existing == memory_gb {
                return Ok(());
            }
            return Err(BackendError::invariant_violation(format!(
                "global capacity allocation {key:?} changed from {existing}GB to {memory_gb}GB"
            )));
        }
        let reserved_gb = allocations.values().sum::<f64>();
        if reserved_gb + memory_gb > self.budget_gb {
            return Err(BackendError::Oom {
                model_id: key.to_string(),
            });
        }
        allocations.insert(key.to_string(), memory_gb);
        Ok(())
    }

    pub fn release(&self, key: &str) -> Result<bool, BackendError> {
        let mut allocations = self
            .allocations
            .lock()
            .map_err(|_| BackendError::lock_poisoned("global capacity ledger lock poisoned"))?;
        Ok(allocations.remove(key).is_some())
    }

    pub fn snapshot(&self) -> Result<GlobalCapacitySnapshot, BackendError> {
        let allocations = self
            .allocations
            .lock()
            .map_err(|_| BackendError::lock_poisoned("global capacity ledger lock poisoned"))?;
        Ok(GlobalCapacitySnapshot {
            budget_gb: self.budget_gb,
            reserved_gb: allocations.values().sum(),
            allocations: allocations.clone(),
        })
    }
}

fn validate(field: &'static str, value: f64) -> Result<(), BackendError> {
    if !value.is_finite() || value < 0.0 {
        return Err(BackendError::invalid_request(format!(
            "invalid global capacity {field}: {value}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn reservations_are_global_idempotent_and_releasable() {
        let ledger = GlobalCapacityLedger::new(24.0).unwrap();
        ledger.reserve("omlx/a", 10.0).unwrap();
        ledger.reserve("omlx/a", 10.0).unwrap();
        ledger.reserve("llama/b", 12.0).unwrap();
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.reserved_gb, 22.0);
        assert_eq!(snapshot.allocations.len(), 2);
        assert!(ledger.release("omlx/a").unwrap());
        assert!(!ledger.release("omlx/a").unwrap());
        assert_eq!(ledger.snapshot().unwrap().reserved_gb, 12.0);
    }

    #[test]
    fn cross_runtime_overcommit_fails_closed_without_partial_reservation() {
        let ledger = GlobalCapacityLedger::new(16.0).unwrap();
        ledger.reserve("omlx/a", 10.0).unwrap();
        let error = ledger.reserve("llama/b", 8.0).unwrap_err();
        assert_eq!(error.reason_code(), "oom");
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.reserved_gb, 10.0);
        assert!(!snapshot.allocations.contains_key("llama/b"));
    }

    #[test]
    fn changed_same_key_and_invalid_capacity_are_rejected() {
        let ledger = GlobalCapacityLedger::new(8.0).unwrap();
        ledger.reserve("runtime/model", 4.0).unwrap();
        assert_eq!(
            ledger
                .reserve("runtime/model", 5.0)
                .unwrap_err()
                .reason_code(),
            "state_invariant_violated"
        );
        assert!(GlobalCapacityLedger::new(f64::NAN).is_err());
        assert!(ledger.reserve("bad", -1.0).is_err());
        assert!(ledger.reserve(" ", 1.0).is_err());
    }
}

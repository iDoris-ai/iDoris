//! Process-wide memory reservations shared by otherwise independent runtimes.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::BackendError;

/// Internal accounting is finer than the public 1e-6 GB admission epsilon:
/// 1 nano-GB = 1e-9 GB, so the existing epsilon is exactly 1000 units.
/// Budgets round down and allocations round up into these units. That makes
/// quantization itself conservative while leaving the already-documented
/// epsilon as the only intentional admission tolerance.
const CAPACITY_UNITS_PER_GB: u128 = 1_000_000_000;
const CAPACITY_EPSILON_UNITS: u128 = 1_000;

#[derive(Debug, Clone, Copy)]
struct CapacityAllocation {
    requested_gb: f64,
    charged_units: u128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalCapacitySnapshot {
    pub budget_gb: f64,
    pub reserved_gb: f64,
    pub allocations: BTreeMap<String, f64>,
}

#[derive(Debug)]
pub struct GlobalCapacityLedger {
    budget_gb: f64,
    budget_units: u128,
    allocations: Mutex<BTreeMap<String, CapacityAllocation>>,
}

impl GlobalCapacityLedger {
    pub fn new(budget_gb: f64) -> Result<Self, BackendError> {
        let budget_units = to_capacity_units("budget_gb", budget_gb, Rounding::Down)?;
        Ok(Self {
            budget_gb,
            budget_units,
            allocations: Mutex::new(BTreeMap::new()),
        })
    }

    /// Reserve one runtime/model allocation. Repeating the exact same key and
    /// amount is idempotent; changing an existing amount is an invariant error
    /// so two owners cannot silently rewrite each other's accounting.
    pub fn reserve(&self, key: &str, memory_gb: f64) -> Result<(), BackendError> {
        let charged_units = to_capacity_units("memory_gb", memory_gb, Rounding::Up)?;
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
            if existing.requested_gb == memory_gb {
                return Ok(());
            }
            return Err(BackendError::invariant_violation(format!(
                "global capacity allocation {key:?} changed from {}GB to {memory_gb}GB",
                existing.requested_gb
            )));
        }
        let Some(requested_total_units) = exact_total_units(&allocations)
            .and_then(|reserved| reserved.checked_add(charged_units))
        else {
            return Err(BackendError::Oom {
                model_id: key.to_string(),
            });
        };
        if requested_total_units > self.budget_units.saturating_add(CAPACITY_EPSILON_UNITS) {
            return Err(BackendError::Oom {
                model_id: key.to_string(),
            });
        }
        allocations.insert(
            key.to_string(),
            CapacityAllocation {
                requested_gb: memory_gb,
                charged_units,
            },
        );
        Ok(())
    }

    pub fn release(&self, key: &str) -> Result<bool, BackendError> {
        let mut allocations = self
            .allocations
            .lock()
            .map_err(|_| BackendError::lock_poisoned("global capacity ledger lock poisoned"))?;
        Ok(allocations.remove(key).is_some())
    }

    /// Atomic same-owner compare-and-set used by lifecycle reloads.
    ///
    /// `expected_gb=None` means the key must be absent; `next_gb=None`
    /// removes it. Growth is admitted against exact fixed-point global units.
    /// Confirmed non-increasing updates remain allowed even if observed truth
    /// has already put the ledger over budget.
    pub fn resize(
        &self,
        key: &str,
        expected_gb: Option<f64>,
        next_gb: Option<f64>,
    ) -> Result<(), BackendError> {
        self.compare_and_set(key, expected_gb, next_gb, true)
    }

    /// Record trusted already-existing residency without applying admission
    /// policy. This may make the snapshot exceed the configured budget because
    /// observation must describe reality rather than pretend it was rejected.
    pub fn adopt_observed(
        &self,
        key: &str,
        expected_gb: Option<f64>,
        observed_gb: f64,
    ) -> Result<(), BackendError> {
        self.compare_and_set(key, expected_gb, Some(observed_gb), false)
    }

    fn compare_and_set(
        &self,
        key: &str,
        expected_gb: Option<f64>,
        next_gb: Option<f64>,
        enforce_growth_admission: bool,
    ) -> Result<(), BackendError> {
        if key.trim().is_empty() {
            return Err(BackendError::invalid_request(
                "global capacity allocation key must not be empty",
            ));
        }
        if let Some(value) = expected_gb {
            let _ = to_capacity_units("expected_gb", value, Rounding::Up)?;
        }
        let next = match next_gb {
            Some(value) => Some(CapacityAllocation {
                requested_gb: value,
                charged_units: to_capacity_units("next_gb", value, Rounding::Up)?,
            }),
            None => None,
        };

        let mut allocations = self
            .allocations
            .lock()
            .map_err(|_| BackendError::lock_poisoned("global capacity ledger lock poisoned"))?;
        let current = allocations.get(key).copied();
        if current.map(|allocation| allocation.requested_gb) != expected_gb {
            return Err(BackendError::invariant_violation(format!(
                "global capacity allocation {key:?} expected {expected_gb:?} but is {:?}",
                current.map(|allocation| allocation.requested_gb)
            )));
        }

        let current_units = current.map_or(0, |allocation| allocation.charged_units);
        let next_units = next.map_or(0, |allocation| allocation.charged_units);
        let other_units = exact_total_units(&allocations)
            .and_then(|total| total.checked_sub(current_units))
            .ok_or_else(|| {
                BackendError::invariant_violation("global capacity exact total overflowed")
            })?;
        let requested_total_units = other_units.checked_add(next_units).ok_or_else(|| {
            BackendError::invariant_violation("global capacity exact total overflowed")
        })?;
        if enforce_growth_admission
            && next_units > current_units
            && requested_total_units > self.budget_units.saturating_add(CAPACITY_EPSILON_UNITS)
        {
            return Err(BackendError::Oom {
                model_id: key.to_string(),
            });
        }

        match next {
            Some(allocation) => {
                allocations.insert(key.to_string(), allocation);
            }
            None => {
                allocations.remove(key);
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<GlobalCapacitySnapshot, BackendError> {
        let allocations = self
            .allocations
            .lock()
            .map_err(|_| BackendError::lock_poisoned("global capacity ledger lock poisoned"))?;
        let reserved_units = exact_total_units(&allocations).ok_or_else(|| {
            BackendError::invariant_violation("global capacity exact total overflowed")
        })?;
        Ok(GlobalCapacitySnapshot {
            budget_gb: self.budget_gb,
            reserved_gb: units_to_gb(reserved_units),
            allocations: allocations
                .iter()
                .map(|(key, allocation)| (key.clone(), allocation.requested_gb))
                .collect(),
        })
    }
}

#[derive(Clone, Copy)]
enum Rounding {
    Down,
    Up,
}

fn to_capacity_units(
    field: &'static str,
    value: f64,
    rounding: Rounding,
) -> Result<u128, BackendError> {
    if !value.is_finite() || value < 0.0 {
        return Err(BackendError::invalid_request(format!(
            "invalid global capacity {field}: {value}"
        )));
    }
    if value == 0.0 {
        return Ok(0);
    }

    // Decode the IEEE-754 value exactly, then scale by 1e9 with integer
    // arithmetic. This avoids both magnitude-dependent loss (e.g. 1e16+1)
    // and any dependence on map iteration/summation order.
    let bits = value.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (significand, exponent) = if exponent_bits == 0 {
        (u128::from(fraction), -1074)
    } else {
        (
            u128::from((1_u64 << 52) | fraction),
            exponent_bits - 1023 - 52,
        )
    };
    let scaled = significand
        .checked_mul(CAPACITY_UNITS_PER_GB)
        .ok_or_else(|| capacity_range_error(field, value))?;

    let units = if exponent >= 0 {
        let factor = 1_u128
            .checked_shl(exponent as u32)
            .ok_or_else(|| capacity_range_error(field, value))?;
        scaled
            .checked_mul(factor)
            .ok_or_else(|| capacity_range_error(field, value))?
    } else {
        let shift = (-exponent) as u32;
        if shift >= u128::BITS {
            match rounding {
                Rounding::Down => 0,
                Rounding::Up => 1,
            }
        } else {
            let divisor = 1_u128 << shift;
            let quotient = scaled / divisor;
            let remainder = scaled % divisor;
            match rounding {
                Rounding::Down => quotient,
                Rounding::Up if remainder != 0 => quotient
                    .checked_add(1)
                    .ok_or_else(|| capacity_range_error(field, value))?,
                Rounding::Up => quotient,
            }
        }
    };
    Ok(units)
}

fn capacity_range_error(field: &'static str, value: f64) -> BackendError {
    BackendError::invalid_request(format!(
        "global capacity {field} is too large for exact fixed-point accounting: {value}"
    ))
}

fn exact_total_units(allocations: &BTreeMap<String, CapacityAllocation>) -> Option<u128> {
    allocations.values().try_fold(0_u128, |total, allocation| {
        total.checked_add(allocation.charged_units)
    })
}

fn units_to_gb(units: u128) -> f64 {
    (units as f64) / (CAPACITY_UNITS_PER_GB as f64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::{Arc, Barrier};
    use std::thread;

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
    fn resize_is_atomic_and_stale_expectations_do_not_mutate() {
        let ledger = GlobalCapacityLedger::new(10.0).unwrap();
        ledger.reserve("runtime/a", 4.0).unwrap();

        let stale = ledger
            .resize("runtime/a", Some(3.0), Some(5.0))
            .unwrap_err();
        assert_eq!(stale.reason_code(), "state_invariant_violated");
        assert_eq!(ledger.snapshot().unwrap().allocations["runtime/a"], 4.0);

        ledger.resize("runtime/a", Some(4.0), Some(6.0)).unwrap();
        assert_eq!(ledger.snapshot().unwrap().allocations["runtime/a"], 6.0);
    }

    #[test]
    fn resize_growth_checks_exact_global_capacity_without_partial_state() {
        let ledger = GlobalCapacityLedger::new(10.0).unwrap();
        ledger.reserve("runtime/a", 6.0).unwrap();
        ledger.reserve("runtime/b", 4.0).unwrap();

        let error = ledger
            .resize("runtime/a", Some(6.0), Some(7.0))
            .unwrap_err();
        assert_eq!(error.reason_code(), "oom");
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.reserved_gb, 10.0);
        assert_eq!(snapshot.allocations["runtime/a"], 6.0);
    }

    #[test]
    fn observed_overbudget_truth_can_shrink_and_release() {
        let ledger = GlobalCapacityLedger::new(8.0).unwrap();
        ledger.adopt_observed("runtime/a", None, 12.0).unwrap();
        assert_eq!(ledger.snapshot().unwrap().reserved_gb, 12.0);

        ledger.resize("runtime/a", Some(12.0), Some(9.0)).unwrap();
        assert_eq!(ledger.snapshot().unwrap().reserved_gb, 9.0);
        ledger.resize("runtime/a", Some(9.0), None).unwrap();
        assert_eq!(ledger.snapshot().unwrap().reserved_gb, 0.0);
    }

    #[test]
    fn cas_validates_inputs_before_mutation_and_distinguishes_zero_from_absent() {
        let ledger = GlobalCapacityLedger::new(1.0).unwrap();
        ledger.resize("runtime/a", None, Some(0.0)).unwrap();
        assert_eq!(ledger.snapshot().unwrap().allocations["runtime/a"], 0.0);

        assert!(
            ledger
                .resize("runtime/a", Some(0.0), Some(f64::MAX))
                .is_err()
        );
        assert_eq!(ledger.snapshot().unwrap().allocations["runtime/a"], 0.0);
        ledger.resize("runtime/a", Some(0.0), None).unwrap();
        assert!(
            !ledger
                .snapshot()
                .unwrap()
                .allocations
                .contains_key("runtime/a")
        );
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
        assert!(GlobalCapacityLedger::new(f64::INFINITY).is_err());
        assert!(GlobalCapacityLedger::new(-1.0).is_err());
        assert!(ledger.reserve("bad-nan", f64::NAN).is_err());
        assert!(ledger.reserve("bad-inf", f64::INFINITY).is_err());
        assert!(ledger.reserve("bad", -1.0).is_err());
        assert!(ledger.reserve(" ", 1.0).is_err());
    }

    #[test]
    fn decimal_boundary_uses_capacity_epsilon_but_real_excess_still_fails() {
        let ledger = GlobalCapacityLedger::new(0.3).unwrap();
        ledger.reserve("runtime/a", 0.1).unwrap();
        ledger.reserve("runtime/b", 0.2).unwrap();
        assert_eq!(ledger.snapshot().unwrap().allocations.len(), 2);

        let excess = crate::eviction::CAPACITY_EPSILON_GB * 10.0;
        let ledger = GlobalCapacityLedger::new(0.3).unwrap();
        ledger.reserve("runtime/a", 0.1).unwrap();
        let error = ledger.reserve("runtime/b", 0.2 + excess).unwrap_err();
        assert_eq!(error.reason_code(), "oom");
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.allocations.len(), 1);
        assert_eq!(snapshot.allocations["runtime/a"], 0.1);
    }

    #[test]
    fn concurrent_last_capacity_race_admits_exactly_one_contender() {
        let ledger = Arc::new(GlobalCapacityLedger::new(4.0).unwrap());
        let start = Arc::new(Barrier::new(2));
        let mut joins = Vec::new();
        for key in ["omlx/a", "llama/b"] {
            let ledger = Arc::clone(&ledger);
            let start = Arc::clone(&start);
            joins.push(thread::spawn(move || {
                start.wait();
                (key, ledger.reserve(key, 4.0))
            }));
        }

        let results = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|(_, result)| result.as_ref().is_err_and(|err| err.reason_code() == "oom"))
                .count(),
            1
        );
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.reserved_gb, 4.0);
        assert_eq!(snapshot.allocations.len(), 1);
        let winner = results
            .iter()
            .find_map(|(key, result)| result.is_ok().then_some(*key))
            .unwrap();
        assert_eq!(snapshot.allocations.get(winner), Some(&4.0));
    }

    #[test]
    fn concurrent_same_key_resize_has_one_compare_and_set_winner() {
        let ledger = Arc::new(GlobalCapacityLedger::new(16.0).unwrap());
        ledger.reserve("runtime/a", 4.0).unwrap();
        let start = Arc::new(Barrier::new(2));
        let mut joins = Vec::new();
        for next_gb in [5.0, 6.0] {
            let ledger = Arc::clone(&ledger);
            let start = Arc::clone(&start);
            joins.push(thread::spawn(move || {
                start.wait();
                (
                    next_gb,
                    ledger.resize("runtime/a", Some(4.0), Some(next_gb)),
                )
            }));
        }

        let results = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|(_, result)| result
                    .as_ref()
                    .is_err_and(|error| { error.reason_code() == "state_invariant_violated" }))
                .count(),
            1
        );
        let winner = results
            .iter()
            .find_map(|(next_gb, result)| result.is_ok().then_some(*next_gb))
            .unwrap();
        assert_eq!(ledger.snapshot().unwrap().allocations["runtime/a"], winner);
    }

    #[test]
    fn large_magnitude_one_gb_excess_is_never_rounded_away() {
        let ledger = GlobalCapacityLedger::new(1.0e16).unwrap();
        ledger.reserve("large", 1.0e16).unwrap();
        let before = ledger.snapshot().unwrap();
        assert!(before.reserved_gb.is_finite());

        let error = ledger.reserve("one-more-gb", 1.0).unwrap_err();
        assert_eq!(error.reason_code(), "oom");
        assert_eq!(ledger.snapshot().unwrap(), before);
    }

    #[test]
    fn large_magnitude_cas_growth_is_never_rounded_away() {
        let ledger = GlobalCapacityLedger::new(1.0e16).unwrap();
        ledger.reserve("large", 1.0e16).unwrap();
        ledger.resize("growth", None, Some(0.0)).unwrap();
        let before = ledger.snapshot().unwrap();

        let error = ledger.resize("growth", Some(0.0), Some(1.0)).unwrap_err();
        assert_eq!(error.reason_code(), "oom");
        assert_eq!(ledger.snapshot().unwrap(), before);
    }

    #[test]
    fn out_of_fixed_point_range_is_rejected_before_state_can_corrupt() {
        assert!(GlobalCapacityLedger::new(f64::MAX).is_err());

        let ledger = GlobalCapacityLedger::new(1.0e16).unwrap();
        let before = ledger.snapshot().unwrap();
        assert!(ledger.reserve("too-large", f64::MAX).is_err());
        assert_eq!(ledger.snapshot().unwrap(), before);
    }

    #[test]
    fn snapshot_total_is_finite_and_independent_of_key_sum_order() {
        let large = 1.0e16;
        let small = 1024.0;
        let budget = large + small;

        let large_first = GlobalCapacityLedger::new(budget).unwrap();
        large_first.reserve("a-large", large).unwrap();
        large_first.reserve("z-small", small).unwrap();

        let small_first = GlobalCapacityLedger::new(budget).unwrap();
        small_first.reserve("a-small", small).unwrap();
        small_first.reserve("z-large", large).unwrap();

        let first = large_first.snapshot().unwrap();
        let second = small_first.snapshot().unwrap();
        assert!(first.reserved_gb.is_finite());
        assert!(second.reserved_gb.is_finite());
        assert_eq!(first.reserved_gb, second.reserved_gb);
    }
}

#![allow(clippy::unwrap_used)]

use idoris_recommender::memory::{
    WiredMode, apple_reserve_gb, apple_usable_gb, recommended_wired_limit_mb,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Vectors {
    apple_budget_64: Vec<BudgetVector>,
}

#[derive(Deserialize)]
struct BudgetVector {
    mode: WiredMode,
    expected_usable_gb: f64,
}

fn vectors() -> Vectors {
    serde_json::from_str(include_str!("../../../testdata/recommender/memory.json")).unwrap()
}

fn close(left: f64, right: f64) {
    assert!((left - right).abs() <= 1e-12, "{left} != {right}");
}

#[test]
fn shared_64gb_budget_vectors_match_ts() {
    for vector in vectors().apple_budget_64 {
        close(
            apple_usable_gb(64.0, vector.mode),
            vector.expected_usable_gb,
        );
    }
}

#[test]
fn reserve_clamps_at_both_bounds_and_usable_keeps_the_minimum() {
    assert_eq!(apple_reserve_gb(8.0), 3.0);
    close(apple_reserve_gb(24.0), 7.2);
    assert_eq!(apple_reserve_gb(128.0), 16.0);

    close(apple_usable_gb(24.0, WiredMode::Conservative), 15.84);
    assert_eq!(recommended_wired_limit_mb(15.84), 16_220);
    close(apple_usable_gb(64.0, WiredMode::Conservative), 42.24);
}

#[test]
fn modes_are_monotonic_but_still_capped_by_system_reserve() {
    let conservative = apple_usable_gb(64.0, WiredMode::Conservative);
    let moderate = apple_usable_gb(64.0, WiredMode::Moderate);
    let aggressive = apple_usable_gb(64.0, WiredMode::Aggressive);
    assert!(conservative < moderate && moderate < aggressive);
    assert!(aggressive <= 64.0 - apple_reserve_gb(64.0));
}

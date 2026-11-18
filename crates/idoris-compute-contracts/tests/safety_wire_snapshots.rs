//! S6 儿童安全字符串的线上快照：用固定的 JSON 文件钉住新增的原因码、类别、检查阶段和「没能给出结论」的原因。
//!
//! 与 `wire_snapshots.rs` 分开：S0 的快照一个字节没动，S6 只加了这个文件和 `testdata/snapshots/safety_codes.json`。
//! 有意改动（只能加新字符串，不能改旧的）时用
//! `UPDATE_SNAPSHOTS=1 cargo test -p idoris-compute-contracts --test safety_wire_snapshots` 重新生成并逐行审阅 diff。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::safety::{
    SafetyCategory, SafetyReasonCode, SafetyStage, SafetyUnavailableCause,
};
use serde_json::json;
use support::assert_snapshot;

#[test]
fn the_safety_wire_strings_are_pinned() {
    assert_snapshot(
        "safety_codes",
        &json!({
            "codes": SafetyReasonCode::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "categories": SafetyCategory::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "unavailable_causes": SafetyUnavailableCause::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "stages": SafetyStage::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        }),
    );
}

#[test]
fn the_existing_s0_reason_codes_are_untouched_and_disjoint_from_the_safety_codes() {
    // S0 的快照文件没有被改：这里再独立核对一遍「S6 是纯加法」。
    assert_eq!(ReasonCode::ALL.len(), 16);
    let s0: Vec<&str> = ReasonCode::ALL.iter().map(|c| c.as_str()).collect();
    for code in SafetyReasonCode::ALL {
        assert!(!s0.contains(&code.as_str()), "{code}");
    }
}

//! 正式许可表（`config/compute/endpoint-permit-table.yaml`）里没有任何条目允许 `child_edu`。
//!
//! 只读仓库里的这份配置，不改契约实现。加载失败或任一条目带上 `child_edu` 都算失败。
//! DPA 未签时，加载器本来也会拒绝 `child_edu`；这里再断言数据本身没有这条类别。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use idoris_compute_contracts::permit::{DataClass, PermitTable};

#[test]
fn formal_permit_table_allows_no_child_edu() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/compute/endpoint-permit-table.yaml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    let table = PermitTable::from_yaml_str(&text)
        .unwrap_or_else(|err| panic!("permit table rejected: {err}"));
    assert!(
        !table.is_empty(),
        "the formal permit table has no endpoints"
    );
    for entry in table.endpoints() {
        assert!(
            !entry.allowed_data_classes.contains(&DataClass::ChildEdu),
            "{} allows child_edu",
            entry.key()
        );
    }
}

#![allow(clippy::unwrap_used)]

use std::path::Path;

use idoris_recommender::catalog::{
    Catalog, ModelKind, OmlxBinding, load_catalog, load_omlx_bindings,
};
use idoris_recommender::chat_recommendation::{
    ChatRecommendation, FALLBACK_ORDER, GIB, MODEL_8B, MODEL_27B, MODEL_35B, RecommendError,
    RecommendInput, probed_gpu_available_bytes, recommend_chat,
};
use idoris_recommender::local_models::LocalModel;

fn repo_file(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

struct Fixture {
    catalog: Catalog,
    bindings: Vec<OmlxBinding>,
}

fn fixture() -> Fixture {
    let path = repo_file("config/catalog.yaml");
    Fixture {
        catalog: load_catalog(&path).unwrap(),
        bindings: load_omlx_bindings(&path).unwrap(),
    }
}

fn run(
    fx: &Fixture,
    ram_bytes: u64,
    allocated: Option<u64>,
    installed: &[LocalModel],
) -> ChatRecommendation {
    recommend_chat(&RecommendInput {
        ram_bytes,
        allocated_budget_bytes: allocated,
        gpu_available_bytes: probed_gpu_available_bytes(ram_bytes),
        installed,
        catalog: &fx.catalog,
        bindings: &fx.bindings,
    })
    .unwrap()
}

fn alt_ids(rec: &ChatRecommendation) -> Vec<&str> {
    rec.alternatives
        .iter()
        .map(|alt| alt.model_id.as_str())
        .collect()
}

#[test]
fn tier_boundaries_are_left_closed_right_open_in_gib() {
    let fx = fixture();
    // (ram_bytes, 推荐, reason, alternatives)
    let cases: [(u64, &str, &str, &[&str]); 6] = [
        (16 * GIB - 1, MODEL_8B, "tier_below_16gib", &[]),
        (16 * GIB, MODEL_8B, "tier_16_48gib", &[]),
        (48 * GIB - 1, MODEL_8B, "tier_16_48gib", &[]),
        (48 * GIB, MODEL_27B, "tier_48_64gib", &[MODEL_8B]),
        (64 * GIB - 1, MODEL_27B, "tier_48_64gib", &[MODEL_8B]),
        (
            64 * GIB,
            MODEL_27B,
            "tier_64gib_plus",
            &[MODEL_35B, MODEL_8B],
        ),
    ];
    for (ram, model, reason, alternatives) in cases {
        let rec = run(&fx, ram, None, &[]);
        assert_eq!(rec.model_id.as_deref(), Some(model), "ram={ram}");
        assert_eq!(rec.reason, reason, "ram={ram}");
        assert_eq!(alt_ids(&rec), alternatives, "ram={ram}");
        assert!(rec.fits, "ram={ram}");
        assert_eq!(rec.role, "chat");
        assert_eq!(rec.host.ram_bytes, ram);
        assert_eq!(rec.host.allocated_budget_bytes, None);
    }
}

#[test]
fn eight_gib_machine_gets_null_in_the_first_tier() {
    let fx = fixture();
    let rec = run(&fx, 8 * GIB, None, &[]);
    assert_eq!(rec.model_id, None);
    assert_eq!(rec.reason, "tier_below_16gib");
    assert!(!rec.fits && !rec.installed);
    assert_eq!((rec.size_bytes, rec.est_memory_bytes), (None, None));
    assert!(rec.alternatives.is_empty());
}

#[test]
fn physical_ram_below_the_catalog_min_ram_gate_blocks_even_when_memory_would_fit() {
    let fx = fixture();
    // 11 GiB：可用 GPU 内存约 7.26 GB，8B 估算约 6.83 GB，单看内存放得下；但目录 min_ram_gb = 12。
    let ram = 11 * GIB;
    assert!(probed_gpu_available_bytes(ram) > 6_900_000_000);
    let rec = run(&fx, ram, None, &[]);
    assert_eq!(rec.model_id, None);
    assert_eq!(rec.reason, "tier_below_16gib");
    // 12 GiB 过门。
    assert_eq!(
        run(&fx, 12 * GIB, None, &[]).model_id.as_deref(),
        Some(MODEL_8B)
    );
}

#[test]
fn fallback_skips_models_at_or_above_the_tier_pick_and_takes_the_first_that_fits() {
    let fx = fixture();
    let est_27b = run(&fx, 64 * GIB, None, &[]).est_memory_bytes.unwrap();
    // 额度恰好等于 27B 的估算：放得下，不回落；少 1 字节：回落。
    let at = run(&fx, 64 * GIB, Some(est_27b), &[]);
    assert_eq!(at.model_id.as_deref(), Some(MODEL_27B));
    assert_eq!(at.reason, "tier_64gib_plus");
    assert_eq!(at.host.allocated_budget_bytes, Some(est_27b));

    // 35B-A3B 的估算比 27B 小：额度落在两者之间时，35B 单看放得下，但回落从「档位推荐的下一个」起，
    // 不会倒回去选 35B，只能到 8B。
    let est_35b = at
        .alternatives
        .iter()
        .find(|alt| alt.model_id == MODEL_35B)
        .unwrap()
        .est_memory_bytes
        .unwrap();
    assert!(est_35b < est_27b);
    let between = run(&fx, 64 * GIB, Some(est_27b - 1), &[]);
    assert_eq!(between.model_id.as_deref(), Some(MODEL_8B));
    assert_eq!(between.reason, "fallback_does_not_fit");
    assert!(between.fits);
    let alt_35 = between
        .alternatives
        .iter()
        .find(|alt| alt.model_id == MODEL_35B)
        .unwrap();
    assert!(alt_35.fits, "35B 自己放得下，只是回落顺序不回头选它");

    assert_eq!(FALLBACK_ORDER, [MODEL_35B, MODEL_27B, MODEL_8B]);
}

#[test]
fn nothing_fits_gives_null_and_insufficient_memory() {
    let fx = fixture();
    for ram in [16 * GIB, 48 * GIB, 64 * GIB, 128 * GIB] {
        let rec = run(&fx, ram, Some(1_000_000_000), &[]);
        assert_eq!(rec.model_id, None, "ram={ram}");
        assert_eq!(rec.reason, "insufficient_memory", "ram={ram}");
        assert!(!rec.fits && !rec.installed);
        assert_eq!((rec.size_bytes, rec.est_memory_bytes), (None, None));
    }
    // 额度 0 是真零，不是「没设置」。
    let zero = run(&fx, 64 * GIB, Some(0), &[]);
    assert_eq!(zero.model_id, None);
    assert_eq!(zero.reason, "insufficient_memory");
}

#[test]
fn allocation_overrides_probed_gpu_memory_in_both_directions() {
    let fx = fixture();
    // 没有额度：按自探的可用 GPU 内存。把它压到 10 GB，27B 放不下，回落 8B。
    let rec = recommend_chat(&RecommendInput {
        ram_bytes: 64 * GIB,
        allocated_budget_bytes: None,
        gpu_available_bytes: 10_000_000_000,
        installed: &[],
        catalog: &fx.catalog,
        bindings: &fx.bindings,
    })
    .unwrap();
    assert_eq!(rec.model_id.as_deref(), Some(MODEL_8B));
    assert_eq!(rec.reason, "fallback_does_not_fit");
    // 设置了额度：忽略自探值，哪怕自探值是 0。
    let rec = recommend_chat(&RecommendInput {
        ram_bytes: 64 * GIB,
        allocated_budget_bytes: Some(40_000_000_000),
        gpu_available_bytes: 0,
        installed: &[],
        catalog: &fx.catalog,
        bindings: &fx.bindings,
    })
    .unwrap();
    assert_eq!(rec.model_id.as_deref(), Some(MODEL_27B));
    assert_eq!(rec.reason, "tier_64gib_plus");
}

#[test]
fn spec_memory_evidence_27b_does_not_fit_24_or_32_gib_but_fits_48() {
    let fx = fixture();
    let est = run(&fx, 64 * GIB, None, &[]).est_memory_bytes.unwrap();
    // 规范：27B「约 23.8 GB」。
    assert!((23_800_000_000..23_900_000_000).contains(&est), "est={est}");
    assert!(probed_gpu_available_bytes(24 * GIB) < est);
    assert!(probed_gpu_available_bytes(32 * GIB) < est);
    assert!(probed_gpu_available_bytes(48 * GIB) >= est);
    // 规范：24 GiB 可用约 15.8–16.8 GB。
    let usable_24 = probed_gpu_available_bytes(24 * GIB);
    assert!((15_800_000_000..=16_800_000_000).contains(&usable_24));
}

#[test]
fn installed_and_size_come_from_the_local_scan_else_known_download_size() {
    let fx = fixture();
    let installed = [LocalModel {
        omlx_id: MODEL_8B.into(),
        kind: ModelKind::Chat,
        size_bytes: 4_700_000_001,
        catalog_id: Some("qwen3-8b".into()),
    }];
    let rec = run(&fx, 64 * GIB, None, &installed);
    // 档位推荐 27B 未安装：预计下载体积。
    assert!(!rec.installed);
    assert_eq!(rec.size_bytes, Some(20_680_000_000));
    let alt_8b = &rec.alternatives[1];
    assert_eq!(alt_8b.model_id, MODEL_8B);
    assert!(alt_8b.installed);
    assert_eq!(alt_8b.size_bytes, Some(4_700_000_001));
    // 规范没有 35B 的实测体积：未安装时 size_bytes 为 null（不是 0），内存估算仍有值。
    let alt_35b = &rec.alternatives[0];
    assert_eq!(alt_35b.model_id, MODEL_35B);
    assert_eq!(alt_35b.size_bytes, None);
    assert!(alt_35b.est_memory_bytes.is_some());

    let rec = run(&fx, 16 * GIB, None, &installed);
    assert!(rec.installed);
    assert_eq!(rec.size_bytes, Some(4_700_000_001));
}

#[test]
fn a_catalog_without_the_tier_models_is_an_error_not_a_guess() {
    let fx = fixture();
    let err = recommend_chat(&RecommendInput {
        ram_bytes: 16 * GIB,
        allocated_budget_bytes: None,
        gpu_available_bytes: 20_000_000_000,
        installed: &[],
        catalog: &fx.catalog,
        bindings: &[],
    })
    .unwrap_err();
    assert_eq!(err, RecommendError::CatalogMissing(MODEL_8B.into()));
}

// ---- §2.9 示例 JSON 当夹具 ----

fn spec_text() -> String {
    std::fs::read_to_string(repo_file(
        "docs/interfaces/iDoris-Agent24-边界与接口规范.md",
    ))
    .unwrap()
}

fn spec_section_29(spec: &str) -> &str {
    let start = spec.find("### 2.9 ").unwrap();
    let rest = &spec[start..];
    let end = rest.find("\n## 3.").unwrap();
    &rest[..end]
}

#[test]
fn spec_example_json_round_trips_through_the_response_type_byte_for_byte() {
    let spec = spec_text();
    let section = spec_section_29(&spec);
    let start = section.find("```json").unwrap() + "```json".len();
    let end = start + section[start..].find("```").unwrap();
    let example: serde_json::Value = serde_json::from_str(&section[start..end]).unwrap();
    let parsed: ChatRecommendation = serde_json::from_value(example.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), example);
    // 字段名与 null 口径：未知字段、缺字段都不能悄悄通过。
    let mut keys: Vec<_> = example.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "alternatives",
            "est_memory_bytes",
            "fits",
            "host",
            "installed",
            "model_id",
            "reason",
            "role",
            "size_bytes"
        ]
    );
    assert_eq!(parsed.host.allocated_budget_bytes, None);
    assert_eq!(parsed.alternatives[0].size_bytes, None);
}

#[test]
fn our_output_has_exactly_the_example_shape() {
    let fx = fixture();
    let rec = run(&fx, 16 * GIB, None, &[]);
    let value = serde_json::to_value(&rec).unwrap();
    let spec = spec_text();
    let section = spec_section_29(&spec);
    let start = section.find("```json").unwrap() + "```json".len();
    let end = start + section[start..].find("```").unwrap();
    let example: serde_json::Value = serde_json::from_str(&section[start..end]).unwrap();
    let keys = |v: &serde_json::Value| -> Vec<String> {
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(keys(&value), keys(&example));
    assert_eq!(keys(&value["host"]), keys(&example["host"]));
    // 一档 16–48 GiB 的 8B：示例里的取值（model_id / installed / fits / reason / host）。
    assert_eq!(value["model_id"], example["model_id"]);
    assert_eq!(value["reason"], example["reason"]);
    assert_eq!(value["fits"], example["fits"]);
    assert_eq!(value["host"]["ram_bytes"], example["host"]["ram_bytes"]);
    assert_eq!(
        value["host"]["allocated_budget_bytes"],
        serde_json::Value::Null
    );
    let run48 = run(&fx, 48 * GIB, None, &[]);
    let alt = serde_json::to_value(&run48.alternatives[0]).unwrap();
    assert_eq!(
        keys(&alt),
        keys(&example["alternatives"][0]),
        "alternatives 项的字段名"
    );
}

#[test]
fn every_reason_code_and_model_name_in_the_code_appears_verbatim_in_the_spec() {
    let spec = spec_text();
    let section = spec_section_29(&spec);
    for code in [
        "tier_below_16gib",
        "tier_16_48gib",
        "tier_48_64gib",
        "tier_64gib_plus",
        "fallback_does_not_fit",
        "insufficient_memory",
    ] {
        assert!(section.contains(&format!("`{code}`")), "{code}");
    }
    for model in [MODEL_8B, MODEL_27B, MODEL_35B] {
        assert!(section.contains(&format!("`{model}`")), "{model}");
    }
    let fx = fixture();
    // 代码里实际产出的每个 reason 都在规范集合里。
    let mut seen = std::collections::BTreeSet::new();
    for ram in [8, 16, 48, 64] {
        for allocated in [None, Some(10_000_000_000), Some(1)] {
            let rec = run(&fx, ram * GIB, allocated, &[]);
            seen.insert(rec.reason);
        }
    }
    for reason in seen {
        assert!(section.contains(&format!("`{reason}`")), "{reason}");
    }
}

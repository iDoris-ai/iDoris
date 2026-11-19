#![allow(clippy::unwrap_used)]

use std::path::Path;

use idoris_recommender::catalog::{ModelKind, load_catalog, load_omlx_bindings, parse_catalog};

fn real_catalog() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/catalog.yaml")
}

fn entry(extra: &str) -> serde_yaml::Value {
    let text = format!(
        "version: 1\ncatalog:\n- id: m1\n  params_total_b: 1\n  arch: {{n_layers: 1, n_kv_heads: 1, head_dim: 1}}\n  roles: []\n  quant_options: [{{label: q4_k_m, bpp: 0.55, quality: 0.98}}]\n  min_ram_gb: 1\n{extra}"
    );
    serde_yaml::from_str(&text).unwrap()
}

#[test]
fn real_catalog_binds_the_three_tier_models_to_chat() {
    let bindings = load_omlx_bindings(real_catalog()).unwrap();
    let pairs: Vec<_> = bindings
        .iter()
        .map(|b| (b.omlx_id.as_str(), b.catalog_id.as_str(), b.kind))
        .collect();
    assert_eq!(
        pairs,
        [
            ("Qwen3-8B-4bit", "qwen3-8b", Some(ModelKind::Chat)),
            (
                "Qwen3.6-35B-A3B-OptiQ-4bit",
                "qwen3.6-35b-a3b",
                Some(ModelKind::Chat)
            ),
            (
                "Qwen3.8-27B-OptiQ-4bit",
                "qwen3.8-27b",
                Some(ModelKind::Chat)
            ),
        ]
    );
    // 新字段只加不改：目录条数、顺序与已有字段不变。
    let catalog = load_catalog(real_catalog()).unwrap();
    assert_eq!(catalog.catalog.len(), 15);
}

#[test]
fn entries_without_the_new_fields_are_unaffected() {
    let value = entry("");
    assert!(parse_catalog(&value).is_ok());
    assert!(
        idoris_recommender::catalog::parse_omlx_bindings(&value)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn omlx_id_without_kind_is_allowed_and_kind_is_none() {
    let value = entry("  omlx_id: Some-Model\n");
    parse_catalog(&value).unwrap();
    let bindings = idoris_recommender::catalog::parse_omlx_bindings(&value).unwrap();
    assert_eq!(bindings[0].kind, None);
    assert_eq!(bindings[0].omlx_id, "Some-Model");
}

#[test]
fn bad_kind_bad_omlx_id_and_duplicates_fail_the_catalog_load() {
    for extra in [
        "  omlx_id: a\n  kind: llm\n",
        "  omlx_id: a\n  kind: 3\n",
        "  kind: chat\n",
        "  omlx_id: a/b\n  kind: chat\n",
        "  omlx_id: ../a\n",
        "  omlx_id: .hidden\n",
        "  omlx_id: ''\n",
        "  omlx_id: 5\n",
    ] {
        assert!(parse_catalog(&entry(extra)).is_err(), "{extra}");
    }
    let dup = format!(
        "{}\n- id: m2\n  params_total_b: 1\n  arch: {{n_layers: 1, n_kv_heads: 1, head_dim: 1}}\n  roles: []\n  quant_options: [{{label: q4_k_m, bpp: 0.55, quality: 0.98}}]\n  min_ram_gb: 1\n  omlx_id: a\n",
        serde_yaml::to_string(&entry("  omlx_id: a\n"))
            .unwrap()
            .trim_end()
    );
    let value: serde_yaml::Value = serde_yaml::from_str(&dup).unwrap();
    assert!(parse_catalog(&value).is_err());
}

#[test]
fn every_kind_value_in_the_spec_parses() {
    for kind in [
        "chat",
        "vision",
        "embedding",
        "rerank",
        "image",
        "ocr",
        "tts",
        "unknown",
    ] {
        let value = entry(&format!("  omlx_id: a\n  kind: {kind}\n"));
        let bindings = idoris_recommender::catalog::parse_omlx_bindings(&value).unwrap();
        assert_eq!(bindings[0].kind.unwrap().as_str(), kind);
    }
}

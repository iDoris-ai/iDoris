#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use idoris_recommender::catalog::load_catalog;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn test_dir(name: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "idoris-recommender-{name}-{}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn real_catalog() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/catalog.yaml")
        .canonicalize()
        .unwrap()
}

#[test]
fn loads_real_catalog_and_excluded_entries_from_explicit_path() {
    let catalog = load_catalog(real_catalog()).unwrap();
    assert_eq!(catalog.version, 1.0);
    assert_eq!(catalog.catalog.len(), 15);
    assert_eq!(catalog.catalog[0].id, "minicpm5-2b");
    let excluded = catalog.excluded.unwrap();
    assert_eq!(excluded.len(), 8);
    assert_eq!(excluded[0].id, "lfm2.5-2.6b");
}

#[test]
fn missing_bad_utf8_bad_yaml_and_duplicate_keys_fail_without_fallback() {
    let dir = test_dir("errors");
    let missing = dir.join("catalog.yaml");
    let err = load_catalog(&missing).unwrap_err();
    assert_eq!(err.path, missing.display().to_string());
    assert!(err.message.contains("failed to read catalog"));

    let bad_utf8 = dir.join("bad-utf8.yaml");
    fs::write(&bad_utf8, [0xff, 0xfe, 0xfd]).unwrap();
    let err = load_catalog(&bad_utf8).unwrap_err();
    assert_eq!(err.path, bad_utf8.display().to_string());
    assert!(err.message.contains("UTF-8"));

    let bad_yaml = dir.join("bad.yaml");
    fs::write(&bad_yaml, "version: 1\ncatalog: [\n").unwrap();
    let err = load_catalog(&bad_yaml).unwrap_err();
    assert_eq!(err.path, bad_yaml.display().to_string());
    assert!(err.message.contains("YAML"));

    let duplicate = dir.join("duplicate.yaml");
    fs::write(&duplicate, "version: 1\nversion: 2\ncatalog: []\n").unwrap();
    assert!(load_catalog(&duplicate).is_err());
}

#[test]
fn yaml_type_errors_reuse_the_semantic_parser_paths() {
    let dir = test_dir("typed");
    let path = dir.join("typed.yaml");
    fs::write(&path, "version: '1'\ncatalog: []\n").unwrap();
    let err = load_catalog(path).unwrap_err();
    assert_eq!(err.path, "$.version");
}

#[test]
fn absolute_explicit_path_is_independent_of_repository_layout() {
    let dir = test_dir("elsewhere");
    let copied = dir.join("elsewhere.yaml");
    fs::copy(real_catalog(), &copied).unwrap();
    let parsed = load_catalog(copied.canonicalize().unwrap()).unwrap();
    assert_eq!(parsed.catalog[4].id, "qwen3.5-9b");
}

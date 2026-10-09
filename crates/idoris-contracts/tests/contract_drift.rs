//! Rust-side equivalent of `pnpm check:contract-drift`: re-reads every
//! `packages/contracts/schema/*.schema.json` and asserts its declared
//! `properties`/`required` sets still match the [`SchemaShape`] constants
//! each Rust type carries. If a schema file changes without its matching
//! type (and this list) being updated, this test fails with `DRIFT: <file>`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use idoris_contracts::adapter_manifest::AdapterManifest;
use idoris_contracts::admin_v0::AdminBackend;
use idoris_contracts::admin_v0::AdminStatusResponse;
use idoris_contracts::component_card::ComponentCard;
use idoris_contracts::deploy_mode::DEPLOY_MODE_VALUES;
use idoris_contracts::load_policy::LoadPolicy;
use idoris_contracts::provider::ProviderDescriptor;
use idoris_contracts::routing_policy::RoutingPolicy;
use idoris_contracts::shape::SchemaShape;
use idoris_contracts::task_profile::TaskProfile;
use idoris_contracts::tenant::TenantContext;
use idoris_contracts::training_sample::TrainingSample;
use serde_json::Value;

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/contracts/schema")
}

fn load_schema(file: &str) -> Value {
    let path = schema_dir().join(file);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|err| panic!("failed to parse {file}: {err}"))
}

fn keys_of(value: &Value, field: &str) -> BTreeSet<String> {
    match value.get(field) {
        None => BTreeSet::new(),
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(other) => panic!("unexpected `{field}` shape: {other:?}"),
    }
}

fn assert_shape<T: SchemaShape>() {
    let schema = load_schema(T::SCHEMA_FILE);
    let actual_properties = keys_of(&schema, "properties");
    let expected_properties: BTreeSet<String> =
        T::PROPERTIES.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        actual_properties,
        expected_properties,
        "DRIFT: {} properties changed — update the struct and its SchemaShape::PROPERTIES",
        T::SCHEMA_FILE
    );

    let actual_required = keys_of(&schema, "required");
    let expected_required: BTreeSet<String> = T::REQUIRED.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        actual_required,
        expected_required,
        "DRIFT: {} required set changed — update the struct and its SchemaShape::REQUIRED",
        T::SCHEMA_FILE
    );
}

#[test]
fn admin_v0_backends_item_shape_matches_schema() {
    let schema = load_schema("admin-v0-backends.schema.json");
    assert_eq!(schema["type"], "array");
    let items = &schema["items"];
    let actual_properties = keys_of(items, "properties");
    let expected_properties: BTreeSet<String> =
        ["provider_id", "locality", "form", "lifecycle_runtime_bound"]
            .into_iter()
            .map(str::to_owned)
            .collect();
    assert_eq!(actual_properties, expected_properties);
    assert_eq!(keys_of(items, "required"), expected_properties);
    let _type_anchor: Option<AdminBackend> = None;
}

#[test]
fn provider_shape_matches_schema() {
    assert_shape::<ProviderDescriptor>();
}

#[test]
fn component_card_shape_matches_schema() {
    assert_shape::<ComponentCard>();
}

#[test]
fn load_policy_shape_matches_schema() {
    assert_shape::<LoadPolicy>();
}

#[test]
fn adapter_manifest_shape_matches_schema() {
    assert_shape::<AdapterManifest>();
}

#[test]
fn training_sample_shape_matches_schema() {
    assert_shape::<TrainingSample>();
}

#[test]
fn tenant_shape_matches_schema() {
    assert_shape::<TenantContext>();
}

#[test]
fn task_profile_shape_matches_schema() {
    assert_shape::<TaskProfile>();
}

#[test]
fn routing_policy_shape_matches_schema() {
    assert_shape::<RoutingPolicy>();
}

#[test]
fn admin_v0_status_shape_matches_schema() {
    assert_shape::<AdminStatusResponse>();
    let schema = load_schema("admin-v0-status.schema.json");
    let capacity = &schema["properties"]["capacity"]["oneOf"][0];
    assert_eq!(
        keys_of(capacity, "required"),
        ["entries".to_string(), "state".to_string()]
            .into_iter()
            .collect()
    );
    let entry = &capacity["properties"]["entries"]["items"];
    assert_eq!(
        keys_of(entry, "properties"),
        [
            "admission_status",
            "capability",
            "ctx_limit",
            "estimated_memory_gb",
            "id",
            "queue_depth",
            "resident",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
}

#[test]
fn deploy_mode_values_match_schema() {
    let schema = load_schema("deploy-mode.schema.json");
    let actual: BTreeSet<String> = schema["enum"]
        .as_array()
        .expect("deploy-mode.schema.json must have a top-level `enum`")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let expected: BTreeSet<String> = DEPLOY_MODE_VALUES.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        actual, expected,
        "DRIFT: deploy-mode.schema.json enum changed — update DeployMode and DEPLOY_MODE_VALUES"
    );
}

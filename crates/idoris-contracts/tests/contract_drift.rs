//! Rust-side equivalent of `pnpm check:contract-drift`: re-reads every
//! `packages/contracts/schema/*.schema.json` and asserts its declared
//! `properties`/`required` sets still match the [`SchemaShape`] constants
//! each Rust type carries. If a schema file changes without its matching
//! type (and this list) being updated, this test fails with `DRIFT: <file>`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use idoris_contracts::adapter_manifest::AdapterManifest;
use idoris_contracts::admin_v0::{
    AdminBackend, AdminModelsResponse, AdminRole, AdminRuntimesResponse, AdminStatusResponse,
};
use idoris_contracts::component_card::ComponentCard;
use idoris_contracts::deploy_mode::DEPLOY_MODE_VALUES;
use idoris_contracts::inspect::{FINDING_KIND_VALUES, InspectRequest, InspectResponse};
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
fn admin_v0_roles_item_shape_matches_schema() {
    let schema = load_schema("admin-v0-roles.schema.json");
    assert_eq!(schema["type"], "array");
    let items = &schema["items"];
    let expected: BTreeSet<String> = ["role", "aliases", "catalog_role"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(keys_of(items, "properties"), expected);
    assert_eq!(keys_of(items, "required"), expected);
    let _type_anchor: Option<AdminRole> = None;
}

#[test]
fn admin_v0_runtimes_shape_matches_schema() {
    assert_shape::<AdminRuntimesResponse>();
    let schema = load_schema("admin-v0-runtimes.schema.json");
    let variants = schema["properties"]["runtimes"]["items"]["oneOf"]
        .as_array()
        .expect("admin runtime entries must be a oneOf");
    assert_eq!(variants.len(), 2);
    let observed: BTreeSet<String> = [
        "provider_id",
        "state",
        "pressure",
        "used_gb",
        "model_memory_max_gb",
        "loaded",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys_of(&variants[0], "properties"), observed);
    assert_eq!(keys_of(&variants[0], "required"), observed);
    let error: BTreeSet<String> = ["provider_id", "state"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(keys_of(&variants[1], "properties"), error);
    assert_eq!(keys_of(&variants[1], "required"), error);
}

#[test]
fn admin_v0_models_shape_matches_schema() {
    assert_shape::<AdminModelsResponse>();
    let schema = load_schema("admin-v0-models.schema.json");
    let variants = schema["properties"]["sources"]["items"]["oneOf"]
        .as_array()
        .expect("admin model sources must be a oneOf");
    assert_eq!(variants.len(), 3);

    let base: BTreeSet<String> = ["models", "provider_id", "source", "state"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(keys_of(&variants[0], "properties"), base);
    assert_eq!(keys_of(&variants[0], "required"), base);
    assert_eq!(keys_of(&variants[1], "properties"), base);
    assert_eq!(keys_of(&variants[1], "required"), base);

    let mut error = base;
    error.insert("error".into());
    assert_eq!(keys_of(&variants[2], "properties"), error);
    assert_eq!(keys_of(&variants[2], "required"), error);
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

fn set_of(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[test]
fn inspect_request_shape_matches_schema() {
    assert_shape::<InspectRequest>();
}

#[test]
fn inspect_response_shape_matches_schema() {
    assert_shape::<InspectResponse>();
    let schema = load_schema("inspect-response.schema.json");
    let finding = &schema["properties"]["findings"]["items"];
    assert_eq!(keys_of(finding, "properties"), set_of(&["kind", "span"]));
    assert_eq!(keys_of(finding, "required"), set_of(&["kind"]));
    let span = &finding["properties"]["span"];
    assert_eq!(keys_of(span, "properties"), set_of(&["start", "length"]));
    assert_eq!(keys_of(span, "required"), set_of(&["start", "length"]));
    assert_eq!(
        keys_of(&finding["properties"]["kind"], "enum"),
        set_of(FINDING_KIND_VALUES),
        "DRIFT: inspect-response.schema.json finding kinds changed — update FindingKind and FINDING_KIND_VALUES"
    );
    assert_eq!(
        keys_of(&schema["properties"]["sensitivity"], "enum"),
        set_of(&["S0", "S1", "S2", "S3"])
    );
    assert_eq!(
        keys_of(&schema["properties"]["action"], "enum"),
        set_of(&["BLOCK", "REDACT", "ALLOW"])
    );
}

/// Every object in both inspect schemas is closed, and the response carries
/// no free-form string anywhere: only enums and integers. That is what makes
/// "no matched content or secret substring in responses" a property of the
/// contract instead of a promise of the implementation.
#[test]
fn inspect_schemas_are_closed_and_response_is_content_free() {
    fn walk(node: &Value, path: &str, allow_strings: bool) {
        match node {
            Value::Object(map) => {
                if map.get("type") == Some(&Value::from("object")) {
                    assert_eq!(
                        map.get("additionalProperties"),
                        Some(&Value::Bool(false)),
                        "{path} must set additionalProperties:false"
                    );
                }
                if !allow_strings {
                    assert_ne!(
                        map.get("type"),
                        Some(&Value::from("string")),
                        "{path} is a free-form string in the inspect response"
                    );
                }
                for (key, child) in map {
                    walk(child, &format!("{path}/{key}"), allow_strings);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}/{index}"), allow_strings);
                }
            }
            _ => {}
        }
    }
    walk(&load_schema("inspect-request.schema.json"), "#", true);
    walk(&load_schema("inspect-response.schema.json"), "#", false);
}

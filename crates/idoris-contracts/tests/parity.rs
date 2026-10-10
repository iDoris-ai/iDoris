//! Parity test: the Rust contracts crate must reach the same
//! valid/invalid verdict as the TS side for the same sample JSON.
//!
//! The corpus below is ported from
//! `packages/contracts/tests/contract/parity.test.ts`, which itself compares
//! the generated zod schemas against Ajv validating the same JSON Schema
//! files directly. Here we run the *same idea* one level down: for each
//! sample, validate it against the live `.schema.json` with the `jsonschema`
//! crate, and separately parse it with this crate's [`idoris_contracts::parse`]
//! — the two must agree on accept/reject.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use idoris_contracts::Contract;
use idoris_contracts::adapter_manifest::AdapterManifest;
use idoris_contracts::admin_v0::AdminBackendsResponse;
use idoris_contracts::admin_v0::{
    AdminAdmissionStatus, AdminCapacityEntry, AdminCapacitySnapshot, AdminCapacityState,
    AdminModelsResponse, AdminStatusResponse,
};
use idoris_contracts::component_card::ComponentCard;
use idoris_contracts::load_policy::LoadPolicy;
use idoris_contracts::provider::ProviderDescriptor;
use idoris_contracts::routing_policy::RoutingPolicy;
use idoris_contracts::task_profile::TaskProfile;
use idoris_contracts::tenant::TenantContext;
use idoris_contracts::training_sample::TrainingSample;
use serde_json::{Value, json};

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/contracts/schema")
}

fn load_raw_schema(file: &str) -> Value {
    let path = schema_dir().join(file);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|err| panic!("failed to parse {file}: {err}"))
}

/// `json-schema-to-zod`/Ajv resolve `$ref` against sibling files; the
/// `jsonschema` crate needs the same references inlined by hand since we
/// aren't standing up a full resolver for two files. Mirrors
/// `scripts/gen-contracts.mjs`'s `inlineRefs`.
fn inline_refs(node: &Value) -> Value {
    match node {
        Value::Array(items) => Value::Array(items.iter().map(inline_refs).collect()),
        Value::Object(map) => {
            if let Some(Value::String(ref_target)) = map.get("$ref") {
                let mut sub = load_raw_schema(ref_target);
                if let Some(obj) = sub.as_object_mut() {
                    obj.remove("$schema");
                    obj.remove("$id");
                    obj.remove("title");
                }
                return inline_refs(&sub);
            }
            Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), inline_refs(v)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

fn schema_says_valid(schema_file: &str, instance: &Value) -> bool {
    let schema = inline_refs(&load_raw_schema(schema_file));
    let validator = jsonschema::validator_for(&schema)
        .unwrap_or_else(|err| panic!("failed to compile {schema_file}: {err}"));
    validator.is_valid(instance)
}

/// Runs one corpus entry through both sides and asserts they agree.
fn assert_parity<T: Contract>(schema_file: &str, instance: &Value) {
    let ajv_like = schema_says_valid(schema_file, instance);
    let rust_side = idoris_contracts::parse::<T>(instance).is_ok();
    assert_eq!(
        rust_side, ajv_like,
        "parity mismatch for {schema_file} on {instance}: rust={rust_side} schema={ajv_like}"
    );
}

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

#[test]
fn admin_v0_backends_corpus() {
    let valid = json!([{
        "provider_id": "omlx-local",
        "locality": "loopback",
        "form": "http_service",
        "lifecycle_runtime_bound": true
    }]);
    assert_parity::<AdminBackendsResponse>("admin-v0-backends.schema.json", &valid);
    assert_parity::<AdminBackendsResponse>("admin-v0-backends.schema.json", &json!([]));
    let mut blank_id = valid.clone();
    blank_id[0]["provider_id"] = json!("");
    assert_parity::<AdminBackendsResponse>("admin-v0-backends.schema.json", &blank_id);
    let mut extra = valid.clone();
    extra[0]["endpoint"] = json!("http://127.0.0.1:8088/v1");
    assert_parity::<AdminBackendsResponse>("admin-v0-backends.schema.json", &extra);
}

#[test]
fn admin_v0_models_corpus() {
    let observed = json!({
        "sources": [{
            "provider_id": "omlx-local",
            "source": "http_models_endpoint",
            "state": "observed",
            "models": ["Qwen3-8B"]
        }]
    });
    assert_parity::<AdminModelsResponse>("admin-v0-models.schema.json", &observed);

    let configured = json!({
        "sources": [{
            "provider_id": "claude-subscription",
            "source": "subscription_registration",
            "state": "configured",
            "models": ["claude-sonnet"]
        }]
    });
    assert_parity::<AdminModelsResponse>("admin-v0-models.schema.json", &configured);

    let error = json!({
        "sources": [{
            "provider_id": "omlx-local",
            "source": "http_models_endpoint",
            "state": "error",
            "models": [],
            "error": "unavailable"
        }]
    });
    assert_parity::<AdminModelsResponse>("admin-v0-models.schema.json", &error);

    let mut observed_with_error = observed.clone();
    observed_with_error["sources"][0]["error"] = json!("unavailable");
    assert_parity::<AdminModelsResponse>("admin-v0-models.schema.json", &observed_with_error);

    let mut configured_empty = configured.clone();
    configured_empty["sources"][0]["models"] = json!([]);
    assert_parity::<AdminModelsResponse>("admin-v0-models.schema.json", &configured_empty);

    let mut error_with_model = error.clone();
    error_with_model["sources"][0]["models"] = json!(["should-not-leak"]);
    assert_parity::<AdminModelsResponse>("admin-v0-models.schema.json", &error_with_model);

    let mut wrong_source = observed.clone();
    wrong_source["sources"][0]["source"] = json!("subscription_registration");
    assert_parity::<AdminModelsResponse>("admin-v0-models.schema.json", &wrong_source);
}

#[test]
fn admin_v0_status_corpus() {
    let valid = json!({
        "status": "ok", "service": "idoris", "version": "0.2.0", "contract_version": "1.0.1",
        "instance_id": "instance-1", "components": 2, "runtimes": 1, "subscriptions": 0,
        "budget_configured": true, "audit_configured": true,
        "capacity": {"state": "observed", "entries": [{
            "id": "model-a", "capability": "reasoning", "resident": true,
            "estimated_memory_gb": 12.5, "ctx_limit": 131072, "queue_depth": 1,
            "admission_status": "ready"
        }]}
    });
    assert_parity::<AdminStatusResponse>("admin-v0-status.schema.json", &valid);
    let mut wrong_service = valid.clone();
    wrong_service["service"] = json!("other");
    assert_parity::<AdminStatusResponse>("admin-v0-status.schema.json", &wrong_service);
    let mut bad_capacity = valid.clone();
    bad_capacity["capacity"] = json!({"state": "error", "entries": []});
    assert_parity::<AdminStatusResponse>("admin-v0-status.schema.json", &bad_capacity);
}

#[test]
fn admin_v0_status_rejects_non_finite_capacity_memory() {
    for estimated_memory_gb in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
        let status = AdminStatusResponse {
            status: "ok".into(),
            service: "idoris".into(),
            version: "0.2.0".into(),
            contract_version: "1.0.1".into(),
            instance_id: "instance-1".into(),
            components: 0,
            runtimes: 0,
            subscriptions: 0,
            budget_configured: false,
            audit_configured: false,
            capacity: AdminCapacitySnapshot {
                state: AdminCapacityState::Observed,
                entries: Some(vec![AdminCapacityEntry {
                    id: "model-a".into(),
                    capability: "reasoning".into(),
                    resident: true,
                    estimated_memory_gb,
                    ctx_limit: 1,
                    queue_depth: 0,
                    admission_status: AdminAdmissionStatus::Ready,
                }]),
            },
        };
        assert!(
            status.validate().is_err(),
            "accepted {estimated_memory_gb:?}"
        );
    }
}

#[test]
fn provider_corpus() {
    let valid = json!({
        "id": "omlx-local", "family": "local", "tier": "local", "capabilities": ["chat"],
        "privacy_class": "local_only", "cost": {"input_per_m": 0, "output_per_m": 0}, "locality": "loopback",
    });
    assert_parity::<ProviderDescriptor>("provider.schema.json", &valid);

    let mut empty_caps = valid.clone();
    empty_caps["capabilities"] = json!([]);
    assert_parity::<ProviderDescriptor>("provider.schema.json", &empty_caps);

    let mut bad_tier = valid.clone();
    bad_tier["tier"] = json!("cloud");
    assert_parity::<ProviderDescriptor>("provider.schema.json", &bad_tier);

    let mut extra_field = valid.clone();
    extra_field["nope"] = json!(true);
    assert_parity::<ProviderDescriptor>("provider.schema.json", &extra_field);
}

#[test]
fn component_card_corpus() {
    let provider = json!({
        "id": "omlx-local", "family": "local", "tier": "local", "capabilities": ["chat"],
        "privacy_class": "local_only", "cost": {"input_per_m": 0, "output_per_m": 0}, "locality": "loopback",
    });
    let valid = json!({
        "provider": provider, "form": "http_service", "endpoint": "http://127.0.0.1:8088/v1",
        "version_pin": "omlx@0.6.4", "privacy_class": "local_only", "allowed_egress": ["loopback"],
        "fallback_policy": "fail_closed", "fail_closed": true,
        "load_policy": {"mode": "resident", "keepalive": {"pinned": true}, "admission": "coexist"},
    });
    assert_parity::<ComponentCard>("component-card.schema.json", &valid);

    let mut blank_pin = valid.clone();
    blank_pin["version_pin"] = json!("");
    assert_parity::<ComponentCard>("component-card.schema.json", &blank_pin);

    let mut bad_egress = valid.clone();
    bad_egress["allowed_egress"] = json!(["internet", "typo"]);
    assert_parity::<ComponentCard>("component-card.schema.json", &bad_egress);

    let mut with_extensions = valid.clone();
    with_extensions["extensions"] = json!({"provider.anthropic.prompt_cache": {"ttl": 5}});
    assert_parity::<ComponentCard>("component-card.schema.json", &with_extensions);
}

#[test]
fn load_policy_corpus() {
    assert_parity::<LoadPolicy>(
        "load-policy.schema.json",
        &json!({"mode": "resident", "keepalive": {"pinned": true}, "admission": "coexist"}),
    );
    assert_parity::<LoadPolicy>(
        "load-policy.schema.json",
        &json!({"mode": "on_demand", "keepalive": {"idle_ttl_s": 300}, "admission": "requires_eviction"}),
    );
    assert_parity::<LoadPolicy>(
        "load-policy.schema.json",
        &json!({"mode": "resident", "keepalive": {}, "admission": "coexist"}),
    );
    assert_parity::<LoadPolicy>(
        "load-policy.schema.json",
        &json!({"mode": "resident", "keepalive": {"pinned": true, "idle_ttl_s": 5}, "admission": "coexist"}),
    );
    assert_parity::<LoadPolicy>(
        "load-policy.schema.json",
        &json!({"mode": "sometimes", "keepalive": {"pinned": true}, "admission": "coexist"}),
    );
    assert_parity::<LoadPolicy>(
        "load-policy.schema.json",
        &json!({"mode": "resident", "keepalive": {"idle_ttl_s": 0}, "admission": "coexist"}),
    );
}

#[test]
fn adapter_manifest_corpus() {
    let valid = json!({
        "adapter_id": "lora-1", "base_model_id": "Qwen3-4B-mlx", "base_digest": digest('a'),
        "tokenizer_digest": digest('b'), "rank": 16, "data_class": "synthetic",
    });
    assert_parity::<AdapterManifest>("adapter-manifest.schema.json", &valid);

    let mut blank_id = valid.clone();
    blank_id["adapter_id"] = json!("");
    assert_parity::<AdapterManifest>("adapter-manifest.schema.json", &blank_id);

    let mut short_digest = valid.clone();
    short_digest["base_digest"] = json!("sha256:abc");
    assert_parity::<AdapterManifest>("adapter-manifest.schema.json", &short_digest);

    let mut zero_rank = valid.clone();
    zero_rank["rank"] = json!(0);
    assert_parity::<AdapterManifest>("adapter-manifest.schema.json", &zero_rank);

    let mut bad_class = valid.clone();
    bad_class["data_class"] = json!("nope");
    assert_parity::<AdapterManifest>("adapter-manifest.schema.json", &bad_class);
}

#[test]
fn training_sample_corpus() {
    let valid = json!({
        "sample_id": "s-1", "data_class": "synthetic",
        "source": {"kind": "refined_from_synthetic"},
        "messages": [{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}],
    });
    assert_parity::<TrainingSample>("training-sample.schema.json", &valid);

    let mut bad_class = valid.clone();
    bad_class["data_class"] = json!("nope");
    assert_parity::<TrainingSample>("training-sample.schema.json", &bad_class);

    let mut one_message = valid.clone();
    one_message["messages"] = json!([{"role": "user", "content": "hi"}]);
    assert_parity::<TrainingSample>("training-sample.schema.json", &one_message);

    let mut bad_source_kind = valid.clone();
    bad_source_kind["source"] = json!({"kind": "from_the_internet"});
    assert_parity::<TrainingSample>("training-sample.schema.json", &bad_source_kind);

    let mut blank_sample_id = valid.clone();
    blank_sample_id["sample_id"] = json!("");
    assert_parity::<TrainingSample>("training-sample.schema.json", &blank_sample_id);
}

#[test]
fn task_profile_corpus() {
    assert_parity::<TaskProfile>("task-profile.schema.json", &json!({}));
    assert_parity::<TaskProfile>("task-profile.schema.json", &json!({"privacy": "nope"}));
    assert_parity::<TaskProfile>("task-profile.schema.json", &json!({"capabilities": []}));
}

#[test]
fn routing_policy_corpus() {
    assert_parity::<RoutingPolicy>(
        "routing-policy.schema.json",
        &json!({"routing_policy": {"version": 1, "rules": [{"if": {"privacy": "local_only"}, "then": {"tiers": ["local", "lora"], "fail_closed": true}}], "default": {"tiers": ["local"], "fail_closed": true}}}),
    );
    assert_parity::<RoutingPolicy>(
        "routing-policy.schema.json",
        &json!({"routing_policy": {"version": 1, "rules": []}}),
    );
    assert_parity::<RoutingPolicy>(
        "routing-policy.schema.json",
        &json!({"routing_policy": {"version": 1, "rules": [], "default": {}}}),
    );
    assert_parity::<RoutingPolicy>(
        "routing-policy.schema.json",
        &json!({"routing_policy": {"version": 1, "rules": [{"if": {}, "then": {}}], "default": {"tiers": ["local"]}}}),
    );
    assert_parity::<RoutingPolicy>(
        "routing-policy.schema.json",
        &json!({"routing_policy": {"version": 1, "rules": [], "default": {"tiers": []}}}),
    );
}

#[test]
fn tenant_corpus() {
    let valid = json!({
        "tenant_id": "acme-co",
        "budget": {"limit_minor": 5_000_000, "spent_minor": 1_234_567, "scope": "paid_only"},
        "billing_timezone": "Asia/Bangkok",
        "quota": {"rpm": 60, "tpm": 200_000},
    });
    assert_parity::<TenantContext>("tenant.schema.json", &valid);

    let mut no_scope = valid.clone();
    no_scope["budget"] = json!({"limit_minor": 1, "spent_minor": 0});
    let parsed = idoris_contracts::parse::<TenantContext>(&no_scope)
        .expect("budget.scope should default to paid_only");
    assert_eq!(
        parsed.budget.scope,
        idoris_contracts::tenant::BudgetScope::PaidOnly
    );

    let mut blank_tenant = valid.clone();
    blank_tenant["tenant_id"] = json!("   ");
    // `pattern: "\\S"` is a core JSON Schema keyword, so both sides reject it.
    assert_parity::<TenantContext>("tenant.schema.json", &blank_tenant);

    // NOT run through `assert_parity`: `billing_timezone`'s
    // `format: "iana-timezone"` is a custom format the `jsonschema` crate
    // doesn't enforce out of the box (unregistered custom formats are
    // no-ops per the JSON Schema spec), so the raw schema alone accepts this
    // — only the TS side's hand-written `superRefine` and our
    // `is_iana_time_zone` actually check it. Asserting the Rust side directly
    // against the known TS-side verdict (reject) instead.
    let mut bad_tz = valid.clone();
    bad_tz["billing_timezone"] = json!("Mars/Phobos");
    assert!(idoris_contracts::parse::<TenantContext>(&bad_tz).is_err());
}

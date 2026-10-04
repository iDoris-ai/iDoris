#![allow(clippy::unwrap_used)]

use idoris_recommender::catalog::parse_catalog;
use serde_json::json;
use serde_yaml::Value;

fn yaml(value: serde_json::Value) -> Value {
    serde_yaml::to_value(value).unwrap()
}

fn valid_model() -> Value {
    yaml(json!({
        "id": "m",
        "family": 42,
        "params_total_b": 1.5,
        "params_active_b": null,
        "arch": {"n_layers": 1.5, "n_kv_heads": -2.0, "head_dim": 3.25},
        "modality": ["text"],
        "roles": ["daily"],
        "load_hint": "on_demand",
        "capability": {"future-capability": 0.25},
        "quant_options": [{"label": "q", "bpp": 0.5, "quality": 0.9}],
        "license": false,
        "min_ram_gb": -4.0,
        "status": null,
        "note": 7,
        "future_field": {"ignored": true}
    }))
}

fn catalog(model: Value) -> Value {
    yaml(json!({"version": 0.5, "catalog": [model], "excluded": "ignored"}))
}

#[test]
fn valid_inputs_preserve_ts_finite_number_and_loose_optional_semantics() {
    let parsed = parse_catalog(&catalog(valid_model())).unwrap();
    let model = &parsed.catalog[0];
    assert_eq!(parsed.version, 0.5);
    assert_eq!(model.arch.n_layers, 1.5);
    assert_eq!(model.arch.n_kv_heads, -2.0);
    assert_eq!(model.arch.head_dim, 3.25);
    assert_eq!(model.min_ram_gb, -4.0);
    assert_eq!(model.family, None);
    assert_eq!(model.license, None);
    assert_eq!(model.note, None);
    assert_eq!(
        model.capability.as_ref().unwrap()["future-capability"],
        0.25
    );
    assert_eq!(parsed.excluded, None);
}

#[test]
fn roles_quant_ids_and_load_hint_are_validated_with_paths() {
    let mut model = valid_model();
    model["roles"] = yaml(json!(["daily "]));
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].roles[0]"
    );

    let mut model = valid_model();
    model["roles"] = yaml(json!(["daily", "daily"]));
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].roles"
    );

    let mut model = valid_model();
    model["quant_options"] = yaml(json!([]));
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].quant_options"
    );

    let mut model = valid_model();
    model["quant_options"] = yaml(json!([{"label":"q","quality":1.0}]));
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].quant_options[0]"
    );

    let mut model = valid_model();
    model["load_hint"] = Value::String("resident".into());
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].load_hint"
    );

    let model = valid_model();
    let raw = yaml(json!({"version":1.0,"catalog":[model.clone(),model]}));
    assert_eq!(parse_catalog(&raw).unwrap_err().path, "$.catalog");
}

#[test]
fn missing_roles_and_bad_shape_fields_fail_at_the_specific_path() {
    let mut model = valid_model();
    let key = Value::String("roles".into());
    model.as_mapping_mut().unwrap().remove(&key);
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].roles"
    );

    let mut model = valid_model();
    model["params_total_b"] = Value::String("NaN".into());
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].params_total_b"
    );

    let mut model = valid_model();
    model["modality"] = Value::String("text".into());
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].modality"
    );

    let mut model = valid_model();
    model["capability"] = yaml(json!({"coding":"high"}));
    assert_eq!(
        parse_catalog(&catalog(model)).unwrap_err().path,
        "$.catalog[0].capability.coding"
    );
}

#[test]
fn excluded_is_ignored_unless_array_and_array_entries_are_validated() {
    let mut raw = catalog(valid_model());
    raw["excluded"] = yaml(json!([{"id":"old","reason":"legacy"}]));
    assert_eq!(parse_catalog(&raw).unwrap().excluded.unwrap()[0].id, "old");

    raw["excluded"] = yaml(json!([{"id":"","reason":"legacy"}]));
    assert_eq!(parse_catalog(&raw).unwrap_err().path, "$.excluded[0].id");
}

#[test]
fn nonfinite_yaml_numbers_are_rejected() {
    let raw: Value = serde_yaml::from_str(
        "version: 1\ncatalog:\n  - id: m\n    params_total_b: .nan\n    arch: {n_layers: 1, n_kv_heads: 1, head_dim: 1}\n    roles: [daily]\n    quant_options: [{label: q, bpp: 1, quality: 1}]\n    min_ram_gb: 1\n",
    )
    .unwrap();
    assert_eq!(
        parse_catalog(&raw).unwrap_err().path,
        "$.catalog[0].params_total_b"
    );
}

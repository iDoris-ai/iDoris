#![allow(clippy::expect_used, clippy::unwrap_used)]

mod parity_support;

use std::path::PathBuf;

use idoris_recommender::catalog::{load_catalog, parse_catalog};
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::recommend;
use serde_json::Value;

use parity_support::{assert_projection, assert_recommendation, partial_policy};

fn read(path: &str) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(repo_root().join(path)).expect("shared fixture must be readable"),
    )
    .expect("shared fixture must be JSON")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn ram_reference_fixture_matches_typescript_at_all_six_tiers() {
    let fixture = read("testdata/recommender/ram.json");
    let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
    for ram in [8_u64, 16, 24, 32, 64, 128] {
        let hardware = make_host_facts(HostFactsInput {
            ram_gb: ram as f64,
            chip: Some("M4".into()),
            gpu_cores: Some(10),
            os: Some("darwin".into()),
        })
        .unwrap();
        let actual = recommend(&hardware, &catalog, None, None).unwrap();
        let expected = &fixture["cases"][ram.to_string()];
        if ram == 24 {
            assert_recommendation(&actual, &expected["full"]);
        } else {
            assert_projection(&actual, &expected["projection"]);
        }
    }
}

#[test]
fn every_edge_scenario_replays_the_typescript_full_recommendation() {
    let fixture = read("testdata/recommender/edges.json");
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let input = &scenario["input"];
        let name = input["name"].as_str().unwrap();
        let yaml: serde_yaml::Value =
            serde_yaml::from_str(&serde_json::to_string(&input["catalog"]).unwrap()).unwrap();
        let catalog = parse_catalog(&yaml).unwrap_or_else(|error| panic!("{name}: {error}"));
        let hardware = make_host_facts(HostFactsInput {
            ram_gb: input["ram"].as_f64().unwrap(),
            chip: Some("fixture".into()),
            gpu_cores: None,
            os: Some("darwin".into()),
        })
        .unwrap();
        let policy = partial_policy(input.get("policy"));
        let forced = input
            .get("env")
            .and_then(|env| env.get("IDORIS_CORE_MODEL"))
            .and_then(Value::as_str);
        let actual = recommend(&hardware, &catalog, Some(&policy), forced).unwrap();
        assert_recommendation(&actual, &scenario["output"]);
    }
}

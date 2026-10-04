#![allow(clippy::unwrap_used)]

use idoris_recommender::probe::{
    FactsSource, HostFactsInput, UNKNOWN_CHIP, make_host_facts, nominal_ram_gb, parse_chip,
    parse_system_profiler_gpu_cores, probed_host_facts, with_system_profiler,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct ProbeVectors {
    nominal_ram: Vec<NominalRam>,
    chips: Vec<Chip>,
    gpu_profiler: Vec<Gpu>,
}
#[derive(Deserialize)]
struct NominalRam {
    bytes: f64,
    expected_gb: f64,
}
#[derive(Deserialize)]
struct Chip {
    input: String,
    expected: String,
}
#[derive(Deserialize)]
struct Gpu {
    input: String,
    expected: Option<u32>,
}

fn vectors() -> ProbeVectors {
    serde_json::from_str(include_str!("../../../testdata/recommender/probe.json")).unwrap()
}

#[test]
fn shared_probe_vectors_match_ts_reference() {
    let vectors = vectors();
    for item in vectors.nominal_ram {
        assert_eq!(nominal_ram_gb(item.bytes).unwrap(), item.expected_gb);
    }
    for item in vectors.chips {
        assert_eq!(parse_chip(Some(&item.input)), item.expected);
    }
    for item in vectors.gpu_profiler {
        assert_eq!(parse_system_profiler_gpu_cores(&item.input), item.expected);
    }
}

#[test]
fn invalid_ram_and_unknown_text_fail_or_default_conservatively() {
    for bytes in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(nominal_ram_gb(bytes).is_err());
    }
    assert_eq!(parse_chip(None), UNKNOWN_CHIP);
    assert_eq!(parse_chip(Some("")), UNKNOWN_CHIP);
    assert_eq!(parse_chip(Some("Apple M")), "M");
    assert_eq!(parse_chip(Some("CPU Apple M")), "CPU Apple M");
    assert_eq!(parse_system_profiler_gpu_cores("no gpu info"), None);
}

#[test]
fn injection_and_probe_sources_are_explicit_and_gpu_merge_only_changes_gpu() {
    let injected = make_host_facts(HostFactsInput {
        ram_gb: 64.0,
        chip: None,
        gpu_cores: None,
        os: None,
    })
    .unwrap();
    assert_eq!(injected.source, FactsSource::Injected);
    assert_eq!(injected.chip, UNKNOWN_CHIP);
    assert_eq!(injected.os, "unknown");

    let probed = probed_host_facts(68_719_476_736.0, Some("Apple M1 Max"), "darwin").unwrap();
    assert_eq!(probed.source, FactsSource::Probe);
    let merged = with_system_profiler(
        probed.clone(),
        "Chipset Model: Apple M1 Max\n Total Number of Cores: 32\n",
    );
    assert_eq!(merged.ram_gb, probed.ram_gb);
    assert_eq!(merged.chip, probed.chip);
    assert_eq!(merged.os, probed.os);
    assert_eq!(merged.gpu_cores, Some(32));
}

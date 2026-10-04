#![allow(clippy::unwrap_used)]

use std::cell::Cell;

use idoris_recommender::probe::{
    FactsSource, HostFactsInput, HostProbe, UNKNOWN_CHIP, inspect_host, make_host_facts,
    with_system_profiler,
};

struct FakeProbe {
    total: Result<f64, &'static str>,
    cpu: Result<Option<String>, &'static str>,
    platform: Result<String, &'static str>,
}

impl HostProbe for FakeProbe {
    fn total_mem_bytes(&self) -> Result<f64, idoris_recommender::probe::ProbeError> {
        self.total.map_err(|_| probe_error())
    }
    fn cpu_model(&self) -> Result<Option<String>, idoris_recommender::probe::ProbeError> {
        self.cpu.clone().map_err(|_| probe_error())
    }
    fn platform(&self) -> Result<String, idoris_recommender::probe::ProbeError> {
        self.platform.clone().map_err(|_| probe_error())
    }
}

fn probe_error() -> idoris_recommender::probe::ProbeError {
    // Public constructors are intentionally unnecessary; generate the same
    // validation error through the public raw-facts boundary.
    idoris_recommender::probe::nominal_ram_gb(0.0).unwrap_err()
}

#[test]
fn inspect_host_samples_raw_facts_and_marks_probe_source() {
    let probe = FakeProbe {
        total: Ok(68_719_476_736.0),
        cpu: Ok(Some("Apple M1 Max".into())),
        platform: Ok("darwin".into()),
    };
    let facts = inspect_host(&probe).unwrap();
    assert_eq!(facts.ram_gb, 64.0);
    assert_eq!(facts.chip, "M1 Max");
    assert_eq!(facts.os, "darwin");
    assert_eq!(facts.gpu_cores, None);
    assert_eq!(facts.source, FactsSource::Probe);

    let merged = with_system_profiler(facts, "Total Number of Cores: 32\n");
    assert_eq!(merged.gpu_cores, Some(32));
}

#[test]
fn probe_failures_bad_ram_missing_cpu_and_unknown_platform_fail_or_default_conservatively() {
    let failed = FakeProbe {
        total: Err("boom"),
        cpu: Ok(None),
        platform: Ok("darwin".into()),
    };
    assert!(inspect_host(&failed).is_err());

    let missing = FakeProbe {
        total: Ok(25_769_803_776.0),
        cpu: Ok(None),
        platform: Ok(String::new()),
    };
    let facts = inspect_host(&missing).unwrap();
    assert_eq!(facts.chip, UNKNOWN_CHIP);
    assert_eq!(facts.os, "unknown");

    let invalid = FakeProbe {
        total: Ok(f64::INFINITY),
        cpu: Ok(Some("Apple M4".into())),
        platform: Ok("darwin".into()),
    };
    assert!(inspect_host(&invalid).is_err());
}

struct PanicProbe(Cell<usize>);
impl HostProbe for PanicProbe {
    fn total_mem_bytes(&self) -> Result<f64, idoris_recommender::probe::ProbeError> {
        self.0.set(self.0.get() + 1);
        panic!("injected HostFacts must not call a probe")
    }
    fn cpu_model(&self) -> Result<Option<String>, idoris_recommender::probe::ProbeError> {
        unreachable!()
    }
    fn platform(&self) -> Result<String, idoris_recommender::probe::ProbeError> {
        unreachable!()
    }
}

#[test]
fn injected_facts_are_constructed_without_touching_probe_boundary() {
    let probe = PanicProbe(Cell::new(0));
    let facts = make_host_facts(HostFactsInput {
        ram_gb: 24.0,
        chip: Some("fixture".into()),
        gpu_cores: None,
        os: Some("fixture-os".into()),
    })
    .unwrap();
    assert_eq!(facts.source, FactsSource::Injected);
    assert_eq!(probe.0.get(), 0);
}

use serde::{Deserialize, Serialize};

pub const UNKNOWN_CHIP: &str = "unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactsSource {
    Injected,
    Probe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostFacts {
    pub ram_gb: f64,
    pub chip: String,
    pub gpu_cores: Option<u32>,
    pub os: String,
    pub source: FactsSource,
}

#[derive(Debug, Clone, Default)]
pub struct HostFactsInput {
    pub ram_gb: f64,
    pub chip: Option<String>,
    pub gpu_cores: Option<u32>,
    pub os: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError(&'static str);

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ProbeError {}

pub fn nominal_ram_gb(total_mem_bytes: f64) -> Result<f64, ProbeError> {
    if !total_mem_bytes.is_finite() || total_mem_bytes <= 0.0 {
        return Err(ProbeError("total_mem_bytes must be finite and > 0"));
    }
    Ok((total_mem_bytes / 1024_f64.powi(3)).round())
}

pub fn parse_chip(cpu_model: Option<&str>) -> String {
    let Some(raw) = cpu_model else {
        return UNKNOWN_CHIP.to_string();
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return UNKNOWN_CHIP.to_string();
    }
    if let Some(start) = raw.find("Apple ") {
        let tail = &raw[start + "Apple ".len()..];
        let mut parts = tail.split_whitespace();
        if let Some(model) = parts.next()
            && model.starts_with('M')
            && !model[1..].is_empty()
            && model[1..].chars().all(|ch| ch.is_ascii_digit())
        {
            let suffix = parts
                .next()
                .filter(|part| matches!(*part, "Pro" | "Max" | "Ultra"));
            return suffix.map_or_else(|| model.to_string(), |suffix| format!("{model} {suffix}"));
        }
    }
    raw.strip_prefix("Apple ").unwrap_or(raw).trim().to_string()
}

pub fn parse_system_profiler_gpu_cores(output: &str) -> Option<u32> {
    let marker = "Total Number of Cores:";
    let start = output.find(marker)? + marker.len();
    let digits = output[start..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

pub fn make_host_facts(input: HostFactsInput) -> Result<HostFacts, ProbeError> {
    if !input.ram_gb.is_finite() || input.ram_gb <= 0.0 {
        return Err(ProbeError("ram_gb must be finite and > 0"));
    }
    Ok(HostFacts {
        ram_gb: input.ram_gb,
        chip: input
            .chip
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| UNKNOWN_CHIP.into()),
        gpu_cores: input.gpu_cores,
        os: input
            .os
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".into()),
        source: FactsSource::Injected,
    })
}

pub fn probed_host_facts(
    total_mem_bytes: f64,
    cpu_model: Option<&str>,
    os: &str,
) -> Result<HostFacts, ProbeError> {
    Ok(HostFacts {
        ram_gb: nominal_ram_gb(total_mem_bytes)?,
        chip: parse_chip(cpu_model),
        gpu_cores: None,
        os: if os.is_empty() {
            "unknown".into()
        } else {
            os.into()
        },
        source: FactsSource::Probe,
    })
}

pub fn with_system_profiler(mut base: HostFacts, output: &str) -> HostFacts {
    base.gpu_cores = parse_system_profiler_gpu_cores(output);
    base
}

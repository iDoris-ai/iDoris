use super::{HostFacts, ProbeError, probed_host_facts};

/// Raw host sampling boundary. Recommendation code consumes `HostFacts`, not
/// this trait; platform-specific collectors live at the application edge.
pub trait HostProbe {
    fn total_mem_bytes(&self) -> Result<f64, ProbeError>;
    fn cpu_model(&self) -> Result<Option<String>, ProbeError>;
    fn platform(&self) -> Result<String, ProbeError>;
}

pub fn inspect_host(probe: &impl HostProbe) -> Result<HostFacts, ProbeError> {
    let total_mem_bytes = probe.total_mem_bytes()?;
    let cpu_model = probe.cpu_model()?;
    let platform = probe.platform()?;
    probed_host_facts(total_mem_bytes, cpu_model.as_deref(), &platform)
}

use idoris_recommender::probe::{HostFacts, probed_host_facts};

pub fn current_host_facts() -> Result<HostFacts, String> {
    let (total_mem_bytes, cpu_model, platform) = platform_sample()?;
    probed_host_facts(total_mem_bytes, cpu_model.as_deref(), platform)
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn platform_sample() -> Result<(f64, Option<String>, &'static str), String> {
    let memory = command_text("sysctl", &["-n", "hw.memsize"])?;
    let total_mem_bytes = memory
        .trim()
        .parse::<f64>()
        .map_err(|_| "sysctl hw.memsize returned an invalid number".to_string())?;
    let cpu = command_text("sysctl", &["-n", "machdep.cpu.brand_string"])?;
    Ok((total_mem_bytes, Some(cpu.trim().to_string()), "darwin"))
}

#[cfg(target_os = "linux")]
fn platform_sample() -> Result<(f64, Option<String>, &'static str), String> {
    let meminfo =
        std::fs::read_to_string("/proc/meminfo").map_err(|_| "cannot read /proc/meminfo")?;
    let mem_kib = meminfo
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemTotal:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<f64>().ok())
        })
        .ok_or_else(|| "MemTotal is unavailable".to_string())?;
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| {
                        matches!(key.trim(), "model name" | "Hardware" | "Processor")
                    })
                    .map(|(_, value)| value.trim().to_string())
            })
        });
    Ok((mem_kib * 1024.0, cpu, "linux"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_sample() -> Result<(f64, Option<String>, &'static str), String> {
    Err("host capacity probing is not implemented on this platform".into())
}

#[cfg(target_os = "macos")]
fn command_text(program: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|_| format!("cannot execute {program}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited unsuccessfully"));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{program} returned non-UTF8 output"))
}

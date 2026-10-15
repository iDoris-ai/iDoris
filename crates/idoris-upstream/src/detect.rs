//! Host-fact backend recommendation.
//!
//! `nvidia-smi` is the only GPU probe. A missing command, a timeout, or a
//! non-zero exit means there is no NVIDIA GPU. AMD and Intel are not probed.
//! The recommendation is not availability. Factory support and a resolved
//! install are separate checks, and a generated card does not skip them.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use idoris_backend::RuntimeAdapter;
use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
use idoris_contracts::{ComponentCard, Contract};

use crate::local_http_runtime::LocalHttpRuntimeConfig;
use crate::local_runtime::LocalRuntimeKind;

/// Release component card port. `OmlxAdapterConfig::default` still uses 8088.
pub const OMLX_CARD_ENDPOINT: &str = "http://127.0.0.1:8000";
pub const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);
pub const INSTALL_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

const NVIDIA_SMI_ARGS: &[&str] = &["--query-gpu=name,memory.total", "--format=csv,noheader"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    Darwin,
    Win32,
    Linux,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostArch {
    Arm64,
    X64,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFacts {
    pub platform: HostPlatform,
    pub arch: HostArch,
    pub has_nvidia_gpu: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Omlx,
    Vllm,
    LlamaCpp,
    Ollama,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendChoice {
    pub kind: BackendKind,
    /// A real adapter or process runtime exists for this slot.
    /// This is not availability, and it is not [`factory_supports`].
    pub implemented: bool,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvidiaQuery {
    Missing,
    TimedOut,
    Failed,
    Empty,
    Present,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelQuery {
    NotQueried,
    Unreachable,
    Empty,
    Listed(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCandidate {
    pub provider_id: String,
    pub executable: PathBuf,
    pub executable_exists: bool,
    pub model_artifact: Option<PathBuf>,
    pub model_artifact_exists: bool,
    pub model_id: String,
    pub endpoint: String,
    pub engine_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvidence {
    Absent,
    Ambiguous,
    Candidate(ProcessCandidate),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallEvidence {
    pub engine_executable: Option<PathBuf>,
    pub endpoint: Option<String>,
    pub models: ModelQuery,
    pub process: ProcessEvidence,
}

impl InstallEvidence {
    pub fn none() -> Self {
        Self {
            engine_executable: None,
            endpoint: None,
            models: ModelQuery::NotQueried,
            process: ProcessEvidence::Absent,
        }
    }
}

/// `create_adapter` accepts this provider id. A process runtime is not enough.
pub fn factory_supports(kind: BackendKind) -> bool {
    matches!(kind, BackendKind::Omlx | BackendKind::Ollama)
}

fn runtime_implemented(kind: BackendKind) -> bool {
    match kind {
        BackendKind::Omlx | BackendKind::Ollama | BackendKind::Vllm | BackendKind::LlamaCpp => true,
    }
}

/// Injected facts make the platform matrix testable without probing hardware.
pub fn detect_backend(facts: HostFacts) -> BackendChoice {
    if facts.platform == HostPlatform::Darwin && facts.arch == HostArch::Arm64 {
        return choice(
            BackendKind::Omlx,
            "macOS Apple Silicon: oMLX is the default local backend",
        );
    }
    if facts.platform == HostPlatform::Win32 {
        return choice(
            BackendKind::Ollama,
            "Windows: Ollama is the default local backend",
        );
    }
    if facts.platform == HostPlatform::Linux && facts.has_nvidia_gpu {
        return choice(
            BackendKind::Vllm,
            "Linux with NVIDIA: vLLM process runtime is the default local backend",
        );
    }
    if facts.platform == HostPlatform::Linux {
        return choice(
            BackendKind::LlamaCpp,
            "Linux without a detected NVIDIA GPU: llama.cpp is the default local runtime",
        );
    }
    choice(
        BackendKind::LlamaCpp,
        "llama.cpp is the default local runtime for this platform",
    )
}

fn choice(kind: BackendKind, reason: &'static str) -> BackendChoice {
    BackendChoice {
        kind,
        implemented: runtime_implemented(kind),
        reason,
    }
}

pub fn current_host_facts() -> HostFacts {
    let mut facts = host_facts(std::env::consts::OS, std::env::consts::ARCH);
    facts.has_nvidia_gpu = probe_nvidia_gpu();
    facts
}

fn host_facts(os: &str, arch: &str) -> HostFacts {
    HostFacts {
        platform: match os {
            "macos" => HostPlatform::Darwin,
            "windows" => HostPlatform::Win32,
            "linux" => HostPlatform::Linux,
            _ => HostPlatform::Other,
        },
        arch: match arch {
            "aarch64" => HostArch::Arm64,
            "x86_64" => HostArch::X64,
            _ => HostArch::Other,
        },
        has_nvidia_gpu: false,
    }
}

pub fn probe_nvidia_gpu() -> bool {
    has_nvidia_gpu(run_nvidia_smi_program("nvidia-smi", NVIDIA_SMI_TIMEOUT))
}

fn has_nvidia_gpu(query: NvidiaQuery) -> bool {
    matches!(query, NvidiaQuery::Present)
}

fn run_nvidia_smi_program(program: impl AsRef<OsStr>, timeout: Duration) -> NvidiaQuery {
    let mut command = Command::new(program);
    command
        .args(NVIDIA_SMI_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return NvidiaQuery::Missing,
        Err(_) => return NvidiaQuery::Failed,
    };
    let status = match poll_child(&mut child, timeout) {
        Poll::TimedOut => return NvidiaQuery::TimedOut,
        Poll::Error => return NvidiaQuery::Failed,
        Poll::Exited(status) => status,
    };
    if !status.success() {
        return NvidiaQuery::Failed;
    }
    let Some(stdout) = read_stdout(&mut child) else {
        return NvidiaQuery::Failed;
    };
    if stdout_lists_gpu(&stdout) {
        NvidiaQuery::Present
    } else {
        NvidiaQuery::Empty
    }
}

enum Poll {
    Exited(std::process::ExitStatus),
    TimedOut,
    Error,
}

fn poll_child(child: &mut std::process::Child, timeout: Duration) -> Poll {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Poll::Exited(status),
            Ok(None) if started.elapsed() >= timeout => {
                stop_child(child);
                return Poll::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return Poll::Error,
        }
    }
}

fn stop_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(raw) = i32::try_from(child.id()) {
        let pid = nix::unistd::Pid::from_raw(raw);
        let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_stdout(child: &mut std::process::Child) -> Option<String> {
    let mut stdout = child.stdout.take()?;
    let mut text = String::new();
    stdout.read_to_string(&mut text).ok()?;
    Some(text)
}

fn stdout_lists_gpu(stdout: &str) -> bool {
    stdout.lines().any(|line| !line.trim().is_empty())
}

pub fn find_on_path(program: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return None;
    }
    let path_env = path_env?;
    for dir in std::env::split_paths(path_env) {
        if let Some(found) = runnable_in(&dir, program) {
            return Some(found);
        }
    }
    None
}

fn runnable_in(dir: &Path, program: &str) -> Option<PathBuf> {
    for name in candidate_names(program) {
        let path = dir.join(&name);
        if is_runnable(&path) {
            return Some(path);
        }
    }
    None
}

#[cfg(not(windows))]
fn candidate_names(program: &str) -> Vec<String> {
    vec![program.to_string()]
}

#[cfg(windows)]
fn candidate_names(program: &str) -> Vec<String> {
    if Path::new(program).extension().is_some() {
        return vec![program.to_string()];
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
    exts.split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!("{program}{ext}"))
        .collect()
}

fn is_runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn default_endpoint(kind: BackendKind) -> Option<&'static str> {
    match kind {
        BackendKind::Omlx => Some(OMLX_CARD_ENDPOINT),
        BackendKind::Ollama => Some(crate::ollama::DEFAULT_BASE_URL),
        BackendKind::Vllm | BackendKind::LlamaCpp => None,
    }
}

pub async fn query_installed_models(
    kind: BackendKind,
    endpoint: &str,
    timeout: Duration,
) -> ModelQuery {
    if timeout.is_zero() {
        return ModelQuery::Unreachable;
    }
    match kind {
        BackendKind::Omlx => query_omlx(endpoint, timeout).await,
        BackendKind::Ollama => query_ollama(endpoint, timeout).await,
        BackendKind::Vllm | BackendKind::LlamaCpp => ModelQuery::NotQueried,
    }
}

async fn query_omlx(endpoint: &str, timeout: Duration) -> ModelQuery {
    let config = crate::OmlxAdapterConfig {
        base_url: endpoint.to_string(),
        api_key: std::env::var(crate::omlx::OMLX_API_KEY_ENV)
            .ok()
            .filter(|value| !value.is_empty()),
        call_timeout: timeout,
    };
    let adapter = match crate::OmlxAdapter::new(config) {
        Ok(adapter) => adapter,
        Err(_) => return ModelQuery::Unreachable,
    };
    match tokio::time::timeout(timeout, adapter.list()).await {
        Ok(Ok(models)) => listed(models),
        Ok(Err(_)) | Err(_) => ModelQuery::Unreachable,
    }
}

async fn query_ollama(endpoint: &str, timeout: Duration) -> ModelQuery {
    let adapter = match crate::OllamaAdapter::new(crate::OllamaAdapterConfig {
        base_url: endpoint.to_string(),
        call_timeout: timeout,
    }) {
        Ok(adapter) => adapter,
        Err(_) => return ModelQuery::Unreachable,
    };
    match tokio::time::timeout(timeout, adapter.list()).await {
        Ok(Ok(models)) => listed(models),
        Ok(Err(_)) | Err(_) => ModelQuery::Unreachable,
    }
}

fn listed(models: Vec<idoris_backend::ModelInfo>) -> ModelQuery {
    if models.is_empty() {
        ModelQuery::Empty
    } else {
        ModelQuery::Listed(models.into_iter().map(|model| model.id).collect())
    }
}

pub fn process_evidence(
    kind: BackendKind,
    configs: &BTreeMap<String, LocalHttpRuntimeConfig>,
) -> ProcessEvidence {
    let runtime_kind = match kind {
        BackendKind::Vllm => LocalRuntimeKind::Vllm,
        BackendKind::LlamaCpp => LocalRuntimeKind::LlamaCpp,
        BackendKind::Omlx | BackendKind::Ollama => return ProcessEvidence::Absent,
    };
    let mut matches = configs
        .iter()
        .filter(|(_, config)| config.launch.kind() == runtime_kind);
    let Some((provider_id, config)) = matches.next() else {
        return ProcessEvidence::Absent;
    };
    if matches.next().is_some() {
        return ProcessEvidence::Ambiguous;
    }
    let model_artifact = config.launch.model_path().map(Path::to_path_buf);
    let model_artifact_exists = model_artifact.as_ref().is_some_and(|path| path.exists());
    ProcessEvidence::Candidate(ProcessCandidate {
        provider_id: provider_id.clone(),
        executable: config.launch.executable().to_path_buf(),
        executable_exists: config.launch.executable().is_file(),
        model_artifact,
        model_artifact_exists,
        model_id: config.model_id.clone(),
        endpoint: config.launch.endpoint(),
        engine_version: config.launch.engine_version().map(str::to_string),
    })
}

pub async fn discover_install<Q, Fut>(
    facts: HostFacts,
    configs: &BTreeMap<String, LocalHttpRuntimeConfig>,
    path_env: Option<&OsStr>,
    query: Q,
) -> InstallEvidence
where
    Q: FnOnce(BackendKind, String) -> Fut,
    Fut: std::future::Future<Output = ModelQuery>,
{
    let choice = detect_backend(facts);
    match choice.kind {
        BackendKind::Omlx | BackendKind::Ollama => {
            let (program, endpoint) = if choice.kind == BackendKind::Omlx {
                ("omlx", OMLX_CARD_ENDPOINT)
            } else {
                ("ollama", crate::ollama::DEFAULT_BASE_URL)
            };
            let engine_executable = find_on_path(program, path_env);
            let models = if engine_executable.is_some() {
                query(choice.kind, endpoint.to_string()).await
            } else {
                ModelQuery::NotQueried
            };
            InstallEvidence {
                engine_executable,
                endpoint: Some(endpoint.to_string()),
                models,
                process: ProcessEvidence::Absent,
            }
        }
        BackendKind::Vllm | BackendKind::LlamaCpp => InstallEvidence {
            engine_executable: None,
            endpoint: None,
            models: ModelQuery::NotQueried,
            process: process_evidence(choice.kind, configs),
        },
    }
}

/// Resolve the recommended backend. Missing engine, model, endpoint, or runtime
/// config returns a hint. This does not download, install, or switch backends.
pub fn recommend_startup(
    facts: HostFacts,
    evidence: &InstallEvidence,
) -> Result<ComponentCard, String> {
    let choice = detect_backend(facts);
    match choice.kind {
        BackendKind::Omlx | BackendKind::Ollama => recommend_factory(choice.kind, facts, evidence),
        BackendKind::Vllm | BackendKind::LlamaCpp => {
            recommend_process(choice.kind, facts, evidence)
        }
    }
}

fn recommend_factory(
    kind: BackendKind,
    facts: HostFacts,
    evidence: &InstallEvidence,
) -> Result<ComponentCard, String> {
    if evidence.engine_executable.is_none() {
        return Err(missing_executable_hint(kind, facts));
    }
    let Some(endpoint) = evidence.endpoint.as_deref() else {
        return Err(missing_endpoint_hint(kind));
    };
    match &evidence.models {
        ModelQuery::Listed(models) if !models.is_empty() => {
            card(kind, kind_provider(kind), endpoint, None)
        }
        ModelQuery::Listed(_) | ModelQuery::Empty => Err(empty_models_hint(kind, endpoint)),
        ModelQuery::Unreachable => Err(unreachable_hint(kind, endpoint)),
        ModelQuery::NotQueried => Err(unresolved_models_hint(kind)),
    }
}

fn recommend_process(
    kind: BackendKind,
    facts: HostFacts,
    evidence: &InstallEvidence,
) -> Result<ComponentCard, String> {
    match &evidence.process {
        ProcessEvidence::Absent => Err(missing_runtime_hint(kind, facts)),
        ProcessEvidence::Ambiguous => Err(ambiguous_runtime_hint(kind)),
        ProcessEvidence::Candidate(candidate) => {
            if !candidate.executable_exists {
                return Err(format!(
                    "默认后端是 {}，但 runtime 配置里的 executable 不存在：{}。请安装该可执行文件并更新 IDORIS_LOCAL_RUNTIME_CONFIG。不会自动安装，也不会改用其他后端。",
                    kind_label(kind),
                    candidate.executable.display()
                ));
            }
            if !candidate.model_artifact_exists {
                return Err(missing_artifact_hint(kind, candidate));
            }
            if candidate.model_id.trim().is_empty() || candidate.endpoint.trim().is_empty() {
                return Err(format!(
                    "默认后端是 {}，但 runtime 配置缺少 model id 或 endpoint。请补全 IDORIS_LOCAL_RUNTIME_CONFIG。不会改用其他后端。",
                    kind_label(kind)
                ));
            }
            card(
                kind,
                &candidate.provider_id,
                &candidate.endpoint,
                candidate.engine_version.as_deref(),
            )
        }
    }
}

fn missing_executable_hint(kind: BackendKind, facts: HostFacts) -> String {
    let program = if kind == BackendKind::Omlx {
        "omlx"
    } else {
        "ollama"
    };
    if kind == BackendKind::Ollama && facts.has_nvidia_gpu {
        return format!(
            "Windows 的默认后端是 Ollama，但 PATH 里没有可执行文件 {program}。请安装 Ollama，或设置 IDORIS_COMPONENTS_DIR。即使检测到 NVIDIA GPU 也不会改用 vLLM。不会自动安装或下载模型。"
        );
    }
    if kind == BackendKind::Ollama {
        return format!(
            "Windows 的默认后端是 Ollama，但 PATH 里没有可执行文件 {program}。请安装 Ollama，或设置 IDORIS_COMPONENTS_DIR。不会自动安装或下载模型，也不会改用其他后端。"
        );
    }
    format!(
        "推荐的默认后端是 {}，但 PATH 里没有可执行文件 {program}。请安装 {}，或设置 IDORIS_COMPONENTS_DIR。不会自动安装，也不会改用其他后端。",
        kind_label(kind),
        kind_label(kind)
    )
}

fn missing_endpoint_hint(kind: BackendKind) -> String {
    format!(
        "推荐的默认后端是 {}，但没有解析到 endpoint。请设置 IDORIS_COMPONENTS_DIR。不会改用其他后端。",
        kind_label(kind)
    )
}

fn empty_models_hint(kind: BackendKind, endpoint: &str) -> String {
    format!(
        "推荐的默认后端是 {}，端点 {endpoint} 没有已安装模型。请安装模型后再启动。不会自动下载，也不会改用其他后端。",
        kind_label(kind)
    )
}

fn unreachable_hint(kind: BackendKind, endpoint: &str) -> String {
    format!(
        "推荐的默认后端是 {}，但端点 {endpoint} 没有响应。请启动 {}。不会自动安装或下载模型，也不会改用其他后端。",
        kind_label(kind),
        kind_label(kind)
    )
}

fn unresolved_models_hint(kind: BackendKind) -> String {
    format!(
        "推荐的默认后端是 {}，但还没有解析已安装模型。不会把未核对的模型标成可用，也不会改用其他后端。",
        kind_label(kind)
    )
}

fn missing_runtime_hint(kind: BackendKind, facts: HostFacts) -> String {
    if kind == BackendKind::Vllm {
        return "Linux 检测到 NVIDIA GPU，默认后端是 vLLM。vLLM 进程 runtime 已实现，但 factory 不能单靠一张 provider 卡把它标成可用。请设置 IDORIS_LOCAL_RUNTIME_CONFIG，其中恰好有一项 kind 为 vllm，executable 存在，model_path 是已经存在的本地模型路径。不会下载模型，不会把 AMD 或 Intel 当成已探测的默认分支，也不会改用其他后端。".to_string();
    }
    if facts.platform == HostPlatform::Linux {
        return "没有检测到 NVIDIA GPU，默认后端是 llama.cpp。本轮不识别 AMD 或 Intel，所以不会把 ROCm 当成已支持的默认后端。请设置 IDORIS_LOCAL_RUNTIME_CONFIG，其中恰好有一项 kind 为 llama_cpp，executable 存在，model_path 是已经存在的 .gguf 文件。不会自动安装，也不会改用其他后端。".to_string();
    }
    "默认后端是 llama.cpp。请设置 IDORIS_LOCAL_RUNTIME_CONFIG，其中恰好有一项 kind 为 llama_cpp，executable 存在，model_path 是已经存在的 .gguf 文件。不会自动安装，也不会改用其他后端。".to_string()
}

fn ambiguous_runtime_hint(kind: BackendKind) -> String {
    format!(
        "默认后端是 {}，但 IDORIS_LOCAL_RUNTIME_CONFIG 里有多项 {}。请只保留一项。不会自动挑选，也不会改用其他后端。",
        kind_label(kind),
        runtime_token(kind)
    )
}

fn missing_artifact_hint(kind: BackendKind, candidate: &ProcessCandidate) -> String {
    if let Some(path) = &candidate.model_artifact {
        return format!(
            "默认后端是 {}，但本地模型路径不存在：{}。请把 model_path 改成已经存在的本地模型。不会下载模型，也不会改用其他后端。",
            kind_label(kind),
            path.display()
        );
    }
    format!(
        "默认后端是 {}，但 model_path 不是已经存在的本地模型。请改成本地路径。不会下载 Hugging Face 仓库，也不会改用其他后端。",
        kind_label(kind)
    )
}

fn kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Omlx => "oMLX",
        BackendKind::Ollama => "Ollama",
        BackendKind::Vllm => "vLLM",
        BackendKind::LlamaCpp => "llama.cpp",
    }
}

fn runtime_token(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Vllm => "vllm",
        BackendKind::LlamaCpp => "llama_cpp",
        BackendKind::Omlx => "omlx",
        BackendKind::Ollama => "ollama",
    }
}

fn kind_provider(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Omlx => "omlx",
        BackendKind::Ollama => "ollama",
        BackendKind::Vllm => "vllm",
        BackendKind::LlamaCpp => "llama_cpp",
    }
}

fn card(
    kind: BackendKind,
    provider_id: &str,
    endpoint: &str,
    engine_version: Option<&str>,
) -> Result<ComponentCard, String> {
    let card = ComponentCard {
        provider: ProviderDescriptor {
            id: provider_id.to_string(),
            family: Family::Local,
            tier: Tier::Local,
            capabilities: vec![Capability::Chat],
            privacy_class: PrivacyClass::LocalOnly,
            cost: Cost {
                input_per_m: 0.0,
                output_per_m: 0.0,
            },
            locality: Locality::Loopback,
            extensions: None,
        },
        form: Form::HttpService,
        endpoint: endpoint.to_string(),
        version_pin: version_pin(kind, engine_version),
        privacy_class: PrivacyClass::LocalOnly,
        allowed_egress: vec![Egress::Loopback],
        fallback_policy: FallbackPolicy::FailClosed,
        fail_closed: true,
        load_policy: Some(LoadPolicy {
            mode: LoadMode::OnDemand,
            keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
            admission: Admission::RequiresEviction,
        }),
        auth: None,
        extensions: None,
    };
    card.validate()
        .map_err(|error| format!("生成的默认组件卡未通过校验：{error}"))?;
    Ok(card)
}

fn version_pin(kind: BackendKind, engine_version: Option<&str>) -> String {
    match kind {
        BackendKind::Omlx => "omlx".to_string(),
        BackendKind::Ollama => "ollama".to_string(),
        BackendKind::LlamaCpp => "llama.cpp".to_string(),
        BackendKind::Vllm => match engine_version {
            Some(version) if !version.is_empty() => format!("vllm@{version}"),
            _ => "vllm".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::factory::create_adapter;
    use BackendKind::{LlamaCpp, Ollama, Omlx, Vllm};
    use HostArch::{Arm64, Other as OtherArch, X64};
    use HostPlatform::{Darwin, Linux, Other, Win32};

    fn make_runnable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    fn facts(platform: HostPlatform, arch: HostArch, has_nvidia_gpu: bool) -> HostFacts {
        HostFacts {
            platform,
            arch,
            has_nvidia_gpu,
        }
    }

    #[test]
    fn platform_matrix_picks_one_default_and_does_not_call_amd_a_vllm_default() {
        let rows = [
            (
                facts(Darwin, Arm64, false),
                Omlx,
                true,
                "macOS Apple Silicon: oMLX is the default local backend",
            ),
            (
                facts(Darwin, Arm64, true),
                Omlx,
                true,
                "macOS Apple Silicon: oMLX is the default local backend",
            ),
            (
                facts(Win32, X64, false),
                Ollama,
                true,
                "Windows: Ollama is the default local backend",
            ),
            (
                facts(Win32, Arm64, true),
                Ollama,
                true,
                "Windows: Ollama is the default local backend",
            ),
            (
                facts(Linux, X64, true),
                Vllm,
                false,
                "Linux with NVIDIA: vLLM process runtime is the default local backend",
            ),
            (
                facts(Linux, Arm64, false),
                LlamaCpp,
                false,
                "Linux without a detected NVIDIA GPU: llama.cpp is the default local runtime",
            ),
            (
                facts(Darwin, X64, true),
                LlamaCpp,
                false,
                "llama.cpp is the default local runtime for this platform",
            ),
            (
                facts(Other, OtherArch, false),
                LlamaCpp,
                false,
                "llama.cpp is the default local runtime for this platform",
            ),
        ];
        for (facts, kind, factory, reason) in rows {
            let choice = detect_backend(facts);
            assert_eq!(choice.kind, kind, "{facts:?}");
            assert!(choice.implemented, "{facts:?}");
            assert_eq!(factory_supports(choice.kind), factory, "{facts:?}");
            assert_eq!(choice.reason, reason, "{facts:?}");
            let lower = choice.reason.to_ascii_lowercase();
            assert!(!lower.contains("rocm"), "{facts:?}");
            assert!(!lower.contains("cuda"), "{facts:?}");
        }
    }

    #[test]
    fn linux_without_gpu_selects_implemented_llama_cpp() {
        let choice = detect_backend(facts(Linux, Arm64, false));
        assert_eq!(choice.kind, LlamaCpp);
        assert!(choice.implemented);
        assert!(!factory_supports(choice.kind));
    }

    #[test]
    fn rust_target_names_map_to_platform_facts_without_probing() {
        for (os, arch, platform, expected_arch) in [
            ("macos", "aarch64", Darwin, Arm64),
            ("macos", "x86_64", Darwin, X64),
            ("windows", "x86_64", Win32, X64),
            ("linux", "aarch64", Linux, Arm64),
            ("linux", "x86_64", Linux, X64),
            ("freebsd", "riscv64", Other, OtherArch),
            ("linux", "x86", Linux, OtherArch),
            ("unknown", "aarch64", Other, Arm64),
        ] {
            assert_eq!(
                host_facts(os, arch),
                HostFacts {
                    platform,
                    arch: expected_arch,
                    has_nvidia_gpu: false
                },
                "{os}/{arch}"
            );
        }
    }

    #[test]
    fn current_host_platform_comes_from_the_compile_target() {
        let facts = current_host_facts();
        let mapped = host_facts(std::env::consts::OS, std::env::consts::ARCH);
        assert_eq!(facts.platform, mapped.platform);
        assert_eq!(facts.arch, mapped.arch);
        assert_eq!(facts.has_nvidia_gpu, probe_nvidia_gpu());
    }

    #[test]
    fn only_a_present_nvidia_query_counts_as_a_gpu() {
        assert!(!has_nvidia_gpu(NvidiaQuery::Missing));
        assert!(!has_nvidia_gpu(NvidiaQuery::TimedOut));
        assert!(!has_nvidia_gpu(NvidiaQuery::Failed));
        assert!(!has_nvidia_gpu(NvidiaQuery::Empty));
        assert!(has_nvidia_gpu(NvidiaQuery::Present));
    }

    #[test]
    fn missing_nvidia_smi_program_is_not_a_gpu() {
        let missing = std::env::temp_dir().join("idoris-nvidia-smi-does-not-exist");
        assert_eq!(
            run_nvidia_smi_program(&missing, Duration::from_secs(1)),
            NvidiaQuery::Missing
        );
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        make_runnable(&path);
        path
    }

    #[cfg(unix)]
    #[test]
    fn nvidia_smi_timeout_nonzero_and_empty_output_are_not_a_gpu() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args");
        let recorded = write_script(
            dir.path(),
            "record",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" \"$2\" > {}\nexit 0\n",
                args.display()
            ),
        );
        assert_eq!(
            run_nvidia_smi_program(&recorded, Duration::from_secs(1)),
            NvidiaQuery::Empty
        );
        assert_eq!(
            fs::read_to_string(&args).unwrap(),
            "--query-gpu=name,memory.total\n--format=csv,noheader\n"
        );

        let present = write_script(
            dir.path(),
            "present",
            "#!/bin/sh\nprintf '%s\\n' 'Fake GPU, 1024 MiB'\n",
        );
        assert_eq!(
            run_nvidia_smi_program(&present, Duration::from_secs(1)),
            NvidiaQuery::Present
        );

        let failed = write_script(
            dir.path(),
            "failed",
            "#!/bin/sh\nprintf '%s\\n' 'Fake GPU, 1024 MiB'\nexit 3\n",
        );
        assert_eq!(
            run_nvidia_smi_program(&failed, Duration::from_secs(1)),
            NvidiaQuery::Failed
        );

        let pidfile = dir.path().join("pid");
        let mut body = String::from("#!/bin/sh\nprintf '%s\\n' \"$$\" > '");
        body.push_str(&pidfile.display().to_string());
        body.push_str("'\nsleep 30\n");
        let sleeper = write_script(dir.path(), "sleeper", &body);
        assert_eq!(
            run_nvidia_smi_program(&sleeper, Duration::from_millis(500)),
            NvidiaQuery::TimedOut
        );
        let pid =
            fs::read_to_string(&pidfile).unwrap_or_else(|error| panic!("{error}; script {body}"));
        let kill = Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap();
        assert!(
            !kill.success(),
            "timed out nvidia-smi stand-in is still running"
        );
    }

    #[test]
    fn path_lookup_requires_a_runnable_file_and_ignores_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("omlx");
        fs::write(&binary, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        assert_eq!(find_on_path("omlx", Some(dir.path().as_os_str())), None);
        make_runnable(&binary);
        assert_eq!(
            find_on_path("omlx", Some(dir.path().as_os_str())),
            Some(binary)
        );
        assert_eq!(find_on_path("ollama", Some(dir.path().as_os_str())), None);
        assert_eq!(find_on_path("omlx", None), None);
    }

    fn listed(ids: &[&str]) -> ModelQuery {
        ModelQuery::Listed(ids.iter().map(|id| (*id).to_string()).collect())
    }

    #[test]
    fn unresolved_install_keeps_the_recommended_backend_and_states_the_gap() {
        let rows = [
            (
                facts(Darwin, Arm64, false),
                InstallEvidence::none(),
                "推荐的默认后端是 oMLX，但 PATH 里没有可执行文件 omlx。请安装 oMLX，或设置 IDORIS_COMPONENTS_DIR。不会自动安装，也不会改用其他后端。",
            ),
            (
                facts(Win32, X64, true),
                InstallEvidence {
                    models: listed(&["qwen3:8b"]),
                    ..InstallEvidence::none()
                },
                "Windows 的默认后端是 Ollama，但 PATH 里没有可执行文件 ollama。请安装 Ollama，或设置 IDORIS_COMPONENTS_DIR。即使检测到 NVIDIA GPU 也不会改用 vLLM。不会自动安装或下载模型。",
            ),
            (
                facts(Win32, X64, false),
                InstallEvidence::none(),
                "Windows 的默认后端是 Ollama，但 PATH 里没有可执行文件 ollama。请安装 Ollama，或设置 IDORIS_COMPONENTS_DIR。不会自动安装或下载模型，也不会改用其他后端。",
            ),
            (
                facts(Linux, X64, true),
                InstallEvidence::none(),
                "Linux 检测到 NVIDIA GPU，默认后端是 vLLM。vLLM 进程 runtime 已实现，但 factory 不能单靠一张 provider 卡把它标成可用。请设置 IDORIS_LOCAL_RUNTIME_CONFIG，其中恰好有一项 kind 为 vllm，executable 存在，model_path 是已经存在的本地模型路径。不会下载模型，不会把 AMD 或 Intel 当成已探测的默认分支，也不会改用其他后端。",
            ),
            (
                facts(Linux, Arm64, false),
                InstallEvidence::none(),
                "没有检测到 NVIDIA GPU，默认后端是 llama.cpp。本轮不识别 AMD 或 Intel，所以不会把 ROCm 当成已支持的默认后端。请设置 IDORIS_LOCAL_RUNTIME_CONFIG，其中恰好有一项 kind 为 llama_cpp，executable 存在，model_path 是已经存在的 .gguf 文件。不会自动安装，也不会改用其他后端。",
            ),
        ];
        for (facts, evidence, hint) in rows {
            let error = recommend_startup(facts, &evidence).unwrap_err();
            assert_eq!(error, hint, "{facts:?}");
        }
    }

    #[test]
    fn omlx_endpoint_gaps_do_not_switch_backends() {
        let host = facts(Darwin, Arm64, false);
        let executable = Some(PathBuf::from("/usr/local/bin/omlx"));
        let unreachable = InstallEvidence {
            engine_executable: executable.clone(),
            endpoint: Some(OMLX_CARD_ENDPOINT.to_string()),
            models: ModelQuery::Unreachable,
            process: ProcessEvidence::Absent,
        };
        assert_eq!(
            recommend_startup(host, &unreachable).unwrap_err(),
            "推荐的默认后端是 oMLX，但端点 http://127.0.0.1:8000 没有响应。请启动 oMLX。不会自动安装或下载模型，也不会改用其他后端。"
        );
        let empty = InstallEvidence {
            models: ModelQuery::Empty,
            ..unreachable
        };
        assert_eq!(
            recommend_startup(host, &empty).unwrap_err(),
            "推荐的默认后端是 oMLX，端点 http://127.0.0.1:8000 没有已安装模型。请安装模型后再启动。不会自动下载，也不会改用其他后端。"
        );
    }

    #[test]
    fn ready_factory_and_process_installs_build_cards_without_marking_vllm_factory_ready() {
        let omlx = recommend_startup(
            facts(Darwin, Arm64, false),
            &InstallEvidence {
                engine_executable: Some(PathBuf::from("/usr/local/bin/omlx")),
                endpoint: Some(OMLX_CARD_ENDPOINT.to_string()),
                models: listed(&["qwen3-8b"]),
                process: ProcessEvidence::Absent,
            },
        )
        .unwrap();
        assert_eq!(omlx.provider.id, "omlx");
        assert_eq!(omlx.endpoint, "http://127.0.0.1:8000");
        assert_eq!(omlx.provider.capabilities, vec![Capability::Chat]);
        assert!(omlx.fail_closed);
        omlx.validate().unwrap();
        assert!(create_adapter(&omlx).is_ok());

        let ollama = recommend_startup(
            facts(Win32, X64, true),
            &InstallEvidence {
                engine_executable: Some(PathBuf::from("/Program Files/Ollama/ollama.exe")),
                endpoint: Some("http://127.0.0.1:11434".to_string()),
                models: listed(&["qwen3:8b"]),
                process: ProcessEvidence::Candidate(ProcessCandidate {
                    provider_id: "vllm-local".to_string(),
                    executable: PathBuf::from("/usr/bin/vllm"),
                    executable_exists: true,
                    model_artifact: Some(PathBuf::from("/models/qwen")),
                    model_artifact_exists: true,
                    model_id: "served".to_string(),
                    endpoint: "http://127.0.0.1:18103".to_string(),
                    engine_version: Some("0.11.0".to_string()),
                }),
            },
        )
        .unwrap();
        assert_eq!(ollama.provider.id, "ollama");
        assert_eq!(ollama.endpoint, "http://127.0.0.1:11434");
        assert!(create_adapter(&ollama).is_ok());

        let vllm = recommend_startup(
            facts(Linux, X64, true),
            &InstallEvidence {
                process: ProcessEvidence::Candidate(ProcessCandidate {
                    provider_id: "vllm-local".to_string(),
                    executable: PathBuf::from("/opt/vllm/vllm"),
                    executable_exists: true,
                    model_artifact: Some(PathBuf::from("/models/qwen")),
                    model_artifact_exists: true,
                    model_id: "served-id".to_string(),
                    endpoint: "http://127.0.0.1:18103".to_string(),
                    engine_version: Some("0.11.0".to_string()),
                }),
                ..InstallEvidence::none()
            },
        )
        .unwrap();
        assert_eq!(vllm.provider.id, "vllm-local");
        assert_eq!(vllm.endpoint, "http://127.0.0.1:18103");
        assert_eq!(vllm.version_pin, "vllm@0.11.0");
        vllm.validate().unwrap();
        match create_adapter(&vllm) {
            Ok(_) => panic!("factory accepted a vLLM process card"),
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("no adapter for provider vllm-local")
            ),
        }
    }

    #[test]
    fn vllm_without_a_local_artifact_is_not_available_and_does_not_download() {
        let host = facts(Linux, X64, true);
        let missing = recommend_startup(
            host,
            &InstallEvidence {
                process: ProcessEvidence::Candidate(ProcessCandidate {
                    provider_id: "vllm-local".to_string(),
                    executable: PathBuf::from("/opt/vllm/vllm"),
                    executable_exists: true,
                    model_artifact: None,
                    model_artifact_exists: false,
                    model_id: "served-id".to_string(),
                    endpoint: "http://127.0.0.1:18103".to_string(),
                    engine_version: Some("0.11.0".to_string()),
                }),
                ..InstallEvidence::none()
            },
        )
        .unwrap_err();
        assert_eq!(
            missing,
            "默认后端是 vLLM，但 model_path 不是已经存在的本地模型。请改成本地路径。不会下载 Hugging Face 仓库，也不会改用其他后端。"
        );
        let ambiguous = recommend_startup(
            host,
            &InstallEvidence {
                process: ProcessEvidence::Ambiguous,
                ..InstallEvidence::none()
            },
        )
        .unwrap_err();
        assert_eq!(
            ambiguous,
            "默认后端是 vLLM，但 IDORIS_LOCAL_RUNTIME_CONFIG 里有多项 vllm。请只保留一项。不会自动挑选，也不会改用其他后端。"
        );
    }

    fn runtime_config(
        kind: LocalRuntimeKind,
        executable: PathBuf,
        model: PathBuf,
        port: u16,
    ) -> LocalHttpRuntimeConfig {
        let launch = match kind {
            LocalRuntimeKind::Vllm => crate::LocalRuntimeLaunch::vllm(crate::VllmLaunch {
                executable,
                model: model.to_str().unwrap().to_string(),
                port,
                served_model_name: "served-id".to_string(),
                engine_version: "0.11.0".to_string(),
                gpu_total_gib: 24.0,
                reserve_gib: 18.0,
                model_revision: None,
            })
            .unwrap(),
            LocalRuntimeKind::LlamaCpp | LocalRuntimeKind::MlxLmServer => {
                crate::LocalRuntimeLaunch::new(kind, executable, model, port).unwrap()
            }
        };
        LocalHttpRuntimeConfig::new(
            launch,
            "served-id",
            if kind == LocalRuntimeKind::Vllm {
                18.0
            } else {
                1.0
            },
            std::env::temp_dir().join("idoris-detect-fence.pending"),
        )
        .unwrap()
    }

    #[test]
    fn process_evidence_requires_one_existing_local_artifact_for_the_recommended_kind() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("vllm");
        let model = dir.path().join("weights");
        fs::write(&executable, "bin").unwrap();
        fs::create_dir(&model).unwrap();
        let mut configs = BTreeMap::new();
        configs.insert(
            "vllm-local".to_string(),
            runtime_config(
                LocalRuntimeKind::Vllm,
                executable.clone(),
                model.clone(),
                18103,
            ),
        );
        configs.insert(
            "llama-long".to_string(),
            runtime_config(
                LocalRuntimeKind::LlamaCpp,
                dir.path().join("llama-server"),
                dir.path().join("model.gguf"),
                18102,
            ),
        );
        let evidence = process_evidence(Vllm, &configs);
        let ProcessEvidence::Candidate(candidate) = evidence else {
            panic!("expected one vllm candidate, got {evidence:?}");
        };
        assert_eq!(candidate.provider_id, "vllm-local");
        assert!(candidate.executable_exists);
        assert_eq!(candidate.model_artifact.as_deref(), Some(model.as_path()));
        assert!(candidate.model_artifact_exists);
        assert_eq!(candidate.endpoint, "http://127.0.0.1:18103");
        assert_eq!(process_evidence(Ollama, &configs), ProcessEvidence::Absent);

        let repo = crate::LocalRuntimeLaunch::vllm(crate::VllmLaunch {
            executable: executable.clone(),
            model: "Qwen/Qwen2.5-7B".to_string(),
            port: 18103,
            served_model_name: "served-id".to_string(),
            engine_version: "0.11.0".to_string(),
            gpu_total_gib: 24.0,
            reserve_gib: 18.0,
            model_revision: Some("a".repeat(40)),
        })
        .unwrap();
        let mut repo_configs = BTreeMap::new();
        repo_configs.insert(
            "vllm-hf".to_string(),
            LocalHttpRuntimeConfig::new(
                repo,
                "served-id",
                18.0,
                std::env::temp_dir().join("idoris-detect-fence.pending"),
            )
            .unwrap(),
        );
        let ProcessEvidence::Candidate(repo_candidate) = process_evidence(Vllm, &repo_configs)
        else {
            panic!("expected repo candidate");
        };
        assert_eq!(repo_candidate.model_artifact, None);
        assert!(!repo_candidate.model_artifact_exists);

        configs.insert(
            "vllm-second".to_string(),
            runtime_config(LocalRuntimeKind::Vllm, executable, model, 18104),
        );
        assert_eq!(process_evidence(Vllm, &configs), ProcessEvidence::Ambiguous);
    }

    #[tokio::test]
    async fn model_queries_read_the_daemon_and_do_not_pull_or_touch_process_runtimes() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "models": [{"name": "qwen3:8b", "size": 6_u64 * 1024 * 1024 * 1024}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let listed = query_installed_models(Ollama, &server.uri(), Duration::from_secs(2)).await;
        assert_eq!(listed, ModelQuery::Listed(vec!["qwen3:8b".to_string()]));

        let omlx = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models/status"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "models": [{"id": "qwen3-8b", "estimated_size": 6_u64 * 1024 * 1024 * 1024}]
                })),
            )
            .expect(1)
            .mount(&omlx)
            .await;
        let listed = query_installed_models(Omlx, &omlx.uri(), Duration::from_secs(2)).await;
        assert_eq!(listed, ModelQuery::Listed(vec!["qwen3-8b".to_string()]));

        let untouched = wiremock::MockServer::start().await;
        for kind in [Vllm, LlamaCpp] {
            assert_eq!(
                query_installed_models(kind, &untouched.uri(), Duration::from_secs(1)).await,
                ModelQuery::NotQueried
            );
        }
        assert!(untouched.received_requests().await.unwrap().is_empty());
        let ollama_paths = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| request.url.path().to_string())
            .collect::<Vec<_>>();
        assert_eq!(ollama_paths, ["/api/tags"]);
    }

    #[tokio::test]
    async fn discover_queries_only_after_the_recommended_executable_is_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let host = facts(Darwin, Arm64, false);
        let configs = BTreeMap::new();
        let skipped =
            discover_install(host, &configs, Some(dir.path().as_os_str()), |_, _| async {
                panic!("model query without an executable")
            })
            .await;
        assert_eq!(skipped.models, ModelQuery::NotQueried);
        assert_eq!(skipped.endpoint.as_deref(), Some(OMLX_CARD_ENDPOINT));

        let binary = dir.path().join("omlx");
        fs::write(&binary, "#!/bin/sh\n").unwrap();
        make_runnable(&binary);
        let found = discover_install(
            host,
            &configs,
            Some(dir.path().as_os_str()),
            |kind, endpoint| async move {
                assert_eq!(kind, Omlx);
                assert_eq!(endpoint, OMLX_CARD_ENDPOINT);
                ModelQuery::Listed(vec!["qwen3-8b".to_string()])
            },
        )
        .await;
        assert_eq!(found.engine_executable.as_deref(), Some(binary.as_path()));
        let card = recommend_startup(host, &found).unwrap();
        assert_eq!(card.provider.id, "omlx");
        assert_eq!(card.endpoint, OMLX_CARD_ENDPOINT);
    }
}

//! Trusted host-local launch configuration for process-owned runtimes.
//!
//! Component cards remain portable provider/capability contracts. Executable
//! paths, model artifacts and local ports live here instead and are only read
//! from an explicitly supplied trusted file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use idoris_contracts::ComponentAuth;
use idoris_upstream::{LocalHttpRuntimeConfig, LocalRuntimeKind, LocalRuntimeLaunch, VllmLaunch};

use crate::upstream_auth::{self, EnvLookup};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeLaunchFile {
    version: u32,
    #[serde(default)]
    runtimes: Vec<RuntimeLaunchEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeLaunchEntry {
    provider_id: String,
    kind: RuntimeKind,
    executable: PathBuf,
    model_path: PathBuf,
    port: u16,
    model_id: String,
    memory_gb: f64,
    load_fence_path: PathBuf,
    #[serde(default)]
    auth: Option<ComponentAuth>,
    #[serde(default)]
    engine_version: Option<String>,
    #[serde(default)]
    model_revision: Option<String>,
    #[serde(default)]
    gpu_total_gib: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeKind {
    MlxLmServer,
    LlamaCpp,
    Vllm,
}

impl From<RuntimeKind> for LocalRuntimeKind {
    fn from(value: RuntimeKind) -> Self {
        match value {
            RuntimeKind::MlxLmServer => Self::MlxLmServer,
            RuntimeKind::LlamaCpp => Self::LlamaCpp,
            RuntimeKind::Vllm => Self::Vllm,
        }
    }
}

fn launch_from_entry(
    provider_id: &str,
    entry: &RuntimeLaunchEntry,
) -> Result<LocalRuntimeLaunch, String> {
    let vllm_only = entry.engine_version.is_some()
        || entry.model_revision.is_some()
        || entry.gpu_total_gib.is_some();
    match entry.kind {
        RuntimeKind::MlxLmServer | RuntimeKind::LlamaCpp => {
            if vllm_only {
                return Err(format!(
                    "本地 runtime {provider_id} 的 engine_version、model_revision、gpu_total_gib 只适用于 kind: vllm"
                ));
            }
            LocalRuntimeLaunch::new(
                entry.kind.into(),
                entry.executable.clone(),
                entry.model_path.clone(),
                entry.port,
            )
            .map_err(|error| error.to_string())
        }
        RuntimeKind::Vllm => {
            let engine_version = entry.engine_version.clone().ok_or_else(|| {
                format!("本地 runtime {provider_id} 必须固定 engine_version（x.y.z）")
            })?;
            let gpu_total_gib = entry.gpu_total_gib.ok_or_else(|| {
                format!(
                    "本地 runtime {provider_id} 必须给出 gpu_total_gib。memory_gb 是实际 GPU 预留，会作为 --gpu-memory-utilization 传给 vLLM，不能只做账"
                )
            })?;
            let model = entry
                .model_path
                .to_str()
                .ok_or_else(|| format!("本地 runtime {provider_id} 的 model_path 必须是 UTF-8"))?;
            LocalRuntimeLaunch::vllm(VllmLaunch {
                executable: entry.executable.clone(),
                model: model.to_string(),
                port: entry.port,
                served_model_name: entry.model_id.clone(),
                engine_version,
                gpu_total_gib,
                reserve_gib: entry.memory_gb,
                model_revision: entry.model_revision.clone(),
            })
            .map_err(|error| error.to_string())
        }
    }
}

pub fn load(path: Option<&Path>) -> Result<BTreeMap<String, LocalHttpRuntimeConfig>, String> {
    load_with(path, &upstream_auth::read_process_env)
}

pub(crate) fn load_with(
    path: Option<&Path>,
    lookup: &impl Fn(&str) -> EnvLookup,
) -> Result<BTreeMap<String, LocalHttpRuntimeConfig>, String> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("无法读取本地 runtime 配置 \"{}\"：{error}", path.display()))?;
    let file: RuntimeLaunchFile = serde_yaml::from_str(&raw)
        .map_err(|error| format!("本地 runtime 配置 \"{}\" 无效：{error}", path.display()))?;
    if file.version != 1 {
        return Err(format!(
            "不支持的本地 runtime 配置版本 {}，当前仅支持 version: 1",
            file.version
        ));
    }

    let mut configs = BTreeMap::new();
    for entry in file.runtimes {
        let provider_id = entry.provider_id.trim();
        if provider_id.is_empty() {
            return Err("本地 runtime provider_id 不能为空".to_string());
        }
        let launch = launch_from_entry(provider_id, &entry)
            .map_err(|error| format!("本地 runtime {provider_id:?} 启动配置无效：{error}"))?;
        let mut config = LocalHttpRuntimeConfig::new(
            launch,
            entry.model_id,
            entry.memory_gb,
            entry.load_fence_path,
        )
        .map_err(|error| format!("本地 runtime {provider_id:?} 配置无效：{error}"))?;
        if let Some(auth) = entry.auth {
            config.bearer = Some(upstream_auth::resolve_bearer(&auth, lookup).map_err(
                |error| format!("本地 runtime {provider_id} 的 auth 无法生效：{error}"),
            )?);
        }
        if configs.insert(provider_id.to_string(), config).is_some() {
            return Err(format!("本地 runtime provider_id 重复：{provider_id}"));
        }
    }
    Ok(configs)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn write(raw: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, raw.as_bytes()).unwrap();
        file
    }

    #[test]
    fn absent_file_means_no_extra_local_runtimes() {
        assert!(load(None).unwrap().is_empty());
    }

    #[test]
    fn parses_mlx_and_llama_entries_into_validated_configs() {
        let file = write(
            r#"version: 1
runtimes:
  - provider_id: mlx-fallback
    kind: mlx_lm_server
    executable: /usr/bin/python3
    model_path: /models/qwen-mlx
    port: 18101
    model_id: qwen-mlx
    memory_gb: 18.5
    load_fence_path: /tmp/idoris/mlx-fallback.pending
  - provider_id: llama-long
    kind: llama_cpp
    executable: /opt/llama/llama-server
    model_path: /models/qwen.gguf
    port: 18102
    model_id: qwen-gguf
    memory_gb: 24.0
    load_fence_path: /tmp/idoris/llama-long.pending
"#,
        );
        let configs = load(Some(file.path())).unwrap();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs["mlx-fallback"].launch.port(), 18101);
        assert_eq!(
            configs["llama-long"].launch.kind(),
            LocalRuntimeKind::LlamaCpp
        );
    }

    #[test]
    fn rejects_unknown_version_duplicate_provider_and_invalid_llama_artifact() {
        for raw in [
            "version: 2\nruntimes: []\n",
            r#"version: 1
runtimes:
  - {provider_id: x, kind: mlx_lm_server, executable: /bin/x, model_path: /m/a, port: 1, model_id: a, memory_gb: 1, load_fence_path: /tmp/a}
  - {provider_id: x, kind: mlx_lm_server, executable: /bin/x, model_path: /m/b, port: 2, model_id: b, memory_gb: 1, load_fence_path: /tmp/b}
"#,
            r#"version: 1
runtimes:
  - {provider_id: x, kind: llama_cpp, executable: /bin/llama, model_path: /m/not-gguf.bin, port: 1, model_id: a, memory_gb: 1, load_fence_path: /tmp/a}
"#,
        ] {
            let file = write(raw);
            assert!(load(Some(file.path())).is_err());
        }
    }

    #[test]
    fn vllm_kind_accepts_a_local_artifact_and_rejects_an_unpinned_repo() {
        let local = write(
            r#"version: 1
runtimes:
  - provider_id: vllm-local
    kind: vllm
    executable: /opt/vllm/vllm
    model_path: /models/qwen
    engine_version: 0.11.0
    gpu_total_gib: 24
    port: 18103
    model_id: served-id
    memory_gb: 18
    load_fence_path: /tmp/idoris/vllm-local.pending
"#,
        );
        let configs = load(Some(local.path())).unwrap();
        let launch = &configs["vllm-local"].launch;
        assert_eq!(launch.kind(), LocalRuntimeKind::Vllm);
        assert_eq!(launch.engine_version(), Some("0.11.0"));
        assert_eq!(
            launch
                .args()
                .into_iter()
                .map(|arg| arg.into_string().unwrap())
                .collect::<Vec<_>>(),
            [
                "serve",
                "/models/qwen",
                "--host",
                "127.0.0.1",
                "--port",
                "18103",
                "--served-model-name",
                "served-id",
                "--gpu-memory-utilization",
                "0.75",
            ]
        );

        let repo = write(
            r#"version: 1
runtimes:
  - provider_id: vllm-hf
    kind: vllm
    executable: /opt/vllm/vllm
    model_path: Qwen/Qwen2.5-7B
    engine_version: 0.11.0
    gpu_total_gib: 24
    port: 18103
    model_id: served-id
    memory_gb: 18
    load_fence_path: /tmp/idoris/vllm-hf.pending
"#,
        );
        let missing_revision = load(Some(repo.path())).unwrap_err();
        assert!(missing_revision.contains("not an offline launch"));
        assert!(missing_revision.contains("revision"));

        let pinned = write(
            r#"version: 1
runtimes:
  - provider_id: vllm-hf
    kind: vllm
    executable: /opt/vllm/vllm
    model_path: Qwen/Qwen2.5-7B
    model_revision: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    engine_version: 0.11.0
    gpu_total_gib: 24
    port: 18103
    model_id: served-id
    memory_gb: 18
    load_fence_path: /tmp/idoris/vllm-hf.pending
"#,
        );
        let pinned_args = load(Some(pinned.path())).unwrap()["vllm-hf"]
            .launch
            .args()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect::<Vec<_>>();
        assert!(
            pinned_args
                .windows(2)
                .any(|pair| { pair == ["--revision", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"] })
        );

        for raw in [
            r#"version: 1
runtimes:
  - {provider_id: vllm-local, kind: vllm, executable: /opt/vllm/vllm, model_path: /models/qwen, port: 18103, model_id: served-id, memory_gb: 18, load_fence_path: /tmp/idoris/vllm-local.pending, gpu_total_gib: 24}
"#,
            r#"version: 1
runtimes:
  - {provider_id: mlx-fallback, kind: mlx_lm_server, executable: /usr/bin/python3, model_path: /models/qwen-mlx, port: 18101, model_id: qwen-mlx, memory_gb: 18.5, load_fence_path: /tmp/idoris/mlx-fallback.pending, engine_version: 0.11.0}
"#,
            r#"version: 1
runtimes:
  - {provider_id: vllm-local, kind: vllm, executable: /opt/vllm/vllm, model_path: /models/qwen, engine_version: 0.11.0, gpu_total_gib: 16, port: 18103, model_id: served-id, memory_gb: 18, load_fence_path: /tmp/idoris/vllm-local.pending}
"#,
        ] {
            let file = write(raw);
            assert!(load(Some(file.path())).is_err(), "{raw}");
        }
    }

    #[test]
    fn shipped_vllm_example_is_a_local_artifact_pin() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let example = root.join("config/examples/multibackend/vllm-local-runtime.yaml");
        let configs = load(Some(&example)).unwrap();
        let launch = &configs["vllm-local"].launch;
        assert_eq!(launch.kind(), LocalRuntimeKind::Vllm);
        let args = launch
            .args()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect::<Vec<_>>();
        assert!(!args.iter().any(|arg| arg == "--revision"));
        assert!(args.iter().any(|arg| arg == "--gpu-memory-utilization"));
        assert_eq!(configs["vllm-local"].bearer, None);
    }

    #[test]
    fn shipped_multibackend_example_matches_component_endpoints() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let example = root.join("config/examples/multibackend");
        let configs = load(Some(&example.join("local-runtimes.yaml"))).unwrap();
        let cards = crate::components::load_components(&example.join("components"), false).unwrap();

        assert_eq!(configs.len(), 2);
        assert_eq!(cards.len(), 2);
        for card in cards {
            let config = &configs[&card.provider.id];
            assert_eq!(card.endpoint, config.launch.endpoint());
            assert!(config.memory_gb > 0.0);
        }
    }

    #[test]
    fn declared_auth_reads_the_named_env_and_a_missing_env_refuses_startup() {
        let present = write(
            r#"version: 1
runtimes:
  - provider_id: mlx-fallback
    kind: mlx_lm_server
    executable: /usr/bin/python3
    model_path: /models/qwen-mlx
    port: 18101
    model_id: qwen-mlx
    memory_gb: 18.5
    load_fence_path: /tmp/idoris/mlx-fallback.pending
    auth: { kind: bearer, env: IDORIS_LAUNCH_BEARER_FOR_TEST }
"#,
        );
        let missing = load_with(Some(present.path()), &|name| {
            assert_eq!(name, "IDORIS_LAUNCH_BEARER_FOR_TEST");
            crate::upstream_auth::EnvLookup::Missing
        })
        .unwrap_err();
        assert!(missing.contains("IDORIS_LAUNCH_BEARER_FOR_TEST"));
        assert!(missing.contains("未设置"));
        assert!(!missing.contains("launch-secret-value"));

        let file = write(&std::fs::read_to_string(present.path()).unwrap());
        let configs = load_with(Some(file.path()), &|name| {
            assert_eq!(name, "IDORIS_LAUNCH_BEARER_FOR_TEST");
            crate::upstream_auth::EnvLookup::Value("launch-secret-value".to_string())
        })
        .unwrap();
        assert_eq!(
            configs["mlx-fallback"]
                .bearer
                .as_ref()
                .unwrap()
                .authorization(),
            "Bearer launch-secret-value"
        );
    }
}

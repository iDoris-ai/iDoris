//! Trusted host-local launch configuration for process-owned runtimes.
//!
//! Component cards remain portable provider/capability contracts. Executable
//! paths, model artifacts and local ports live here instead and are only read
//! from an explicitly supplied trusted file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use idoris_upstream::{LocalHttpRuntimeConfig, LocalRuntimeKind, LocalRuntimeLaunch};

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
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeKind {
    MlxLmServer,
    LlamaCpp,
}

impl From<RuntimeKind> for LocalRuntimeKind {
    fn from(value: RuntimeKind) -> Self {
        match value {
            RuntimeKind::MlxLmServer => Self::MlxLmServer,
            RuntimeKind::LlamaCpp => Self::LlamaCpp,
        }
    }
}

pub fn load(path: Option<&Path>) -> Result<BTreeMap<String, LocalHttpRuntimeConfig>, String> {
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
        let launch = LocalRuntimeLaunch::new(
            entry.kind.into(),
            entry.executable,
            entry.model_path,
            entry.port,
        )
        .map_err(|error| format!("本地 runtime {provider_id:?} 启动配置无效：{error}"))?;
        let config = LocalHttpRuntimeConfig::new(
            launch,
            entry.model_id,
            entry.memory_gb,
            entry.load_fence_path,
        )
        .map_err(|error| format!("本地 runtime {provider_id:?} 配置无效：{error}"))?;
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
}

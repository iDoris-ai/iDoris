//! Choose startup component cards.
//!
//! A non-empty `IDORIS_COMPONENTS_DIR` is loaded as written, even when that
//! directory is missing. A blank or unset override uses the bundled directory
//! beside the executable when that directory exists. Detection runs only when
//! neither directory is present. A failed explicit load does not fall back.

use std::path::{Path, PathBuf};

use idoris_contracts::ComponentCard;
use idoris_upstream::LocalHttpRuntimeConfig;
use idoris_upstream::detect::{self, HostFacts, InstallEvidence};

use crate::components::{self, LoadError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentSource {
    Configured(PathBuf),
    Detected,
}

pub fn classify(explicit: Option<&str>, bundled_dir: &Path) -> ComponentSource {
    if let Some(value) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return ComponentSource::Configured(PathBuf::from(value));
    }
    if bundled_dir.is_dir() {
        return ComponentSource::Configured(bundled_dir.to_path_buf());
    }
    ComponentSource::Detected
}

pub fn load_configured(path: &Path, allow_mock: bool) -> Result<Vec<ComponentCard>, String> {
    components::load_components(path, allow_mock).map_err(|error| component_error(path, &error))
}

fn component_error(path: &Path, error: &LoadError) -> String {
    format!(
        "无法加载组件目录 \"{}\"（IDORIS_COMPONENTS_DIR）：{error}",
        path.display()
    )
}

pub async fn startup_cards<H, Q, Fut>(
    source: ComponentSource,
    allow_mock: bool,
    configs: &std::collections::BTreeMap<String, LocalHttpRuntimeConfig>,
    path_env: Option<&std::ffi::OsStr>,
    host: H,
    query: Q,
) -> Result<Vec<ComponentCard>, String>
where
    H: FnOnce() -> HostFacts,
    Q: FnOnce(detect::BackendKind, String) -> Fut,
    Fut: std::future::Future<Output = detect::ModelQuery>,
{
    match source {
        ComponentSource::Configured(path) => load_configured(&path, allow_mock),
        ComponentSource::Detected => {
            let facts = host();
            let evidence = detect::discover_install(facts, configs, path_env, query).await;
            detected_card(facts, &evidence)
        }
    }
}

fn detected_card(
    facts: HostFacts,
    evidence: &InstallEvidence,
) -> Result<Vec<ComponentCard>, String> {
    detect::recommend_startup(facts, evidence).map(|card| vec![card])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use idoris_upstream::detect::{
        BackendKind, HostArch, HostFacts, HostPlatform, ModelQuery, OMLX_CARD_ENDPOINT,
    };
    use std::fs;

    const OMLX_CARD: &str = r#"
provider:
  id: omlx
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:8000"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: on_demand, keepalive: { idle_ttl_s: 300 }, admission: requires_eviction }
"#;

    fn write_card(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("omlx.yaml"), OMLX_CARD).unwrap();
    }

    #[tokio::test]
    async fn explicit_components_dir_does_not_probe_or_fall_back() {
        let explicit = tempfile::tempdir().unwrap();
        let bundled = tempfile::tempdir().unwrap();
        write_card(explicit.path());
        fs::write(bundled.path().join("ignored.yaml"), "not: [valid").unwrap();
        let cards = startup_cards(
            classify(Some(explicit.path().to_str().unwrap()), bundled.path()),
            false,
            &std::collections::BTreeMap::new(),
            None,
            || panic!("host facts were probed"),
            |_: BackendKind, _: String| async { panic!("install was queried") },
        )
        .await
        .unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "omlx");
        assert_eq!(cards[0].version_pin, "omlx@0.6.4");

        let missing = startup_cards(
            classify(Some("missing-components"), bundled.path()),
            false,
            &std::collections::BTreeMap::new(),
            None,
            || panic!("host facts were probed for an explicit directory"),
            |_: BackendKind, _: String| async { panic!("install was queried") },
        )
        .await
        .unwrap_err();
        assert!(missing.contains("无法加载组件目录"), "{missing}");
        assert!(missing.contains("missing-components"), "{missing}");
        assert!(!missing.contains("NVIDIA"), "{missing}");
    }

    #[tokio::test]
    async fn bundled_directory_wins_when_the_override_is_absent_or_blank() {
        let bundled = tempfile::tempdir().unwrap();
        write_card(bundled.path());
        for explicit in [None, Some(""), Some("   ")] {
            let cards = startup_cards(
                classify(explicit, bundled.path()),
                false,
                &std::collections::BTreeMap::new(),
                None,
                || panic!("bundled config probed the host"),
                |_: BackendKind, _: String| async { panic!("bundled config queried an install") },
            )
            .await
            .unwrap();
            assert_eq!(cards[0].provider.id, "omlx");
        }
    }

    #[tokio::test]
    async fn absent_directories_use_injected_host_facts() {
        let missing = std::env::temp_dir().join("idoris-a5-components-not-created");
        let _ = fs::remove_dir_all(&missing);
        let source = classify(None, &missing);
        assert_eq!(source, ComponentSource::Detected);
        let cards = startup_cards(
            source,
            false,
            &std::collections::BTreeMap::new(),
            None,
            || HostFacts {
                platform: HostPlatform::Win32,
                arch: HostArch::X64,
                has_nvidia_gpu: true,
            },
            |_: BackendKind, _: String| async {
                panic!("windows recommendation has no executable")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            cards,
            "Windows 的默认后端是 Ollama，但 PATH 里没有可执行文件 ollama。请安装 Ollama，或设置 IDORIS_COMPONENTS_DIR。即使检测到 NVIDIA GPU 也不会改用 vLLM。不会自动安装或下载模型。"
        );

        let ready = startup_cards(
            ComponentSource::Detected,
            false,
            &std::collections::BTreeMap::new(),
            None,
            || HostFacts {
                platform: HostPlatform::Darwin,
                arch: HostArch::Arm64,
                has_nvidia_gpu: false,
            },
            |kind, endpoint| async move {
                assert_eq!(kind, BackendKind::Omlx);
                assert_eq!(endpoint, OMLX_CARD_ENDPOINT);
                ModelQuery::Listed(vec!["qwen3-8b".to_string()])
            },
        )
        .await
        .unwrap_err();
        assert!(ready.contains("PATH 里没有可执行文件 omlx"), "{ready}");

        let with_binary = tempfile::tempdir().unwrap();
        let binary = with_binary.path().join("omlx");
        fs::write(&binary, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&binary).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&binary, permissions).unwrap();
        }
        let cards = startup_cards(
            ComponentSource::Detected,
            false,
            &std::collections::BTreeMap::new(),
            Some(with_binary.path().as_os_str()),
            || HostFacts {
                platform: HostPlatform::Darwin,
                arch: HostArch::Arm64,
                has_nvidia_gpu: false,
            },
            |_, _| async { ModelQuery::Listed(vec!["qwen3-8b".to_string()]) },
        )
        .await
        .unwrap();
        assert_eq!(cards[0].provider.id, "omlx");
        assert_eq!(cards[0].endpoint, OMLX_CARD_ENDPOINT);
    }
}

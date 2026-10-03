//! Loads and validates the routing-policy YAML file (`IDORIS_ROUTING_POLICY`,
//! interface spec §4), mirroring `packages/router/src/serve.ts`'s
//! `DEFAULT_ROUTING_POLICY` fallback and fail-fast-on-a-bad-path behavior
//! (conformance suite: "不传时落到仓库自带的 config/routing-policy.yaml，
//! 不是未配置；指向不存在的文件时启动直接失败").
//!
//! The `idoris` binary resolves an unset/blank policy path next to its
//! executable so a release can ship with a sibling `config/` directory.
//! Explicit relative paths still resolve against the current working
//! directory. [`resolve_routing_policy_path`] remains a cwd-relative path
//! helper for library callers.
//!
//! This crate's decision pipeline ([`idoris_policy::decide`]) doesn't
//! consume [`RoutingPolicy`]'s rules yet — R2-D only wires up its
//! load-and-validate-at-startup semantics (fail-fast on a bad file), same
//! as `loadRoutingPolicy` is used for in the TS reference today.

use std::fs;
use std::path::{Path, PathBuf};

use idoris_contracts::{Contract, RoutingPolicy};

/// `serve.ts`'s `DEFAULT_ROUTING_POLICY`.
pub const DEFAULT_ROUTING_POLICY_PATH: &str = "config/routing-policy.yaml";

#[derive(Debug)]
pub enum RoutingPolicyLoadError {
    Io { file: String, message: String },
    Parse { file: String, message: String },
    Invalid { file: String, message: String },
}

impl std::fmt::Display for RoutingPolicyLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoutingPolicyLoadError::Io { file, message } => {
                write!(f, "无法读取路由策略文件 \"{file}\"：{message}")
            }
            RoutingPolicyLoadError::Parse { file, message } => {
                write!(f, "路由策略文件 \"{file}\" 不是合法 YAML：{message}")
            }
            RoutingPolicyLoadError::Invalid { file, message } => {
                write!(f, "路由策略文件 \"{file}\" 校验失败：{message}")
            }
        }
    }
}

impl std::error::Error for RoutingPolicyLoadError {}

/// `IDORIS_ROUTING_POLICY` (blank/unset → [`DEFAULT_ROUTING_POLICY_PATH`]) →
/// the path to load from. Absolute paths pass through unchanged; relative
/// ones resolve against the current working directory (see module docs for
/// why this differs from `serve.ts`'s repo-root resolution).
pub fn resolve_routing_policy_path(raw: Option<&str>) -> PathBuf {
    let value = match raw.map(str::trim) {
        None | Some("") => DEFAULT_ROUTING_POLICY_PATH,
        Some(v) => v,
    };
    PathBuf::from(value)
}

/// Reads, parses, and validates the routing policy at `path`. Any failure
/// (missing file, invalid YAML, failed [`Contract::validate`]) is meant to
/// be fail-fast at startup — this crate has no notion of "policy
/// unconfigured" once a path is resolved, unlike the R1 skeleton's old
/// 503 `policy_unconfigured` behavior for a wholly absent policy.
pub fn load_routing_policy(path: &Path) -> Result<RoutingPolicy, RoutingPolicyLoadError> {
    let file = path.display().to_string();
    let raw = fs::read_to_string(path).map_err(|e| RoutingPolicyLoadError::Io {
        file: file.clone(),
        message: e.to_string(),
    })?;
    let policy: RoutingPolicy =
        serde_yaml::from_str(&raw).map_err(|e| RoutingPolicyLoadError::Parse {
            file: file.clone(),
            message: e.to_string(),
        })?;
    policy
        .validate()
        .map_err(|e| RoutingPolicyLoadError::Invalid {
            file: file.clone(),
            message: e.to_string(),
        })?;
    Ok(policy)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn resolves_to_the_default_when_env_is_absent_or_blank() {
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(
                resolve_routing_policy_path(raw),
                PathBuf::from(DEFAULT_ROUTING_POLICY_PATH)
            );
        }
    }

    #[test]
    fn resolves_to_the_given_path_when_set() {
        assert_eq!(
            resolve_routing_policy_path(Some("custom/policy.yaml")),
            PathBuf::from("custom/policy.yaml")
        );
    }

    const VALID_POLICY: &str = r#"
routing_policy:
  version: 1
  rules:
    - if: { privacy: local_only }
      then: { tiers: [local], fail_closed: true }
  default: { tiers: [local], fail_closed: true }
"#;

    #[test]
    fn loads_a_well_formed_policy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("policy.yaml");
        fs::write(&path, VALID_POLICY).unwrap();
        let policy = load_routing_policy(&path).unwrap();
        assert_eq!(policy.routing_policy.version, 1);
    }

    #[test]
    fn fails_fast_on_a_missing_file() {
        let err = load_routing_policy(Path::new("/does/not/exist.yaml")).unwrap_err();
        assert!(matches!(err, RoutingPolicyLoadError::Io { .. }));
    }

    #[test]
    fn fails_fast_on_invalid_yaml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("broken.yaml");
        fs::write(&path, "not: [valid").unwrap();
        let err = load_routing_policy(&path).unwrap_err();
        assert!(matches!(err, RoutingPolicyLoadError::Parse { .. }));
    }

    #[test]
    fn fails_fast_on_a_structurally_invalid_policy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("invalid.yaml");
        // version must be >= 1 (Contract::validate).
        fs::write(
            &path,
            "routing_policy:\n  version: 0\n  rules: []\n  default: { tiers: [local] }\n",
        )
        .unwrap();
        let err = load_routing_policy(&path).unwrap_err();
        assert!(matches!(err, RoutingPolicyLoadError::Invalid { .. }));
    }
}

//! Loads and validates the routing-policy YAML file (`IDORIS_ROUTING_POLICY`,
//! interface spec §4), mirroring `packages/router/src/serve.ts`'s
//! `DEFAULT_ROUTING_POLICY` fallback and fail-fast-on-a-bad-path behavior
//! (conformance suite: "不传时落到仓库自带的 config/routing-policy.yaml，
//! 不是未配置；指向不存在的文件时启动直接失败").
//!
//! **Deviation from `serve.ts`**: relative paths resolve against the
//! current working directory, not a computed repo root. `serve.ts` derives
//! the repo root from its own module's location on disk specifically to
//! survive an unusual `cwd` (e.g. a LaunchAgent's default `cwd=/`) — a
//! compiled Rust binary has no equivalent "next to the source tree"
//! location to introspect (`std::env::current_exe()` gives the install
//! path, not a repo path), so resolve-against-cwd is the only option that
//! generalizes to a real deployment, at the cost of requiring the operator
//! to run this binary from (or point `IDORIS_ROUTING_POLICY`/
//! `IDORIS_COMPONENTS_DIR` as absolute paths at) the intended working
//! directory.
//!
//! [`decide`] evaluates rules as a pure function. The request pipeline
//! applies its tier restrictions before intent selection on both execution
//! paths.

use std::fs;
use std::path::{Path, PathBuf};

use idoris_contracts::TaskProfile;
use idoris_contracts::common::{Capability, PrivacyClass, Tier};
use idoris_contracts::load_policy::LoadMode;
use idoris_contracts::routing_policy::Condition;
use idoris_contracts::{Contract, RoutingPolicy};

/// Zero-based rule index, or the policy's default action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedRule {
    Rule(usize),
    Default,
}

/// Pure policy result, mirroring `packages/router/src/policy.ts`.
/// `capability`/`load` are metadata only; this evaluator performs no dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    pub tiers: Vec<Tier>,
    pub fail_closed: bool,
    pub capability: Option<Capability>,
    pub load: Option<LoadMode>,
    pub matched_rule: MatchedRule,
}

fn matches(condition: &Condition, profile: &TaskProfile) -> bool {
    let Condition {
        privacy,
        intent,
        complexity,
        capabilities,
    } = condition;
    privacy.is_none_or(|v| profile.privacy == Some(v))
        && intent
            .as_ref()
            .is_none_or(|v| profile.intent.as_ref() == Some(v))
        && complexity.is_none_or(|v| profile.complexity == Some(v))
        && capabilities.as_ref().is_none_or(|need| {
            need.iter().all(|c| {
                profile
                    .capabilities
                    .as_ref()
                    .is_some_and(|have| have.contains(c))
            })
        })
}

/// First matching rule wins; otherwise use the default. Intersect requested
/// tiers with privacy permissions without changing their order. Even an
/// untrusted remote-only action cannot relax `local_only` or fail-closed.
/// Callers normally pass a parsed profile; absent privacy also fails closed.
pub fn decide(policy: &RoutingPolicy, profile: &TaskProfile) -> RouteDecision {
    let matched = policy
        .routing_policy
        .rules
        .iter()
        .enumerate()
        .find(|(_, rule)| matches(&rule.if_, profile));
    let (matched_rule, action) = match matched {
        Some((index, rule)) => (MatchedRule::Rule(index), &rule.then),
        None => (MatchedRule::Default, &policy.routing_policy.default),
    };
    let local_only = profile.privacy.unwrap_or(PrivacyClass::LocalOnly) == PrivacyClass::LocalOnly;
    let tiers = action
        .tiers
        .as_deref()
        .unwrap_or(&[Tier::Local])
        .iter()
        .copied()
        .filter(|tier| !local_only || *tier != Tier::Remote)
        .collect();
    RouteDecision {
        tiers,
        fail_closed: local_only || action.fail_closed.unwrap_or(false),
        capability: action.capability,
        load: action.load,
        matched_rule,
    }
}

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

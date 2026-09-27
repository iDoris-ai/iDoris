//! Loads and validates `config/components/*.yaml` component cards
//! (interface spec §3.1/§3.3), mirroring `packages/router/src/registry.ts`'s
//! `loadComponents` — this module only covers the pure load+validate path.
//! Subscription-provider deployment gating (`assertSubscriptionSource`,
//! `subscriptionStartupGate`) is out of scope for R2-D and left for a
//! follow-up.
//!
//! Unlike `registry.ts`, a `provider.id: mock` card here additionally
//! requires this crate's own `dev-mock` cargo feature (which forwards to
//! `idoris-policy/dev-mock`, the actual `mock://` scheme gate) — mirroring
//! `serve.ts`'s `IDORIS_ALLOW_MOCK=1` env gate *plus* a Rust-only
//! compile-time one a TS build has no equivalent of. A mock card missing
//! either is **skipped, not a hard startup failure** — same as
//! `serve.ts`'s behavior for a missing `IDORIS_ALLOW_MOCK`, and it is
//! filtered out *before* [`idoris_policy::validate_registration`] runs, so
//! a disallowed mock card's `mock://` endpoint never has a chance to trip
//! that function's own (unrelated to this gate) scheme check.

use std::fs;
use std::path::Path;

use idoris_contracts::{ComponentCard, Contract};
use idoris_policy::{AdmissionStatus, Card, RegistrationError, validate_registration};

#[derive(Debug)]
pub enum LoadError {
    NotADirectory(String),
    Io { file: String, message: String },
    Parse { file: String, message: String },
    Invalid { file: String, message: String },
    Registration(RegistrationError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NotADirectory(dir) => write!(f, "components dir is not a directory: {dir}"),
            LoadError::Io { file, message } => {
                write!(f, "无法读取组件卡文件 \"{file}\"：{message}")
            }
            LoadError::Parse { file, message } => {
                write!(f, "组件卡文件 \"{file}\" 不是合法 YAML：{message}")
            }
            LoadError::Invalid { file, message } => {
                write!(f, "组件卡文件 \"{file}\" 校验失败：{message}")
            }
            LoadError::Registration(err) => write!(f, "组件注册校验失败：{err}"),
        }
    }
}

impl std::error::Error for LoadError {}

fn is_mock_card(card: &ComponentCard) -> bool {
    card.provider.id == "mock"
}

/// Both the `dev-mock` cargo feature (compile-time) and `IDORIS_ALLOW_MOCK=1`
/// (runtime, `allow_mock_env`) are required — matching `serve.ts`'s env gate
/// plus the Rust-only compile-time one.
fn mock_allowed(allow_mock_env: bool) -> bool {
    allow_mock_env && cfg!(feature = "dev-mock")
}

/// Wraps a validated [`ComponentCard`] in a decision-time [`Card`] purely to
/// call [`validate_registration`], which only reads `component`/`id()` —
/// never the placeholder decision-time fields below. Building real `Card`s
/// (with actual roles/admission/cost) is the request-handling layer's job
/// (R2-D task 3), not this loader's.
fn placeholder_card(component: ComponentCard) -> Card {
    Card {
        component,
        roles: Vec::new(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: Some(0),
        admission_status: AdmissionStatus::Ready,
    }
}

/// Loads every `*.yaml`/`*.yml` file directly under `dir` (sorted by
/// filename, matching `registry.ts`'s `.sort()`), parses + structurally
/// validates each [`ComponentCard`], skips any `provider.id: mock` card
/// unless allowed (compile-time `dev-mock` feature *and* `allow_mock_env`),
/// then runs [`validate_registration`] on what's
/// left (duplicate ids, loopback/endpoint consistency, contradictory relay
/// claims — see that function's own docs).
pub fn load_components(dir: &Path, allow_mock_env: bool) -> Result<Vec<ComponentCard>, LoadError> {
    if !dir.is_dir() {
        return Err(LoadError::NotADirectory(dir.display().to_string()));
    }
    let read_dir = fs::read_dir(dir).map_err(|e| LoadError::Io {
        file: dir.display().to_string(),
        message: e.to_string(),
    })?;
    let mut paths: Vec<_> = read_dir
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yaml") | Some("yml")
            )
        })
        .collect();
    paths.sort();

    let allow_mock = mock_allowed(allow_mock_env);
    let mut cards = Vec::new();
    for path in paths {
        let file = path.display().to_string();
        let raw = fs::read_to_string(&path).map_err(|e| LoadError::Io {
            file: file.clone(),
            message: e.to_string(),
        })?;
        let card: ComponentCard = serde_yaml::from_str(&raw).map_err(|e| LoadError::Parse {
            file: file.clone(),
            message: e.to_string(),
        })?;
        card.validate().map_err(|e| LoadError::Invalid {
            file: file.clone(),
            message: e.to_string(),
        })?;
        if is_mock_card(&card) && !allow_mock {
            continue;
        }
        cards.push(card);
    }

    let placeholders: Vec<Card> = cards.iter().cloned().map(placeholder_card).collect();
    validate_registration(&placeholders).map_err(LoadError::Registration)?;

    Ok(cards)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_card(dir: &TempDir, filename: &str, contents: &str) {
        fs::write(dir.path().join(filename), contents).unwrap();
    }

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
endpoint: "http://127.0.0.1:8088"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
"#;

    const MOCK_CARD: &str = r#"
provider:
  id: mock
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "mock://in-memory"
version_pin: "mock@0.1.0"
privacy_class: local_only
allowed_egress: [none]
fallback_policy: fail_closed
fail_closed: true
"#;

    #[test]
    fn loads_a_well_formed_directory() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "omlx.yaml", OMLX_CARD);
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "omlx");
    }

    #[test]
    fn rejects_a_missing_directory() {
        let err = load_components(Path::new("/does/not/exist"), false).unwrap_err();
        assert!(matches!(err, LoadError::NotADirectory(_)));
    }

    #[test]
    fn rejects_invalid_yaml() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "broken.yaml", "not: [valid");
        let err = load_components(dir.path(), false).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }));
    }

    #[test]
    fn rejects_duplicate_provider_ids() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "a.yaml", OMLX_CARD);
        write_card(&dir, "b.yaml", OMLX_CARD);
        let err = load_components(dir.path(), false).unwrap_err();
        assert!(matches!(
            err,
            LoadError::Registration(RegistrationError::DuplicateProviderId { .. })
        ));
    }

    /// Without the compile-time `dev-mock` feature, a mock card is skipped
    /// regardless of `allow_mock_env` — the directory ends up with zero
    /// registered cards, not a hard failure.
    #[cfg(not(feature = "dev-mock"))]
    #[test]
    fn mock_card_is_skipped_without_the_dev_mock_feature() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "mock.yaml", MOCK_CARD);
        let cards = load_components(dir.path(), true).unwrap();
        assert!(cards.is_empty());
    }

    /// With the compile-time `dev-mock` feature AND the env flag, the mock
    /// card registers normally.
    #[cfg(feature = "dev-mock")]
    #[test]
    fn mock_card_registers_with_the_dev_mock_feature_and_env_flag() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "mock.yaml", MOCK_CARD);
        let cards = load_components(dir.path(), true).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "mock");
    }

    /// With the feature compiled in but the env flag off, it's still
    /// skipped — both gates are required.
    #[cfg(feature = "dev-mock")]
    #[test]
    fn mock_card_still_skipped_with_feature_but_no_env_flag() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "mock.yaml", MOCK_CARD);
        let cards = load_components(dir.path(), false).unwrap();
        assert!(cards.is_empty());
    }

    #[test]
    fn mock_card_is_skipped_when_env_flag_is_off_even_with_other_cards_present() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "mock.yaml", MOCK_CARD);
        write_card(&dir, "omlx.yaml", OMLX_CARD);
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "omlx");
    }
}

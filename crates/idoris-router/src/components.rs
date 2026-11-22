//! Loads and validates `config/components/*.yaml` component cards
//! (interface spec §3.1/§3.3), mirroring `packages/router/src/registry.ts`'s
//! `loadComponents` — this module only covers the pure load+validate path.
//! Subscription cards are structurally admitted only through B3's fixed
//! card boundary; deployment/sandbox/kill-switch authorization is applied
//! separately at startup before any runtime is constructed.
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
use std::path::{Path, PathBuf};

use idoris_contracts::{ComponentCard, Contract};
use idoris_policy::{AdmissionStatus, Card, RegistrationError, validate_registration};

use crate::subscription::card::classify_subscription_card;

/// `serve.ts`'s `DEFAULT_COMPONENTS_DIR`.
pub const DEFAULT_COMPONENTS_DIR: &str = "config/components";

/// `IDORIS_COMPONENTS_DIR` (blank/unset → [`DEFAULT_COMPONENTS_DIR`]) → the
/// directory to load from. Relative paths resolve against the current
/// working directory; the `idoris` binary anchors its default to the
/// executable's directory before loading, while explicit paths keep this
/// cwd-relative behavior.
pub fn resolve_components_dir(raw: Option<&str>) -> PathBuf {
    // `trim` 只用来判断是不是整个都是空白；非空的路径原样保留（路径末尾的空格是路径的一部分，#L1）。
    match raw {
        Some(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => PathBuf::from(DEFAULT_COMPONENTS_DIR),
    }
}

#[derive(Debug)]
pub enum LoadError {
    NotADirectory(String),
    Io {
        file: String,
        message: String,
    },
    Parse {
        file: String,
        message: String,
    },
    Invalid {
        file: String,
        message: String,
    },
    Registration(RegistrationError),
    SubscriptionCard {
        id: String,
        message: String,
    },
    /// 等 `setup` 放开组件目录的锁等了 `waited_secs` 秒仍没等到（#M2）。
    DirLocked {
        dir: String,
        waited_secs: u64,
    },
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
            LoadError::SubscriptionCard { id, message } => write!(
                f,
                "subscription provider {id} failed its fixed card boundary: {message}"
            ),
            LoadError::DirLocked { dir, waited_secs } => write!(
                f,
                "组件目录 \"{dir}\" 正被 `idoris setup` 改写，等了 {waited_secs} 秒仍没有放开：为了不读到改了一半的卡，启动失败；等 setup 结束后重试"
            ),
        }
    }
}

impl std::error::Error for LoadError {}

/// 加载器读哪些文件：目录下直接的 `*.yaml` / `*.yml`。`setup upstream` 往已有目录里加卡之前，按同一个判据
/// 镜像现有的卡去校验整个目录（`setup_engine::write_added`）。
pub(crate) fn is_card_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}

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

/// serve 等 `setup` 放开组件目录的锁的上限。`setup` 的落盘只是几次改名，正常情况下几毫秒；等不到说明 setup 卡住了。
pub const DIR_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// 读到的卡的一致性保证（#M2）：先对组件目录拿**共享** `flock`（与 `setup` 的独占锁是同一把，`setup_engine::lock_out_dir`），
/// 拿着它枚举并读完所有卡再放开。`setup` 落盘时持有独占锁，所以这里读到的要么是落盘前完整的旧集合，要么是落盘后完整的
/// 新集合，不会是 `setup` 一个一个挪动文件的中间状态。拿不到锁就等，最多 `wait`，超时是启动失败（[`LoadError::DirLocked`]）。
///
/// 只在启动时读一次卡的 serve 用它；其他调用方仍用 [`load_components`]。建议锁约束不了不遵守它的写入者（手工改文件），
/// 那一类改动不在这个保证里。目录不存在或不是目录时不拿锁，原样交给 [`load_components`] 报错。
pub fn load_components_locked(
    dir: &Path,
    allow_mock_env: bool,
    wait: std::time::Duration,
) -> Result<Vec<ComponentCard>, LoadError> {
    let _shared = lock_shared(dir, wait)?;
    load_components(dir, allow_mock_env)
}

/// 共享锁的持有者；Drop 时释放。
struct SharedDirLock {
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<fs::File>,
}

#[cfg(unix)]
fn lock_shared(dir: &Path, wait: std::time::Duration) -> Result<Option<SharedDirLock>, LoadError> {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    if !dir.is_dir() {
        return Ok(None);
    }
    let io = |message: String| LoadError::Io {
        file: dir.display().to_string(),
        message,
    };
    let mut file = fs::File::open(dir).map_err(|e| io(e.to_string()))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match Flock::lock(file, FlockArg::LockSharedNonblock) {
            Ok(flock) => return Ok(Some(SharedDirLock { _flock: flock })),
            Err((back, Errno::EWOULDBLOCK)) => {
                if std::time::Instant::now() >= deadline {
                    return Err(LoadError::DirLocked {
                        dir: dir.display().to_string(),
                        waited_secs: wait.as_secs(),
                    });
                }
                file = back;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err((_, errno)) => return Err(io(format!("无法锁定组件目录：{errno}"))),
        }
    }
}

#[cfg(not(unix))]
fn lock_shared(
    _dir: &Path,
    _wait: std::time::Duration,
) -> Result<Option<SharedDirLock>, LoadError> {
    Ok(None)
}

/// Loads every `*.yaml`/`*.yml` file directly under `dir` (sorted by
/// filename, matching `registry.ts`'s `.sort()`), parses + structurally
/// validates each [`ComponentCard`], skips any `provider.id: mock` card
/// unless allowed (compile-time `dev-mock` feature *and* `allow_mock_env`),
/// validates the fixed subscription card boundary, then runs
/// [`validate_registration`] on the admitted set (duplicate ids,
/// loopback/endpoint consistency, contradictory relay claims).
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
        .filter(|path| is_card_file(path))
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
        crate::dispatch::role_models(&card).map_err(|message| LoadError::Invalid {
            file: file.clone(),
            message,
        })?;
        crate::engine_kind::check_upstream_declaration(&card).map_err(|error| {
            LoadError::Invalid {
                file: file.clone(),
                message: error.to_string(),
            }
        })?;
        classify_subscription_card(&card).map_err(|error| LoadError::SubscriptionCard {
            id: card.provider.id.clone(),
            message: error.to_string(),
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

    #[test]
    fn resolve_components_dir_defaults_when_env_is_absent_or_blank() {
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(
                resolve_components_dir(raw),
                PathBuf::from(DEFAULT_COMPONENTS_DIR)
            );
        }
    }

    /// #L1：非空的路径原样保留（末尾空格是路径的一部分），整个是空白才用默认。
    #[test]
    fn resolve_components_dir_keeps_a_non_blank_path_exactly() {
        assert_eq!(
            resolve_components_dir(Some("custom/dir ")),
            PathBuf::from("custom/dir ")
        );
        assert_eq!(
            resolve_components_dir(Some(" lead")),
            PathBuf::from(" lead")
        );
    }

    #[test]
    fn resolve_components_dir_uses_the_given_path_when_set() {
        assert_eq!(
            resolve_components_dir(Some("custom/dir")),
            PathBuf::from("custom/dir")
        );
    }

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
load_policy: { mode: on_demand, keepalive: { pinned: false }, admission: coexist }
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
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
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

    /// A `LoadMode::Resident` `http_service` card — the shape R2-G's
    /// direct-forward path serves. `cost` is templated in so the same
    /// builder covers free/paid/unknown-price fixtures below.
    fn resident_card(id: &str, cost_yaml: &str) -> String {
        format!(
            r#"
provider:
  id: {id}
  family: other
  tier: remote
  capabilities: [chat]
  privacy_class: any
  cost: {cost_yaml}
  locality: remote
form: http_service
endpoint: "http://127.0.0.1:9"
version_pin: "{id}@0.0.0"
privacy_class: any
allowed_egress: [internet]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
"#
        )
    }

    #[test]
    fn a_free_resident_http_service_card_is_admitted() {
        let dir = TempDir::new().unwrap();
        write_card(
            &dir,
            "proxy.yaml",
            &resident_card("proxy-free", "{ input_per_m: 0, output_per_m: 0 }"),
        );
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "proxy-free");
    }

    #[test]
    fn shipped_subscription_card_is_structurally_admitted() {
        let dir = TempDir::new().unwrap();
        write_card(
            &dir,
            "subscription.yaml",
            include_str!("../../../config/components/subscription.yaml"),
        );
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "subscription");
        assert_eq!(
            cards[0].form,
            idoris_contracts::component_card::Form::SpawnCli
        );
    }

    #[test]
    fn spawn_cli_impersonation_and_wrong_subscription_form_are_rejected() {
        let dir = TempDir::new().unwrap();
        let impostor = include_str!("../../../config/components/subscription.yaml")
            .replace("id: subscription", "id: impostor");
        write_card(&dir, "impostor.yaml", &impostor);
        assert!(matches!(
            load_components(dir.path(), false).unwrap_err(),
            LoadError::SubscriptionCard { .. }
        ));

        let dir = TempDir::new().unwrap();
        let wrong_form = include_str!("../../../config/components/subscription.yaml")
            .replace("form: spawn_cli", "form: http_service")
            .replace("spawn://subscription", "http://127.0.0.1:9");
        write_card(&dir, "subscription.yaml", &wrong_form);
        assert!(matches!(
            load_components(dir.path(), false).unwrap_err(),
            LoadError::SubscriptionCard { .. }
        ));
    }

    #[test]
    fn a_paid_resident_http_service_card_is_admitted_after_budget_wiring() {
        let dir = TempDir::new().unwrap();
        write_card(
            &dir,
            "proxy.yaml",
            &resident_card(
                "proxy-paid",
                "{ input_per_m: 1000000, output_per_m: 2000000 }",
            ),
        );
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "proxy-paid");
    }

    /// Loading and request-time pricing are separate boundaries: malformed
    /// pricing may be parsed here, but the policy Pricing stage must still
    /// reject it before any egress.
    #[test]
    fn a_resident_http_service_card_with_an_unknown_price_can_load_for_request_time_rejection() {
        let dir = TempDir::new().unwrap();
        write_card(
            &dir,
            "proxy.yaml",
            &resident_card(
                "proxy-unknown-cost",
                "{ input_per_m: .nan, output_per_m: 0 }",
            ),
        );
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "proxy-unknown-cost");
    }

    #[test]
    fn an_input_only_priced_resident_http_service_card_is_admitted() {
        let dir = TempDir::new().unwrap();
        write_card(
            &dir,
            "proxy.yaml",
            &resident_card("proxy-input-only", "{ input_per_m: 5, output_per_m: 0 }"),
        );
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "proxy-input-only");
    }

    /// A non-Resident card remains unaffected; the Supervisor path continues
    /// to reserve/settle through the same budget wrapper.
    #[test]
    fn a_paid_on_demand_card_is_unaffected_by_this_gate() {
        let dir = TempDir::new().unwrap();
        let card = resident_card(
            "omlx-paid",
            "{ input_per_m: 1000000, output_per_m: 2000000 }",
        )
        .replace("mode: resident", "mode: on_demand")
        .replace("pinned: true", "pinned: false");
        write_card(&dir, "omlx.yaml", &card);
        let cards = load_components(dir.path(), false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].provider.id, "omlx-paid");
    }
}

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
    /// 等 `setup` 放开组件目录的锁等了 `waited_ms` 毫秒仍没等到（#M2）。
    DirLocked {
        dir: String,
        waited_ms: u64,
    },
    /// 等锁的时候被取消了（停止信号到了，H1）：不是加载失败，没有读卡。
    Cancelled,
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
            LoadError::DirLocked { dir, waited_ms } => write!(
                f,
                "组件目录 \"{dir}\" 正被 `idoris setup` 改写，等了 {waited_ms} 毫秒仍没有放开：为了不读到改了一半的卡，启动失败；等 setup 结束后重试"
            ),
            LoadError::Cancelled => write!(f, "等组件目录的锁时收到停止信号，没有读卡"),
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

/// serve 等 `setup` 放开组件目录的锁的默认上限。`setup` 的落盘只是几次改名，正常情况下几毫秒；等不到说明 setup 卡住了。
pub const DIR_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// 等锁时间的上限（再大的值按它截断）：10 分钟。
pub const DIR_LOCK_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(600);

/// 等锁时每次重试之间的轮询间隔（剩余时间不足时取剩余时间）。
const DIR_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// `IDORIS_COMPONENTS_LOCK_WAIT_MS`（毫秒，诊断与测试用）的解析：没设或整个是空白用默认 30 秒；`0` 是只试一次、锁着就立刻失败；
/// 纯数字就按毫秒（超过 [`DIR_LOCK_WAIT_MAX`] 的值按上限截断，过大到装不进 u64 的数字也一样）；其他（负数、小数、带单位、
/// 乱写）是启动错误，说明里点名变量与取值，不悄悄退回默认。
pub fn parse_lock_wait_ms(raw: Option<&str>) -> Result<std::time::Duration, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(DIR_LOCK_WAIT);
    };
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "环境变量 IDORIS_COMPONENTS_LOCK_WAIT_MS 必须是毫秒数（非负整数），拿到 {raw:?}"
        ));
    }
    let millis: u128 = raw.parse().unwrap_or(u128::MAX);
    let capped = millis.min(DIR_LOCK_WAIT_MAX.as_millis());
    // `capped` 不超过 600_000，装得进 u64。
    Ok(std::time::Duration::from_millis(capped as u64))
}

/// 组件目录的 canonical 路径，只确定**一次**：加锁、读卡、发布给 launcher 的启动快照都用这同一个结果，
/// 不会因为中途 `current` 这类符号链接被换而指向不同的目录。不存在或不是目录是 [`LoadError::NotADirectory`]，立刻返回，
/// 不会在「先看一眼不存在、再看一眼又存在了」之间漏过一次不加锁的读取。
pub fn canonical_components_dir(dir: &Path) -> Result<PathBuf, LoadError> {
    match fs::canonicalize(dir) {
        Ok(canonical) if canonical.is_dir() => Ok(canonical),
        Ok(_) => Err(LoadError::NotADirectory(dir.display().to_string())),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound) => {
            Err(LoadError::NotADirectory(dir.display().to_string()))
        }
        Err(error) if error.raw_os_error() == Some(20) => {
            // ENOTDIR：路径中间有一个普通文件。
            Err(LoadError::NotADirectory(dir.display().to_string()))
        }
        Err(error) => Err(LoadError::Io {
            file: dir.display().to_string(),
            message: error.to_string(),
        }),
    }
}

/// 测试挂钩：`(阶段, 目录)`。
pub type LoadHook<'a> = dyn Fn(LoadPhase, &Path) + 'a;

/// 读卡进行到哪一步（只给测试挂钩用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPhase {
    /// 目录已经打开、还没加锁。
    Opened,
    /// 共享锁已经拿到、还没开始枚举卡：这个阶段里读卡一定持有共享锁。
    Locked,
}

/// 读到的卡的一致性保证（#M2）：先对组件目录拿**共享** `flock`（与 `setup` 的独占锁是同一把，`setup_engine::lock_out_dir`），
/// 拿着它枚举并读完所有卡再放开。`setup` 落盘时持有独占锁，所以这里读到的要么是落盘前完整的旧集合，要么是落盘后完整的
/// 新集合，不会是 `setup` 一个一个挪动文件的中间状态。拿不到锁就等，最多 `wait`，超时是启动失败（[`LoadError::DirLocked`]）。
///
/// 路径先 [`canonical_components_dir`] 一次，之后加锁与读卡都用打开的目录 fd（`openat`），不再按路径重新解析，所以目录本身
/// 中途被改名也读的是加锁的那一个。不存在或不是目录立刻 [`LoadError::NotADirectory`]，不拿锁也不读卡；在 Unix 上，每一条
/// 走到读卡的成功路径都持有共享锁。
///
/// 只保证**这个目录里**的卡：指向目录之外的符号链接是 [`LoadError::Invalid`]（外面那个目录的锁我们拿不到，读到的可能是别人
/// 改了一半的东西）。建议锁约束不了不遵守它的写入者（手工改文件），那一类改动不在这个保证里。
pub fn load_components_locked(
    dir: &Path,
    allow_mock_env: bool,
    wait: std::time::Duration,
) -> Result<Vec<ComponentCard>, LoadError> {
    let canonical = canonical_components_dir(dir)?;
    load_components_locked_canonical(&canonical, allow_mock_env, wait)
}

/// 同上，路径是调用方已经 [`canonical_components_dir`] 过的结果（启动时只确定一次，加锁、读卡、发布快照共用）。
pub fn load_components_locked_canonical(
    canonical: &Path,
    allow_mock_env: bool,
    wait: std::time::Duration,
) -> Result<Vec<ComponentCard>, LoadError> {
    load_components_locked_with_hook(canonical, allow_mock_env, wait, None)
}

/// 同上，带测试挂钩（生产路径传 `None`）。
pub fn load_components_locked_with_hook(
    canonical: &Path,
    allow_mock_env: bool,
    wait: std::time::Duration,
    hook: Option<&LoadHook<'_>>,
) -> Result<Vec<ComponentCard>, LoadError> {
    load_components_locked_cancellable(canonical, allow_mock_env, wait, hook, &|| false)
}

/// 同上，等锁的时候可以被取消（H1）：`cancel()` 在每次发现目录还锁着的时候看一次（每 [`DIR_LOCK_POLL`] 一次），为真就返回
/// [`LoadError::Cancelled`]，不再等到 `wait` 用完。`idoris serve` 用它：启动期间收到的 SIGTERM 不能等 `setup` 放开锁（最长 30 秒，
/// 可配到 10 分钟），而 launcher 17 秒就 SIGKILL。拿到锁之后读卡是几个文件的事，不再看 `cancel`（读到一半放弃没有意义）。
pub fn load_components_locked_cancellable(
    canonical: &Path,
    allow_mock_env: bool,
    wait: std::time::Duration,
    hook: Option<&LoadHook<'_>>,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<ComponentCard>, LoadError> {
    #[cfg(unix)]
    {
        locked_load_unix(canonical, allow_mock_env, wait, hook, cancel)
    }
    #[cfg(not(unix))]
    {
        let _ = (wait, hook, cancel);
        load_components(canonical, allow_mock_env)
    }
}

#[cfg(unix)]
fn locked_load_unix(
    canonical: &Path,
    allow_mock_env: bool,
    wait: std::time::Duration,
    hook: Option<&LoadHook<'_>>,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<ComponentCard>, LoadError> {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    use std::os::fd::AsFd;
    let label = canonical.display().to_string();
    let io = |message: String| LoadError::Io {
        file: label.clone(),
        message,
    };
    let file = fs::File::open(canonical).map_err(|e| {
        if matches!(e.kind(), std::io::ErrorKind::NotFound) {
            LoadError::NotADirectory(label.clone())
        } else {
            io(e.to_string())
        }
    })?;
    if !file.metadata().map_err(|e| io(e.to_string()))?.is_dir() {
        return Err(LoadError::NotADirectory(label));
    }
    if let Some(hook) = hook {
        hook(LoadPhase::Opened, canonical);
    }
    // 先试一次；锁着就重试，每次重试之前看剩余时间，睡 min(剩余, 轮询间隔)；等到的毫秒数是实际等的。
    let started = std::time::Instant::now();
    let mut file = file;
    let flock = loop {
        match Flock::lock(file, FlockArg::LockSharedNonblock) {
            Ok(flock) => break flock,
            Err((back, Errno::EWOULDBLOCK)) => {
                if cancel() {
                    return Err(LoadError::Cancelled);
                }
                let remaining = wait.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(LoadError::DirLocked {
                        dir: label,
                        waited_ms: started.elapsed().as_millis() as u64,
                    });
                }
                file = back;
                std::thread::sleep(remaining.min(DIR_LOCK_POLL));
            }
            Err((_, errno)) => return Err(io(format!("无法锁定组件目录：{errno}"))),
        }
    };
    if let Some(hook) = hook {
        hook(LoadPhase::Locked, canonical);
    }
    // 读卡用锁住的那个目录 fd 的副本（共用同一个打开文件描述，锁随它）。
    let dir_fd = flock
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| io(e.to_string()))?;
    let items = read_cards_via_fd(dir_fd, canonical)?;
    build_cards(items, allow_mock_env)
}

/// 枚举并读取目录 fd 下的卡文件（`*.yaml` / `*.yml`），全部相对这个 fd（`fstatat`、`openat`），按文件名排序。
/// 普通文件直接读；符号链接要解析到目录之内才读（目录之外是 [`LoadError::Invalid`]）；别的类型（目录、FIFO……）读不了。
#[cfg(unix)]
fn read_cards_via_fd(
    dir_fd: std::os::fd::OwnedFd,
    canonical: &Path,
) -> Result<Vec<(String, String)>, LoadError> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::{Mode, SFlag, fstatat};
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    let dir_label = canonical.display().to_string();
    let mut dir = nix::dir::Dir::from_fd(dir_fd).map_err(|e| LoadError::Io {
        file: dir_label.clone(),
        message: e.to_string(),
    })?;
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    for entry in dir.iter() {
        let entry = entry.map_err(|e| LoadError::Io {
            file: dir_label.clone(),
            message: e.to_string(),
        })?;
        let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes()).to_os_string();
        if is_card_file(Path::new(&name)) {
            names.push(name);
        }
    }
    names.sort();
    // `Dir` 持有 fd；`openat` 与 `fstatat` 借它的 fd 用。
    let raw = std::os::fd::AsFd::as_fd(&dir)
        .try_clone_to_owned()
        .map_err(|e| LoadError::Io {
            file: dir_label.clone(),
            message: e.to_string(),
        })?;
    let mut items = Vec::with_capacity(names.len());
    for name in names {
        let file_label = canonical.join(&name).display().to_string();
        let io = |message: String| LoadError::Io {
            file: file_label.clone(),
            message,
        };
        let stat = fstatat(
            &raw,
            name.as_os_str(),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .map_err(|e| io(e.to_string()))?;
        let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
        if kind == SFlag::S_IFLNK {
            let target = fs::canonicalize(canonical.join(&name)).map_err(|e| io(e.to_string()))?;
            if !target.starts_with(canonical) {
                return Err(LoadError::Invalid {
                    file: file_label,
                    message: format!(
                        "符号链接指向组件目录之外（{}）：只保证这个目录里的卡读得一致，请把卡放进目录里，或去掉这个链接",
                        target.display()
                    ),
                });
            }
        } else if kind != SFlag::S_IFREG {
            return Err(io("不是普通文件".to_string()));
        }
        let fd = openat(
            &raw,
            name.as_os_str(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| io(e.to_string()))?;
        let mut file = fs::File::from(fd);
        if !file.metadata().map_err(|e| io(e.to_string()))?.is_file() {
            return Err(io("不是普通文件".to_string()));
        }
        let mut raw_text = String::new();
        file.read_to_string(&mut raw_text)
            .map_err(|e| io(e.to_string()))?;
        items.push((file_label, raw_text));
    }
    Ok(items)
}

/// 路径上的卡文件各自解析、校验，再整体做注册校验。读卡的方式（按路径，或锁住的目录 fd）与这里无关。
fn build_cards(
    items: Vec<(String, String)>,
    allow_mock_env: bool,
) -> Result<Vec<ComponentCard>, LoadError> {
    let allow_mock = mock_allowed(allow_mock_env);
    let mut cards = Vec::new();
    for (file, raw) in items {
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

/// `IDORIS_ALLOW_MOCK` 的取值（只有 `1` 算开）。serve 与 `setup` 的校验共用，所以 `setup` 放行的目录与 serve 看到的卡集合
/// 在「mock 卡要不要进来」上是同一个答案。还要同时有 `dev-mock` 编译特性才算数（[`mock_allowed`]）。
pub fn allow_mock_env(value: Option<&str>) -> bool {
    value == Some("1")
}

/// 从进程环境里读 [`allow_mock_env`]。
pub fn allow_mock_from_env() -> bool {
    allow_mock_env(std::env::var("IDORIS_ALLOW_MOCK").ok().as_deref())
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

    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        let file = path.display().to_string();
        let raw = fs::read_to_string(&path).map_err(|e| LoadError::Io {
            file: file.clone(),
            message: e.to_string(),
        })?;
        items.push((file, raw));
    }
    build_cards(items, allow_mock_env)
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

    /// #7：`IDORIS_ALLOW_MOCK` 的取值只有 `1` 算开（serve 与 setup 共用 [`allow_mock_env`]）；还要同时有 `dev-mock` 编译特性才算数。
    #[test]
    fn the_mock_switch_is_shared_and_needs_both_the_env_value_and_the_feature() {
        assert!(allow_mock_env(Some("1")));
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some(" 1"),
        ] {
            assert!(!allow_mock_env(off), "{off:?}");
        }
        assert_eq!(mock_allowed(true), cfg!(feature = "dev-mock"));
        assert!(!mock_allowed(false));
    }

    /// #7：未开启（缺 env 值，或没有 `dev-mock` 特性）时 mock 卡被跳过；两个同 id 的 mock 卡不会冲突。
    #[test]
    fn mock_cards_are_skipped_unless_both_conditions_hold() {
        let dir = TempDir::new().unwrap();
        write_card(&dir, "a.yaml", MOCK_CARD);
        write_card(&dir, "b.yaml", MOCK_CARD);
        let disabled = load_components(dir.path(), false).unwrap();
        assert!(disabled.is_empty());
        let env_on = load_components(dir.path(), true);
        if cfg!(feature = "dev-mock") {
            // 两个都开：两张 mock 卡都进注册，同 id 重复就是校验失败。
            assert!(matches!(
                env_on,
                Err(LoadError::Registration(
                    RegistrationError::DuplicateProviderId { .. }
                ))
            ));
        } else {
            assert!(env_on.unwrap().is_empty());
        }
    }
}

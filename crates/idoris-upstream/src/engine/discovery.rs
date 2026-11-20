//! 引擎可执行文件的发现与校验的通用部分（设计稿 §2.7.3）。
//!
//! **去哪里找**是引擎相关的（oMLX 在 `/Applications/oMLX.app` 里，llama.cpp 在 PATH 上，……），由配方
//! 经 [`DiscoveryPlan`] 给出（纯数据）；**怎么校验**是通用的，在这里。发现顺序：
//! 1. 显式路径（launcher 帧 `engine.executables[<engine_kind>]` 给的，规范 §2.7.1，视为可信；本模块不解析帧，
//!    调用方取出这一项传进来；**不回退**：给了而它不合格就是错误，不悄悄改用别的）；
//! 2. [`DiscoveryPlan::app_executables`]（`.app` 里的位置，按顺序）；
//! 3. [`DiscoveryPlan::search_dirs`] 里名为 [`DiscoveryPlan::binary_names`] 的文件（缺省为空：**不查 PATH**。
//!    Agent24 启动 iDoris 时把 PATH 固定为 `/usr/bin:/bin:/usr/sbin:/sbin`，Homebrew 路径本来就不在里面；
//!    要在宿主上多找几个目录就显式给，这样的候选属于「用户自装的命令行版」，不做 app 级校验）。
//!
//! 每个候选都要过的校验（在 `canonicalize` 之后的真实文件上）：普通文件、有可执行位、没有 setuid/setgid、
//! 别人不可写、属主是本用户或 root。通过后返回的是 canonical 路径，后面 spawn 的就是校验过的这个文件。
//!
//! 位于 `.app` 里的自动发现候选还要：
//! - **不得逃逸**：canonical 路径必须仍在（canonical 的）`.app` 目录之内；指向 app 外面的符号链接一律拒绝；
//! - **签名**（[`CodesignPolicy::Verify`]）：`/usr/bin/codesign --verify --strict <app>`，再读 `TeamIdentifier`，
//!   必须等于配方要求的团队。签名检查可关（[`CodesignPolicy::Off`]），失败带原因
//!   （[`RejectReason::CodesignFailed`] / [`RejectReason::TeamMismatch`]），**不作用于**显式路径与
//!   `search_dirs`：不阻塞用户自装的命令行版。实测校验 oMLX.app 约 5 秒（40k 个文件做哈希），
//!   调用方要放在启动门控里，不要放在请求路径上。

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::fsutil::effective_uid;
use super::inspect::{InspectError, run_tool};

/// `codesign` 的位置。
pub const CODESIGN_TOOL: &str = "/usr/bin/codesign";

/// 候选的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutableSource {
    /// 显式给的路径。
    Explicit,
    /// `.app` 里的自动发现位置。
    AppBundle,
    /// 调用方给的额外搜索目录。
    SearchDir,
}

/// 发现并校验通过的可执行文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineExecutable {
    /// canonical 路径。
    pub path: PathBuf,
    pub source: ExecutableSource,
    /// 做过签名检查时，读到的团队 id。
    pub team_id: Option<String>,
}

/// 签名检查的参数。
#[derive(Debug, Clone)]
pub struct CodesignCheck {
    /// `codesign` 的绝对路径。
    pub tool: PathBuf,
    /// 要求的 `TeamIdentifier`。
    pub team_id: String,
    pub timeout: Duration,
}

impl CodesignCheck {
    /// `/usr/bin/codesign`，要求给定的团队，60 秒。
    pub fn new(team_id: impl Into<String>) -> Self {
        Self {
            tool: PathBuf::from(CODESIGN_TOOL),
            team_id: team_id.into(),
            timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CodesignPolicy {
    /// 不做签名检查。
    Off,
    /// 对 `.app` 里的自动发现候选做签名检查。
    Verify(CodesignCheck),
}

/// 配方给出的「去哪里找」：纯数据，由 [`discover_executable`] 执行。
/// 显式路径不在这里：它来自 launcher 帧，是 [`discover_executable`] 的另一个输入。
#[derive(Debug, Clone)]
pub struct DiscoveryPlan {
    /// `.app` 里的候选可执行文件，按顺序；每个路径里必须有一级 `*.app`。
    pub app_executables: Vec<PathBuf>,
    /// 额外的搜索目录。缺省为空：不查 PATH。
    pub search_dirs: Vec<PathBuf>,
    /// 在 `search_dirs` 的每个目录里找哪些文件名。
    pub binary_names: Vec<String>,
    pub codesign: CodesignPolicy,
}

impl Default for DiscoveryPlan {
    /// 什么都不找；不做签名检查。
    fn default() -> Self {
        Self {
            app_executables: Vec::new(),
            search_dirs: Vec::new(),
            binary_names: Vec::new(),
            codesign: CodesignPolicy::Off,
        }
    }
}

impl DiscoveryPlan {
    /// 把一个 `PATH` 形式的字符串里的（绝对）目录放进 [`Self::search_dirs`]。默认**不**这么做；
    /// 给「要在宿主 PATH 上找」的场合用，不是缺省行为。
    pub fn with_path_dirs(mut self, path: &std::ffi::OsStr) -> Self {
        self.search_dirs
            .extend(std::env::split_paths(path).filter(|dir| dir.is_absolute()));
        self
    }
}

/// 一个候选被拒绝的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// 显式路径指向的文件不存在（自动发现位置不存在不算拒绝，只是没装）。
    Missing,
    NotAbsolute,
    /// `canonicalize` 失败（符号链接成环、没有权限……）。
    Unresolvable,
    NotRegularFile,
    NotExecutable,
    SetuidBinary,
    WorldWritable,
    UntrustedOwner,
    /// 自动发现位置的路径里没有 `*.app`。
    NotInAppBundle,
    /// canonical 路径跑到了 `.app` 之外。
    SymlinkEscape,
    /// 没有 `codesign` 工具。
    CodesignUnavailable,
    /// `codesign --verify --strict` 不通过，或没能跑完；附 codesign 的第一行输出（已清洗截断）。
    CodesignFailed(String),
    /// 签名通过，但团队不是要求的那个（`None` = 没有 TeamIdentifier，例如 ad-hoc 签名）。
    TeamMismatch {
        found: Option<String>,
    },
}

impl RejectReason {
    /// 稳定的原因码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotAbsolute => "not_absolute",
            Self::Unresolvable => "unresolvable",
            Self::NotRegularFile => "not_regular_file",
            Self::NotExecutable => "not_executable",
            Self::SetuidBinary => "setuid_binary",
            Self::WorldWritable => "world_writable",
            Self::UntrustedOwner => "untrusted_owner",
            Self::NotInAppBundle => "not_in_app_bundle",
            Self::SymlinkEscape => "symlink_escape",
            Self::CodesignUnavailable => "codesign_unavailable",
            Self::CodesignFailed(_) => "codesign_failed",
            Self::TeamMismatch { .. } => "team_mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub path: PathBuf,
    pub reason: RejectReason,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    /// 一个候选都没有：没装这个引擎。
    #[error("no engine executable found")]
    NotFound,
    /// 有候选，但都没通过；逐个附原因。
    #[error("engine executable candidates were rejected")]
    Rejected(Vec<Rejection>),
}

impl DiscoveryError {
    /// `/health.upstreams[].reason`：`engine_not_found`。有候选被拒时同样是它（用户要处理的是同一件事：
    /// 装一个合格的引擎），逐个原因在 [`DiscoveryError::Rejected`] 里，给日志与诊断。
    pub fn reason_code(&self) -> &'static str {
        "engine_not_found"
    }
}

enum Outcome {
    Accepted(EngineExecutable),
    /// 位置上没有文件。
    Missing,
    Rejected(RejectReason),
}

/// 发现并校验可执行文件。
///
/// `explicit` 是 launcher 显式给的路径（帧里的 `engine.executables[<engine_kind>]`，规范 §2.7.1；
/// 本模块不解析帧）：给了就**只**看它，它不合格就是错误，不回退到 [`DiscoveryPlan`] 里的任何位置。
/// 没给就按计划的顺序找。
pub async fn discover_executable(
    plan: &DiscoveryPlan,
    explicit: Option<&Path>,
) -> Result<EngineExecutable, DiscoveryError> {
    if let Some(explicit) = explicit {
        // Explicit paths come from the launcher and are trusted: no app-level signature check
        // (the check only applies to auto-discovered app bundles).
        return match check_candidate(explicit, ExecutableSource::Explicit, &plan.codesign).await {
            Outcome::Accepted(executable) => Ok(executable),
            Outcome::Missing => Err(rejected_one(explicit, RejectReason::Missing)),
            Outcome::Rejected(reason) => Err(rejected_one(explicit, reason)),
        };
    }
    let mut rejections = Vec::new();
    // Collected up front: no iterator adapter (and the closures in it) is held across an `.await`.
    let mut candidates: Vec<(PathBuf, ExecutableSource)> = plan
        .app_executables
        .iter()
        .map(|path| (path.clone(), ExecutableSource::AppBundle))
        .collect();
    for dir in &plan.search_dirs {
        for name in &plan.binary_names {
            candidates.push((dir.join(name), ExecutableSource::SearchDir));
        }
    }
    for (path, source) in candidates {
        match check_candidate(&path, source, &plan.codesign).await {
            Outcome::Accepted(executable) => return Ok(executable),
            Outcome::Missing => {}
            Outcome::Rejected(reason) => rejections.push(Rejection { path, reason }),
        }
    }
    if rejections.is_empty() {
        Err(DiscoveryError::NotFound)
    } else {
        Err(DiscoveryError::Rejected(rejections))
    }
}

fn rejected_one(path: &Path, reason: RejectReason) -> DiscoveryError {
    DiscoveryError::Rejected(vec![Rejection {
        path: path.to_owned(),
        reason,
    }])
}

async fn check_candidate(
    path: &Path,
    source: ExecutableSource,
    codesign: &CodesignPolicy,
) -> Outcome {
    if !path.is_absolute() {
        return Outcome::Rejected(RejectReason::NotAbsolute);
    }
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Outcome::Missing,
        Err(_) => return Outcome::Rejected(RejectReason::Unresolvable),
    };

    // App 内的候选：先确认没有逃出 `.app`，再做文件检查与签名检查。
    let bundle = if source == ExecutableSource::AppBundle {
        let Some(bundle) = app_bundle_root(path) else {
            return Outcome::Rejected(RejectReason::NotInAppBundle);
        };
        let Ok(canonical_bundle) = std::fs::canonicalize(&bundle) else {
            return Outcome::Rejected(RejectReason::Unresolvable);
        };
        if !canonical.starts_with(&canonical_bundle) {
            return Outcome::Rejected(RejectReason::SymlinkEscape);
        }
        Some(canonical_bundle)
    } else {
        None
    };

    if let Some(reason) = file_rejection(&canonical) {
        return Outcome::Rejected(reason);
    }

    let mut team_id = None;
    if let (Some(bundle), CodesignPolicy::Verify(check)) = (&bundle, codesign) {
        match verify_codesign(check, bundle).await {
            Ok(found) => team_id = Some(found),
            Err(reason) => return Outcome::Rejected(reason),
        }
    }
    Outcome::Accepted(EngineExecutable {
        path: canonical,
        source,
        team_id,
    })
}

/// 路径里最靠里的一级 `*.app`（按给定的、未解析的路径找）。
fn app_bundle_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.len() > 4 && name.ends_with(".app"))
        })
        .map(Path::to_owned)
}

/// canonical 路径上的文件本身要满足的条件。
fn file_rejection(canonical: &Path) -> Option<RejectReason> {
    let Ok(meta) = std::fs::metadata(canonical) else {
        return Some(RejectReason::Unresolvable);
    };
    if !meta.is_file() {
        return Some(RejectReason::NotRegularFile);
    }
    let mode = meta.permissions().mode();
    if mode & 0o111 == 0 {
        return Some(RejectReason::NotExecutable);
    }
    if mode & 0o6000 != 0 {
        return Some(RejectReason::SetuidBinary);
    }
    if mode & 0o002 != 0 {
        return Some(RejectReason::WorldWritable);
    }
    if meta.uid() != effective_uid() && meta.uid() != 0 {
        return Some(RejectReason::UntrustedOwner);
    }
    None
}

/// `codesign --verify --strict <bundle>`，再要求 `TeamIdentifier` 等于期望。返回读到的团队 id。
async fn verify_codesign(check: &CodesignCheck, bundle: &Path) -> Result<String, RejectReason> {
    let failed = |error: InspectError| match error {
        InspectError::ToolMissing => RejectReason::CodesignUnavailable,
        InspectError::TimedOut => RejectReason::CodesignFailed("codesign timed out".to_owned()),
        InspectError::NoAnswer => {
            RejectReason::CodesignFailed("codesign could not be run".to_owned())
        }
    };
    let bundle = bundle.as_os_str().to_owned();

    let verify_args = ["--verify".into(), "--strict".into(), bundle.clone()];
    let verified = run_tool(&check.tool, &verify_args, check.timeout)
        .await
        .map_err(failed)?;
    if verified.code != Some(0) {
        return Err(RejectReason::CodesignFailed(summarize(&verified.stderr)));
    }

    // `codesign -d` 把说明写到 stderr。
    let describe_args = ["-d".into(), "--verbose=2".into(), bundle];
    let described = run_tool(&check.tool, &describe_args, check.timeout)
        .await
        .map_err(failed)?;
    if described.code != Some(0) {
        return Err(RejectReason::CodesignFailed(summarize(&described.stderr)));
    }
    let found = team_identifier(&described.stderr).or_else(|| team_identifier(&described.stdout));
    match found {
        Some(team) if team == check.team_id => Ok(team),
        found => Err(RejectReason::TeamMismatch { found }),
    }
}

/// `TeamIdentifier=XXXXXXXXXX`；`not set`（ad-hoc 签名）不算。
fn team_identifier(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .find(|team| {
            !team.is_empty()
                && *team != "not set"
                && team.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(str::to_owned)
}

/// 第一行非空输出，去控制字符，截到 200 个字符：给人看的原因，不会很长。
fn summarize(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect::<String>()
        })
        .unwrap_or_else(|| "no output".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::symlink;

    use super::*;

    /// The team the fake `codesign` reports and the plan requires.
    const TEAM: &str = "TEAM12345A";

    fn script(path: &Path, body: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// `<root>/<name>.app/Contents/MacOS/engine-cli`, the real layout.
    fn app_cli(root: &Path, name: &str, mode: u32) -> PathBuf {
        let path = root.join(format!("{name}.app/Contents/MacOS/engine-cli"));
        script(&path, "echo 0.6.4", mode);
        path
    }

    /// Discovery without an explicit path.
    async fn find(plan: &DiscoveryPlan) -> Result<EngineExecutable, DiscoveryError> {
        discover_executable(plan, None).await
    }

    fn config(apps: Vec<PathBuf>) -> DiscoveryPlan {
        DiscoveryPlan {
            app_executables: apps,
            ..DiscoveryPlan::default()
        }
    }

    /// A fake `codesign`: `--verify` exits `verify_code`; `-d` prints `describe` on stderr.
    fn fake_codesign(dir: &Path, verify_code: i32, describe: &str) -> CodesignCheck {
        let tool = dir.join("codesign");
        script(
            &tool,
            &format!(
                "case \"$1\" in --verify) echo 'bundle: a verification message' >&2; exit {verify_code};; -d) cat >&2 <<'EOT'\n{describe}\nEOT\nexit 0;; esac"
            ),
            0o755,
        );
        CodesignCheck {
            tool,
            team_id: TEAM.to_owned(),
            // Far above anything but a hang: macOS serializes the first exec of every freshly
            // written file machine-wide (about 0.2 s each), so a tight bound is flaky under load.
            timeout: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn nothing_installed_is_not_found_not_a_rejection() {
        let root = tempfile::tempdir().unwrap();
        let apps = vec![root.path().join("A.app/Contents/MacOS/engine-cli")];
        let error = find(&config(apps)).await.unwrap_err();
        assert_eq!(error, DiscoveryError::NotFound);
        assert_eq!(error.reason_code(), "engine_not_found");
    }

    #[tokio::test]
    async fn the_first_valid_app_location_wins_and_the_path_is_canonical() {
        let root = tempfile::tempdir().unwrap();
        let first = app_cli(root.path(), "First", 0o755);
        let second = app_cli(root.path(), "Second", 0o755);
        let found = find(&config(vec![first.clone(), second])).await.unwrap();
        assert_eq!(found.path, std::fs::canonicalize(&first).unwrap());
        assert_eq!(found.source, ExecutableSource::AppBundle);
        assert_eq!(found.team_id, None, "codesign was off");

        // A missing first location falls through to the next.
        let absent = root.path().join("Absent.app/Contents/MacOS/engine-cli");
        let found = find(&config(vec![absent, first.clone()])).await.unwrap();
        assert_eq!(found.path, std::fs::canonicalize(&first).unwrap());
    }

    #[tokio::test]
    async fn an_explicit_path_wins_and_is_never_replaced_by_another_location() {
        let root = tempfile::tempdir().unwrap();
        let app = app_cli(root.path(), "Engine", 0o755);
        let custom = root.path().join("custom/engine");
        script(&custom, "echo 1", 0o755);

        let cfg = config(vec![app.clone()]);
        let found = discover_executable(&cfg, Some(&custom)).await.unwrap();
        assert_eq!(found.source, ExecutableSource::Explicit);
        assert_eq!(found.path, std::fs::canonicalize(&custom).unwrap());
        // Without it the plan's own locations are used.
        let found = find(&cfg).await.unwrap();
        assert_eq!(found.source, ExecutableSource::AppBundle);
        assert_eq!(found.path, std::fs::canonicalize(&app).unwrap());

        // An explicit path that does not validate is an error: no silent fall-through, even
        // though the plan has a perfectly good app location.
        for (path, reason) in [
            (root.path().join("custom/missing"), RejectReason::Missing),
            (PathBuf::from("relative/engine"), RejectReason::NotAbsolute),
        ] {
            assert_eq!(
                discover_executable(&cfg, Some(&path)).await.unwrap_err(),
                DiscoveryError::Rejected(vec![Rejection { path, reason }])
            );
        }
    }

    #[tokio::test]
    async fn files_that_are_not_safe_to_exec_are_rejected_with_the_reason() {
        let root = tempfile::tempdir().unwrap();
        let reject = |mode: u32, reason: RejectReason| {
            let name = format!("m{mode:o}");
            let path = app_cli(root.path(), &name, mode);
            (path, reason)
        };
        let cases = [
            reject(0o644, RejectReason::NotExecutable),
            reject(0o4755, RejectReason::SetuidBinary),
            reject(0o2755, RejectReason::SetuidBinary),
            reject(0o757, RejectReason::WorldWritable),
        ];
        for (path, reason) in cases {
            let error = find(&config(vec![path.clone()])).await.unwrap_err();
            assert_eq!(
                error,
                DiscoveryError::Rejected(vec![Rejection {
                    path,
                    reason: reason.clone()
                }]),
                "{reason:?}"
            );
        }

        // A directory, and a FIFO, are not regular files.
        let dir = root.path().join("Dir.app/Contents/MacOS/engine-cli");
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = config(vec![dir.clone()]);
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            DiscoveryError::Rejected(vec![Rejection {
                path: dir,
                reason: RejectReason::NotRegularFile
            }])
        );
        let fifo = root.path().join("Fifo.app/Contents/MacOS/engine-cli");
        std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
        assert!(
            std::process::Command::new("/usr/bin/mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        cfg.app_executables = vec![fifo.clone()];
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            DiscoveryError::Rejected(vec![Rejection {
                path: fifo,
                reason: RejectReason::NotRegularFile
            }])
        );
    }

    #[tokio::test]
    async fn a_symlink_that_escapes_the_app_bundle_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside/engine");
        script(&outside, "echo evil", 0o755);
        let link = root.path().join("User.app/Contents/MacOS/engine-cli");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&outside, &link).unwrap();
        assert_eq!(
            find(&config(vec![link.clone()])).await.unwrap_err(),
            DiscoveryError::Rejected(vec![Rejection {
                path: link,
                reason: RejectReason::SymlinkEscape
            }])
        );
    }

    #[tokio::test]
    async fn a_symlink_that_stays_inside_the_bundle_or_whose_bundle_is_a_link_is_fine() {
        let root = tempfile::tempdir().unwrap();
        // Inside: Contents/MacOS/engine-cli -> ../Resources/real-cli
        let real = root.path().join("In.app/Contents/Resources/real-cli");
        script(&real, "echo 1", 0o755);
        let link = root.path().join("In.app/Contents/MacOS/engine-cli");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink("../Resources/real-cli", &link).unwrap();
        let found = find(&config(vec![link])).await.unwrap();
        assert_eq!(found.path, std::fs::canonicalize(&real).unwrap());

        // The .app itself is a symlink (e.g. /Applications/Engine.app -> elsewhere): its
        // canonical location is the containment root, so this is not an escape.
        let actual = app_cli(root.path(), "Actual", 0o755);
        let alias = root.path().join("Alias.app");
        symlink(root.path().join("Actual.app"), &alias).unwrap();
        let found = find(&config(vec![alias.join("Contents/MacOS/engine-cli")]))
            .await
            .unwrap();
        assert_eq!(found.path, std::fs::canonicalize(&actual).unwrap());
    }

    #[tokio::test]
    async fn a_dangling_or_looping_link_is_missing_or_unresolvable_never_accepted() {
        let root = tempfile::tempdir().unwrap();
        let dangling = root.path().join("D.app/Contents/MacOS/engine-cli");
        std::fs::create_dir_all(dangling.parent().unwrap()).unwrap();
        symlink(root.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(
            find(&config(vec![dangling])).await.unwrap_err(),
            DiscoveryError::NotFound
        );
        let looping = root.path().join("L.app/Contents/MacOS/engine-cli");
        std::fs::create_dir_all(looping.parent().unwrap()).unwrap();
        symlink(&looping, &looping).unwrap();
        assert_eq!(
            find(&config(vec![looping.clone()])).await.unwrap_err(),
            DiscoveryError::Rejected(vec![Rejection {
                path: looping,
                reason: RejectReason::Unresolvable
            }])
        );
    }

    #[tokio::test]
    async fn an_app_location_without_an_app_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let loose = root.path().join("bin/engine-cli");
        script(&loose, "echo 1", 0o755);
        assert_eq!(
            find(&config(vec![loose.clone()])).await.unwrap_err(),
            DiscoveryError::Rejected(vec![Rejection {
                path: loose,
                reason: RejectReason::NotInAppBundle
            }])
        );
    }

    #[tokio::test]
    async fn path_style_directories_are_not_searched_unless_given() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        script(&bin.join("engine"), "echo 1", 0o755);
        script(&bin.join("engine-alt"), "echo 2", 0o755);

        // Default: no search directories, so a perfectly good binary in some directory is invisible.
        let plan = DiscoveryPlan::default();
        assert!(plan.search_dirs.is_empty());
        assert_eq!(find(&plan).await.unwrap_err(), DiscoveryError::NotFound);

        // Opt-in: directories from a PATH-shaped string become candidates, in order, each
        // looked at for every binary name, and a user-installed CLI is exempt from the
        // app-level signature check.
        let mut plan = DiscoveryPlan {
            binary_names: vec!["missing".into(), "engine-alt".into(), "engine".into()],
            ..DiscoveryPlan::default()
        };
        plan.codesign =
            CodesignPolicy::Verify(fake_codesign(root.path(), 1, "TeamIdentifier=EVIL"));
        let path = std::env::join_paths([
            PathBuf::from("relative-is-ignored"),
            root.path().join("empty"),
            bin.clone(),
        ])
        .unwrap();
        let found = find(&plan.with_path_dirs(&path)).await.unwrap();
        assert_eq!(found.source, ExecutableSource::SearchDir);
        assert_eq!(
            found.path,
            std::fs::canonicalize(bin.join("engine-alt")).unwrap(),
            "binary names are tried in the order the plan lists them"
        );
        assert_eq!(found.team_id, None);
    }

    #[tokio::test]
    async fn codesign_passes_only_for_the_expected_team() {
        let root = tempfile::tempdir().unwrap();
        let app = app_cli(root.path(), "Engine", 0o755);
        let tools = root.path().join("tools");
        let describe = "Executable=/x\nIdentifier=app.engine\nTeamIdentifier=TEAM12345A\nSealed Resources version=2";
        let mut cfg = config(vec![app.clone()]);

        cfg.codesign = CodesignPolicy::Verify(fake_codesign(&tools, 0, describe));
        let found = find(&cfg).await.unwrap();
        assert_eq!(found.team_id.as_deref(), Some("TEAM12345A"));

        let rejected = |reason: RejectReason| {
            DiscoveryError::Rejected(vec![Rejection {
                path: app.clone(),
                reason,
            }])
        };
        cfg.codesign = CodesignPolicy::Verify(fake_codesign(
            &tools,
            0,
            "Identifier=app.engine\nTeamIdentifier=OTHERTEAM1",
        ));
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            rejected(RejectReason::TeamMismatch {
                found: Some("OTHERTEAM1".into())
            })
        );
        cfg.codesign = CodesignPolicy::Verify(fake_codesign(
            &tools,
            0,
            "Identifier=app.engine\nTeamIdentifier=not set",
        ));
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            rejected(RejectReason::TeamMismatch { found: None })
        );
        cfg.codesign = CodesignPolicy::Verify(fake_codesign(&tools, 0, "Identifier=app.engine"));
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            rejected(RejectReason::TeamMismatch { found: None })
        );
        // A team id appearing in another field is not the team identifier.
        cfg.codesign = CodesignPolicy::Verify(fake_codesign(
            &tools,
            0,
            "Authority=Developer ID Application: X (TEAM12345A)\nTeamIdentifier=not set",
        ));
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            rejected(RejectReason::TeamMismatch { found: None })
        );
    }

    #[tokio::test]
    async fn a_failed_signature_check_reports_why_and_a_missing_tool_is_its_own_reason() {
        let root = tempfile::tempdir().unwrap();
        let app = app_cli(root.path(), "Engine", 0o755);
        let tools = root.path().join("tools");
        let mut cfg = config(vec![app.clone()]);

        cfg.codesign =
            CodesignPolicy::Verify(fake_codesign(&tools, 3, "TeamIdentifier=TEAM12345A"));
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            DiscoveryError::Rejected(vec![Rejection {
                path: app.clone(),
                reason: RejectReason::CodesignFailed("bundle: a verification message".into())
            }]),
            "a bad signature is a failure even when the team id would match"
        );

        let mut check = fake_codesign(&tools, 0, "TeamIdentifier=TEAM12345A");
        check.tool = root.path().join("no-such-codesign");
        cfg.codesign = CodesignPolicy::Verify(check);
        assert_eq!(
            find(&cfg).await.unwrap_err(),
            DiscoveryError::Rejected(vec![Rejection {
                path: app.clone(),
                reason: RejectReason::CodesignUnavailable
            }])
        );

        let hang = tools.join("hang");
        script(&hang, "sleep 30", 0o755);
        cfg.codesign = CodesignPolicy::Verify(CodesignCheck {
            tool: hang,
            team_id: TEAM.into(),
            timeout: Duration::from_millis(200),
        });
        assert!(matches!(
            find(&cfg).await.unwrap_err(),
            DiscoveryError::Rejected(r) if matches!(&r[0].reason, RejectReason::CodesignFailed(m) if m.contains("timed out"))
        ));
    }

    #[tokio::test]
    async fn signature_checks_do_not_apply_to_an_explicit_user_installed_cli() {
        let root = tempfile::tempdir().unwrap();
        let custom = root.path().join("homebrew/engine");
        script(&custom, "echo 1", 0o755);
        let mut cfg = config(Vec::new());
        // Even a codesign that fails every call is never consulted.
        cfg.codesign = CodesignPolicy::Verify(fake_codesign(root.path(), 9, "TeamIdentifier=EVIL"));
        let found = discover_executable(&cfg, Some(&custom)).await.unwrap();
        assert_eq!(found.source, ExecutableSource::Explicit);
        assert_eq!(found.team_id, None);
    }

    #[test]
    fn team_identifier_is_read_only_from_its_own_line() {
        assert_eq!(
            team_identifier("TeamIdentifier=TEAM12345A\n"),
            Some("TEAM12345A".into())
        );
        assert_eq!(
            team_identifier("  TeamIdentifier=AB12 \n"),
            Some("AB12".into())
        );
        for none in [
            "",
            "TeamIdentifier=not set",
            "TeamIdentifier=",
            "Team=AB",
            "TeamIdentifier=a b",
            "xTeamIdentifier=AB",
        ] {
            assert_eq!(team_identifier(none), None, "{none:?}");
        }
    }
}

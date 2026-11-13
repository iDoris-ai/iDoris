//! 云凭证：类型、存储、来源。
//!
//! # 三类密钥，三个类型，三个存放位置，互不通用
//!
//! | 密钥 | 代码类型 | 存放位置 | 用在哪 |
//! |---|---|---|---|
//! | 本地数据密钥（`idoris key issue`） | `idoris_tenancy::virtual_key::VirtualKeySecret` | 库里只有 SHA-256 哈希（`virtual_keys` 表），明文只在签发时出现一次 | 本机数据面（loopback） |
//! | **云凭证**（配对兑换出来的设备凭证，或运维签发的 Node 凭证） | 本模块的 [`CloudCredential`] | 权限 0600 的文件（[`DeviceStore`]、Node 凭证文件）或环境变量；**不在 SQLite 里，不在配置文件里** | 只作为 `Authorization: Bearer` 发往配置的云端源 |
//! | Admin v0 会话 token | `crate::admin::AdminSessionToken` | 只在内存里（启动时由 launcher 经 stdin 给，或随机生成） | 本机 Admin 监听（loopback，只读） |
//!
//! 它们之间**没有任何转换**：[`CloudCredential`] 没有 `From<VirtualKeySecret>`、没有 `From<AdminSessionToken>`，
//! 构造它的唯一入口是 [`CloudCredential::parse`]，而 `parse` 只认 `idk_` + 64 位小写十六进制：
//!
//! - Admin token 是 64 位十六进制、**没有** `idk_` 前缀，`parse` 直接拒绝：**Admin token 不可能被当成云凭证发出去**；
//! - 本地数据密钥和云凭证长得一样（同一套虚拟 key 机制），格式分不开，所以每次加载都再问一次
//!   [`LocalSecretProbe`]：这个密钥是不是本机库里一把有效的本地数据密钥？是就拒绝：**本机的数据密钥不会被发给云端**
//!   （运维把本地密钥错贴进 Node 凭证文件的那种事故，在出网之前就被挡住）；
//! - 反过来，云凭证拿到本机数据面或 Admin 端口也过不了认证（本机库里没有它，Admin 的 token 没有 `idk_` 前缀）。
//!
//! # 设备凭证文件（个人形态）
//!
//! `idoris cloud pair` 把兑换出来的凭证写成 `<state_dir>/device.json`（JSON，含明文密钥）：
//! 目录 0700（我们创建时）、文件 **0600**，原子**新装**（同目录里 `O_EXCL|0600` 的临时文件 → `fsync` → 硬链接到目标名 →
//! 删临时名 → `fsync` 目录；**不覆盖**已有的文件，见 [`DeviceStore::install`]），所以任何时刻磁盘上看到的要么没有、要么是完整的、
//! 权限正确的文件。`pair` / `unpair` 的整个流程在状态目录的跨进程锁里（[`DeviceStore::try_lock`]）。
//! 读取时**再检查一次**：先用 `O_NOFOLLOW|O_NONBLOCK` 打开，再用描述符验证：必须是普通文件（不是符号链接）、
//! 权限不能有 group / other 位，否则按「不可用」处理（失败即关闭，并告诉用户 `chmod 600`），不会拿一个别人也读得到的密钥去出网。
//!
//! 文件里还记了**凭证被签发的源**（`origin`）：配置里的云端地址换了源，这张凭证就不再被使用（`OriginMismatch`）——
//! 凭证只会发往签发它的那个源，配置被改成指向别处不会把它带过去。
//!
//! 密钥不进日志、不进错误信息、不进 `Debug` / `Display`：本模块所有错误都是固定文案，不带文件内容也不带 IO 错误文本。
//! 内存里的明文无法保证清零（工作区 `forbid(unsafe_code)`，也没有 `zeroize` 依赖）：这是已知的、接受的限制。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};

/// 凭证明文的格式：和本地虚拟 key 一样是 `idk_` + 64 位小写十六进制。
const PREFIX: &str = "idk_";
const HEX_LEN: usize = 64;

/// 设备凭证文件的文件名。
pub const DEVICE_FILE: &str = "device.json";

/// 文件里的版本号。
const DEVICE_FILE_VERSION: u32 = 1;

/// 凭证加载失败。**固定文案**，没有密钥、没有文件内容、没有 IO 错误文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    /// 个人形态：还没有配对（没有设备凭证文件）。
    NotPaired,
    /// 文件或环境变量不存在 / 读不了。
    Unreadable,
    /// 内容不是 `idk_` + 64 位小写十六进制（比如 Admin token、被截断、多余的内容）。
    Malformed,
    /// 文件不是普通文件，或者 group / other 能读写它。
    InsecureFile,
    /// 设备凭证签发的源和配置里的云端源不一样。
    OriginMismatch,
    /// 这是本机一把有效的本地数据密钥：本地密钥不发给云端。
    LocalKeyRefused,
    /// 本平台不支持（凭证文件的权限检查只做了 Unix）。
    Unsupported,
    /// 状态目录的锁被另一个 `idoris cloud pair` / `unpair` 占着。
    Busy,
    /// 设备凭证文件已经存在：安装不覆盖已有的凭证。
    AlreadyPaired,
}

impl CredentialError {
    /// 稳定字符串（给日志和 `X-iDoris-Cloud-Attempt` 之外的诊断用）。
    pub fn code(self) -> &'static str {
        match self {
            CredentialError::NotPaired => "not_paired",
            CredentialError::Unreadable => "credential_unreadable",
            CredentialError::Malformed => "credential_malformed",
            CredentialError::InsecureFile => "credential_file_insecure",
            CredentialError::OriginMismatch => "credential_origin_mismatch",
            CredentialError::LocalKeyRefused => "credential_is_a_local_key",
            CredentialError::Unsupported => "unsupported_platform",
            CredentialError::Busy => "state_busy",
            CredentialError::AlreadyPaired => "credential_already_present",
        }
    }
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CredentialError::NotPaired => "还没有配对云账号",
            CredentialError::Unreadable => "云凭证读取失败（文件或环境变量不存在，或没有读权限）",
            CredentialError::Malformed => "云凭证格式不对（应为 idk_ 加 64 位小写十六进制）",
            CredentialError::InsecureFile => {
                "云凭证文件不是普通文件，或者其他用户能读写它（请 chmod 600，并确认不是符号链接）"
            }
            CredentialError::OriginMismatch => {
                "云凭证签发的云端地址和配置里的云端地址不一致：凭证只会发往签发它的地址"
            }
            CredentialError::LocalKeyRefused => {
                "这把密钥是本机的本地数据密钥，不能当云凭证（本地密钥不发给云端）"
            }
            CredentialError::Unsupported => "云凭证文件的权限检查只支持 Unix 系统",
            CredentialError::Busy => {
                "另一个 idoris cloud pair / unpair 正在这台机器上运行（它占着设备凭证目录的锁）"
            }
            CredentialError::AlreadyPaired => "设备凭证文件已经存在（安装不会覆盖已有的凭证）",
        })
    }
}

impl std::error::Error for CredentialError {}

/// 云凭证：只能发往云端源的 bearer 密钥。**不是**本地数据密钥，也**不是** Admin token。
pub struct CloudCredential(String);

impl CloudCredential {
    /// 唯一的构造入口。只认 `idk_` + 64 位小写十六进制。
    pub fn parse(raw: &str) -> Result<Self, CredentialError> {
        let well_formed = raw.len() == PREFIX.len() + HEX_LEN
            && raw.starts_with(PREFIX)
            && raw[PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if well_formed {
            Ok(Self(raw.to_string()))
        } else {
            Err(CredentialError::Malformed)
        }
    }

    /// 显式取出明文：只给构造 `Authorization` 头和写设备凭证文件用。永远不要记日志。
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// `Authorization: Bearer …` 的头值，标记为 sensitive（`Debug` 里不显示）。
    pub fn authorization(&self) -> HeaderValue {
        // 格式已校验：只有 ASCII 字母数字和 `_`，一定是合法的头值。
        let mut value = HeaderValue::from_str(&format!("Bearer {}", self.0))
            .unwrap_or_else(|_| HeaderValue::from_static("Bearer invalid"));
        value.set_sensitive(true);
        value
    }
}

impl Clone for CloudCredential {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for CloudCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudCredential([REDACTED])")
    }
}

impl std::fmt::Display for CloudCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// 「这个密钥是不是本机一把有效的本地数据密钥」。生产里由本机的虚拟 key 库实现；
/// 本地密钥和云凭证格式一样，只能靠它分开。
pub trait LocalSecretProbe: Send + Sync {
    fn is_local_secret(&self, secret: &str) -> bool;
}

/// 加载出来的云凭证和它的来源（给 `status` 和日志用，不含密钥）。
#[derive(Debug, Clone)]
pub struct LoadedCredential {
    pub secret: CloudCredential,
    pub kind: CredentialKind,
}

/// 凭证的来源种类（不含密钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialKind {
    /// 配对兑换出来的设备凭证。
    Device {
        credential_id: String,
        membership: String,
    },
    /// 运维签发的 Node 凭证（文件或环境变量）。
    Node,
}

/// 云凭证的来源。每次调用 `load` 都重新取（设备凭证被 `unpair` 删掉、Node 凭证文件被轮换，下一次调用立刻生效）。
///
/// **实现者的 `Debug` 不能打印密钥**：这个 trait 要求 `Debug`，所以任何持有 `dyn CredentialProvider` 的地方
/// （比如 [`super::CloudTier`]）都可能被 `{:?}`。持有明文的实现必须手写脱敏的 `Debug`，不能 derive。
pub trait CredentialProvider: Send + Sync + std::fmt::Debug {
    fn load(&self) -> Result<LoadedCredential, CredentialError>;
}

// ------------------------------------------------------------------------------------------------
// 设备凭证文件
// ------------------------------------------------------------------------------------------------

/// `device.json` 的内容。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceFile {
    version: u32,
    origin: String,
    credential_id: String,
    membership: String,
    pairing_id: String,
    paired_at_ms: i64,
    secret: String,
}

/// 一台设备的配对记录。
#[derive(Debug, Clone)]
pub struct DeviceRecord {
    /// 凭证被签发的源（`scheme://host[:port]`）。
    pub origin: String,
    pub credential_id: String,
    /// 云账号绑定：兑换响应里的成员关系 id。
    pub membership: String,
    pub pairing_id: String,
    pub paired_at_ms: i64,
    pub secret: CloudCredential,
}

/// 非密钥的展示字段（`idoris cloud status` 用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceSummary {
    pub origin: String,
    pub membership: String,
    /// 凭证 id 的前缀（11 个字符），不是密钥的任何部分。
    pub credential_id_prefix: String,
    pub paired_at_ms: i64,
}

/// 凭证 id 的展示前缀长度。
pub const CREDENTIAL_ID_PREFIX_CHARS: usize = 11;

impl DeviceRecord {
    pub fn summary(&self) -> DeviceSummary {
        DeviceSummary {
            origin: self.origin.clone(),
            membership: self.membership.clone(),
            credential_id_prefix: self
                .credential_id
                .chars()
                .take(CREDENTIAL_ID_PREFIX_CHARS)
                .collect(),
            paired_at_ms: self.paired_at_ms,
        }
    }
}

/// 展示用的 id（凭证 id、成员关系 id、配对 id）：非空、不长、没有空白和控制字符。
fn valid_display_id(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

/// 状态目录里的锁文件名（`flock` 的对象）。文件一直留在目录里：锁在文件描述符上，不在文件的有无上。
pub const LOCK_FILE: &str = ".lock";

/// 状态目录上的跨进程独占锁（见 [`DeviceStore::try_lock`]）。丢掉它（或者进程退出）就释放。
#[derive(Debug)]
pub struct StateLock {
    _file: std::fs::File,
}

/// 设备凭证文件的存储：一个目录里的 `device.json`。
#[derive(Debug, Clone)]
pub struct DeviceStore {
    dir: PathBuf,
}

impl DeviceStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(DEVICE_FILE)
    }

    /// 读设备凭证。没有文件是 `Ok(None)`；有文件但不可用（权限、格式、符号链接）是错误：失败即关闭。
    #[cfg(unix)]
    pub fn read(&self) -> Result<Option<DeviceRecord>, CredentialError> {
        let Some(text) = read_private_regular_file(&self.path())? else {
            return Ok(None);
        };
        let file: DeviceFile =
            serde_json::from_str(&text).map_err(|_| CredentialError::Malformed)?;
        if file.version != DEVICE_FILE_VERSION
            || !valid_display_id(&file.credential_id)
            || !valid_display_id(&file.membership)
            || !valid_display_id(&file.pairing_id)
            || file.origin.is_empty()
            || file.origin.len() > 256
        {
            return Err(CredentialError::Malformed);
        }
        Ok(Some(DeviceRecord {
            origin: file.origin,
            credential_id: file.credential_id,
            membership: file.membership,
            pairing_id: file.pairing_id,
            paired_at_ms: file.paired_at_ms,
            secret: CloudCredential::parse(&file.secret)?,
        }))
    }

    #[cfg(not(unix))]
    pub fn read(&self) -> Result<Option<DeviceRecord>, CredentialError> {
        Err(CredentialError::Unsupported)
    }

    /// 兑换**之前**调用：确认目录能建、能写。兑换是一次性的，兑换成功后才发现写不进去，凭证就丢了
    /// （云端那张凭证还有效，本机却没有它）。预检不能保证之后一定写得进去，但挡得住最常见的那些原因（权限、只读卷）。
    #[cfg(unix)]
    pub fn preflight(&self) -> Result<(), CredentialError> {
        let probe = self.create_private_dir()?;
        let path = probe.join(format!(".preflight-{}", uuid::Uuid::new_v4().simple()));
        let mut file = open_private_new(&path).map_err(|_| CredentialError::Unreadable)?;
        let wrote = file.write_all(b"ok").and_then(|()| file.sync_all());
        drop(file);
        let _ = std::fs::remove_file(&path);
        wrote.map_err(|_| CredentialError::Unreadable)
    }

    #[cfg(not(unix))]
    pub fn preflight(&self) -> Result<(), CredentialError> {
        Err(CredentialError::Unsupported)
    }

    /// 原子地**新装**设备凭证，**不覆盖**已有的文件（已经有一张就是 [`CredentialError::AlreadyPaired`]）。
    ///
    /// 同目录里 `O_EXCL|0600` 的临时文件 → `fsync` → **硬链接**到目标名（`link` 在目标已存在时失败，不会覆盖，
    /// 也不跟随目标处的符号链接）→ 删掉临时名 → `fsync` 目录。所以任何时刻磁盘上看到的要么没有、要么是完整的、权限正确的文件，
    /// 并且两个进程同时安装，恰好一个成功。文件系统不支持硬链接（FAT 之类）时退到 `O_EXCL` 直接创建目标文件，同样不覆盖
    /// （那条路上写完之前读取者可能读到半截文件，读取是失败即关闭的，不会拿它出网）。
    ///
    /// 以前这里是 `rename`：它会覆盖已有文件。两个进程各兑换一个账号的配对码、都先读到文件不存在，先装的那张被后装的覆盖，
    /// 两个都报告成功，先装的那张云凭证仍然有效、本机却丢了它（审查 r4 Medium#4）。
    #[cfg(unix)]
    pub fn install(&self, record: &DeviceRecord) -> Result<(), CredentialError> {
        let dir = self.create_private_dir()?;
        let file = DeviceFile {
            version: DEVICE_FILE_VERSION,
            origin: record.origin.clone(),
            credential_id: record.credential_id.clone(),
            membership: record.membership.clone(),
            pairing_id: record.pairing_id.clone(),
            paired_at_ms: record.paired_at_ms,
            secret: record.secret.expose_secret().to_string(),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&file).map_err(|_| CredentialError::Unreadable)?;
        bytes.push(b'\n');
        let target = self.path();
        let tmp = dir.join(format!(
            ".device.json.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let staged = open_private_new(&tmp).and_then(|mut out| {
            out.write_all(&bytes)?;
            out.sync_all()
        });
        if staged.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(CredentialError::Unreadable);
        }
        let outcome = match std::fs::hard_link(&tmp, &target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(CredentialError::AlreadyPaired)
            }
            Err(_) => create_exclusive(&target, &bytes),
        };
        let _ = std::fs::remove_file(&tmp);
        if outcome.is_ok() {
            // 目录项也落盘；失败不致命（文件已经完整地在位）
            if let Ok(handle) = std::fs::File::open(&dir) {
                let _ = handle.sync_all();
            }
        }
        outcome
    }

    #[cfg(not(unix))]
    pub fn install(&self, _record: &DeviceRecord) -> Result<(), CredentialError> {
        Err(CredentialError::Unsupported)
    }

    /// 拿状态目录上的**跨进程独占锁**（`<state_dir>/.lock` 上的 `flock`，立即返回，不等）：`idoris cloud pair` 和
    /// `idoris cloud unpair` 的整个流程都在它里面——「有没有配对」的检查、兑换、安装是一个整体，不能被另一个进程的同一个
    /// 流程穿插（审查 r4 Medium#4：两个进程各兑换一个账号的配对码，先装的被后装的覆盖）。
    ///
    /// 内核在持有者退出（包括崩溃）时释放它，没有陈旧锁；文件是 close-on-exec 的，子进程不继承。服务进程读凭证不拿这把锁
    /// （安装是原子的，读到的要么没有要么完整）。
    ///
    /// `create_dir` 为真时目录不存在就建（0700，配对用）；为假时目录不存在返回 `Ok(None)`：没有目录就没有配对，
    /// 解绑不需要为了拿锁去建一个目录。已经被别人占着是 [`CredentialError::Busy`]。
    #[cfg(unix)]
    pub fn try_lock(&self, create_dir: bool) -> Result<Option<StateLock>, CredentialError> {
        use std::os::unix::fs::OpenOptionsExt;
        if create_dir {
            self.create_private_dir()?;
        } else if !self.dir.is_dir() {
            return Ok(None);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.dir.join(LOCK_FILE))
            .map_err(|_| CredentialError::Unreadable)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(StateLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Err(CredentialError::Busy),
            Err(std::fs::TryLockError::Error(_)) => Err(CredentialError::Unreadable),
        }
    }

    #[cfg(not(unix))]
    pub fn try_lock(&self, _create_dir: bool) -> Result<Option<StateLock>, CredentialError> {
        Err(CredentialError::Unsupported)
    }

    /// 删除设备凭证文件。返回有没有删掉东西；本来就没有是 `Ok(false)`。
    pub fn remove(&self) -> Result<bool, CredentialError> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(CredentialError::Unreadable),
        }
    }

    #[cfg(unix)]
    fn create_private_dir(&self) -> Result<PathBuf, CredentialError> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|_| CredentialError::Unreadable)?;
        Ok(self.dir.clone())
    }
}

/// 凭证文件的大小上限：一个凭证文件只有几百个字节，更大的东西不是凭证文件。
#[cfg(unix)]
const MAX_CREDENTIAL_FILE_BYTES: u64 = 16 * 1024;

/// 读一个只有属主能读写的普通文件（`Ok(None)` = 不存在）。
///
/// **先打开，再用描述符验证**——检查和读取落在同一个已打开的文件上，中间没有「检查完、打开前被换掉」的窗口：
///
/// - `O_NOFOLLOW`：最后一级是符号链接就直接打开失败，不跟随；
/// - `O_NONBLOCK`：打开 FIFO 不会阻塞在「没有写端」的 `open` 上。以前先 `lstat` 检查、再阻塞地 `open`：检查之后目录项被换成
///   FIFO，`open` 永远不返回，`fstat` 也没机会做，读取线程被无限挂住（凭证加载发生在并发槽位之前，不受云请求总超时限制）——
///   审查 r4 Medium#6。对普通文件没有影响；
/// - 打开之后 `fstat` 描述符（[`check_opened_credential_file`]）：必须是普通文件、没有 group / other 位。
///
/// 审查还建议验证属主和目录的安全性。验证属主要取得进程的有效 uid：工作区 `forbid(unsafe_code)`，而 `nix` 没有开
/// `user` feature（`Uid` 在那里面），改依赖的声明不在这次修复的范围内，所以没做，留给 owner 决定。
#[cfg(unix)]
fn read_private_regular_file(path: &Path) -> Result<Option<String>, CredentialError> {
    read_private_regular_file_with(path, &mut || {})
}

/// [`read_private_regular_file`]；`before_open` 在打开之前被调用（测试用它模拟另一个进程在最坏的时刻换掉目录项）。
#[cfg(unix)]
fn read_private_regular_file_with(
    path: &Path,
    before_open: &mut dyn FnMut(),
) -> Result<Option<String>, CredentialError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    before_open();
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // 最后一级是符号链接：带 O_NOFOLLOW 的 open 以 ELOOP 失败（个别 BSD 是 EMLINK）。符号链接一概不用
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(code) if code == nix::libc::ELOOP || code == nix::libc::EMLINK
            ) =>
        {
            return Err(CredentialError::InsecureFile);
        }
        Err(_) => return Err(CredentialError::Unreadable),
    };
    let metadata = file.metadata().map_err(|_| CredentialError::Unreadable)?;
    check_opened_credential_file(&metadata)?;
    let mut text = String::new();
    file.take(MAX_CREDENTIAL_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| CredentialError::Unreadable)?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_CREDENTIAL_FILE_BYTES {
        return Err(CredentialError::Malformed);
    }
    Ok(Some(text))
}

/// 用**已打开文件的描述符**的元数据验证它：普通文件、没有 group / other 位。
#[cfg(unix)]
fn check_opened_credential_file(metadata: &std::fs::Metadata) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(CredentialError::InsecureFile);
    }
    Ok(())
}

/// 硬链接不可用的文件系统上的安装：`O_EXCL` 直接创建目标文件（已存在就是 `AlreadyPaired`），写完 `fsync`。
/// 写到一半失败就把半截的目标文件删掉：留着它，下一次就成了一个「不可用的凭证文件」。
#[cfg(unix)]
fn create_exclusive(target: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    let mut out = match open_private_new(target) {
        Ok(out) => out,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(CredentialError::AlreadyPaired);
        }
        Err(_) => return Err(CredentialError::Unreadable),
    };
    let written = out.write_all(bytes).and_then(|()| out.sync_all());
    drop(out);
    if written.is_err() {
        let _ = std::fs::remove_file(target);
        return Err(CredentialError::Unreadable);
    }
    Ok(())
}

/// 新建（`O_EXCL`）一个只有属主能读写的文件。
#[cfg(unix)]
fn open_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// 个人形态：`idoris cloud pair` 写下的设备凭证。
#[derive(Debug)]
pub struct PairedDeviceProvider {
    store: DeviceStore,
    origin: String,
    probe: Option<Arc<dyn LocalSecretProbe>>,
}

impl PairedDeviceProvider {
    pub fn new(
        store: DeviceStore,
        origin: impl Into<String>,
        probe: Option<Arc<dyn LocalSecretProbe>>,
    ) -> Self {
        Self {
            store,
            origin: origin.into(),
            probe,
        }
    }
}

impl std::fmt::Debug for dyn LocalSecretProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalSecretProbe")
    }
}

impl CredentialProvider for PairedDeviceProvider {
    fn load(&self) -> Result<LoadedCredential, CredentialError> {
        let record = self.store.read()?.ok_or(CredentialError::NotPaired)?;
        if record.origin != self.origin {
            return Err(CredentialError::OriginMismatch);
        }
        refuse_local_key(self.probe.as_deref(), &record.secret)?;
        Ok(LoadedCredential {
            kind: CredentialKind::Device {
                credential_id: record.credential_id,
                membership: record.membership,
            },
            secret: record.secret,
        })
    }
}

fn refuse_local_key(
    probe: Option<&dyn LocalSecretProbe>,
    secret: &CloudCredential,
) -> Result<(), CredentialError> {
    match probe {
        Some(probe) if probe.is_local_secret(secret.expose_secret()) => {
            Err(CredentialError::LocalKeyRefused)
        }
        _ => Ok(()),
    }
}

// ------------------------------------------------------------------------------------------------
// Node 凭证（组织形态）
// ------------------------------------------------------------------------------------------------

/// 组织形态：运维签发的 Node 凭证，放在一个文件里（每次调用重新读，轮换不用重启）。
#[derive(Debug)]
pub struct NodeFileProvider {
    path: PathBuf,
    probe: Option<Arc<dyn LocalSecretProbe>>,
}

impl NodeFileProvider {
    pub fn new(path: impl Into<PathBuf>, probe: Option<Arc<dyn LocalSecretProbe>>) -> Self {
        Self {
            path: path.into(),
            probe,
        }
    }
}

impl CredentialProvider for NodeFileProvider {
    #[cfg(unix)]
    fn load(&self) -> Result<LoadedCredential, CredentialError> {
        let text = read_private_regular_file(&self.path)?.ok_or(CredentialError::Unreadable)?;
        let secret = CloudCredential::parse(text.trim())?;
        refuse_local_key(self.probe.as_deref(), &secret)?;
        Ok(LoadedCredential {
            secret,
            kind: CredentialKind::Node,
        })
    }

    #[cfg(not(unix))]
    fn load(&self) -> Result<LoadedCredential, CredentialError> {
        Err(CredentialError::Unsupported)
    }
}

/// 组织形态：Node 凭证放在环境变量里（启动时读一次；不推荐，同用户的进程读得到环境变量）。
///
/// `Debug` 是手写的：`secret` 里存的是明文（启动时读到的环境变量值）。不能 derive：`CredentialProvider: Debug`，
/// 云端档这样持有 `dyn CredentialProvider` 的地方随手一个 `{:?}` 就会把整个密钥打出来（审查 r4 Medium#3）。
pub struct NodeEnvProvider {
    secret: Option<String>,
    probe: Option<Arc<dyn LocalSecretProbe>>,
}

impl std::fmt::Debug for NodeEnvProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeEnvProvider")
            // 只说有没有，不说是什么
            .field("secret", &self.secret.as_ref().map(|_| "[REDACTED]"))
            .field("probe", &self.probe)
            .finish()
    }
}

impl NodeEnvProvider {
    /// `value` 是启动时读到的环境变量值（没设是 `None`）。
    pub fn new(value: Option<String>, probe: Option<Arc<dyn LocalSecretProbe>>) -> Self {
        Self {
            secret: value,
            probe,
        }
    }
}

impl CredentialProvider for NodeEnvProvider {
    fn load(&self) -> Result<LoadedCredential, CredentialError> {
        let raw = self.secret.as_deref().ok_or(CredentialError::Unreadable)?;
        let secret = CloudCredential::parse(raw.trim())?;
        refuse_local_key(self.probe.as_deref(), &secret)?;
        Ok(LoadedCredential {
            secret,
            kind: CredentialKind::Node,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn secret(seed: char) -> String {
        format!("idk_{}", seed.to_string().repeat(HEX_LEN))
    }

    fn record(origin: &str, seed: char) -> DeviceRecord {
        DeviceRecord {
            origin: origin.to_string(),
            credential_id: "vk_0123456789abcdef0123456789abcdef".to_string(),
            membership: "m-agent-1".to_string(),
            pairing_id: "pc_0123456789abcdef".to_string(),
            paired_at_ms: 1_700_000_000_000,
            secret: CloudCredential::parse(&secret(seed)).unwrap(),
        }
    }

    struct Probe(String);
    impl LocalSecretProbe for Probe {
        fn is_local_secret(&self, candidate: &str) -> bool {
            candidate == self.0
        }
    }

    #[test]
    fn only_a_virtual_key_shaped_secret_is_a_cloud_credential() {
        assert!(CloudCredential::parse(&secret('a')).is_ok());
        let admin_token = "ab".repeat(32); // Admin v0 会话 token：64 位十六进制，没有 idk_ 前缀
        assert_eq!(admin_token.len(), 64);
        for bad in [
            admin_token.as_str(),
            "",
            "idk_",
            &secret('a')[..40],
            &format!("{}0", secret('a')),
            &secret('A'),
            &secret('g'),
            "sk-0123456789abcdef",
            " idk_aaaa",
            &format!("{} ", secret('a')),
            &format!("{}\n", secret('a')),
        ] {
            assert_eq!(
                CloudCredential::parse(bad).unwrap_err(),
                CredentialError::Malformed,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_secret_never_enters_debug_display_or_errors() {
        let credential = CloudCredential::parse(&secret('c')).unwrap();
        let rendered = format!("{credential:?} {credential}");
        assert!(!rendered.contains(&"c".repeat(16)));
        assert!(!format!("{:?}", credential.authorization()).contains(&"c".repeat(16)));
        assert!(credential.authorization().is_sensitive());
        assert_eq!(
            credential.authorization().to_str().unwrap(),
            format!("Bearer {}", secret('c'))
        );
        for error in [
            CredentialError::NotPaired,
            CredentialError::Unreadable,
            CredentialError::Malformed,
            CredentialError::InsecureFile,
            CredentialError::OriginMismatch,
            CredentialError::LocalKeyRefused,
            CredentialError::Unsupported,
            CredentialError::Busy,
            CredentialError::AlreadyPaired,
        ] {
            let text = format!("{error} {error:?} {}", error.code());
            // 提示文案里可以写「idk_ 加 64 位十六进制」这种格式说明，但不能出现任何一段像密钥的十六进制
            assert!(
                !text
                    .as_bytes()
                    .windows(16)
                    .any(|w| w.iter().all(u8::is_ascii_hexdigit)),
                "{text}"
            );
        }
        let loaded = LoadedCredential {
            secret: credential,
            kind: CredentialKind::Node,
        };
        assert!(!format!("{loaded:?}").contains(&"c".repeat(16)));
    }

    #[cfg(unix)]
    #[test]
    fn the_device_file_round_trips_with_mode_600_and_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path().join("cloud-tier"));
        assert!(store.read().unwrap().is_none(), "no file = not paired");
        store.preflight().unwrap();
        store.install(&record("https://idoris.ai", 'a')).unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "a directory we create is private");
        let read = store.read().unwrap().unwrap();
        assert_eq!(read.secret.expose_secret(), secret('a'));
        assert_eq!(read.origin, "https://idoris.ai");
        assert_eq!(read.membership, "m-agent-1");
        // 没有留下临时文件、预检文件
        let names: Vec<String> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, [DEVICE_FILE]);
        // 安装不覆盖（审查 r4 Medium#4：以前这里是 rename，会覆盖已有的凭证）：已经有一张就拒绝，已有的原样不动，
        // 权限仍然是 0600，也没有留下临时文件
        assert_eq!(
            store
                .install(&record("https://idoris.ai", 'b'))
                .unwrap_err(),
            CredentialError::AlreadyPaired
        );
        assert_eq!(
            store.read().unwrap().unwrap().secret.expose_secret(),
            secret('a')
        );
        assert_eq!(
            std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let names: Vec<String> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, [DEVICE_FILE]);
        // 删掉之后可以重新安装
        assert!(store.remove().unwrap());
        assert!(!store.remove().unwrap(), "already gone is not an error");
        assert!(store.read().unwrap().is_none());
        store.install(&record("https://idoris.ai", 'b')).unwrap();
        assert_eq!(
            store.read().unwrap().unwrap().secret.expose_secret(),
            secret('b')
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_other_users_can_read_or_a_symlink_is_refused_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path());
        store.install(&record("https://idoris.ai", 'a')).unwrap();
        for mode in [0o640, 0o604, 0o644, 0o660, 0o666, 0o700 | 0o077] {
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                store.read().unwrap_err(),
                CredentialError::InsecureFile,
                "{mode:o}"
            );
        }
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(store.read().unwrap().is_some(), "owner-only is fine");
        // 符号链接：即使指向一个权限正确的真实文件，也不用
        let real = dir.path().join("elsewhere.json");
        std::fs::rename(store.path(), &real).unwrap();
        std::os::unix::fs::symlink(&real, store.path()).unwrap();
        assert_eq!(store.read().unwrap_err(), CredentialError::InsecureFile);
    }

    #[cfg(unix)]
    #[test]
    fn a_corrupt_or_extended_device_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path());
        store.install(&record("https://idoris.ai", 'a')).unwrap();
        let good = std::fs::read_to_string(store.path()).unwrap();
        let tamper = |edit: &dyn Fn(&str) -> String| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(store.path(), edit(&good)).unwrap();
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
            store.read().unwrap_err()
        };
        assert_eq!(
            tamper(&|text| text.replace(&secret('a'), "idk_short")),
            CredentialError::Malformed
        );
        assert_eq!(
            tamper(&|text| text.replace("\"version\": 1", "\"version\": 2")),
            CredentialError::Malformed
        );
        assert_eq!(
            tamper(&|text| text.replace("m-agent-1", "has space")),
            CredentialError::Malformed
        );
        assert_eq!(
            tamper(&|text| text.replace('}', ", \"extra\": 1}")),
            CredentialError::Malformed
        );
        assert_eq!(
            tamper(&|_| "not json".to_string()),
            CredentialError::Malformed
        );
        assert_eq!(tamper(&|_| String::new()), CredentialError::Malformed);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_neither_a_temp_file_nor_a_half_written_credential() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path().join("ro"));
        store.install(&record("https://idoris.ai", 'a')).unwrap();
        // 目录变成只读：写失败，旧凭证原样还在，没有 .tmp 残留
        std::fs::set_permissions(store.dir(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = store.install(&record("https://idoris.ai", 'b'));
        // 以 root 运行时只读目录不拦人，这个用例没有意义（要在恢复权限之前探测）
        let blocks_writes = std::fs::File::create(store.dir().join(".probe")).is_err();
        std::fs::set_permissions(store.dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if !blocks_writes {
            let _ = std::fs::remove_file(store.dir().join(".probe"));
            return;
        }
        assert!(outcome.is_err());
        assert_eq!(
            store.read().unwrap().unwrap().secret.expose_secret(),
            secret('a')
        );
        let names: Vec<String> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, [DEVICE_FILE]);
    }

    /// 安装是「新装」：同时有很多个进程安装，恰好一个成功，其它的都是 `AlreadyPaired`，文件里是赢家的内容，
    /// 谁都没有被半截覆盖，也没有留下临时文件。
    #[cfg(unix)]
    #[test]
    fn many_concurrent_installs_have_exactly_one_winner_and_nobody_overwrites_anybody() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DeviceStore::new(dir.path().join("cloud-tier")));
        let seeds: Vec<char> = vec!['a', 'b', 'c', 'd', 'e', 'f', '0', '1']; // 十六进制字符：密钥只认小写十六进制
        let start = Arc::new(std::sync::Barrier::new(seeds.len()));
        let handles: Vec<_> = seeds
            .iter()
            .map(|&seed| {
                let store = store.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    (seed, store.install(&record("https://idoris.ai", seed)))
                })
            })
            .collect();
        let results: Vec<(char, Result<(), CredentialError>)> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<char> = results
            .iter()
            .filter(|(_, outcome)| outcome.is_ok())
            .map(|(seed, _)| *seed)
            .collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        for (seed, outcome) in &results {
            if *seed != winners[0] {
                assert_eq!(
                    outcome.as_ref().unwrap_err(),
                    &CredentialError::AlreadyPaired,
                    "{seed}"
                );
            }
        }
        assert_eq!(
            store.read().unwrap().unwrap().secret.expose_secret(),
            secret(winners[0])
        );
        let names: Vec<String> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, [DEVICE_FILE], "no temp file may be left behind");
    }

    /// 目标处是个悬空的符号链接：安装不跟随它，也不在链接指向的地方创建文件。
    #[cfg(unix)]
    #[test]
    fn an_install_never_follows_a_symlink_that_sits_at_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path());
        let elsewhere = dir.path().join("elsewhere.json");
        std::os::unix::fs::symlink(&elsewhere, store.path()).unwrap();
        assert_eq!(
            store
                .install(&record("https://idoris.ai", 'a'))
                .unwrap_err(),
            CredentialError::AlreadyPaired
        );
        assert!(!elsewhere.exists(), "the link target must not be created");
        assert_eq!(store.read().unwrap_err(), CredentialError::InsecureFile);
    }

    /// 文件系统不支持硬链接时的退路：`O_EXCL` 直接创建，同样不覆盖，权限 0600。
    #[cfg(unix)]
    #[test]
    fn the_exclusive_create_fallback_does_not_overwrite_either() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("device.json");
        create_exclusive(&target, b"first").unwrap();
        assert_eq!(
            create_exclusive(&target, b"second").unwrap_err(),
            CredentialError::AlreadyPaired
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// 状态目录的跨进程锁：立即返回，不等；丢掉就释放；没有目录就没有锁（解绑不为了拿锁去建目录）。
    #[cfg(unix)]
    #[test]
    fn the_state_lock_excludes_a_second_holder_and_is_released_when_dropped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path().join("cloud-tier"));
        // 目录不存在：不建（解绑），也就没有锁可拿
        assert!(store.try_lock(false).unwrap().is_none());
        assert!(!store.dir().exists());
        // 配对：建目录（0700）拿锁
        let first = store.try_lock(true).unwrap().expect("a lock");
        assert_eq!(
            std::fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(store.dir().join(LOCK_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // 第二个持有者（另一个 DeviceStore、另一个文件描述符，和另一个进程里的情形一样）：立即 Busy，pair / unpair 都一样
        let other = DeviceStore::new(store.dir());
        let started = std::time::Instant::now();
        assert_eq!(other.try_lock(true).unwrap_err(), CredentialError::Busy);
        assert_eq!(other.try_lock(false).unwrap_err(), CredentialError::Busy);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "it must not wait"
        );
        // 释放之后别人能拿；锁文件留在目录里
        drop(first);
        let second = other
            .try_lock(false)
            .unwrap()
            .expect("the lock is free again");
        assert_eq!(store.try_lock(true).unwrap_err(), CredentialError::Busy);
        drop(second);
        assert!(store.dir().join(LOCK_FILE).exists());
        assert!(store.try_lock(true).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn preflight_fails_when_the_state_directory_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let store = DeviceStore::new(&locked);
        let outcome = store.preflight();
        let blocks_writes = std::fs::File::create(locked.join(".probe")).is_err();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        if !blocks_writes {
            return; // root：只读目录不拦人
        }
        assert_eq!(outcome.unwrap_err(), CredentialError::Unreadable);
    }

    #[cfg(unix)]
    #[test]
    fn the_paired_provider_refuses_a_different_origin_and_a_local_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path());
        let none = PairedDeviceProvider::new(store.clone(), "https://idoris.ai", None);
        assert_eq!(none.load().unwrap_err(), CredentialError::NotPaired);

        store.install(&record("https://idoris.ai", 'a')).unwrap();
        let ok = PairedDeviceProvider::new(store.clone(), "https://idoris.ai", None);
        let loaded = ok.load().unwrap();
        assert_eq!(loaded.secret.expose_secret(), secret('a'));
        assert_eq!(
            loaded.kind,
            CredentialKind::Device {
                credential_id: "vk_0123456789abcdef0123456789abcdef".into(),
                membership: "m-agent-1".into()
            }
        );
        // 配置被改成指向别处：这张凭证不会被带过去
        for other in [
            "https://evil.example",
            "http://idoris.ai",
            "https://idoris.ai:8443",
        ] {
            assert_eq!(
                PairedDeviceProvider::new(store.clone(), other, None)
                    .load()
                    .unwrap_err(),
                CredentialError::OriginMismatch,
                "{other}"
            );
        }
        // 本机的本地数据密钥写进了设备凭证文件：拒绝
        let probe: Arc<dyn LocalSecretProbe> = Arc::new(Probe(secret('a')));
        assert_eq!(
            PairedDeviceProvider::new(store, "https://idoris.ai", Some(probe))
                .load()
                .unwrap_err(),
            CredentialError::LocalKeyRefused
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_node_file_is_reread_every_time_and_the_same_safety_rules_apply() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");
        let provider = NodeFileProvider::new(&path, None);
        assert_eq!(provider.load().unwrap_err(), CredentialError::Unreadable);
        std::fs::write(&path, format!("{}\n", secret('a'))).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(provider.load().unwrap_err(), CredentialError::InsecureFile);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(provider.load().unwrap().secret.expose_secret(), secret('a'));
        assert_eq!(provider.load().unwrap().kind, CredentialKind::Node);
        // 轮换：换了文件内容，下一次调用就用新的
        std::fs::write(&path, format!("  {}  \r\n", secret('b'))).unwrap();
        assert_eq!(provider.load().unwrap().secret.expose_secret(), secret('b'));
        // Admin token 写进了 Node 凭证文件：格式就不对
        std::fs::write(&path, format!("{}\n", "ab".repeat(32))).unwrap();
        assert_eq!(provider.load().unwrap_err(), CredentialError::Malformed);
        // 本机的本地数据密钥：拒绝
        std::fs::write(&path, secret('a')).unwrap();
        let probe: Arc<dyn LocalSecretProbe> = Arc::new(Probe(secret('a')));
        assert_eq!(
            NodeFileProvider::new(&path, Some(probe))
                .load()
                .unwrap_err(),
            CredentialError::LocalKeyRefused
        );
    }

    #[test]
    fn a_node_env_credential_follows_the_same_rules() {
        let ok = NodeEnvProvider::new(Some(format!(" {} ", secret('d'))), None);
        assert_eq!(ok.load().unwrap().secret.expose_secret(), secret('d'));
        assert_eq!(
            NodeEnvProvider::new(None, None).load().unwrap_err(),
            CredentialError::Unreadable
        );
        assert_eq!(
            NodeEnvProvider::new(Some("ab".repeat(32)), None)
                .load()
                .unwrap_err(),
            CredentialError::Malformed
        );
        let probe: Arc<dyn LocalSecretProbe> = Arc::new(Probe(secret('d')));
        assert_eq!(
            NodeEnvProvider::new(Some(secret('d')), Some(probe))
                .load()
                .unwrap_err(),
            CredentialError::LocalKeyRefused
        );
    }

    #[test]
    fn the_summary_shows_only_a_prefix_of_the_credential_id_and_never_the_secret() {
        let summary = record("https://idoris.ai", 'a').summary();
        assert_eq!(summary.credential_id_prefix, "vk_01234567");
        assert_eq!(
            summary.credential_id_prefix.len(),
            CREDENTIAL_ID_PREFIX_CHARS
        );
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("idk_") && !json.contains("secret"));
        assert!(!json.contains("0123456789abcdef0123456789abcdef"));
    }

    /// 在另一个线程里跑 `f`，5 秒没返回就判失败。回归（去掉 `O_NONBLOCK`）时 `open` 会永远卡在没有写端的 FIFO 上，
    /// 不能让它把整个测试进程挂住：先给 FIFO 一个写端放开卡住的线程，再报告失败。
    #[cfg(unix)]
    fn finishes_within_5s<T: Send + 'static>(
        what: &str,
        fifo: &Path,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(f());
        });
        match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(value) => value,
            Err(_) => {
                let _writer = std::fs::OpenOptions::new().write(true).open(fifo);
                panic!("{what} hung on a FIFO");
            }
        }
    }

    #[cfg(unix)]
    fn make_fifo(path: &Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo failed");
    }

    /// 审查 r4 Medium#6：以前先 `lstat` 检查、再 `File::open`。检查之后、打开之前，目录项被另一个进程换成一个没有写端的 FIFO，
    /// 阻塞的 `open` 永远不返回，后面的 `fstat` 也没机会做（读取线程被无限挂住；凭证加载发生在并发槽位之前，也不受云请求总超时限制）。
    /// 现在用 `O_NONBLOCK` 打开（FIFO 的 open 立刻返回），再用描述符验证它是不是普通文件。
    #[cfg(unix)]
    #[test]
    fn a_path_swapped_for_a_fifo_right_before_the_open_is_refused_and_never_hangs_the_reader() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");
        std::fs::write(&path, secret('a')).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let reader_path = path.clone();
        let result =
            finishes_within_5s("reading the credential after the swap", &path, move || {
                let swap_path = reader_path.clone();
                read_private_regular_file_with(&reader_path, &mut || {
                    // 另一个进程在这个窗口里把文件换成 FIFO（没有任何写端）
                    std::fs::remove_file(&swap_path).unwrap();
                    make_fifo(&swap_path);
                })
            });
        assert_eq!(result.unwrap_err(), CredentialError::InsecureFile);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_swapped_for_a_symlink_right_before_the_open_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");
        let elsewhere = dir.path().join("elsewhere.key");
        for file in [&path, &elsewhere] {
            std::fs::write(file, secret('a')).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let result = read_private_regular_file_with(&path, &mut || {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        });
        assert_eq!(result.unwrap_err(), CredentialError::InsecureFile);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_or_a_directory_at_the_path_is_refused_at_once_and_a_symlinked_parent_directory_is_fine()
     {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // 一直就是 FIFO（没有写端）：`O_NONBLOCK` 的 open 立刻返回，`fstat` 说它不是普通文件
        let fifo = dir.path().join("fifo.key");
        make_fifo(&fifo);
        let loader = NodeFileProvider::new(&fifo, None);
        let error = finishes_within_5s("loading a FIFO", &fifo, move || loader.load().unwrap_err());
        assert_eq!(error, CredentialError::InsecureFile);
        // 目录
        assert_eq!(
            NodeFileProvider::new(dir.path(), None).load().unwrap_err(),
            CredentialError::InsecureFile
        );
        // `O_NOFOLLOW` 只管最后一级：路径中间有符号链接（比如 macOS 的 /var -> /private/var）照常能读
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let file = real.join("node.key");
        std::fs::write(&file, secret('c')).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let linked_dir = dir.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked_dir).unwrap();
        assert_eq!(
            NodeFileProvider::new(linked_dir.join("node.key"), None)
                .load()
                .unwrap()
                .secret
                .expose_secret(),
            secret('c')
        );
        // 检查本身：描述符的元数据是普通文件且没有 group / other 位才放行
        let metadata = std::fs::metadata(&file).unwrap();
        assert!(check_opened_credential_file(&metadata).is_ok());
        assert_eq!(
            check_opened_credential_file(&std::fs::metadata(dir.path()).unwrap()).unwrap_err(),
            CredentialError::InsecureFile
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_credential_file_is_read_through_one_inode_and_is_size_capped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.key");
        std::fs::write(&path, format!("{}\n{}", secret('a'), "x".repeat(20 * 1024))).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // 一个凭证文件只有几百个字节；更大的东西不是凭证文件，不读
        assert_eq!(
            NodeFileProvider::new(&path, None).load().unwrap_err(),
            CredentialError::Malformed
        );
        // 不存在是 None（设备凭证用它表示「还没配对」），Node 凭证文件不存在是 Unreadable
        let missing = dir.path().join("missing.key");
        assert_eq!(read_private_regular_file(&missing).unwrap(), None);
        assert_eq!(
            NodeFileProvider::new(&missing, None).load().unwrap_err(),
            CredentialError::Unreadable
        );
        // 目录、符号链接都不是「普通文件」
        assert_eq!(
            read_private_regular_file(dir.path()).unwrap_err(),
            CredentialError::InsecureFile
        );
        let target = dir.path().join("target.key");
        std::fs::write(&target, secret('b')).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link.key");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            NodeFileProvider::new(&link, None).load().unwrap_err(),
            CredentialError::InsecureFile
        );
        assert_eq!(
            NodeFileProvider::new(&target, None)
                .load()
                .unwrap()
                .secret
                .expose_secret(),
            secret('b')
        );
    }
}

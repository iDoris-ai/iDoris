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
//! 目录 0700（我们创建时）、文件 **0600**，原子写（同目录里 `O_EXCL|0600` 的临时文件 → `fsync` → `rename` → `fsync` 目录），
//! 所以任何时刻磁盘上看到的要么没有、要么是完整的、权限正确的文件。读取时**再检查一次**：必须是普通文件（不是符号链接）、
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
        use std::os::unix::fs::PermissionsExt;

        let path = self.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(CredentialError::Unreadable),
        };
        if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(CredentialError::InsecureFile);
        }
        let text = std::fs::read_to_string(&path).map_err(|_| CredentialError::Unreadable)?;
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

    /// 原子地写设备凭证：同目录里 `O_EXCL|0600` 的临时文件 → `fsync` → `rename` → `fsync` 目录。
    #[cfg(unix)]
    pub fn write(&self, record: &DeviceRecord) -> Result<(), CredentialError> {
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
        let bytes = serde_json::to_vec_pretty(&file).map_err(|_| CredentialError::Unreadable)?;
        let tmp = dir.join(format!(
            ".device.json.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| -> std::io::Result<()> {
            let mut out = open_private_new(&tmp)?;
            out.write_all(&bytes)?;
            out.write_all(b"\n")?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&tmp, self.path())?;
            // 目录项也落盘；失败不致命（文件已经完整地在位）
            if let Ok(handle) = std::fs::File::open(&dir) {
                let _ = handle.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(|_| CredentialError::Unreadable)
    }

    #[cfg(not(unix))]
    pub fn write(&self, _record: &DeviceRecord) -> Result<(), CredentialError> {
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
        use std::os::unix::fs::PermissionsExt;

        let metadata =
            std::fs::symlink_metadata(&self.path).map_err(|_| CredentialError::Unreadable)?;
        if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(CredentialError::InsecureFile);
        }
        let text = std::fs::read_to_string(&self.path).map_err(|_| CredentialError::Unreadable)?;
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
#[derive(Debug)]
pub struct NodeEnvProvider {
    secret: Option<String>,
    probe: Option<Arc<dyn LocalSecretProbe>>,
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
        store.write(&record("https://idoris.ai", 'a')).unwrap();
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
        // 覆盖写：新内容原子替换，权限仍然是 0600
        store.write(&record("https://idoris.ai", 'b')).unwrap();
        assert_eq!(
            store.read().unwrap().unwrap().secret.expose_secret(),
            secret('b')
        );
        assert_eq!(
            std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(store.remove().unwrap());
        assert!(!store.remove().unwrap(), "already gone is not an error");
        assert!(store.read().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_other_users_can_read_or_a_symlink_is_refused_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(dir.path());
        store.write(&record("https://idoris.ai", 'a')).unwrap();
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
        store.write(&record("https://idoris.ai", 'a')).unwrap();
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
        store.write(&record("https://idoris.ai", 'a')).unwrap();
        // 目录变成只读：写失败，旧凭证原样还在，没有 .tmp 残留
        std::fs::set_permissions(store.dir(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = store.write(&record("https://idoris.ai", 'b'));
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

        store.write(&record("https://idoris.ai", 'a')).unwrap();
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
}

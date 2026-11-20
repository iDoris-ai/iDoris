//! 发现文件（接口规范 §2.7.3，修订 1.6）：iDoris 就绪后写给 launcher 的一份实例快照。
//!
//! 它**只是提示，不是事实来源**：同一个数据库只能有一个实例，由实例锁保证（退出码 3）。拿到锁的新实例把
//! 已有文件视为陈旧并覆盖（前提是它是属主正确的普通文件；已有目标是符号链接、目录、FIFO、socket 或别人属主的
//! 文件时，启动失败、不删除）；没拿到锁（退出码 3）的实例**不碰**文件，因为它属于还活着的那个实例。
//!
//! # 目录操作全部基于已打开的 parent fd（PR-Daemon 对设计稿 D3 的评审补充）
//!
//! 校验 parent（存在、目录、属主为当前有效 uid、组与其他人不可写、中间没有符号链接）之后，以
//! `O_DIRECTORY|O_NOFOLLOW` 打开 parent；**此后创建临时文件（0600、`O_EXCL`）、写入、`fsync`、
//! `renameat` 到最终名、`fsync` parent、读与删除旁路锁与发现文件，全部相对这个已打开的 fd**，不再按路径
//! 操作。于是校验之后有人把路径换成符号链接（或把目录改名后再放一个链接），写入仍落在校验过的那个目录里。
//! workspace `unsafe_code = "forbid"`：用 router 已有的 `nix`（加 `fs`、`user` 两个 feature，没有新 crate）
//! 的安全封装做 `openat`/`renameat`/`fstatat`/`unlinkat`/`fsync`。
//!
//! # 旁路锁
//!
//! `<发现文件>.lock`（同目录，0600）。iDoris 在**删除**时（读、比对 `instance_id`、unlink 都在独占 flock 下）
//! 持有它，消费者清理陈旧文件时也持有同一把锁（规范 §2.7.3、R12 已确认项 4）。iDoris 在**发布**的最后一步
//! `renameat` 时同样持有它：否则消费者「持锁、比对出旧 `instance_id`、unlink」与新实例的 rename 交错，会把新文件
//! 删掉。规范没有明说发布也要持锁（规范没写清，这里按设计意图保守地多持一把，不改变文件内容与语义）。
//! 锁文件本身从不删除（删除锁文件会引入经典的「锁文件被 unlink 后各持一把」竞态）。
//!
//! # 不用 `F_FULLFSYNC`
//!
//! 用 `fsync(2)`，不用 std 的 `File::sync_all`（macOS 上它是 `F_FULLFSYNC`）：重启后 pid 本来就没了，
//! 不需要掉电后文件仍在。

use std::path::PathBuf;

use serde::Serialize;

use crate::instance_nonce::InstanceNonce;

/// 发现文件的 `schema` 字段。
pub const SCHEMA: u32 = 1;
/// 发现文件的 `service` 字段。
pub const SERVICE: &str = "idoris";
/// 发现文件内容上限（含结尾 LF）。
pub const MAX_BYTES: usize = 4096;
/// 路径上限（字节），同 launcher 帧的 `discovery_path`。
pub const MAX_PATH_BYTES: usize = 1024;

/// 发现文件不可用。**只有稳定的原因码，没有路径、没有内容**：不会带出 nonce，也不会把路径变成日志注入的入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryError {
    /// 不是绝对路径、过长、含 NUL、或没有可用的文件名。
    PathInvalid,
    /// `parent` 不存在。
    ParentMissing,
    /// `canonicalize(parent)` 与 `parent` 不逐字节相等（中间有符号链接、`.`/`..`、重复斜杠）。
    ParentNotCanonical,
    /// `parent` 本身是符号链接（以 `O_NOFOLLOW` 打开被拒）。
    ParentSymlink,
    ParentNotDirectory,
    /// `parent` 的属主不是当前有效 uid。
    ParentWrongOwner,
    /// `parent` 对组或其他人可写（`mode & 0o022 != 0`）。
    ParentWritableByOthers,
    /// 其他无法打开 / 检查 `parent` 的原因。
    ParentUnusable,
    /// 目标已存在，但不是普通文件（符号链接、目录、FIFO、socket）。
    TargetNotRegular,
    /// 目标已存在，但属主不是当前有效 uid。
    TargetWrongOwner,
    TargetUnusable,
    /// 序列化后超过 [`MAX_BYTES`]。
    RecordTooLarge,
    SerializeFailed,
    TempCreateFailed,
    WriteFailed,
    SyncFailed,
    RenameFailed,
    /// 旁路锁不可用（打不开、属主不对、或等锁超时）。
    LockUnavailable,
    /// 本平台没有 fd 相对的目录操作。
    Unsupported,
}

impl DiscoveryError {
    pub fn code(self) -> &'static str {
        match self {
            Self::PathInvalid => "path_invalid",
            Self::ParentMissing => "parent_missing",
            Self::ParentNotCanonical => "parent_not_canonical",
            Self::ParentSymlink => "parent_symlink",
            Self::ParentNotDirectory => "parent_not_directory",
            Self::ParentWrongOwner => "parent_wrong_owner",
            Self::ParentWritableByOthers => "parent_writable_by_others",
            Self::ParentUnusable => "parent_unusable",
            Self::TargetNotRegular => "target_not_regular",
            Self::TargetWrongOwner => "target_wrong_owner",
            Self::TargetUnusable => "target_unusable",
            Self::RecordTooLarge => "record_too_large",
            Self::SerializeFailed => "serialize_failed",
            Self::TempCreateFailed => "temp_create_failed",
            Self::WriteFailed => "write_failed",
            Self::SyncFailed => "sync_failed",
            Self::RenameFailed => "rename_failed",
            Self::LockUnavailable => "lock_unavailable",
            Self::Unsupported => "unsupported_platform",
        }
    }
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "发现文件不可用（{}）", self.code())
    }
}

impl std::error::Error for DiscoveryError {}

/// [`DiscoveryTarget::remove_if_owner`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// 文件里的 `instance_id` 等于自己，已删除。
    Removed,
    /// 文件已经不在了。
    Absent,
    /// 文件存在但不属于自己（`instance_id` 不同、不是普通文件、属主不对、内容读不懂）：原样保留。
    NotOwned,
    /// 旁路锁不可用：打不开、不是普通文件、属主不对，或者等锁超时。没有删，文件保持原样
    /// （成为「崩溃残留」，下一个实例会覆盖它）。
    LockUnavailable,
    Failed,
}

/// `launch.engines[]` 的元素：启动时从组件卡读到的引擎型上游的静态清单（不含状态、不含默认引擎）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EngineEntry {
    pub id: String,
    pub engine_kind: String,
    pub managed: bool,
}

/// `launch`：启动输入快照，供 agent24d 判断「这个孤儿的启动输入还是不是我要的」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaunchSnapshot {
    /// canonical 的锁基路径（与实例锁用的是同一个）。
    pub db_path: String,
    /// `IDORIS_STATE_DIR`（设置了就只看它，相对路径即「没有」）；没设置时 `$HOME/.local/state/idoris`；
    /// 得不到绝对路径时为 null。
    pub state_dir: Option<String>,
    pub components_dir: String,
    pub allocated_budget_bytes: Option<u64>,
    pub engines: Vec<EngineEntry>,
}

/// 发现文件的内容。`Debug` 不会渲染 nonce（[`InstanceNonce`] 自己脱敏）；序列化只在
/// [`DiscoveryRecord::to_line`] 里发生，nonce 以 `&str` 借用进一个私有的线格式结构。
#[derive(Debug, Clone)]
pub struct DiscoveryRecord {
    pub instance_id: String,
    pub instance_nonce: InstanceNonce,
    pub pid: u32,
    pub data_port: u16,
    pub admin_port: u16,
    pub version: String,
    pub contract_version: String,
    pub started_at_ms: u64,
    pub launch: Option<LaunchSnapshot>,
}

#[derive(Serialize)]
struct WireRecord<'a> {
    schema: u32,
    service: &'static str,
    instance_id: &'a str,
    instance_nonce: &'a str,
    pid: u32,
    data_port: u16,
    admin_port: u16,
    version: &'a str,
    contract_version: &'a str,
    started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    launch: Option<&'a LaunchSnapshot>,
}

impl DiscoveryRecord {
    /// 紧凑的一行 JSON 加 LF，≤ [`MAX_BYTES`]。
    pub fn to_line(&self) -> Result<Vec<u8>, DiscoveryError> {
        let wire = WireRecord {
            schema: SCHEMA,
            service: SERVICE,
            instance_id: &self.instance_id,
            instance_nonce: self.instance_nonce.expose_hex(),
            pid: self.pid,
            data_port: self.data_port,
            admin_port: self.admin_port,
            version: &self.version,
            contract_version: &self.contract_version,
            started_at_ms: self.started_at_ms,
            launch: self.launch.as_ref(),
        };
        let mut line = serde_json::to_vec(&wire).map_err(|_| DiscoveryError::SerializeFailed)?;
        line.push(b'\n');
        if line.len() > MAX_BYTES {
            return Err(DiscoveryError::RecordTooLarge);
        }
        Ok(line)
    }
}

/// `IDORIS_STATE_DIR`，缺省 `$HOME/.local/state/idoris`；必须是绝对路径（与加载围栏
/// `OmlxAdapter::load_fence_path` 的解析一致），否则没有。
pub fn resolve_state_dir() -> Option<PathBuf> {
    state_dir_from(
        std::env::var_os("IDORIS_STATE_DIR"),
        std::env::var_os("HOME"),
    )
}

/// [`resolve_state_dir`] 的纯函数形式：`IDORIS_STATE_DIR` 一旦设置就只看它（相对路径即「没有」，不退到 HOME）。
fn state_dir_from(
    state_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    state_dir
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/state/idoris")))
        .filter(|path| path.is_absolute())
}

#[cfg(unix)]
pub use unix_impl::DiscoveryTarget;

#[cfg(not(unix))]
pub use fallback_impl::DiscoveryTarget;

#[cfg(not(unix))]
mod fallback_impl {
    use super::{DiscoveryError, DiscoveryRecord, RemoveOutcome};
    use std::path::Path;

    /// 非 unix 平台没有 fd 相对的目录操作：launcher 要求发现文件时直接启动失败，不退化成按路径写。
    #[derive(Debug)]
    pub struct DiscoveryTarget;

    impl DiscoveryTarget {
        pub fn open(_path: &Path) -> Result<Self, DiscoveryError> {
            Err(DiscoveryError::Unsupported)
        }

        pub fn publish(&self, _record: &DiscoveryRecord) -> Result<(), DiscoveryError> {
            Err(DiscoveryError::Unsupported)
        }

        pub fn remove_if_owner(&self, _instance_id: &str) -> RemoveOutcome {
            RemoveOutcome::Failed
        }
    }
}

#[cfg(unix)]
mod unix_impl {
    use std::ffi::{OsStr, OsString};
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use nix::fcntl::{self, AtFlags, OFlag};
    use nix::libc;
    use nix::sys::stat::{self, Mode};
    use nix::unistd::{self, UnlinkatFlags};
    use ring::rand::{SecureRandom, SystemRandom};

    use super::{DiscoveryError, DiscoveryRecord, MAX_BYTES, MAX_PATH_BYTES, RemoveOutcome};
    use crate::instance_nonce::hex_lower;

    /// 文件名上限（`NAME_MAX`）。
    const MAX_NAME_BYTES: usize = 255;
    /// 发布（`renameat`）等旁路锁的上限：消费者持锁只做读、比对、unlink，正常是毫秒级。
    const PUBLISH_LOCK_WAIT: Duration = Duration::from_secs(5);
    /// 退出时删除文件等旁路锁的上限：停机预算很紧（launcher 17 秒后 SIGKILL），宁可留下残留文件。
    const REMOVE_LOCK_WAIT: Duration = Duration::from_secs(2);
    const LOCK_POLL: Duration = Duration::from_millis(10);
    const TEMP_ATTEMPTS: usize = 8;

    /// 校验过、并已经以 `O_DIRECTORY|O_NOFOLLOW` 打开了 parent 的发现文件位置。之后的所有操作都相对这个 fd。
    #[derive(Debug)]
    pub struct DiscoveryTarget {
        parent: OwnedFd,
        file_name: OsString,
        lock_name: OsString,
    }

    /// `(parent 路径字节, 文件名字节)`；只做语法检查，不碰文件系统。
    fn split_path(path: &Path) -> Result<(&Path, &OsStr), DiscoveryError> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.first() != Some(&b'/') || bytes.len() > MAX_PATH_BYTES || bytes.contains(&0) {
            return Err(DiscoveryError::PathInvalid);
        }
        let slash = bytes
            .iter()
            .rposition(|byte| *byte == b'/')
            .ok_or(DiscoveryError::PathInvalid)?;
        let name = &bytes[slash + 1..];
        if name.is_empty() || name == b"." || name == b".." || name.len() > MAX_NAME_BYTES {
            return Err(DiscoveryError::PathInvalid);
        }
        // `/idoris.json` 的 parent 是 `/`；`/a//b.json` 的 parent 是 `/a/`（带尾斜杠，下一步会因不 canonical 被拒）。
        let parent = if slash == 0 {
            &bytes[..1]
        } else {
            &bytes[..slash]
        };
        Ok((
            Path::new(OsStr::from_bytes(parent)),
            OsStr::from_bytes(name),
        ))
    }

    fn file_type(mode: libc::mode_t) -> libc::mode_t {
        mode & libc::S_IFMT
    }

    fn effective_uid() -> u32 {
        unistd::geteuid().as_raw()
    }

    /// `fstat(parent)` 之后对 parent 的三项核对，顺序固定：是目录、属主是当前有效 uid、组与其他人不可写。
    /// 拆成只吃事实值的纯函数，是因为第一项在真实文件系统上走不到（`O_DIRECTORY` 已经先拒绝了非目录），
    /// 第二项需要别的用户的目录（以 root 运行时没有这个前提）：单元测试直接喂事实值，逐项钉住。
    pub(super) fn check_parent_stat(
        mode: libc::mode_t,
        uid: u32,
        euid: u32,
    ) -> Result<(), DiscoveryError> {
        if file_type(mode) != libc::S_IFDIR {
            return Err(DiscoveryError::ParentNotDirectory);
        }
        if uid != euid {
            return Err(DiscoveryError::ParentWrongOwner);
        }
        if mode & 0o022 != 0 {
            return Err(DiscoveryError::ParentWritableByOthers);
        }
        Ok(())
    }

    /// 旁路锁文件打开之后的核对：必须是属主为当前有效 uid 的普通文件。同样拆成纯函数：属主不对需要 root 才能造出来，
    /// 类型不对（FIFO）可以在文件系统上造，两种都由单元测试逐项钉住。
    pub(super) fn sidecar_lock_is_ours(mode: libc::mode_t, uid: u32, euid: u32) -> bool {
        file_type(mode) == libc::S_IFREG && uid == euid
    }

    fn lock_file_name(file_name: &OsStr) -> OsString {
        let mut name = file_name.to_os_string().into_vec();
        name.extend_from_slice(b".lock");
        OsString::from_vec(name)
    }

    impl DiscoveryTarget {
        /// 规范 §2.7.3 的路径校验（1）到（5）：都在写任何东西之前，不创建任何文件或目录。
        pub fn open(path: &Path) -> Result<Self, DiscoveryError> {
            Self::open_with_hook(path, || {})
        }

        /// `before_open` 在 canonical 校验之后、打开 parent 之前调用：只给测试用来模拟这一段里的路径替换。
        pub(crate) fn open_with_hook(
            path: &Path,
            before_open: impl FnOnce(),
        ) -> Result<Self, DiscoveryError> {
            let (parent_path, file_name) = split_path(path)?;
            // （2）`canonicalize(parent)` 与 `parent` 逐字节相等：中间不允许有符号链接。
            let canonical = std::fs::canonicalize(parent_path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    DiscoveryError::ParentMissing
                } else {
                    DiscoveryError::ParentUnusable
                }
            })?;
            if canonical.as_os_str().as_bytes() != parent_path.as_os_str().as_bytes() {
                return Err(DiscoveryError::ParentNotCanonical);
            }
            before_open();
            // （3）以不跟随符号链接的方式打开 parent，并在 fd 上核对属主与权限。
            let parent = fcntl::open(
                parent_path,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|errno| match errno {
                Errno::ENOENT => DiscoveryError::ParentMissing,
                // 不同系统对「O_NOFOLLOW|O_DIRECTORY 遇到符号链接」给 ELOOP 或 ENOTDIR，这里统一用 lstat 区分。
                Errno::ELOOP | Errno::ENOTDIR => {
                    if std::fs::symlink_metadata(parent_path)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink())
                    {
                        DiscoveryError::ParentSymlink
                    } else {
                        DiscoveryError::ParentNotDirectory
                    }
                }
                _ => DiscoveryError::ParentUnusable,
            })?;
            let parent_stat = stat::fstat(&parent).map_err(|_| DiscoveryError::ParentUnusable)?;
            check_parent_stat(parent_stat.st_mode, parent_stat.st_uid, effective_uid())?;
            let target = Self {
                parent,
                lock_name: lock_file_name(file_name),
                file_name: file_name.to_os_string(),
            };
            // （4）目标若已存在，必须是属主正确的普通文件，否则拒绝（不删除）。
            target.check_target()?;
            // （5）iDoris 不创建 parent：到这里 parent 已经存在，缺了就在上面报 `parent_missing`。
            Ok(target)
        }

        fn check_target(&self) -> Result<(), DiscoveryError> {
            match stat::fstatat(
                &self.parent,
                self.file_name.as_os_str(),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Err(Errno::ENOENT) => Ok(()),
                Err(_) => Err(DiscoveryError::TargetUnusable),
                Ok(target) if file_type(target.st_mode) != libc::S_IFREG => {
                    Err(DiscoveryError::TargetNotRegular)
                }
                Ok(target) if target.st_uid != effective_uid() => {
                    Err(DiscoveryError::TargetWrongOwner)
                }
                Ok(_) => Ok(()),
            }
        }

        /// 原子写：同目录不可预测名字的临时文件（`O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`，0600，`fchmod` 到 0600
        /// 以不受 umask 影响），写入并 `fsync`，持旁路锁 `renameat` 覆盖目标，再尽力 `fsync` 目录；失败路径删临时文件。
        /// 读者要么看到旧文件，要么看到完整的新文件。
        pub fn publish(&self, record: &DiscoveryRecord) -> Result<(), DiscoveryError> {
            self.publish_with_lock_wait(record, PUBLISH_LOCK_WAIT)
        }

        /// [`Self::publish`] 的等锁时限可调版本：生产只用 [`PUBLISH_LOCK_WAIT`]；测试用它把「等锁超时」压到毫秒级，
        /// 或者放宽到足以在很忙的机器上等完一次与消费者的竞争。
        pub(crate) fn publish_with_lock_wait(
            &self,
            record: &DiscoveryRecord,
            lock_wait: Duration,
        ) -> Result<(), DiscoveryError> {
            let line = record.to_line()?;
            let temp_name = self.write_temp_file(&line)?;
            let installed = self.install(&temp_name, lock_wait);
            if installed.is_err() {
                let _ =
                    unistd::unlinkat(&self.parent, temp_name.as_str(), UnlinkatFlags::NoRemoveDir);
                return installed;
            }
            // 尽力而为：不用 F_FULLFSYNC，失败也不算发布失败（重启后 pid 本来就没了）。
            let _ = unistd::fsync(&self.parent);
            Ok(())
        }

        /// 写好、`fsync` 过的临时文件的名字。任何一步失败都先删掉临时文件再返回错误。
        fn write_temp_file(&self, line: &[u8]) -> Result<String, DiscoveryError> {
            let random = SystemRandom::new();
            for _ in 0..TEMP_ATTEMPTS {
                let mut suffix = [0_u8; 8];
                random
                    .fill(&mut suffix)
                    .map_err(|_| DiscoveryError::TempCreateFailed)?;
                let temp_name = format!(
                    ".idoris-discovery.{}.{}.tmp",
                    std::process::id(),
                    hex_lower(&suffix)
                );
                let fd = match fcntl::openat(
                    &self.parent,
                    temp_name.as_str(),
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    Mode::from_bits_truncate(0o600),
                ) {
                    Ok(fd) => fd,
                    Err(Errno::EEXIST) => continue,
                    Err(_) => return Err(DiscoveryError::TempCreateFailed),
                };
                let written = (|| {
                    // 不受 umask 影响。
                    stat::fchmod(&fd, Mode::from_bits_truncate(0o600))
                        .map_err(|_| DiscoveryError::WriteFailed)?;
                    let mut file = std::fs::File::from(fd);
                    file.write_all(line)
                        .map_err(|_| DiscoveryError::WriteFailed)?;
                    unistd::fsync(&file).map_err(|_| DiscoveryError::SyncFailed)
                })();
                return match written {
                    Ok(()) => Ok(temp_name),
                    Err(error) => {
                        let _ = unistd::unlinkat(
                            &self.parent,
                            temp_name.as_str(),
                            UnlinkatFlags::NoRemoveDir,
                        );
                        Err(error)
                    }
                };
            }
            Err(DiscoveryError::TempCreateFailed)
        }

        /// 持旁路锁（至多等 `lock_wait`），复核目标仍然是自己该替换的东西，再 `renameat`。
        fn install(&self, temp_name: &str, lock_wait: Duration) -> Result<(), DiscoveryError> {
            let _lock = self.lock_sidecar(lock_wait)?;
            self.check_target()?;
            fcntl::renameat(
                &self.parent,
                temp_name,
                &self.parent,
                self.file_name.as_os_str(),
            )
            .map_err(|_| DiscoveryError::RenameFailed)
        }

        /// 旁路锁 `<发现文件>.lock` 上的独占 flock，最多等 `wait`。返回的 `File` 在 drop 时解锁。
        fn lock_sidecar(&self, wait: Duration) -> Result<std::fs::File, DiscoveryError> {
            let fd = fcntl::openat(
                &self.parent,
                self.lock_name.as_os_str(),
                OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(|_| DiscoveryError::LockUnavailable)?;
            let lock_stat = stat::fstat(&fd).map_err(|_| DiscoveryError::LockUnavailable)?;
            if !sidecar_lock_is_ours(lock_stat.st_mode, lock_stat.st_uid, effective_uid()) {
                return Err(DiscoveryError::LockUnavailable);
            }
            let file = std::fs::File::from(fd);
            let deadline = Instant::now() + wait;
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(file),
                    Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(LOCK_POLL);
                    }
                    Err(_) => return Err(DiscoveryError::LockUnavailable),
                }
            }
        }

        /// 优雅退出时调用：**仅当文件里的 `instance_id` 等于自己**才删；读、比对、unlink 都在旁路锁下完成。
        /// SIGKILL、panic、掉电不会走到这里（崩溃残留，由下一个拿到实例锁的实例覆盖）。
        pub fn remove_if_owner(&self, instance_id: &str) -> RemoveOutcome {
            let Ok(_lock) = self.lock_sidecar(REMOVE_LOCK_WAIT) else {
                return RemoveOutcome::LockUnavailable;
            };
            // `O_NONBLOCK`：目标若被换成 FIFO，打开也不会卡住（随后按「不是普通文件」处理）。
            let fd = match fcntl::openat(
                &self.parent,
                self.file_name.as_os_str(),
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(Errno::ENOENT) => return RemoveOutcome::Absent,
                Err(Errno::ELOOP) => return RemoveOutcome::NotOwned,
                Err(_) => return RemoveOutcome::Failed,
            };
            let Ok(file_stat) = stat::fstat(&fd) else {
                return RemoveOutcome::Failed;
            };
            if file_type(file_stat.st_mode) != libc::S_IFREG || file_stat.st_uid != effective_uid()
            {
                return RemoveOutcome::NotOwned;
            }
            let mut content = Vec::new();
            if std::fs::File::from(fd)
                .take(MAX_BYTES as u64 + 1)
                .read_to_end(&mut content)
                .is_err()
            {
                return RemoveOutcome::Failed;
            }
            let owned = content.len() <= MAX_BYTES
                && serde_json::from_slice::<serde_json::Value>(&content)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("instance_id")
                            .and_then(serde_json::Value::as_str)
                            .map(|found| found == instance_id)
                    })
                    .unwrap_or(false);
            if !owned {
                return RemoveOutcome::NotOwned;
            }
            match unistd::unlinkat(
                &self.parent,
                self.file_name.as_os_str(),
                UnlinkatFlags::NoRemoveDir,
            ) {
                Ok(()) => RemoveOutcome::Removed,
                Err(Errno::ENOENT) => RemoveOutcome::Absent,
                Err(_) => RemoveOutcome::Failed,
            }
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::unix_impl::{check_parent_stat, sidecar_lock_is_ours};
    use super::*;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
    use std::path::Path;
    use std::time::{Duration, Instant};

    const NONCE: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const INSTANCE_ID: &str = "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11";

    /// 规范 §2.7.3 示例的原文输入（那是 v0.6.0 发版之后的取值：`version` 0.6.0、`contract_version` 1.3.0）。
    /// 这两个版本串在这里只是输入的字符串，被锁的是字段与顺序；S1 的二进制实际写入的是当前常量
    /// （`/health.contract_version` 的同一个值，发版前仍是 1.2.0），见集成测试
    /// `discovery_file_has_the_specified_fields_permissions_and_launch_snapshot`。
    fn record() -> DiscoveryRecord {
        DiscoveryRecord {
            instance_id: INSTANCE_ID.to_string(),
            instance_nonce: InstanceNonce::from_hex(NONCE).unwrap(),
            pid: 41234,
            data_port: 51234,
            admin_port: 51235,
            version: "0.6.0".to_string(),
            contract_version: "1.3.0".to_string(),
            started_at_ms: 1_763_625_330_123,
            launch: Some(LaunchSnapshot {
                db_path: "/Users/x/.agent24/idoris/idoris.sqlite3".to_string(),
                state_dir: Some("/Users/x/.agent24/idoris/state".to_string()),
                components_dir: "/Users/x/.agent24/components/idoris-config".to_string(),
                allocated_budget_bytes: Some(17_179_869_184),
                engines: vec![
                    EngineEntry {
                        id: "omlx".to_string(),
                        engine_kind: "omlx".to_string(),
                        managed: true,
                    },
                    EngineEntry {
                        id: "omlx-embed".to_string(),
                        engine_kind: "omlx".to_string(),
                        managed: false,
                    },
                ],
            }),
        }
    }

    /// 一个属主为当前用户、0700 的 canonical 目录（macOS 的临时目录在 `/var` 下，实为 `/private/var`）。
    fn private_dir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o700)).unwrap();
        (temp, canonical)
    }

    fn open_error(path: &Path) -> DiscoveryError {
        DiscoveryTarget::open(path).unwrap_err()
    }

    /// 目录里有没有发布方的临时文件。
    fn has_temp_file(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(".idoris-discovery.") && name.ends_with(".tmp")
            })
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// 把目录权限恢复成 0700：断言失败时也要恢复，否则临时目录删不掉。
    struct RestoreMode(PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    #[test]
    fn the_record_matches_the_specified_example_byte_for_byte() {
        let line = String::from_utf8(record().to_line().unwrap()).unwrap();
        assert_eq!(
            line,
            "{\"schema\":1,\"service\":\"idoris\",\"instance_id\":\"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\",\"instance_nonce\":\"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\",\"pid\":41234,\"data_port\":51234,\"admin_port\":51235,\"version\":\"0.6.0\",\"contract_version\":\"1.3.0\",\"started_at_ms\":1763625330123,\"launch\":{\"db_path\":\"/Users/x/.agent24/idoris/idoris.sqlite3\",\"state_dir\":\"/Users/x/.agent24/idoris/state\",\"components_dir\":\"/Users/x/.agent24/components/idoris-config\",\"allocated_budget_bytes\":17179869184,\"engines\":[{\"id\":\"omlx\",\"engine_kind\":\"omlx\",\"managed\":true},{\"id\":\"omlx-embed\",\"engine_kind\":\"omlx\",\"managed\":false}]}}\n"
        );
    }

    #[test]
    fn launch_is_optional_and_nulls_serialize_as_null() {
        let mut bare = record();
        bare.launch = None;
        let line = String::from_utf8(bare.to_line().unwrap()).unwrap();
        assert!(!line.contains("launch"), "{line}");
        let mut nulls = record();
        if let Some(launch) = nulls.launch.as_mut() {
            launch.state_dir = None;
            launch.allocated_budget_bytes = None;
            launch.engines.clear();
        }
        let value: serde_json::Value = serde_json::from_slice(&nulls.to_line().unwrap()).unwrap();
        assert!(value["launch"]["state_dir"].is_null());
        assert!(value["launch"]["allocated_budget_bytes"].is_null());
        assert_eq!(value["launch"]["engines"], serde_json::json!([]));
    }

    #[test]
    fn a_record_over_4096_bytes_is_refused_not_truncated() {
        let mut huge = record();
        if let Some(launch) = huge.launch.as_mut() {
            launch.components_dir = format!("/{}", "c".repeat(MAX_BYTES));
        }
        assert_eq!(huge.to_line().unwrap_err(), DiscoveryError::RecordTooLarge);
    }

    #[test]
    fn debug_of_a_record_never_renders_the_nonce() {
        let rendered = format!("{:?}", record());
        assert!(!rendered.contains(NONCE), "{rendered}");
        assert!(!rendered.contains(&NONCE[..16]), "{rendered}");
    }

    #[test]
    fn path_syntax_is_checked_before_the_filesystem_is_touched() {
        let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        let long_name = format!("/tmp/{}", "n".repeat(256));
        for path in [
            "",
            "relative/idoris.json",
            "idoris.json",
            "/",
            "/dir/",
            "/dir/.",
            "/dir/..",
            long.as_str(),
            long_name.as_str(),
        ] {
            assert_eq!(
                open_error(Path::new(path)),
                DiscoveryError::PathInvalid,
                "{path}"
            );
        }
        assert_eq!(
            open_error(Path::new("/a\u{0}b/idoris.json")),
            DiscoveryError::PathInvalid
        );
    }

    #[test]
    fn a_missing_parent_is_refused_and_never_created() {
        let (_temp, dir) = private_dir();
        let missing = dir.join("run");
        assert_eq!(
            open_error(&missing.join("idoris.json")),
            DiscoveryError::ParentMissing
        );
        assert!(!missing.exists(), "iDoris must not create the parent");
    }

    #[test]
    fn a_parent_reached_through_a_symlink_is_not_canonical_and_is_refused() {
        let (_temp, dir) = private_dir();
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.join("link");
        symlink(&real, &link).unwrap();
        assert_eq!(
            open_error(&link.join("idoris.json")),
            DiscoveryError::ParentNotCanonical
        );
        // 中间目录是链接也一样。
        std::fs::create_dir(real.join("inner")).unwrap();
        std::fs::set_permissions(real.join("inner"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_eq!(
            open_error(&link.join("inner").join("idoris.json")),
            DiscoveryError::ParentNotCanonical
        );
        for dotted in [
            format!("{}/./idoris.json", real.display()),
            format!("{}/inner/../idoris.json", real.display()),
            format!("{}//idoris.json", real.display()),
        ] {
            assert_eq!(
                open_error(Path::new(&dotted)),
                DiscoveryError::ParentNotCanonical,
                "{dotted}"
            );
        }
        // 对照：canonical 的路径通过。
        assert!(DiscoveryTarget::open(&real.join("idoris.json")).is_ok());
    }

    #[test]
    fn a_parent_that_is_a_file_is_refused() {
        let (_temp, dir) = private_dir();
        let file = dir.join("plain");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            open_error(&file.join("idoris.json")),
            DiscoveryError::ParentNotDirectory
        );
    }

    #[test]
    fn group_or_other_writable_parents_are_refused_and_readable_ones_are_not() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        for mode in [0o770, 0o775, 0o777, 0o707, 0o722, 0o720, 0o702, 0o1777] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                open_error(&path),
                DiscoveryError::ParentWritableByOthers,
                "{mode:o}"
            );
        }
        // 组与其他人可读、可进入但不可写：规范只要求 `mode & 0o022 == 0`（建议 0700）。
        for mode in [0o700, 0o750, 0o755, 0o705, 0o500] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(DiscoveryTarget::open(&path).is_ok(), "{mode:o}");
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn a_parent_owned_by_someone_else_is_refused() {
        // 根目录属于 root，不是当前（非 root）用户。以 root 运行时这个前提不成立，跳过。
        if nix::unistd::geteuid().is_root() {
            return;
        }
        assert_eq!(
            open_error(Path::new("/idoris-discovery-test.json")),
            DiscoveryError::ParentWrongOwner
        );
        assert!(!Path::new("/idoris-discovery-test.json").exists());
    }

    #[test]
    fn an_existing_target_must_be_a_regular_file_and_is_never_deleted() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");

        let outside = dir.join("outside.txt");
        std::fs::write(&outside, b"precious").unwrap();
        symlink(&outside, &path).unwrap();
        assert_eq!(open_error(&path), DiscoveryError::TargetNotRegular);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"precious");
        std::fs::remove_file(&path).unwrap();

        std::fs::create_dir(&path).unwrap();
        assert_eq!(open_error(&path), DiscoveryError::TargetNotRegular);
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();

        nix::unistd::mkfifo(&path, stat_mode(0o600)).unwrap();
        assert_eq!(open_error(&path), DiscoveryError::TargetNotRegular);
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();

        // 常规文件（陈旧的发现文件）是可以覆盖的。
        std::fs::write(&path, b"stale").unwrap();
        assert!(DiscoveryTarget::open(&path).is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), b"stale");
    }

    /// `mode_t` 在 macOS 上是 u16、在 Linux 上是 u32：参数直接取 `mode_t`，调用点的字面量自己推断类型，
    /// 不写任何在某个平台上多余（会触发 `clippy::unnecessary_cast`）的转换。
    fn stat_mode(bits: nix::libc::mode_t) -> nix::sys::stat::Mode {
        nix::sys::stat::Mode::from_bits_truncate(bits)
    }

    #[test]
    fn publish_writes_a_0600_single_line_file_and_leaves_no_temp_file_behind() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        target.publish(&record()).unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content, record().to_line().unwrap());
        assert!(content.len() <= MAX_BYTES);
        assert_eq!(content.iter().filter(|byte| **byte == b'\n').count(), 1);

        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".idoris-discovery."))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn publish_is_independent_of_the_process_umask() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        // 一个宽松的 umask 不能让文件比 0600 更开放；这里用 fchmod 的效果验证：直接看结果权限。
        target.publish(&record()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn publishing_over_a_stale_file_replaces_it_atomically() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        std::fs::write(&path, b"{\"instance_id\":\"stale\"}\n").unwrap();
        let target = DiscoveryTarget::open(&path).unwrap();
        target.publish(&record()).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["instance_id"], INSTANCE_ID);
    }

    #[test]
    fn a_failed_install_removes_the_temp_file() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        // 校验之后目标被换成一个目录（里面有东西）：`install` 持锁后的 `check_target` 先拒绝（`TargetNotRegular`）；
        // 就算它放行，把普通文件 rename 到一个目录上也会失败（`EISDIR`，与目录是否为空无关），所以两种结果都接受。
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), b"x").unwrap();
        let error = target.publish(&record()).unwrap_err();
        assert!(
            matches!(
                error,
                DiscoveryError::TargetNotRegular | DiscoveryError::RenameFailed
            ),
            "{error:?}"
        );
        assert!(path.is_dir() && path.join("inside").exists());
        let temps = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temps, 0);
    }

    #[test]
    fn the_parent_is_opened_once_and_every_later_write_lands_in_that_directory() {
        // 打开 parent 之后，把路径上的目录改名，再在原路径放一个指向「攻击者目录」的符号链接。
        let (_temp, dir) = private_dir();
        let original = dir.join("run");
        let moved = dir.join("run-moved");
        let attacker = dir.join("attacker");
        for directory in [&original, &attacker] {
            std::fs::create_dir(directory).unwrap();
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = original.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();

        std::fs::rename(&original, &moved).unwrap();
        symlink(&attacker, &original).unwrap();

        target.publish(&record()).unwrap();
        assert!(
            moved.join("idoris.json").exists(),
            "written into the original directory"
        );
        assert!(
            !attacker.join("idoris.json").exists(),
            "must not follow the replaced path"
        );
        assert_eq!(std::fs::read_dir(&attacker).unwrap().count(), 0);

        // 删除同样只作用于原目录。
        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::Removed);
        assert!(!moved.join("idoris.json").exists());
        assert_eq!(std::fs::read_dir(&attacker).unwrap().count(), 0);
    }

    #[test]
    fn a_symlink_swapped_in_between_the_canonical_check_and_the_open_is_refused() {
        let (_temp, dir) = private_dir();
        let run = dir.join("run");
        let attacker = dir.join("attacker");
        for directory in [&run, &attacker] {
            std::fs::create_dir(directory).unwrap();
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = run.join("idoris.json");
        let error = DiscoveryTarget::open_with_hook(&path, || {
            std::fs::remove_dir(&run).unwrap();
            symlink(&attacker, &run).unwrap();
        })
        .unwrap_err();
        assert_eq!(error, DiscoveryError::ParentSymlink);
        assert_eq!(std::fs::read_dir(&attacker).unwrap().count(), 0);
    }

    #[test]
    fn remove_if_owner_deletes_only_a_file_carrying_our_instance_id() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();

        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::Absent);

        // 别的实例的文件：原样保留。
        let mut other = record();
        other.instance_id = "11111111-2222-4333-8444-555555555555".to_string();
        target.publish(&other).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::NotOwned);
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // 内容读不懂 / 超长：同样不是自己的。
        for junk in [
            b"not json".to_vec(),
            b"{}".to_vec(),
            b"{\"instance_id\":7}".to_vec(),
            vec![b'x'; MAX_BYTES + 10],
            format!(
                "{{\"instance_id\":\"{INSTANCE_ID}\",\"pad\":\"{}\"}}",
                "p".repeat(MAX_BYTES)
            )
            .into_bytes(),
        ] {
            std::fs::write(&path, &junk).unwrap();
            assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::NotOwned);
            assert_eq!(std::fs::read(&path).unwrap(), junk);
        }

        // 自己的：删除。
        target.publish(&record()).unwrap();
        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::Removed);
        assert!(!path.exists());
        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::Absent);
    }

    #[test]
    fn remove_if_owner_never_follows_a_symlink_or_opens_a_fifo() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();

        // 校验之后，目标被换成指向「自己 instance_id 文件」的符号链接：不跟随、不删除、不删链接指向的文件。
        let elsewhere = dir.join("elsewhere.json");
        std::fs::write(&elsewhere, record().to_line().unwrap()).unwrap();
        symlink(&elsewhere, &path).unwrap();
        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::NotOwned);
        assert!(elsewhere.exists());
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&path).unwrap();

        // 换成 FIFO：打开不会卡住，按「不是普通文件」处理。
        nix::unistd::mkfifo(&path, stat_mode(0o600)).unwrap();
        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::NotOwned);
        assert!(path.exists());
    }

    #[test]
    fn the_sidecar_lock_serializes_removal_with_a_consumer_that_holds_it() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        target.publish(&record()).unwrap();

        // 消费者（Agent24）清理陈旧文件时持同一把旁路锁。
        let lock_path = dir.join("idoris.json.lock");
        assert!(lock_path.exists(), "publish creates the sidecar lock");
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let consumer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        consumer.lock().unwrap();

        let started = std::time::Instant::now();
        // iDoris 这边等不到锁：不删，文件原样保留（成为崩溃残留，由下一个实例覆盖）。
        assert_eq!(
            target.remove_if_owner(INSTANCE_ID),
            RemoveOutcome::LockUnavailable
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(1500));
        assert!(path.exists());

        consumer.unlock().unwrap();
        assert_eq!(target.remove_if_owner(INSTANCE_ID), RemoveOutcome::Removed);
        assert!(lock_path.exists(), "the lock file itself is never deleted");
    }

    #[test]
    fn a_sidecar_lock_that_is_not_a_regular_file_makes_publish_fail_instead_of_following_it() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        symlink(&victim, dir.join("idoris.json.lock")).unwrap();
        assert_eq!(
            target.publish(&record()).unwrap_err(),
            DiscoveryError::LockUnavailable
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(!path.exists());
    }

    /// `open` 之后的 `fstat(parent)` 核对（`check_parent_stat`）：类型、属主、权限，顺序固定。第一项在真实文件系统上
    /// 走不到（`O_DIRECTORY` 已经先拒绝了非目录），第二项需要别的用户的目录，所以直接喂事实值，逐项钉住。
    #[test]
    fn fstat_facts_about_the_parent_are_checked_type_first_then_owner_then_mode() {
        use nix::libc::{S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG};
        let (me, other) = (1000_u32, 1001_u32);

        // 干净的 parent：目录、属主是自己、组与其他人不可写（0500、0750、0755 都只要求不可写）。
        for mode in [
            S_IFDIR | 0o700,
            S_IFDIR | 0o750,
            S_IFDIR | 0o755,
            S_IFDIR | 0o500,
        ] {
            assert_eq!(check_parent_stat(mode, me, me), Ok(()), "{mode:o}");
        }
        // 第一项：不是目录。排在属主与权限之前：别人属主、别人可写的非目录也报 `ParentNotDirectory`。
        for kind in [S_IFREG, S_IFIFO, S_IFLNK] {
            for (uid, permissions) in [(me, 0o700), (other, 0o700), (me, 0o777), (other, 0o777)] {
                assert_eq!(
                    check_parent_stat(kind | permissions, uid, me),
                    Err(DiscoveryError::ParentNotDirectory),
                    "{kind:o} {permissions:o} uid {uid}"
                );
            }
        }
        // 第二项：属主不是当前有效 uid。排在权限之前。
        for permissions in [0o700, 0o777] {
            assert_eq!(
                check_parent_stat(S_IFDIR | permissions, other, me),
                Err(DiscoveryError::ParentWrongOwner),
                "{permissions:o}"
            );
        }
        // 第三项：组或其他人可写。
        for permissions in [0o770, 0o707, 0o722, 0o720, 0o702, 0o1777] {
            assert_eq!(
                check_parent_stat(S_IFDIR | permissions, me, me),
                Err(DiscoveryError::ParentWritableByOthers),
                "{permissions:o}"
            );
        }
    }

    /// 发布时建临时文件失败（不是 `EEXIST`）：报 `temp_create_failed`，什么都不留。`0500` 的 parent 通过 `open` 的
    /// 「组与其他人不可写」校验，之后对属主也不可写，在里面建文件得到 `EACCES`。把这一支改成重试得到的是同一个错误，
    /// 所以这条测试钉的是「不是 `EEXIST` 的失败如实报告、不留任何东西」，不是「只试一次」。
    #[test]
    fn an_uncreatable_temp_file_fails_publish_as_temp_create_failed_and_leaves_nothing() {
        // root 不受目录权限限制，没有这个前提：跳过。
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let _restore = RestoreMode(dir.clone());

        assert_eq!(
            target.publish(&record()).unwrap_err(),
            DiscoveryError::TempCreateFailed
        );
        // 没有临时文件、没有目标、没有旁路锁（锁是 `install` 才建的，发布在它之前就失败了）。
        assert!(listing(&dir).is_empty(), "{:?}", listing(&dir));
    }

    /// 旁路锁文件的核对（`sidecar_lock_is_ours`）：必须是属主为当前有效 uid 的普通文件。属主不对需要 root 才造得出来，
    /// 所以喂事实值；类型不对的真实文件系统版本见下一条（FIFO）。
    #[test]
    fn the_sidecar_lock_must_be_a_regular_file_owned_by_us() {
        use nix::libc::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, S_IFSOCK};
        let (me, other) = (1000_u32, 1001_u32);
        assert!(sidecar_lock_is_ours(S_IFREG | 0o600, me, me));
        for kind in [S_IFIFO, S_IFDIR, S_IFLNK, S_IFSOCK, S_IFCHR, S_IFBLK] {
            assert!(!sidecar_lock_is_ours(kind | 0o600, me, me), "{kind:o}");
        }
        assert!(!sidecar_lock_is_ours(S_IFREG | 0o600, other, me));
        assert!(!sidecar_lock_is_ours(S_IFREG | 0o666, other, me));
    }

    /// 旁路锁文件是 FIFO：`O_RDWR` 打开 FIFO 不会阻塞，所以它越过 `lock_sidecar` 里的 `open`（符号链接则被 `O_NOFOLLOW`
    /// 在更早处拦下，见上一条），一直走到 `open` 之后的类型核对。macOS 上 `flock` 对 FIFO 本身就失败（`ENOTSUP`，
    /// 本机实测），同一个结果也会由 `try_lock` 的失败分支给出，所以在 macOS 上单靠这条抓不住类型核对被拿掉；
    /// Linux 上 `flock` 不按文件类型拒绝（没有在 Linux 上验证过，等 CI），预期没有这道核对发布会悄悄成功，
    /// 这条测试在那里单独钉住核对被调用。类型与属主的判据由 `the_sidecar_lock_must_be_a_regular_file_owned_by_us`
    /// 在所有平台上钉住。
    #[test]
    fn a_sidecar_lock_that_is_a_fifo_is_refused_by_the_type_check_after_it_was_opened() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        let mut stale = record();
        stale.instance_id = "11111111-2222-4333-8444-555555555555".to_string();
        target.publish(&stale).unwrap();
        let stale_bytes = std::fs::read(&path).unwrap();

        // 旁路锁文件换成 FIFO（已经建好的先删掉；不假设发布一定建过它）。
        let lock_path = dir.join("idoris.json.lock");
        let _ = std::fs::remove_file(&lock_path);
        nix::unistd::mkfifo(&lock_path, stat_mode(0o600)).unwrap();

        assert_eq!(
            target.publish(&record()).unwrap_err(),
            DiscoveryError::LockUnavailable
        );
        // 失败的发布没有动旧文件，没有留下临时文件，FIFO 还是 FIFO。
        assert_eq!(std::fs::read(&path).unwrap(), stale_bytes);
        assert!(!has_temp_file(&dir));
        assert!(
            std::fs::symlink_metadata(&lock_path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
        // 同一个类型核对也挡住退出时的删除：`remove_if_owner` 报 `LockUnavailable`，文件原样。
        assert_eq!(
            target.remove_if_owner(&stale.instance_id),
            RemoveOutcome::LockUnavailable
        );
        assert_eq!(std::fs::read(&path).unwrap(), stale_bytes);
    }

    /// 「发布」与「消费者持锁清理」真正竞争（规范 §2.7.3、R12 已确认项 4：消费者清理陈旧文件时持同一把旁路锁，
    /// 持锁、读、比对出旧 `instance_id`、unlink）。新实例的发布必须等锁，拿到锁之后再复核目标、`renameat`：
    /// 于是消费者持锁期间做的 unlink 不会删掉新文件。如果发布不持锁，它会在消费者「读完、unlink 之前」把新文件
    /// rename 进来，消费者的 unlink 就把新文件删了（这条测试里发布方会在消费者持锁期间提前完成，或最后文件不见了）。
    #[test]
    fn a_publish_racing_a_consumer_that_holds_the_sidecar_lock_waits_for_it_and_is_not_undone() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = std::sync::Arc::new(DiscoveryTarget::open(&path).unwrap());
        let stale_id = "11111111-2222-4333-8444-555555555555";
        let mut stale = record();
        stale.instance_id = stale_id.to_string();
        target.publish(&stale).unwrap();

        // 消费者：持锁（锁文件没有就建，不依赖发布建过它），读，比对出旧 instance_id。
        let consumer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("idoris.json.lock"))
            .unwrap();
        consumer.lock().unwrap();
        let seen: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(seen["instance_id"], stale_id);

        // 新实例开始发布。等锁时限给得很宽：很忙的机器上也不会因为等锁超时而误报。
        let (done_sender, done_receiver) = std::sync::mpsc::channel();
        let publisher = {
            let target = target.clone();
            std::thread::spawn(move || {
                let result = target.publish_with_lock_wait(&record(), Duration::from_secs(30));
                let _ = done_sender.send(());
                result
            })
        };
        // 等到发布方确实走到了等锁这一步（它的临时文件已经写好）。它在这之前就完成了，说明它没有等锁。
        let deadline = Instant::now() + Duration::from_secs(30);
        while !has_temp_file(&dir) {
            assert!(
                done_receiver.try_recv().is_err(),
                "the publish finished without waiting for the sidecar lock"
            );
            assert!(
                Instant::now() < deadline,
                "the publisher never wrote its temp file"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // 再给它一个越过锁的机会：消费者仍持锁，发布方不得完成。
        assert!(
            done_receiver
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "the publish finished while the consumer still held the sidecar lock"
        );
        // 消费者 unlink 它读到的旧文件，然后放锁。
        std::fs::remove_file(&path).unwrap();
        consumer.unlock().unwrap();

        publisher.join().unwrap().unwrap();
        // 在的是新文件：不是被消费者的 unlink 删掉了。目录里只有发现文件与旁路锁，没有临时文件残留。
        let now: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(now["instance_id"], INSTANCE_ID);
        assert_eq!(listing(&dir), ["idoris.json", "idoris.json.lock"]);
    }

    /// 发布方等不到旁路锁：到了时限就失败，旧文件原样，临时文件被清掉；放锁之后同一个 target 能正常发布。
    #[test]
    fn a_publish_that_cannot_get_the_sidecar_lock_in_time_fails_cleanly_and_keeps_the_old_file() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        let mut stale = record();
        stale.instance_id = "11111111-2222-4333-8444-555555555555".to_string();
        target.publish(&stale).unwrap();
        let stale_bytes = std::fs::read(&path).unwrap();

        let consumer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("idoris.json.lock"))
            .unwrap();
        consumer.lock().unwrap();

        let started = Instant::now();
        let error = target
            .publish_with_lock_wait(&record(), Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(error, DiscoveryError::LockUnavailable);
        // 它等满了时限（时限从取锁开始算，所以不会比 200 毫秒短）。
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(std::fs::read(&path).unwrap(), stale_bytes);
        assert!(!has_temp_file(&dir));

        consumer.unlock().unwrap();
        target.publish(&record()).unwrap();
        let now: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(now["instance_id"], INSTANCE_ID);
    }

    #[test]
    fn concurrent_readers_never_observe_a_partial_or_empty_file_while_it_is_rewritten() {
        let (_temp, dir) = private_dir();
        let path = dir.join("idoris.json");
        let target = DiscoveryTarget::open(&path).unwrap();
        target.publish(&record()).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers = (0..4)
            .map(|_| {
                let path = path.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut reads = 0_u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        // 文件要么是旧的完整内容，要么是新的完整内容；任何时刻都不能读到半截。
                        let bytes = std::fs::read(&path).unwrap();
                        assert!(
                            bytes.ends_with(b"\n"),
                            "partial file: {} bytes",
                            bytes.len()
                        );
                        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(value["schema"], 1);
                        assert_eq!(value["instance_nonce"].as_str().unwrap().len(), 64);
                        reads += 1;
                    }
                    reads
                })
            })
            .collect::<Vec<_>>();
        for round in 0..300_u32 {
            let mut next = record();
            next.pid = round;
            next.started_at_ms += u64::from(round);
            target.publish(&next).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let total: u64 = readers
            .into_iter()
            .map(|reader| reader.join().unwrap())
            .sum();
        assert!(total > 0);
    }

    #[test]
    fn state_dir_prefers_the_env_var_then_home_and_must_be_absolute() {
        use std::ffi::OsString;
        let abs = |value: &str| Some(OsString::from(value));
        assert_eq!(
            state_dir_from(abs("/var/idoris"), abs("/Users/x")),
            Some(PathBuf::from("/var/idoris"))
        );
        assert_eq!(
            state_dir_from(None, abs("/Users/x")),
            Some(PathBuf::from("/Users/x/.local/state/idoris"))
        );
        // 设置了但不是绝对路径：就是没有（与加载围栏一致，不悄悄退到 HOME）。
        assert_eq!(state_dir_from(abs("relative/state"), abs("/Users/x")), None);
        assert_eq!(state_dir_from(None, abs("relative-home")), None);
        assert_eq!(state_dir_from(None, None), None);
    }
}

//! 引擎日志（规范 §2.7.6）：stdout 与 stderr 追加到 `<状态目录>/logs/<engine_kind>.log`（oMLX 即 `omlx.log`），
//! 超过 10 MiB 滚动，保留 3 份。
//!
//! 日志按**种类**命名：同一种类的多个实例共用一个日志文件（`O_APPEND`，各写各的）；不同种类各有各的。
//!
//! 引擎的 stdout/stderr 直接接到这个**文件**，而不是管道：iDoris 不在数据通路上，iDoris 被 SIGKILL 之后
//! 引擎照常写日志，不会因为管道另一端没了而吃 SIGPIPE（设计稿 §2.7.5 要求孤儿引擎继续活着）。
//!
//! - 目录 0700、文件 0600（已存在而权限更宽就收紧；符号链接、非普通文件、别人的文件一律拒绝）。
//! - 滚动用「复制再截断」：引擎可能正握着这个文件（`O_APPEND`），改名会让它继续写进改名后的文件；截断到 0
//!   之后，它的下一次追加落在新的末尾。没有引擎在写时（拉起之前）不丢任何一行；有引擎在写时，复制与截断
//!   之间的几毫秒可能丢几行，日志可以接受。
//! - 滚动发生在每次拉起引擎之前；[`EngineLog::rotate_if_oversized`] 也可以由调用方周期性调用，
//!   让一个长跑的引擎的日志同样有上限。

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::fsutil::{DirError, effective_uid, ensure_private_dir};
use super::instance::EngineInstance;

/// 单个日志文件的大小上限。
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 保留的历史份数（`<engine_kind>.log.1` 至 `.3`）。
pub const DEFAULT_KEEP: u32 = 3;

#[derive(Debug, Clone)]
pub struct EngineLog {
    dir: PathBuf,
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
}

impl EngineLog {
    /// 这个实例的种类的日志 `<状态目录>/logs/<engine_kind>.log`，默认 10 MiB、保留 3 份。
    pub fn new(instance: &EngineInstance) -> Self {
        let path = instance.log_path();
        Self {
            dir: path.parent().map(Path::to_owned).unwrap_or_default(),
            path,
            max_bytes: DEFAULT_MAX_BYTES,
            keep: DEFAULT_KEEP,
        }
    }

    /// 换上限与份数（测试用小值）。
    pub fn with_limits(mut self, max_bytes: u64, keep: u32) -> Self {
        self.max_bytes = max_bytes;
        self.keep = keep;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 滚动（需要的话），然后打开两个指向日志的句柄，分别交给子进程的 stdout 与 stderr
    /// （同一个打开的文件描述，`O_APPEND`，两路输出不会互相覆盖）。
    pub fn open_for_child(&self) -> std::io::Result<(std::fs::File, std::fs::File)> {
        ensure_private_dir(&self.dir).map_err(dir_error)?;
        self.rotate_if_oversized()?;
        let file = self.open_append()?;
        let twin = file.try_clone()?;
        Ok((file, twin))
    }

    /// 文件超过上限就滚动：`<engine_kind>.log.2` → `.3`、`.1` → `.2`、当前内容复制成 `.1`，再把当前文件截断到 0。
    /// 返回是否滚动了。
    pub fn rotate_if_oversized(&self) -> std::io::Result<bool> {
        let meta = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !meta.file_type().is_file() {
            return Err(refuse("engine log is not a regular file"));
        }
        if meta.len() <= self.max_bytes {
            return Ok(false);
        }
        if self.keep > 0 {
            remove_if_present(&self.backup(self.keep))?;
            for index in (1..self.keep).rev() {
                match std::fs::rename(self.backup(index), self.backup(index + 1)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            let mut source = std::fs::File::open(&self.path)?;
            // 新建（`O_EXCL`）：上面已经把 `.1` 让位，这里不会写进一个别人放好的文件。
            let mut copy = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.backup(1))?;
            copy.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = source.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                copy.write_all(&buffer[..read])?;
            }
            copy.sync_all()?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&self.path)?
            .set_len(0)?;
        Ok(true)
    }

    fn backup(&self, index: u32) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_owned();
        name.push(format!(".{index}"));
        self.dir.join(name)
    }

    fn open_append(&self) -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&self.path)
            .map_err(|error| {
                // 最后一级是符号链接：带 O_NOFOLLOW 的 open 以 ELOOP 失败（个别 BSD 是 EMLINK）。
                if matches!(
                    error.raw_os_error(),
                    Some(code) if code == nix::libc::ELOOP || code == nix::libc::EMLINK
                ) {
                    refuse("engine log path is a symbolic link")
                } else {
                    error
                }
            })?;
        let meta = file.metadata()?;
        if !meta.file_type().is_file() || meta.uid() != effective_uid() {
            return Err(refuse(
                "engine log is not a regular file owned by this user",
            ));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(file)
    }
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn refuse(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)
}

fn dir_error(error: DirError) -> std::io::Error {
    match error {
        DirError::Io(kind) => std::io::Error::from(kind),
        DirError::Insecure => refuse("engine log directory is not a private directory"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::symlink;

    use super::*;
    use crate::engine::test_support::instance;

    /// An instance of kind `fakeserve` under `state`: its log is `<state>/logs/fakeserve.log`.
    fn inst(state: &Path) -> EngineInstance {
        instance(state, "fakeserve", 19731)
    }

    /// `<state>/logs`: where the log and its copies live.
    fn logs_dir(state: &Path) -> PathBuf {
        state.join("logs")
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_log_is_private_and_both_handles_append_to_one_file() {
        let state = tempfile::tempdir().unwrap();
        let log = EngineLog::new(&inst(state.path()));
        let (mut out, mut err) = log.open_for_child().unwrap();
        assert_eq!(log.path(), logs_dir(state.path()).join("fakeserve.log"));
        assert_eq!(mode(log.path()), 0o600);
        assert_eq!(mode(&logs_dir(state.path())), 0o700);

        out.write_all(b"one\n").unwrap();
        err.write_all(b"two\n").unwrap();
        out.write_all(b"three\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(log.path()).unwrap(),
            "one\ntwo\nthree\n"
        );

        // A second launch appends, it does not truncate.
        let (mut again, _) = log.open_for_child().unwrap();
        again.write_all(b"four\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(log.path()).unwrap(),
            "one\ntwo\nthree\nfour\n"
        );
    }

    #[test]
    fn a_log_left_with_wide_permissions_is_tightened() {
        let state = tempfile::tempdir().unwrap();
        let log = EngineLog::new(&inst(state.path()));
        std::fs::create_dir_all(logs_dir(state.path())).unwrap();
        std::fs::set_permissions(
            logs_dir(state.path()),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(log.path(), b"old\n").unwrap();
        std::fs::set_permissions(log.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(log.open_for_child().unwrap());
        assert_eq!(mode(log.path()), 0o600);
        assert_eq!(mode(&logs_dir(state.path())), 0o700);
        assert_eq!(std::fs::read_to_string(log.path()).unwrap(), "old\n");
    }

    #[test]
    fn a_symlinked_log_or_log_directory_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let log = EngineLog::new(&inst(state.path()));
        symlink(elsewhere.path(), logs_dir(state.path())).unwrap();
        assert!(log.open_for_child().is_err());
        std::fs::remove_file(logs_dir(state.path())).unwrap();

        std::fs::create_dir(logs_dir(state.path())).unwrap();
        let target = elsewhere.path().join("target.log");
        std::fs::write(&target, b"keep\n").unwrap();
        symlink(&target, log.path()).unwrap();
        assert!(log.open_for_child().is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep\n");
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 1);
    }

    #[test]
    fn rotation_keeps_the_newest_copies_and_empties_the_live_file() {
        let state = tempfile::tempdir().unwrap();
        let log = EngineLog::new(&inst(state.path())).with_limits(8, 2);
        let backup = |n: u32| logs_dir(state.path()).join(format!("fakeserve.log.{n}"));

        // Under the limit: nothing happens.
        let (mut out, _) = log.open_for_child().unwrap();
        out.write_all(b"12345678").unwrap();
        assert!(!log.rotate_if_oversized().unwrap());

        for round in 1..=4u32 {
            out.write_all(format!("-round{round}").as_bytes()).unwrap();
            assert!(log.rotate_if_oversized().unwrap(), "round {round}");
            assert_eq!(std::fs::metadata(log.path()).unwrap().len(), 0);
            // The engine keeps its descriptor: the next write lands in the live file.
            out.write_all(b"12345678").unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(backup(1)).unwrap(),
            "12345678-round4"
        );
        assert_eq!(
            std::fs::read_to_string(backup(2)).unwrap(),
            "12345678-round3"
        );
        assert!(!backup(3).exists(), "only two copies are kept");
        assert_eq!(std::fs::read_to_string(log.path()).unwrap(), "12345678");
        for n in 1..=2 {
            assert_eq!(mode(&backup(n)), 0o600);
        }
    }

    #[test]
    fn opening_for_a_new_launch_rotates_an_oversized_log_first() {
        let state = tempfile::tempdir().unwrap();
        let log = EngineLog::new(&inst(state.path())).with_limits(4, 3);
        let (mut first, _) = log.open_for_child().unwrap();
        first.write_all(b"old-launch\n").unwrap();
        let (mut second, _) = log.open_for_child().unwrap();
        second.write_all(b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(log.path()).unwrap(), "new\n");
        assert_eq!(
            std::fs::read_to_string(logs_dir(state.path()).join("fakeserve.log.1")).unwrap(),
            "old-launch\n"
        );
    }

    #[test]
    fn rotating_with_no_copies_kept_just_truncates() {
        let state = tempfile::tempdir().unwrap();
        let log = EngineLog::new(&inst(state.path())).with_limits(2, 0);
        let (mut out, _) = log.open_for_child().unwrap();
        out.write_all(b"abcdef").unwrap();
        assert!(log.rotate_if_oversized().unwrap());
        assert_eq!(std::fs::metadata(log.path()).unwrap().len(), 0);
        assert!(!logs_dir(state.path()).join("fakeserve.log.1").exists());
    }

    #[test]
    fn the_defaults_are_the_design_values() {
        assert_eq!(DEFAULT_MAX_BYTES, 10 * 1024 * 1024);
        assert_eq!(DEFAULT_KEEP, 3);
    }
}

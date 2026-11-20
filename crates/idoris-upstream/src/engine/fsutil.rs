//! 状态目录下私有目录的共用辅助（所有权记录目录、日志目录）。

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// 建 / 校验私有目录的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirError {
    Io(std::io::ErrorKind),
    /// 路径是符号链接、不是目录、或不是自己的目录。
    Insecure,
}

pub(crate) fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// `meta`（`symlink_metadata` 得来，不跟随链接）是自己的真目录。
pub(crate) fn is_own_directory(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_dir() && meta.uid() == effective_uid()
}

/// 建 0700 目录（连同缺的上级）。已存在则必须是自己的真目录，权限比 0700 宽就收紧到 0700：
/// 目录是我们的，上一版或 umask 留下的宽权限该修，而不是拒绝启动；符号链接、别人的目录不碰。
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), DirError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| DirError::Io(e.kind()))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| DirError::Io(e.kind()))?;
    if !is_own_directory(&meta) {
        return Err(DirError::Insecure);
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| DirError::Io(e.kind()))?;
    }
    Ok(())
}

//! 单元测试共用的小工具（只在 `cfg(test)` 下编译）。tenancy 的依赖里没有 `tempfile`，不为测试新增依赖，自带一个最小的。

use std::path::PathBuf;

/// 临时目录：drop 时（包括测试 panic 时）连同里面的库文件、`-wal`、`-shm` 一起删掉。
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "idoris-tenancy-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        #[allow(clippy::expect_used)]
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub(crate) fn db(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

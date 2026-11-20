//! 引擎实例：配方、端口、状态目录，以及它在状态目录里的布局（规范 §2.7.6、§2.12.1）。
//!
//! 规范里「引擎实例」是**种类加 endpoint**：同一种类可以有多个实例（两个端口上各一个 oMLX），不同种类也可以并存。
//! 状态按规范的布局存放：
//!
//! ```text
//! <状态目录>/engines-owned/<engine_kind>/                  0700  按种类分目录
//!     <endpoint 编码>.json                                   0600  所有权记录，同一种类的每个实例各一份
//! <状态目录>/logs/                                          0700
//!     <engine_kind>.log, <engine_kind>.log.1 … .3           0600  引擎的 stdout/stderr，10 MiB 滚动，保留 3 份
//! ```
//!
//! `<endpoint 编码>` 是 `http://127.0.0.1:<port>` 的整体百分号编码（与 oMLX 加载围栏的目录名同一个编码）。
//! 日志按**种类**命名（规范 §2.7.6）：同一种类的多个实例共用一个日志文件（`O_APPEND`，各写各的）。
//! 引擎的工作目录是状态目录本身。状态目录由调用方给（[`super::default_state_dir`] 是 iDoris 的惯例位置）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use super::launch::ENGINE_HOST;
use super::recipe::{EngineKind, EngineRecipe};
use super::record::OwnershipStore;

/// 状态目录下放所有权记录的那一级。
pub const ENGINES_OWNED_DIR: &str = "engines-owned";
/// 状态目录下放引擎日志的那一级。
pub const LOGS_DIR: &str = "logs";
/// 所有权记录文件的扩展名。
pub const RECORD_EXTENSION: &str = "json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InstanceError {
    #[error("state directory must be an absolute path")]
    StateDirNotAbsolute,
    #[error("engine port must be non-zero")]
    PortZero,
    /// `unknown` 表示「无法识别种类」：这样的引擎没有配方可言，不能被管理。
    #[error("an engine of unknown kind cannot be managed")]
    UnknownKind,
}

/// 一个被管理的引擎实例：哪种引擎（配方）、监听哪个端口、状态放哪（状态目录）。
#[derive(Clone)]
pub struct EngineInstance {
    recipe: Arc<dyn EngineRecipe>,
    kind: EngineKind,
    state_dir: PathBuf,
    port: u16,
}

impl EngineInstance {
    pub fn new(
        recipe: Arc<dyn EngineRecipe>,
        state_dir: &Path,
        port: u16,
    ) -> Result<Self, InstanceError> {
        if !state_dir.is_absolute() {
            return Err(InstanceError::StateDirNotAbsolute);
        }
        if port == 0 {
            return Err(InstanceError::PortZero);
        }
        let kind = recipe.kind();
        if kind.is_unknown() {
            return Err(InstanceError::UnknownKind);
        }
        Ok(Self {
            recipe,
            kind,
            state_dir: state_dir.to_owned(),
            port,
        })
    }

    /// 配方的种类（写进所有权记录的那个）。
    pub fn kind(&self) -> &EngineKind {
        &self.kind
    }

    pub fn recipe(&self) -> &Arc<dyn EngineRecipe> {
        &self.recipe
    }

    /// 状态目录（也是引擎的工作目录）。
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>`。
    pub fn endpoint(&self) -> String {
        format!("http://{ENGINE_HOST}:{}", self.port)
    }

    /// 这个种类的所有权记录目录：`<状态目录>/engines-owned/<engine_kind>`。
    pub fn record_dir(&self) -> PathBuf {
        record_dir(&self.state_dir, &self.kind)
    }

    /// 这个实例的所有权记录：`<记录目录>/<endpoint 编码>.json`。
    pub fn record_path(&self) -> PathBuf {
        self.record_dir().join(record_file_name(&self.endpoint()))
    }

    /// 这个种类的引擎日志：`<状态目录>/logs/<engine_kind>.log`。
    pub fn log_path(&self) -> PathBuf {
        log_path(&self.state_dir, &self.kind)
    }

    /// 这个实例的所有权记录文件。
    pub fn store(&self) -> OwnershipStore {
        OwnershipStore::new(&self.state_dir, &self.kind, self.port)
    }
}

impl std::fmt::Debug for EngineInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineInstance")
            .field("kind", &self.kind)
            .field("state_dir", &self.state_dir)
            .field("port", &self.port)
            .finish()
    }
}

/// `<状态目录>/engines-owned/<engine_kind>`。
pub(crate) fn record_dir(state_dir: &Path, kind: &EngineKind) -> PathBuf {
    state_dir.join(ENGINES_OWNED_DIR).join(kind.as_str())
}

/// `<状态目录>/logs/<engine_kind>.log`。
pub(crate) fn log_path(state_dir: &Path, kind: &EngineKind) -> PathBuf {
    state_dir
        .join(LOGS_DIR)
        .join(format!("{}.log", kind.as_str()))
}

/// endpoint 的整体百分号编码（编码集同 oMLX 加载围栏的目录名：`NON_ALPHANUMERIC`）。
pub(crate) fn endpoint_encoding(endpoint: &str) -> String {
    utf8_percent_encode(endpoint, NON_ALPHANUMERIC).to_string()
}

/// `<endpoint 编码>.json`。
pub(crate) fn record_file_name(endpoint: &str) -> String {
    format!("{}.{RECORD_EXTENSION}", endpoint_encoding(endpoint))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::engine::test_support::TestRecipe;

    fn instance(kind: &str, port: u16) -> EngineInstance {
        EngineInstance::new(
            Arc::new(TestRecipe::new(kind)),
            Path::new("/var/state/idoris"),
            port,
        )
        .unwrap()
    }

    #[test]
    fn the_layout_is_the_specs_kind_directories_endpoint_files_and_kind_logs() {
        let a = instance("omlx", 19731);
        assert_eq!(a.endpoint(), "http://127.0.0.1:19731");
        assert_eq!(
            a.record_dir(),
            Path::new("/var/state/idoris/engines-owned/omlx")
        );
        assert_eq!(
            a.record_path(),
            Path::new(
                "/var/state/idoris/engines-owned/omlx/http%3A%2F%2F127%2E0%2E0%2E1%3A19731.json"
            )
        );
        assert_eq!(a.log_path(), Path::new("/var/state/idoris/logs/omlx.log"));
        assert_eq!(a.state_dir(), Path::new("/var/state/idoris"));
        assert_eq!(a.kind().as_str(), "omlx");
        assert_eq!(a.store().path(), a.record_path());
    }

    #[test]
    fn instances_of_one_kind_share_a_directory_and_a_log_but_not_a_record() {
        let (a, b) = (instance("omlx", 19731), instance("omlx", 19732));
        assert_eq!(a.record_dir(), b.record_dir());
        assert_eq!(a.log_path(), b.log_path(), "the log is named by the kind");
        assert_ne!(a.record_path(), b.record_path(), "one record per endpoint");
    }

    #[test]
    fn different_kinds_share_nothing() {
        let (a, b) = (instance("omlx", 19731), instance("ollama", 19731));
        assert_ne!(a.record_dir(), b.record_dir());
        assert_ne!(a.record_path(), b.record_path());
        assert_ne!(a.log_path(), b.log_path());
        // Even for the very same endpoint: the record directory is per kind.
        assert_eq!(
            a.record_path().file_name(),
            b.record_path().file_name(),
            "the file name is the endpoint's encoding, nothing else"
        );
    }

    #[test]
    fn an_instance_needs_an_absolute_state_dir_a_port_and_a_known_kind() {
        let recipe = |kind: &str| -> Arc<dyn EngineRecipe> { Arc::new(TestRecipe::new(kind)) };
        assert_eq!(
            EngineInstance::new(recipe("fake"), Path::new("state"), 1).unwrap_err(),
            InstanceError::StateDirNotAbsolute
        );
        assert_eq!(
            EngineInstance::new(recipe("fake"), Path::new("/state"), 0).unwrap_err(),
            InstanceError::PortZero
        );
        assert_eq!(
            EngineInstance::new(recipe("unknown"), Path::new("/state"), 1).unwrap_err(),
            InstanceError::UnknownKind
        );
        // A kind this version has never heard of is fine: the set is open.
        assert!(EngineInstance::new(recipe("future_engine"), Path::new("/state"), 1).is_ok());
    }
}

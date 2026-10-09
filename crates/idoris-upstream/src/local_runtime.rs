//! Validated launch contract for single-model local HTTP runtimes.
//!
//! This module does not spawn processes. It owns the stable, shell-free argv
//! contract that B8's platform process adapter executes. Binding is fixed to
//! loopback so a runtime fallback cannot silently widen its network surface.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use idoris_backend::BackendError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRuntimeKind {
    MlxLmServer,
    LlamaCpp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRuntimeLaunch {
    kind: LocalRuntimeKind,
    executable: PathBuf,
    model_path: PathBuf,
    port: u16,
}

impl LocalRuntimeLaunch {
    pub fn new(
        kind: LocalRuntimeKind,
        executable: impl Into<PathBuf>,
        model_path: impl Into<PathBuf>,
        port: u16,
    ) -> Result<Self, BackendError> {
        let executable = executable.into();
        let model_path = model_path.into();
        if executable.as_os_str().is_empty() {
            return Err(BackendError::internal("local runtime executable is empty"));
        }
        if !model_path.is_absolute() {
            return Err(BackendError::internal(
                "local runtime model path must be absolute",
            ));
        }
        if port == 0 {
            return Err(BackendError::internal(
                "local runtime port must be non-zero",
            ));
        }
        if kind == LocalRuntimeKind::LlamaCpp && model_path.extension() != Some(OsStr::new("gguf"))
        {
            return Err(BackendError::internal(
                "llama.cpp model path must point to a .gguf artifact",
            ));
        }
        Ok(Self {
            kind,
            executable,
            model_path,
            port,
        })
    }

    pub fn kind(&self) -> LocalRuntimeKind {
        self.kind
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Exact argv passed directly to `Command`; no shell is involved.
    pub fn args(&self) -> Vec<OsString> {
        let port = OsString::from(self.port.to_string());
        match self.kind {
            LocalRuntimeKind::MlxLmServer => vec![
                "-m".into(),
                "mlx_lm.server".into(),
                "--model".into(),
                self.model_path.as_os_str().to_owned(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port,
            ],
            LocalRuntimeKind::LlamaCpp => vec![
                "--model".into(),
                self.model_path.as_os_str().to_owned(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port,
            ],
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn mlx_launch_is_loopback_and_shell_free_argv() {
        let launch = LocalRuntimeLaunch::new(
            LocalRuntimeKind::MlxLmServer,
            "python3",
            "/models/qwen-mlx",
            8081,
        )
        .unwrap();
        assert_eq!(launch.endpoint(), "http://127.0.0.1:8081");
        assert_eq!(
            strings(launch.args()),
            [
                "-m",
                "mlx_lm.server",
                "--model",
                "/models/qwen-mlx",
                "--host",
                "127.0.0.1",
                "--port",
                "8081",
            ]
        );
    }

    #[test]
    fn llama_launch_requires_gguf_and_loopback() {
        let launch = LocalRuntimeLaunch::new(
            LocalRuntimeKind::LlamaCpp,
            "llama-server",
            "/models/qwen.gguf",
            8082,
        )
        .unwrap();
        assert_eq!(launch.endpoint(), "http://127.0.0.1:8082");
        assert_eq!(
            strings(launch.args()),
            [
                "--model",
                "/models/qwen.gguf",
                "--host",
                "127.0.0.1",
                "--port",
                "8082",
            ]
        );
    }

    #[test]
    fn invalid_launch_specs_fail_closed() {
        for result in [
            LocalRuntimeLaunch::new(LocalRuntimeKind::MlxLmServer, "", "/models/qwen-mlx", 8081),
            LocalRuntimeLaunch::new(
                LocalRuntimeKind::MlxLmServer,
                "python3",
                "relative/model",
                8081,
            ),
            LocalRuntimeLaunch::new(
                LocalRuntimeKind::LlamaCpp,
                "llama-server",
                "/models/qwen.bin",
                8082,
            ),
            LocalRuntimeLaunch::new(
                LocalRuntimeKind::LlamaCpp,
                "llama-server",
                "/models/qwen.gguf",
                0,
            ),
        ] {
            assert!(result.is_err());
        }
    }
}

//! Validated launch contract for single-model local HTTP runtimes.
//!
//! This module does not spawn processes. It owns the stable, shell-free argv
//! contract that the platform process adapter executes. Binding is fixed to
//! loopback so a runtime fallback cannot silently widen its network surface.
//!
//! A vLLM Hugging Face repo id is not an offline launch. vLLM downloads that
//! commit on first start unless the cache already has it. An absolute model
//! path is the local artifact and does not pass `--revision`. `memory_gb`
//! is the GPU reserve: vLLM receives it as
//! `--gpu-memory-utilization reserve/gpu_total_gib`. A Mac adapter test is
//! not Linux CUDA acceptance.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use idoris_backend::BackendError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRuntimeKind {
    MlxLmServer,
    LlamaCpp,
    Vllm,
}

/// Inputs for one `vllm serve` process. One process serves one model.
#[derive(Debug, Clone, PartialEq)]
pub struct VllmLaunch {
    pub executable: PathBuf,
    pub model: String,
    pub port: u16,
    pub served_model_name: String,
    pub engine_version: String,
    pub gpu_total_gib: f64,
    pub reserve_gib: f64,
    pub model_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum LaunchBody {
    MlxLm {
        model_path: PathBuf,
    },
    Llama {
        model_path: PathBuf,
    },
    Vllm {
        model_arg: OsString,
        artifact: Option<PathBuf>,
        served_model_name: String,
        engine_version: String,
        reserve_gib: f64,
        revision: Option<String>,
        gpu_memory_utilization: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalRuntimeLaunch {
    executable: PathBuf,
    port: u16,
    body: LaunchBody,
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
        let body = match kind {
            LocalRuntimeKind::MlxLmServer => LaunchBody::MlxLm { model_path },
            LocalRuntimeKind::LlamaCpp => LaunchBody::Llama { model_path },
            LocalRuntimeKind::Vllm => {
                return Err(BackendError::internal(
                    "vllm launch requires an engine version, a model pin, and a GPU reserve",
                ));
            }
        };
        Ok(Self {
            executable,
            port,
            body,
        })
    }

    pub fn vllm(spec: VllmLaunch) -> Result<Self, BackendError> {
        if spec.executable.as_os_str().is_empty() {
            return Err(BackendError::internal("local runtime executable is empty"));
        }
        if spec.port == 0 {
            return Err(BackendError::internal(
                "local runtime port must be non-zero",
            ));
        }
        if spec.served_model_name.trim().is_empty() {
            return Err(BackendError::internal("vllm served model name is empty"));
        }
        if !is_exact_engine_version(&spec.engine_version) {
            return Err(BackendError::internal(
                "vllm engine_version must be an exact x.y.z pin",
            ));
        }
        let (model_arg, artifact, revision) =
            parse_vllm_model(&spec.model, spec.model_revision.as_deref())?;
        let gpu_memory_utilization = gpu_utilization(spec.reserve_gib, spec.gpu_total_gib)?;
        Ok(Self {
            executable: spec.executable,
            port: spec.port,
            body: LaunchBody::Vllm {
                model_arg,
                artifact,
                served_model_name: spec.served_model_name,
                engine_version: spec.engine_version,
                reserve_gib: spec.reserve_gib,
                revision,
                gpu_memory_utilization,
            },
        })
    }

    pub fn kind(&self) -> LocalRuntimeKind {
        match self.body {
            LaunchBody::MlxLm { .. } => LocalRuntimeKind::MlxLmServer,
            LaunchBody::Llama { .. } => LocalRuntimeKind::LlamaCpp,
            LaunchBody::Vllm { .. } => LocalRuntimeKind::Vllm,
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn model_path(&self) -> Option<&Path> {
        match &self.body {
            LaunchBody::MlxLm { model_path } | LaunchBody::Llama { model_path } => Some(model_path),
            LaunchBody::Vllm { artifact, .. } => artifact.as_deref(),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn engine_version(&self) -> Option<&str> {
        match &self.body {
            LaunchBody::Vllm { engine_version, .. } => Some(engine_version),
            _ => None,
        }
    }

    pub fn gpu_reserve_gib(&self) -> Option<f64> {
        match &self.body {
            LaunchBody::Vllm { reserve_gib, .. } => Some(*reserve_gib),
            _ => None,
        }
    }

    pub fn served_model_name(&self) -> Option<&str> {
        match &self.body {
            LaunchBody::Vllm {
                served_model_name, ..
            } => Some(served_model_name),
            _ => None,
        }
    }

    /// Model id that readiness must see. Other engines keep the historical
    /// non-empty `/v1/models` check.
    pub fn readiness_model_id(&self) -> Option<&str> {
        self.served_model_name()
    }

    pub fn chat_model<'a>(&'a self, configured_model_id: &'a str) -> &'a str {
        match &self.body {
            LaunchBody::MlxLm { .. } => "default_model",
            LaunchBody::Llama { .. } => configured_model_id,
            LaunchBody::Vllm {
                served_model_name, ..
            } => served_model_name,
        }
    }

    /// Exact argv passed directly to `Command`; no shell is involved.
    /// The bearer secret is not included. [`Self::args_with_api_key`] adds it.
    pub fn args(&self) -> Vec<OsString> {
        let port = OsString::from(self.port.to_string());
        match &self.body {
            LaunchBody::MlxLm { model_path } => vec![
                "-m".into(),
                "mlx_lm.server".into(),
                "--model".into(),
                model_path.as_os_str().to_owned(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port,
            ],
            LaunchBody::Llama { model_path } => vec![
                "--model".into(),
                model_path.as_os_str().to_owned(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port,
            ],
            LaunchBody::Vllm {
                model_arg,
                served_model_name,
                revision,
                gpu_memory_utilization,
                ..
            } => {
                let mut args = vec![
                    "serve".into(),
                    model_arg.clone(),
                    "--host".into(),
                    "127.0.0.1".into(),
                    "--port".into(),
                    port,
                    "--served-model-name".into(),
                    served_model_name.into(),
                ];
                if let Some(revision) = revision {
                    args.push("--revision".into());
                    args.push(revision.into());
                }
                args.push("--gpu-memory-utilization".into());
                args.push(gpu_memory_utilization.into());
                args
            }
        }
    }

    pub fn args_with_api_key(&self, api_key: Option<&str>) -> Result<Vec<OsString>, BackendError> {
        let mut args = self.args();
        if !matches!(self.body, LaunchBody::Vllm { .. }) {
            return Ok(args);
        }
        let Some(api_key) = api_key else {
            return Ok(args);
        };
        if api_key.is_empty()
            || !api_key.is_ascii()
            || api_key.bytes().any(|byte| !(0x21..=0x7e).contains(&byte))
        {
            return Err(BackendError::internal(
                "vllm --api-key must be the bearer secret",
            ));
        }
        args.push("--api-key".into());
        args.push(api_key.into());
        Ok(args)
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn parse_vllm_model(
    model: &str,
    revision: Option<&str>,
) -> Result<(OsString, Option<PathBuf>, Option<String>), BackendError> {
    if model.is_empty() || model.chars().any(char::is_whitespace) || model.starts_with('-') {
        return Err(BackendError::internal(
            "vllm model must be an absolute path or a Hugging Face repo id",
        ));
    }
    let path = Path::new(model);
    if path.is_absolute() {
        if revision.is_some() {
            return Err(BackendError::internal(
                "vllm local artifact is the pin; model_revision is only for a Hugging Face repo id",
            ));
        }
        return Ok((OsString::from(model), Some(path.to_path_buf()), None));
    }
    if !is_hf_repo_id(model) {
        return Err(BackendError::internal(
            "vllm model must be an absolute path or a Hugging Face repo id org/name",
        ));
    }
    let Some(revision) = revision else {
        return Err(BackendError::internal(
            "vllm Hugging Face repo id requires a 40-character commit revision; vLLM downloads that revision on first start, so a repo id is not an offline launch",
        ));
    };
    if !is_commit_revision(revision) {
        return Err(BackendError::internal(
            "vllm model_revision must be a 40-character lowercase commit; a branch or tag is not a fixed pin",
        ));
    }
    Ok((OsString::from(model), None, Some(revision.to_string())))
}

fn is_hf_repo_id(model: &str) -> bool {
    let Some((org, name)) = model.split_once('/') else {
        return false;
    };
    !name.contains('/') && is_repo_segment(org) && is_repo_segment(name)
}

fn is_repo_segment(segment: &str) -> bool {
    if segment.is_empty() || segment == "." || segment == ".." {
        return false;
    }
    segment
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_commit_revision(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_exact_engine_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && is_numeric_component(major)
        && is_numeric_component(minor)
        && is_numeric_component(patch)
}

fn is_numeric_component(part: &str) -> bool {
    let Some(first) = part.as_bytes().first() else {
        return false;
    };
    if part.len() > 1 && *first == b'0' {
        return false;
    }
    part.bytes().all(|byte| byte.is_ascii_digit())
}

fn gpu_utilization(reserve_gib: f64, total_gib: f64) -> Result<String, BackendError> {
    if !reserve_gib.is_finite()
        || !total_gib.is_finite()
        || reserve_gib <= 0.0
        || total_gib <= 0.0
        || reserve_gib > total_gib
    {
        return Err(BackendError::internal(
            "vllm memory reserve must be finite, greater than 0, and no larger than gpu_total_gib; it is the GPU reservation passed as --gpu-memory-utilization",
        ));
    }
    let fraction = reserve_gib / total_gib;
    let rendered = format!("{fraction:.6}");
    let trimmed = rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string();
    if trimmed.is_empty() || trimmed == "0" {
        return Err(BackendError::internal(
            "vllm GPU reservation is too small to pass to the engine",
        ));
    }
    Ok(trimmed)
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

    fn revision() -> String {
        "a".repeat(40)
    }

    fn vllm_spec(model: &str, revision: Option<String>) -> VllmLaunch {
        VllmLaunch {
            executable: PathBuf::from("vllm"),
            model: model.to_string(),
            port: 8083,
            served_model_name: "served-id".to_string(),
            engine_version: "0.11.0".to_string(),
            gpu_total_gib: 24.0,
            reserve_gib: 18.0,
            model_revision: revision,
        }
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
    fn vllm_absolute_artifact_argv_is_loopback_and_has_no_shell() {
        let launch = LocalRuntimeLaunch::vllm(vllm_spec("/models/qwen.gguf", None)).unwrap();
        assert_eq!(launch.kind(), LocalRuntimeKind::Vllm);
        assert_eq!(launch.engine_version(), Some("0.11.0"));
        assert_eq!(launch.gpu_reserve_gib(), Some(18.0));
        assert_eq!(launch.model_path(), Some(Path::new("/models/qwen.gguf")));
        assert_eq!(launch.endpoint(), "http://127.0.0.1:8083");
        assert_eq!(
            strings(launch.args()),
            [
                "serve",
                "/models/qwen.gguf",
                "--host",
                "127.0.0.1",
                "--port",
                "8083",
                "--served-model-name",
                "served-id",
                "--gpu-memory-utilization",
                "0.75",
            ]
        );
        assert!(
            !strings(launch.args())
                .iter()
                .any(|arg| arg == "sh" || arg == "-c")
        );
    }

    #[test]
    fn vllm_repo_id_pins_revision_and_api_key_matches_the_supplied_secret() {
        let launch =
            LocalRuntimeLaunch::vllm(vllm_spec("Qwen/Qwen2.5-7B-Instruct", Some(revision())))
                .unwrap();
        assert_eq!(launch.model_path(), None);
        assert_eq!(
            strings(launch.args()),
            [
                "serve",
                "Qwen/Qwen2.5-7B-Instruct",
                "--host",
                "127.0.0.1",
                "--port",
                "8083",
                "--served-model-name",
                "served-id",
                "--revision",
                &revision(),
                "--gpu-memory-utilization",
                "0.75",
            ]
        );
        let secret = "fixture-token";
        let with_key = strings(launch.args_with_api_key(Some(secret)).unwrap());
        assert_eq!(with_key[with_key.len() - 2], "--api-key");
        assert_eq!(with_key.last().map(String::as_str), Some(secret));
        assert!(!strings(launch.args()).iter().any(|arg| arg == secret));
    }

    #[test]
    fn vllm_repo_and_pin_negatives_fail_closed() {
        let rejected = [
            (vllm_spec("./models/qwen", None), "absolute path"),
            (vllm_spec("org/name/extra", Some(revision())), "org/name"),
            (vllm_spec("../secret", Some(revision())), "org/name"),
            (vllm_spec("org/name", None), "not an offline launch"),
            (
                vllm_spec("org/name", Some("main".to_string())),
                "lowercase commit",
            ),
            (
                vllm_spec("org/name", Some("A".repeat(40))),
                "lowercase commit",
            ),
            (
                vllm_spec("/models/qwen", Some(revision())),
                "local artifact is the pin",
            ),
            (vllm_spec("--help", None), "absolute path"),
            (
                {
                    let mut spec = vllm_spec("/models/qwen", None);
                    spec.engine_version = "0.11".to_string();
                    spec
                },
                "x.y.z",
            ),
            (
                {
                    let mut spec = vllm_spec("/models/qwen", None);
                    spec.engine_version = "v0.11.0".to_string();
                    spec
                },
                "x.y.z",
            ),
            (
                {
                    let mut spec = vllm_spec("/models/qwen", None);
                    spec.reserve_gib = 30.0;
                    spec
                },
                "GPU reservation",
            ),
        ];
        for (spec, needle) in rejected {
            let message = LocalRuntimeLaunch::vllm(spec).unwrap_err().to_string();
            assert!(message.contains(needle), "{message}");
        }
        assert!(
            LocalRuntimeLaunch::new(LocalRuntimeKind::Vllm, "vllm", "/models/qwen", 1)
                .unwrap_err()
                .to_string()
                .contains("GPU reserve")
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

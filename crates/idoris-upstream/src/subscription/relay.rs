use std::collections::BTreeMap;
use std::ffi::{OsString, OsString as EnvironmentValue};
use std::time::Duration;

use idoris_backend::{ChatRequest, ChatResponse, ModelInfo};
use tokio_util::sync::CancellationToken;

use super::command::build_command;
use super::environment::{sanitize_environment, sanitized_process_environment};
use super::error::{SubscriptionDiagnostics, SubscriptionErrorCode, SubscriptionRelayError};
use super::output::select_output;
use super::process::{
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_PROCESS_TIMEOUT, DEFAULT_TERMINATION_GRACE,
    run_process_controlled,
};
use super::profile::{SandboxProfile, SubscriptionCli};
use super::workspace::SubscriptionWorkspace;

#[derive(Debug, Clone)]
pub struct SubscriptionRelayConfig {
    pub profile: SandboxProfile,
    pub process_timeout: Duration,
    pub termination_grace: Duration,
    pub max_output_bytes: usize,
    environment: BTreeMap<OsString, EnvironmentValue>,
}

impl SubscriptionRelayConfig {
    pub fn from_process(profile: SandboxProfile) -> Self {
        Self {
            profile,
            process_timeout: DEFAULT_PROCESS_TIMEOUT,
            termination_grace: DEFAULT_TERMINATION_GRACE,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            environment: sanitized_process_environment(),
        }
    }

    pub fn with_environment<I, K, V>(profile: SandboxProfile, environment: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        Self {
            profile,
            process_timeout: DEFAULT_PROCESS_TIMEOUT,
            termination_grace: DEFAULT_TERMINATION_GRACE,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            environment: sanitize_environment(environment),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubscriptionRelay {
    config: SubscriptionRelayConfig,
}

impl SubscriptionRelay {
    pub fn new(config: SubscriptionRelayConfig) -> Self {
        Self { config }
    }

    pub fn model_id(&self) -> &'static str {
        match self.config.profile.cli {
            SubscriptionCli::Claude => "claude-subscription",
            SubscriptionCli::Codex => "codex-subscription",
        }
    }

    pub fn list(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model_id().to_string(),
            memory_gb: 0.0,
        }]
    }

    pub async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, SubscriptionRelayError> {
        let workspace = SubscriptionWorkspace::create()
            .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CleanupFailed))?;
        let result_path = workspace
            .result_file()
            .to_str()
            .ok_or_else(|| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
        let command = build_command(self.config.profile, &request.messages, result_path);
        let run = run_process_controlled(
            &command,
            workspace.cwd(),
            &self.config.environment,
            self.config.max_output_bytes,
            self.config.process_timeout,
            self.config.termination_grace,
            cancel,
        )
        .await;

        let result = match run {
            Ok(output) => {
                if output.exit_code != Some(0) {
                    Err(SubscriptionRelayError::with_diagnostics(
                        SubscriptionErrorCode::CliFailed,
                        SubscriptionDiagnostics::from_stderr(
                            output.exit_code,
                            output.stderr.as_bytes(),
                        ),
                    ))
                } else {
                    select_output(&workspace, &output.stdout, output.exit_code).map(|content| {
                        ChatResponse {
                            model: request.model,
                            content,
                        }
                    })
                }
            }
            Err(error) => Err(error),
        };

        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.code() == SubscriptionErrorCode::CleanupFailed)
        {
            return result;
        }
        workspace
            .cleanup_after_reap()
            .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CleanupFailed))?;
        result
    }
}

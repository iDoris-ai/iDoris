//! Stable, non-leaking subscription relay diagnostics.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionErrorCode {
    CliFailed,
    EmptyOutput,
    SpawnFailed,
    OutputLimit,
    Timeout,
    Cancelled,
    CleanupFailed,
}

impl SubscriptionErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CliFailed => "RELAY_CLI_FAILED",
            Self::EmptyOutput => "RELAY_EMPTY_OUTPUT",
            Self::SpawnFailed => "RELAY_SPAWN_FAILED",
            Self::OutputLimit => "RELAY_OUTPUT_LIMIT",
            Self::Timeout => "RELAY_TIMEOUT",
            Self::Cancelled => "RELAY_CANCELLED",
            Self::CleanupFailed => "RELAY_CLEANUP_FAILED",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionDiagnostics {
    pub exit_code: Option<i32>,
    pub stderr_bytes: usize,
}

impl SubscriptionDiagnostics {
    pub fn from_stderr(exit_code: Option<i32>, stderr: &[u8]) -> Self {
        Self {
            exit_code,
            stderr_bytes: stderr.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionRelayError {
    code: SubscriptionErrorCode,
    diagnostics: Option<SubscriptionDiagnostics>,
}

impl SubscriptionRelayError {
    pub const fn new(code: SubscriptionErrorCode) -> Self {
        Self {
            code,
            diagnostics: None,
        }
    }

    pub const fn with_diagnostics(
        code: SubscriptionErrorCode,
        diagnostics: SubscriptionDiagnostics,
    ) -> Self {
        Self {
            code,
            diagnostics: Some(diagnostics),
        }
    }

    pub const fn code(&self) -> SubscriptionErrorCode {
        self.code
    }

    pub const fn reason_code(&self) -> &'static str {
        self.code.as_str()
    }

    pub const fn diagnostics(&self) -> Option<SubscriptionDiagnostics> {
        self.diagnostics
    }

    /// Safe for an HTTP error envelope; never includes CLI stderr/prompt text.
    pub const fn public_message(&self) -> &'static str {
        "subscription relay failed"
    }
}

impl std::fmt::Display for SubscriptionRelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "subscription relay failed ({})", self.reason_code())
    }
}

impl std::error::Error for SubscriptionRelayError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn reason_codes_are_fixed_and_complete() {
        let cases = [
            (SubscriptionErrorCode::CliFailed, "RELAY_CLI_FAILED"),
            (SubscriptionErrorCode::EmptyOutput, "RELAY_EMPTY_OUTPUT"),
            (SubscriptionErrorCode::SpawnFailed, "RELAY_SPAWN_FAILED"),
            (SubscriptionErrorCode::OutputLimit, "RELAY_OUTPUT_LIMIT"),
            (SubscriptionErrorCode::Timeout, "RELAY_TIMEOUT"),
            (SubscriptionErrorCode::Cancelled, "RELAY_CANCELLED"),
            (SubscriptionErrorCode::CleanupFailed, "RELAY_CLEANUP_FAILED"),
        ];
        for (code, expected) in cases {
            assert_eq!(code.as_str(), expected);
        }
    }

    #[test]
    fn stderr_and_prompt_sentinels_never_enter_display_debug_or_public_message() {
        let stderr = b"boom SECRET_STDERR_SENTINEL prompt=SECRET_PROMPT_SENTINEL";
        let diagnostics = SubscriptionDiagnostics::from_stderr(Some(3), stderr);
        let error =
            SubscriptionRelayError::with_diagnostics(SubscriptionErrorCode::CliFailed, diagnostics);

        assert_eq!(error.diagnostics().unwrap().stderr_bytes, stderr.len());
        let rendered = format!("{error} {error:?} {}", error.public_message());
        assert!(!rendered.contains("SECRET_STDERR_SENTINEL"));
        assert!(!rendered.contains("SECRET_PROMPT_SENTINEL"));
        assert!(!rendered.contains("boom"));
        assert_eq!(error.reason_code(), "RELAY_CLI_FAILED");
    }
}

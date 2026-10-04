use std::collections::BTreeSet;

pub const SANDBOX_PROFILE_ID: &str = "idoris-subscription-no-tools-readonly-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionCli {
    Claude,
    Codex,
}

impl SubscriptionCli {
    pub const fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxProfile {
    pub id: &'static str,
    pub cli: SubscriptionCli,
    pub tools_off: bool,
    pub workspace_read_only: bool,
}

impl SandboxProfile {
    pub const fn fixed(cli: SubscriptionCli) -> Self {
        Self {
            id: SANDBOX_PROFILE_ID,
            cli,
            tools_off: true,
            workspace_read_only: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliCapabilities {
    pub tools_off: bool,
    pub supported_flags: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    ToolsOffUnproven,
    MissingFlag(&'static str),
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolsOffUnproven => f.write_str("CLI no-tools capability is not proven"),
            Self::MissingFlag(flag) => write!(f, "CLI is missing required safety flag {flag}"),
        }
    }
}

impl std::error::Error for CapabilityError {}

pub const fn required_flags(cli: SubscriptionCli) -> &'static [&'static str] {
    match cli {
        SubscriptionCli::Claude => &[
            "-p",
            "--output-format",
            "--tools",
            "--restricted",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--permission-prompts",
        ],
        SubscriptionCli::Codex => &[
            "exec",
            "--sandbox",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--color",
            "-o",
        ],
    }
}

pub fn validate_cli_capabilities(
    profile: SandboxProfile,
    capabilities: &CliCapabilities,
) -> Result<(), CapabilityError> {
    if profile.tools_off && !capabilities.tools_off {
        return Err(CapabilityError::ToolsOffUnproven);
    }
    for flag in required_flags(profile.cli) {
        if !capabilities.supported_flags.contains(*flag) {
            return Err(CapabilityError::MissingFlag(flag));
        }
    }
    Ok(())
}

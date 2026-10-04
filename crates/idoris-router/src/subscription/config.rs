pub const SANDBOX_PROFILE_ID: &str = "idoris-subscription-no-tools-readonly-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionCli {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    pub id: &'static str,
    pub cli: SubscriptionCli,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionConfig {
    pub deploy_mode: String,
    pub enabled: bool,
    pub disabled: bool,
    pub sandbox: Option<String>,
    pub cli: Option<String>,
}

impl SubscriptionConfig {
    pub fn snapshot(
        deploy_mode: Option<&str>,
        enable: Option<&str>,
        disable: Option<&str>,
        sandbox: Option<&str>,
        cli: Option<&str>,
    ) -> Self {
        let mode = deploy_mode.unwrap_or("").trim().to_ascii_lowercase();
        Self {
            deploy_mode: if mode.is_empty() {
                "personal".into()
            } else {
                mode
            },
            enabled: enable == Some("1"),
            disabled: disable == Some("1"),
            sandbox: sandbox
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            cli: cli
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_ascii_lowercase()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationAction {
    Register,
    Skip,
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationDecision {
    pub action: RegistrationAction,
    pub reason: &'static str,
    pub sandbox: Option<SandboxProfile>,
}

pub fn decide_registration(config: &SubscriptionConfig) -> RegistrationDecision {
    if config.deploy_mode != "personal" {
        return decision(RegistrationAction::Refuse, "subscription is personal-only");
    }
    if config.disabled {
        return decision(RegistrationAction::Skip, "subscription explicitly disabled");
    }
    if !config.enabled {
        return decision(RegistrationAction::Skip, "subscription disabled by default");
    }
    if config.sandbox.as_deref() != Some(SANDBOX_PROFILE_ID) {
        return decision(
            RegistrationAction::Refuse,
            "subscription sandbox profile missing or unknown",
        );
    }
    let cli = match config.cli.as_deref().unwrap_or("claude") {
        "claude" => SubscriptionCli::Claude,
        "codex" => SubscriptionCli::Codex,
        _ => {
            return decision(
                RegistrationAction::Refuse,
                "subscription CLI is unsupported",
            );
        }
    };
    RegistrationDecision {
        action: RegistrationAction::Register,
        reason: "personal + enabled + validated sandbox",
        sandbox: Some(SandboxProfile {
            id: SANDBOX_PROFILE_ID,
            cli,
        }),
    }
}

fn decision(action: RegistrationAction, reason: &'static str) -> RegistrationDecision {
    RegistrationDecision {
        action,
        reason,
        sandbox: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(
        mode: Option<&str>,
        enable: Option<&str>,
        disable: Option<&str>,
        sandbox: Option<&str>,
        cli: Option<&str>,
    ) -> SubscriptionConfig {
        SubscriptionConfig::snapshot(mode, enable, disable, sandbox, cli)
    }

    #[test]
    fn registration_truth_table_is_fail_closed() {
        let ok = Some(SANDBOX_PROFILE_ID);
        for (input, expected) in [
            (cfg(None, None, None, None, None), RegistrationAction::Skip),
            (
                cfg(Some("personal"), Some("1"), Some("1"), ok, None),
                RegistrationAction::Skip,
            ),
            (
                cfg(Some("tenant"), Some("1"), Some("1"), ok, None),
                RegistrationAction::Refuse,
            ),
            (
                cfg(Some("community"), None, Some("1"), ok, None),
                RegistrationAction::Refuse,
            ),
            (
                cfg(Some("city"), Some("1"), None, ok, None),
                RegistrationAction::Refuse,
            ),
            (
                cfg(Some("unknown"), Some("1"), None, ok, None),
                RegistrationAction::Refuse,
            ),
            (
                cfg(Some("personal"), Some("1"), None, None, None),
                RegistrationAction::Refuse,
            ),
            (
                cfg(Some("personal"), Some("1"), None, Some("wrong"), None),
                RegistrationAction::Refuse,
            ),
            (
                cfg(Some("personal"), Some("1"), None, ok, Some("gemini")),
                RegistrationAction::Refuse,
            ),
            (
                cfg(Some("personal"), Some("1"), None, ok, None),
                RegistrationAction::Register,
            ),
            (
                cfg(Some(" PERSONAL "), Some("1"), None, ok, Some("CODEX")),
                RegistrationAction::Register,
            ),
        ] {
            assert_eq!(decide_registration(&input).action, expected, "{input:?}");
        }
    }

    #[test]
    fn valid_registration_carries_a_typed_snapshot() {
        let config = cfg(
            None,
            Some("1"),
            None,
            Some(SANDBOX_PROFILE_ID),
            Some("codex"),
        );
        let decision = decide_registration(&config);
        assert_eq!(config.deploy_mode, "personal");
        assert_eq!(
            decision.sandbox,
            Some(SandboxProfile {
                id: SANDBOX_PROFILE_ID,
                cli: SubscriptionCli::Codex,
            })
        );
    }
}

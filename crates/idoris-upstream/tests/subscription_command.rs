use std::collections::BTreeSet;

use idoris_backend::ChatMessage;
use idoris_upstream::subscription::command::{build_command, build_prompt};
use idoris_upstream::subscription::profile::{
    CapabilityError, CliCapabilities, SandboxProfile, SubscriptionCli, required_flags,
    validate_cli_capabilities,
};

fn message(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: content.into(),
    }
}

#[test]
fn claude_and_codex_use_exact_fixed_safety_arguments() {
    let prompt = [message("user", "-not-a-flag")];
    let claude = build_command(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        &prompt,
        "/tmp/out",
    );
    assert_eq!(claude.program, "claude");
    assert_eq!(
        claude.args,
        [
            "-p",
            "--output-format",
            "text",
            "--tools",
            "",
            "--restricted",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--permission-prompts",
            "none",
        ]
    );
    assert_eq!(claude.stdin.as_deref(), Some("-not-a-flag"));

    let codex = build_command(
        SandboxProfile::fixed(SubscriptionCli::Codex),
        &prompt,
        "/tmp/out",
    );
    assert_eq!(codex.program, "codex");
    assert_eq!(
        codex.args,
        [
            "exec",
            "--sandbox",
            "read-only",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--color",
            "never",
            "-o",
            "/tmp/out",
            "-",
        ]
    );
    assert_eq!(codex.stdin.as_deref(), Some("-not-a-flag"));
    assert!(SandboxProfile::fixed(SubscriptionCli::Claude).tools_off);
    assert!(!SandboxProfile::fixed(SubscriptionCli::Codex).tools_off);
}

#[test]
fn multi_message_prompt_preserves_roles_and_order() {
    let messages = [
        message("system", "rules"),
        message("user", "hello"),
        message("assistant", "prior"),
        message("user", "next"),
    ];
    assert_eq!(
        build_prompt(&messages),
        "system: rules\n\nuser: hello\n\nassistant: prior\n\nuser: next"
    );
}

#[test]
fn unsupported_safety_capability_fails_closed_without_flag_stripping() {
    let profile = SandboxProfile::fixed(SubscriptionCli::Claude);
    let all = required_flags(profile.cli)
        .iter()
        .map(|flag| (*flag).to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        validate_cli_capabilities(
            profile,
            &CliCapabilities {
                tools_off: true,
                supported_flags: all.clone()
            },
        ),
        Ok(())
    );
    assert_eq!(
        validate_cli_capabilities(
            profile,
            &CliCapabilities {
                tools_off: false,
                supported_flags: all.clone()
            },
        ),
        Err(CapabilityError::ToolsOffUnproven)
    );
    let mut missing = all;
    missing.remove("--tools");
    assert_eq!(
        validate_cli_capabilities(
            profile,
            &CliCapabilities {
                tools_off: true,
                supported_flags: missing
            },
        ),
        Err(CapabilityError::MissingFlag("--tools"))
    );

    let codex = SandboxProfile::fixed(SubscriptionCli::Codex);
    let codex_flags = required_flags(codex.cli)
        .iter()
        .map(|flag| (*flag).to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        validate_cli_capabilities(
            codex,
            &CliCapabilities {
                tools_off: false,
                supported_flags: codex_flags,
            },
        ),
        Ok(())
    );
}

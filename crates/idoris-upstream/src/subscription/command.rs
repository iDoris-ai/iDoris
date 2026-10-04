use idoris_backend::ChatMessage;

use super::profile::{SandboxProfile, SubscriptionCli};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: &'static str,
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

pub fn build_prompt(messages: &[ChatMessage]) -> String {
    if let [only] = messages
        && only.role == "user"
    {
        return only.content.clone();
    }
    messages
        .iter()
        .map(|message| format!("{}: {}", message.role, message.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn build_command(
    profile: SandboxProfile,
    messages: &[ChatMessage],
    output_file: &str,
) -> CommandSpec {
    let prompt = build_prompt(messages);
    match profile.cli {
        SubscriptionCli::Claude => CommandSpec {
            program: profile.cli.program(),
            args: vec![
                "-p".into(),
                "--output-format".into(),
                "text".into(),
                "--tools".into(),
                "".into(),
                "--restricted".into(),
                "--strict-mcp-config".into(),
                "--no-session-persistence".into(),
                "--permission-prompts".into(),
                "none".into(),
            ],
            // `claude -p/--print` is a boolean switch; the prompt is not
            // its value. Feed caller-controlled text over stdin so leading
            // '-' content can never be reinterpreted as a CLI option.
            stdin: Some(prompt),
        },
        SubscriptionCli::Codex => CommandSpec {
            program: profile.cli.program(),
            args: vec![
                "exec".into(),
                "--sandbox".into(),
                "read-only".into(),
                "--ephemeral".into(),
                "--skip-git-repo-check".into(),
                "--ignore-user-config".into(),
                "--ignore-rules".into(),
                "--color".into(),
                "never".into(),
                "-o".into(),
                output_file.into(),
                "-".into(),
            ],
            stdin: Some(prompt),
        },
    }
}

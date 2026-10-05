//! Subscription child-process environment filtering.
//!
//! This is deliberately a blacklist, matching the TS reference: it removes
//! known host credentials and every `IDORIS_*` internal switch while
//! preserving the rest of the caller environment (HOME/PATH included). It is
//! not a credential-whitelist sandbox; the CLI may still read its own login
//! state from the user's home directory.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

pub const CREDENTIAL_ENV_KEYS: [&str; 9] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

fn blocked_key(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    CREDENTIAL_ENV_KEYS.contains(&key) || key.starts_with("IDORIS_")
}

pub fn sanitize_environment<I, K, V>(env: I) -> BTreeMap<OsString, OsString>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    env.into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .filter(|(key, _)| !blocked_key(key))
        .collect()
}

pub fn sanitized_process_environment() -> BTreeMap<OsString, OsString> {
    sanitize_environment(std::env::vars_os())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn strips_known_credentials_and_all_idoris_switches_but_preserves_other_values() {
        let filtered = sanitize_environment([
            ("HOME", "/Users/test"),
            ("PATH", "/usr/bin"),
            ("OTHER_TOKEN", "keep-me"),
            ("ANTHROPIC_API_KEY", "secret-a"),
            ("OPENAI_API_KEY", "secret-b"),
            ("AWS_ACCESS_KEY_ID", "secret-c"),
            ("AWS_SECRET_ACCESS_KEY", "secret-d"),
            ("AWS_SESSION_TOKEN", "secret-e"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_PRIVATE_SENTINEL", "secret-f"),
        ]);

        assert_eq!(filtered.get(OsStr::new("HOME")).unwrap(), "/Users/test");
        assert_eq!(filtered.get(OsStr::new("PATH")).unwrap(), "/usr/bin");
        assert_eq!(filtered.get(OsStr::new("OTHER_TOKEN")).unwrap(), "keep-me");
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "IDORIS_ENABLE_SUBSCRIPTION",
            "IDORIS_PRIVATE_SENTINEL",
        ] {
            assert!(!filtered.contains_key(OsStr::new(key)), "{key} leaked");
        }
    }

    #[test]
    fn every_declared_credential_key_is_actually_filtered() {
        let env = CREDENTIAL_ENV_KEYS
            .iter()
            .map(|key| ((*key).to_string(), format!("secret-{key}")))
            .collect::<Vec<_>>();
        let filtered = sanitize_environment(env);
        assert!(filtered.is_empty());
    }
}

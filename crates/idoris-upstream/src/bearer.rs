//! Resolved upstream bearer. The value is not serialized and its Debug output
//! does not include the secret.

use reqwest::header::{AUTHORIZATION, HeaderValue};

#[derive(Clone, PartialEq, Eq)]
pub struct UpstreamBearer {
    authorization: String,
    header: HeaderValue,
}

impl std::fmt::Debug for UpstreamBearer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UpstreamBearer([redacted])")
    }
}

impl UpstreamBearer {
    pub fn from_secret(secret: &str) -> Result<Self, &'static str> {
        if secret.is_empty()
            || !secret.is_ascii()
            || secret.bytes().any(|byte| !(0x21..=0x7e).contains(&byte))
        {
            return Err("must be non-empty visible ASCII");
        }
        let authorization = format!("Bearer {secret}");
        let mut header = HeaderValue::from_str(&authorization)
            .map_err(|_| "must be a valid Authorization header value")?;
        header.set_sensitive(true);
        Ok(Self {
            authorization,
            header,
        })
    }

    pub fn on(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header(AUTHORIZATION, self.header.clone())
    }

    pub fn authorization(&self) -> &str {
        &self.authorization
    }

    /// The secret without the `Bearer ` prefix. This is the value vLLM's
    /// `--api-key` must use so the client header and the engine agree.
    pub fn secret(&self) -> &str {
        self.authorization.strip_prefix("Bearer ").unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn secret_is_prefixed_once_and_absent_from_debug() {
        let bearer = UpstreamBearer::from_secret("sk-test-token").unwrap();
        assert_eq!(bearer.authorization(), "Bearer sk-test-token");
        assert_eq!(bearer.secret(), "sk-test-token");
        let rendered = format!("{bearer:?}");
        assert!(!rendered.contains("sk-test-token"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn blank_or_spaced_secrets_are_rejected_without_echoing_them() {
        let secret = "has space";
        let error = UpstreamBearer::from_secret(secret).unwrap_err();
        assert_eq!(error, "must be non-empty visible ASCII");
        assert!(!error.contains(secret));
        assert!(UpstreamBearer::from_secret("").is_err());
    }
}

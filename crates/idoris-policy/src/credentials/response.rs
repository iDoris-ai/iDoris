//! Buffered C-3 core. No HTTP, storage or streaming integration lives here.

use std::fmt;

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::InspectAction;

use crate::inspect::{self, DEFAULT_MAX_INPUT_UTF16, InspectCtx};

/// Contains a credential; its Debug output deliberately carries no response or value.
pub struct ResponseGuard(String);

impl fmt::Debug for ResponseGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResponseGuard(<redacted>)")
    }
}

/// No findings, spans or upstream text cross this error boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseBlocked;

impl ResponseGuard {
    pub(super) fn new(credential: String) -> Self {
        Self(credential)
    }

    /// Check a complete, decoded UTF-8 body and every response header name/value.
    /// Invalid UTF-8 and aggregate oversize responses fail closed. The returned
    /// body borrows the inspected input; do not replace it before delivery.
    /// Known credentials are checked in one base64/hex/percent layer too.
    /// This is not a streaming guard or an arbitrary encoding decoder.
    pub fn check<'a>(
        &self,
        headers: &[(String, String)],
        body: &'a [u8],
    ) -> Result<&'a str, ResponseBlocked> {
        let body = std::str::from_utf8(body).map_err(|_| ResponseBlocked)?;
        let mut remaining = DEFAULT_MAX_INPUT_UTF16;
        for text in std::iter::once(body).chain(
            headers
                .iter()
                .flat_map(|(name, value)| [name.as_str(), value.as_str()]),
        ) {
            let length = text.encode_utf16().count();
            remaining = remaining.checked_sub(length).ok_or(ResponseBlocked)?;
            let ctx = InspectCtx {
                declared: PrivacyClass::Any,
                remote_allowed: true,
                redact_consent: false,
                max_input_utf16: length,
            };
            if inspect::contains_credential(text, &self.0)
                || inspect::inspect(text, &ctx).action != InspectAction::Allow
            {
                return Err(ResponseBlocked);
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::credentials::*;
    use std::cell::Cell;

    struct Source(Cell<usize>);

    impl SecretSource for Source {
        fn reveal(&self, _: &SecretRef) -> Result<Secret, CredentialError> {
            self.0.set(self.0.get() + 1);
            Ok(Secret::new("ordinary-secret".into()))
        }
    }

    #[test]
    fn guarded_broker_reveals_once_and_blocks_body_and_header_echoes() {
        let source = Source(Cell::new(0));
        let secret = SecretRef::parse("secret://openai/default").unwrap();
        let grant = Grant {
            secret: secret.clone(),
            host: "api.openai.com".into(),
            methods: vec!["POST".into()],
            path_prefix: "/v1/chat".into(),
            header: "Authorization".into(),
            scheme: HeaderScheme::Bearer,
        };
        let request = BrokerRequest {
            secret: &secret,
            url: "https://api.openai.com/v1/chat",
            method: "POST",
        };
        let mut headers = vec![("authorization".into(), "caller-key".into())];
        let (audit, guard) = broker_guarded(&[grant], &source, &request, &mut headers).unwrap();
        assert_eq!(source.0.get(), 1);
        assert_eq!(
            headers,
            [("Authorization".into(), "Bearer ordinary-secret".into())]
        );
        assert!(!format!("{audit:?} {guard:?}").contains("ordinary-secret"));
        for body in [
            "ordinary-secret",
            "Bearer ordinary-secret",
            r#"{"echo":"ordinary\u002dsecret"}"#,
        ] {
            assert_eq!(guard.check(&[], body.as_bytes()), Err(ResponseBlocked));
        }
        for header in ["Authorization", "X-Upstream-Diagnostic"] {
            assert_eq!(
                guard.check(&[(header.into(), "ordinary-secret".into())], b"ok"),
                Err(ResponseBlocked)
            );
        }
        assert_eq!(
            guard.check(&[("ordinary-secret".into(), "ok".into())], b"ok"),
            Err(ResponseBlocked)
        );
        assert_eq!(guard.check(&[], b"ordinary result"), Ok("ordinary result"));
        assert_eq!(source.0.get(), 1);
    }

    #[test]
    fn denied_guarded_request_does_not_reveal_or_mutate_headers() {
        let source = Source(Cell::new(0));
        let secret = SecretRef::parse("secret://openai/default").unwrap();
        let request = BrokerRequest {
            secret: &secret,
            url: "https://api.openai.com/v1/chat",
            method: "POST",
        };
        let mut headers = vec![("Authorization".into(), "caller-key".into())];
        assert!(matches!(
            broker_guarded(&[], &source, &request, &mut headers),
            Err(CredentialError::NotGranted)
        ));
        assert_eq!(source.0.get(), 0);
        assert_eq!(headers, [("Authorization".into(), "caller-key".into())]);
        let other = ResponseGuard::new("different-secret".into());
        assert_eq!(other.check(&[], b"ordinary-secret"), Ok("ordinary-secret"));
    }

    #[test]
    fn gate_one_blocks_other_secrets_and_uninspectable_responses() {
        let guard = ResponseGuard::new("ordinary-secret".into());
        let other = b"sk-123456789012345678901234";
        assert_eq!(guard.check(&[], other), Err(ResponseBlocked));
        assert_eq!(
            guard.check(
                &[(
                    "X-Diagnostic".into(),
                    String::from_utf8(other.to_vec()).unwrap()
                )],
                b"ok"
            ),
            Err(ResponseBlocked)
        );
        assert_eq!(guard.check(&[], &[0xff]), Err(ResponseBlocked));
        let oversized = "a".repeat(DEFAULT_MAX_INPUT_UTF16 + 1);
        assert_eq!(guard.check(&[], oversized.as_bytes()), Err(ResponseBlocked));
        let full = "a".repeat(DEFAULT_MAX_INPUT_UTF16);
        assert_eq!(
            guard.check(&[("x".into(), "b".into())], full.as_bytes()),
            Err(ResponseBlocked)
        );
    }

    #[test]
    fn encoded_known_credential_is_blocked_in_body_and_headers() {
        let guard = ResponseGuard::new("ordinary-secret".into());
        for encoded in [
            "b3JkaW5hcnktc2VjcmV0",
            "encoded/b3JkaW5hcnktc2VjcmV0",
            "6f7264696e6172792d736563726574",
            "0x6F7264696E6172792D736563726574",
            "%6f%72%64%69%6e%61%72%79%2d%73%65%63%72%65%74",
            "ordinary%2Dsecret",
        ] {
            let body = format!(r#"{{"echo":"{encoded}"}}"#);
            assert_eq!(guard.check(&[], body.as_bytes()), Err(ResponseBlocked));
            assert_eq!(
                guard.check(&[("X-Echo".into(), encoded.into())], b"ok"),
                Err(ResponseBlocked)
            );
        }
        let clean = "b3JkaW5hcnkgcmVzdWx0"; // "ordinary result", not this credential.
        assert_eq!(guard.check(&[], clean.as_bytes()), Ok(clean));
    }

    #[test]
    fn exact_match_covers_short_and_url_safe_encodings_without_changing_l0() {
        for (secret, encodings) in [
            ("a", vec!["YQ==", "YQ", "61", "%61"]),
            ("??>>", vec!["Pz8+Pg==", "Pz8-Pg==", "Pz8-Pg"]),
        ] {
            let guard = ResponseGuard::new(secret.into());
            for encoded in encodings {
                // L0 deliberately ignores these short, non-sensitive tokens.
                let ctx = InspectCtx {
                    declared: PrivacyClass::Any,
                    remote_allowed: true,
                    redact_consent: false,
                    max_input_utf16: encoded.len(),
                };
                assert_eq!(inspect::inspect(encoded, &ctx).action, InspectAction::Allow);
                assert_eq!(guard.check(&[], encoded.as_bytes()), Err(ResponseBlocked));
                assert_eq!(
                    guard.check(&[("X-Echo".into(), encoded.into())], b"ok"),
                    Err(ResponseBlocked)
                );
            }
        }
    }

    #[test]
    fn known_credential_in_encoded_binary_is_blocked() {
        let guard = ResponseGuard::new("ordinary-secret".into());
        // Base64 of a non-UTF-8 byte followed by the credential.
        let encoded = "/29yZGluYXJ5LXNlY3JldA==";
        assert_eq!(guard.check(&[], encoded.as_bytes()), Err(ResponseBlocked));
        assert_eq!(
            guard.check(&[("X-Echo".into(), encoded.into())], b"ok"),
            Err(ResponseBlocked)
        );
    }
}

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
        assert_eq!(
            guard.check(&[], "ordinary\u{200b}-secret".as_bytes()),
            Err(ResponseBlocked)
        );
        assert_eq!(
            guard.check(&[], "\u{ff4f}rdinary-secret".as_bytes()),
            Err(ResponseBlocked)
        );
    }

    #[test]
    fn empty_or_control_secret_is_unavailable_and_headers_stay() {
        for bad in ["", "a\r\nb", "a\tb", "a\u{007f}b"] {
            struct Bad(String);
            impl SecretSource for Bad {
                fn reveal(&self, _: &SecretRef) -> Result<Secret, CredentialError> {
                    Ok(Secret::new(self.0.clone()))
                }
            }
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
            assert!(
                matches!(
                    broker_guarded(&[grant], &Bad(bad.into()), &request, &mut headers),
                    Err(CredentialError::Unavailable)
                ),
                "{bad:?}"
            );
            assert_eq!(headers, [("authorization".into(), "caller-key".into())]);
        }
    }

    #[test]
    fn unicode_escape_markers_are_not_optional() {
        let guard = ResponseGuard::new("ab1234cd".into());
        assert_eq!(guard.check(&[], b"ab1234cd"), Err(ResponseBlocked));
        let hyphen = ResponseGuard::new("ab-cd".into());
        assert_eq!(hyphen.check(&[], b"abXu002dcd"), Ok("abXu002dcd"));
        assert_eq!(hyphen.check(&[], b"ab\\X002dcd"), Ok("ab\\X002dcd"));
    }

    #[test]
    fn one_layer_encoded_echoes_are_blocked_in_body_and_headers() {
        let guard = ResponseGuard::new("ordinary-secret".into());
        let hex = "6f7264696e6172792d736563726574";
        let base64 = "b3JkaW5hcnktc2VjcmV0";
        let percent = "ordinary%2dsecret";
        for encoded in [hex, base64, percent] {
            assert_eq!(
                guard.check(&[], encoded.as_bytes()),
                Err(ResponseBlocked),
                "{encoded}"
            );
            assert_eq!(
                guard.check(&[(format!("X-{encoded}"), "ok".into())], b"ok"),
                Err(ResponseBlocked),
                "header name {encoded}"
            );
            assert_eq!(
                guard.check(
                    &[("X-Echo".into(), format!("prefix {encoded} suffix"))],
                    b"ok"
                ),
                Err(ResponseBlocked),
                "header value {encoded}"
            );
        }
        // Known historical bypass: the X- prefix must not shift the hex view.
        assert_eq!(
            guard.check(&[], format!("X-{hex}").as_bytes()),
            Err(ResponseBlocked)
        );
        let wrapped_base64 = "QUFvcmRpbmFyeS1zZWNyZXRaWg==";
        assert_eq!(
            guard.check(&[], wrapped_base64.as_bytes()),
            Err(ResponseBlocked)
        );
    }

    #[test]
    fn encoded_echoes_cover_short_secrets_and_json_simple_escapes() {
        let short = ResponseGuard::new("xy".into());
        assert_eq!(short.check(&[], b"eHk="), Err(ResponseBlocked));
        assert_eq!(short.check(&[], b"%78y"), Err(ResponseBlocked));

        let slash = ResponseGuard::new("a/b".into());
        assert_eq!(slash.check(&[], br#"a\/b"#), Err(ResponseBlocked));
        let backslash = ResponseGuard::new("a\\b".into());
        assert_eq!(backslash.check(&[], br#"a\\b"#), Err(ResponseBlocked));
        let quote = ResponseGuard::new("a\"b".into());
        assert_eq!(quote.check(&[], br#"a\"b"#), Err(ResponseBlocked));

        // Raw matching stays active alongside decoded-only matching (G2).
        assert_eq!(slash.check(&[], b"a/b"), Err(ResponseBlocked));
    }

    #[test]
    fn unrelated_encoded_values_remain_clean_and_decoding_is_not_recursive() {
        let guard = ResponseGuard::new("ordinary-secret".into());
        for clean in ["c2FmZS1yZXN1bHQ", "1234abcd", "ordinary%2dresult"] {
            assert_eq!(guard.check(&[], clean.as_bytes()), Ok(clean), "{clean}");
        }
        let twice = "YjNKa2FXNWFjbXR0YzJWamNtVjA";
        assert_eq!(guard.check(&[], twice.as_bytes()), Ok(twice));
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
}

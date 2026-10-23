//! 闸二（docs/15 §2）的纯核心：`secret://` 引用、按目的地授权、强制覆盖式
//! 注入、不含值的审计。**不含存储**——钥匙串 / Secure Enclave / 独立 UID 的
//! broker 进程是部署决定，经 [`SecretSource`] 接进来。
//!
//! 三条硬性验收断言（docs/15 §2.3）的纯逻辑基础：
//!
//! - **C-1** 注入**覆盖**调用方带来的同名头（大小写不敏感，重复的全删），
//!   而不是「缺了才填」——否则调用方自带的 `Authorization` 会赢。
//! - **C-2** 授权**先于取值**：[`broker`] 只有在 [`authorize`] 通过之后才调用
//!   [`SecretSource::reveal`]，被拒的请求根本不接触密钥。
//! - **C-3** [`broker_guarded`] 返回缓冲响应检查器；部署层仍须在交付前调用。
//!
//! 模型、策略层与审计只见到引用；broker 内部的 [`Secret`] 和响应检查器
//! 保存值，它们的 `Debug` 均不打印值。

use std::fmt;

pub mod response;

/// `secret://<provider>/<name>`：两段都是小写 ASCII 字母、数字、`-`、`_`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretRef {
    provider: String,
    name: String,
}

impl SecretRef {
    pub fn parse(raw: &str) -> Result<Self, CredentialError> {
        let rest = raw
            .strip_prefix("secret://")
            .ok_or(CredentialError::InvalidRef)?;
        let (provider, name) = rest.split_once('/').ok_or(CredentialError::InvalidRef)?;
        let valid = |part: &str| {
            !part.is_empty()
                && part.len() <= 64
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        };
        if !valid(provider) || !valid(name) {
            return Err(CredentialError::InvalidRef);
        }
        Ok(Self {
            provider: provider.to_string(),
            name: name.to_string(),
        })
    }

    /// The handle as written; safe to log.
    pub fn id(&self) -> String {
        format!("secret://{}/{}", self.provider, self.name)
    }
}

/// A revealed credential value. Never printed.
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// How the value is written into the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderScheme {
    /// `Bearer <value>`.
    Bearer,
    /// The value as-is (e.g. `x-api-key`).
    Raw,
}

/// Caller headers that carry credentials or select the billed account.
/// They are cleared before the single authorized credential is injected.
const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "openai-organization",
    "openai-project",
    "cookie",
];

/// One allowed use of one secret: exact HTTPS host, methods, path prefix,
/// and the header the value goes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub secret: SecretRef,
    pub host: String,
    pub methods: Vec<String>,
    pub path_prefix: String,
    pub header: String,
    pub scheme: HeaderScheme,
}

impl Grant {
    /// Reject grants that cannot be scoped or written as a valid header.
    fn is_valid(&self) -> bool {
        !self.host.is_empty()
            && !self.methods.is_empty()
            && self.path_prefix.starts_with('/')
            && !self.header.is_empty()
            && self
                .header
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    }
}

/// What a caller asks the broker to do; carries no value.
#[derive(Debug, Clone, Copy)]
pub struct BrokerRequest<'a> {
    pub secret: &'a SecretRef,
    /// The upstream URL the request will be sent to.
    pub url: &'a str,
    pub method: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    InvalidRef,
    /// Not HTTPS, unparsable, or a path that escapes its prefix.
    InvalidDestination,
    /// No grant allows this secret for this host, method and path.
    NotGranted,
    /// The source could not produce the value.
    Unavailable,
}

/// Where values come from (a keychain broker, in tests a fake).
pub trait SecretSource {
    fn reveal(&self, secret: &SecretRef) -> Result<Secret, CredentialError>;
}

/// The audit fact of one brokered use: handle and destination, never value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub secret_id: String,
    pub host: String,
    pub method: String,
    pub path: String,
}

/// C-2: decide from the request alone, before any value is touched.
pub fn authorize<'g>(
    grants: &'g [Grant],
    request: &BrokerRequest<'_>,
) -> Result<&'g Grant, CredentialError> {
    let (host, path) = destination(request.url)?;
    grants
        .iter()
        .find(|grant| {
            grant.is_valid()
                && grant.secret == *request.secret
                && grant.host == host
                && grant
                    .methods
                    .iter()
                    .any(|method| method.eq_ignore_ascii_case(request.method))
                && within_prefix(&path, &grant.path_prefix)
        })
        .ok_or(CredentialError::NotGranted)
}

/// Authorize, then reveal, then inject (C-1), then return the audit fact.
pub fn broker(
    grants: &[Grant],
    source: &dyn SecretSource,
    request: &BrokerRequest<'_>,
    headers: &mut Vec<(String, String)>,
) -> Result<AuditRecord, CredentialError> {
    broker_guarded(grants, source, request, headers).map(|(audit, _guard)| audit)
}

/// Like [`broker`], with a guard retaining this call's credential for response checks.
/// The caller must check the complete decoded response before returning or caching it.
/// Streaming and arbitrary encoded payloads require separate integration.
pub fn broker_guarded(
    grants: &[Grant],
    source: &dyn SecretSource,
    request: &BrokerRequest<'_>,
    headers: &mut Vec<(String, String)>,
) -> Result<(AuditRecord, response::ResponseGuard), CredentialError> {
    let grant = authorize(grants, request)?;
    let secret = source.reveal(&grant.secret)?;
    // A value with control characters would split or forge headers.
    if secret.0.is_empty() || secret.0.bytes().any(|b| b.is_ascii_control()) {
        return Err(CredentialError::Unavailable);
    }
    let guard = response::ResponseGuard::new(secret.0.clone());
    let value = match grant.scheme {
        HeaderScheme::Bearer => format!("Bearer {}", secret.0),
        HeaderScheme::Raw => secret.0,
    };
    inject(headers, &grant.header, value);
    let (host, path) = destination(request.url)?;
    Ok((
        AuditRecord {
            secret_id: grant.secret.id(),
            host,
            method: request.method.to_ascii_uppercase(),
            path,
        },
        guard,
    ))
}

/// C-1: every caller credential/billing header and every copy of the selected
/// header (any case, any count) is removed before the authorized value is added.
pub fn inject(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(existing, _)| {
        let existing = existing.trim();
        !existing.eq_ignore_ascii_case(name)
            && !CREDENTIAL_HEADERS
                .iter()
                .any(|credential| existing.eq_ignore_ascii_case(credential))
    });
    headers.push((name.to_string(), value));
}

/// HTTPS host (lowercase, explicit non-default ports included) and path.
fn destination(raw: &str) -> Result<(String, String), CredentialError> {
    let url = url::Url::parse(raw).map_err(|_| CredentialError::InvalidDestination)?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(CredentialError::InvalidDestination);
    }
    let host = url.host_str().ok_or(CredentialError::InvalidDestination)?;
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    // The URL parser already resolved literal `.`/`..` segments. A denylist
    // of specific encodings (`%2e`, `%2f`, `%5c`, …) keeps missing the next
    // one (`;` path parameters, double encoding, IIS `%u` escapes, …)
    // because each upstream decodes differently, so instead allowlist the
    // raw path: only unreserved characters and `/` survive, which means `%`
    // itself — and therefore every percent-encoded or double-encoded
    // traversal — is refused outright, with no encoding left to enumerate.
    let path = url.path().to_string();
    if !path_is_safe(&path) {
        return Err(CredentialError::InvalidDestination);
    }
    Ok((host, path))
}

/// Every byte is `[A-Za-z0-9._~-]` or `/`, and no segment is `.` or `..`
/// (defense in depth; the URL parser already resolves those before this
/// runs, since the path came from an absolute URL).
fn path_is_safe(path: &str) -> bool {
    path.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b))
        && path
            .split('/')
            .all(|segment| segment != "." && segment != "..")
}

/// `path` equals `prefix` or continues it at a segment boundary.
fn within_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| prefix.ends_with('/') || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::Cell;

    use super::*;

    struct CountingSource {
        reveals: Cell<u32>,
    }

    impl SecretSource for CountingSource {
        fn reveal(&self, _secret: &SecretRef) -> Result<Secret, CredentialError> {
            self.reveals.set(self.reveals.get() + 1);
            Ok(Secret::new("sk-real-value".to_string()))
        }
    }

    fn openai() -> SecretRef {
        SecretRef::parse("secret://openai/default").unwrap()
    }

    fn grants() -> Vec<Grant> {
        vec![Grant {
            secret: openai(),
            host: "api.openai.com".into(),
            methods: vec!["POST".into()],
            path_prefix: "/v1/chat".into(),
            header: "Authorization".into(),
            scheme: HeaderScheme::Bearer,
        }]
    }

    fn request<'a>(secret: &'a SecretRef, url: &'a str, method: &'a str) -> BrokerRequest<'a> {
        BrokerRequest {
            secret,
            url,
            method,
        }
    }

    #[test]
    fn refs_are_strict() {
        assert_eq!(openai().id(), "secret://openai/default");
        for raw in [
            "secret://openai",
            "secret:///default",
            "secret://OpenAI/default",
            "secret://openai/def ault",
            "http://openai/default",
            "secret://openai/default/extra",
        ] {
            assert_eq!(
                SecretRef::parse(raw),
                Err(CredentialError::InvalidRef),
                "{raw}"
            );
        }
    }

    #[test]
    fn c2_a_denied_request_never_touches_the_secret() {
        let source = CountingSource {
            reveals: Cell::new(0),
        };
        let secret = openai();
        for (url, method) in [
            (
                "https://api.openai.com.evil.example/v1/chat/completions",
                "POST",
            ),
            ("https://evil.example/v1/chat/completions", "POST"),
            ("http://api.openai.com/v1/chat/completions", "POST"),
            ("https://api.openai.com/v1/files", "POST"),
            ("https://api.openai.com/v1/chatty", "POST"),
            ("https://api.openai.com/v1/chat/completions", "GET"),
            ("https://api.openai.com:8443/v1/chat/completions", "POST"),
            ("https://user:pw@api.openai.com/v1/chat/completions", "POST"),
            ("https://api.openai.com/v1/chat/%2e%2e%2ffiles", "POST"),
            ("https://api.openai.com/v1/chat/x%2f..%2f..%2ffiles", "POST"),
            ("https://evilapi.openai.com/v1/chat/completions", "POST"),
            ("https://proxy.api.openai.com/v1/chat/completions", "POST"),
        ] {
            let mut headers = Vec::new();
            let result = broker(
                &grants(),
                &source,
                &request(&secret, url, method),
                &mut headers,
            );
            assert!(result.is_err(), "{method} {url}");
            assert!(headers.is_empty());
        }
        let other = SecretRef::parse("secret://anthropic/default").unwrap();
        let mut headers = Vec::new();
        assert_eq!(
            broker(
                &grants(),
                &source,
                &request(&other, "https://api.openai.com/v1/chat/completions", "POST"),
                &mut headers
            ),
            Err(CredentialError::NotGranted)
        );
        assert_eq!(source.reveals.get(), 0, "no denied request may reveal");
    }

    #[test]
    fn dot_segments_are_resolved_before_the_prefix_check() {
        let secret = openai();
        assert_eq!(
            authorize(
                &grants(),
                &request(&secret, "https://api.openai.com/v1/chat/../files", "POST")
            ),
            Err(CredentialError::NotGranted)
        );
    }

    #[test]
    fn c1_injection_overwrites_every_caller_supplied_copy() {
        let source = CountingSource {
            reveals: Cell::new(0),
        };
        let secret = openai();
        let mut headers = vec![
            ("authorization".to_string(), "Bearer attacker".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("AUTHORIZATION".to_string(), "Bearer attacker-2".to_string()),
        ];
        let audit = broker(
            &grants(),
            &source,
            &request(
                &secret,
                "https://API.openai.com/v1/chat/completions",
                "post",
            ),
            &mut headers,
        )
        .unwrap();
        let auth: Vec<_> = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .collect();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].1, "Bearer sk-real-value");
        assert!(headers.iter().any(|(name, _)| name == "Content-Type"));
        assert_eq!(source.reveals.get(), 1);
        assert_eq!(
            audit,
            AuditRecord {
                secret_id: "secret://openai/default".into(),
                host: "api.openai.com".into(),
                method: "POST".into(),
                path: "/v1/chat/completions".into(),
            }
        );
        assert!(!format!("{audit:?}").contains("sk-real-value"));
    }

    #[test]
    fn malformed_grants_are_rejected_before_reveal_or_header_mutation() {
        let source = CountingSource {
            reveals: Cell::new(0),
        };
        let secret = openai();
        let original_headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let base = grants().remove(0);
        let invalid = [
            Grant {
                host: String::new(),
                ..base.clone()
            },
            Grant {
                methods: Vec::new(),
                ..base.clone()
            },
            Grant {
                path_prefix: String::new(),
                ..base.clone()
            },
            Grant {
                path_prefix: "v1/chat".into(),
                ..base.clone()
            },
            Grant {
                header: "Authorization\r\nX-Evil: 1".into(),
                ..base.clone()
            },
            Grant {
                header: "Authorization ".into(),
                ..base
            },
        ];

        for grant in invalid {
            let mut headers = original_headers.clone();
            assert_eq!(
                broker(
                    &[grant],
                    &source,
                    &request(
                        &secret,
                        "https://api.openai.com/v1/chat/completions",
                        "POST"
                    ),
                    &mut headers,
                ),
                Err(CredentialError::NotGranted),
            );
            assert_eq!(headers, original_headers);
        }
        assert_eq!(source.reveals.get(), 0, "invalid grants must not reveal");
    }

    #[test]
    fn broker_scrubs_alternate_credentials_and_billing_headers() {
        let source = CountingSource {
            reveals: Cell::new(0),
        };
        let secret = openai();
        let raw_key = vec![Grant {
            header: "x-api-key".into(),
            scheme: HeaderScheme::Raw,
            ..grants().remove(0)
        }];
        let mut headers = vec![
            ("Authorization".into(), "Bearer attacker".into()),
            ("aUtHoRiZaTiOn".into(), "Bearer duplicate".into()),
            ("Proxy-Authorization".into(), "Basic attacker".into()),
            ("API-Key".into(), "alternate".into()),
            ("X-Goog-Api-Key".into(), "alternate".into()),
            ("OpenAI-Organization".into(), "org-attacker".into()),
            ("openai-project".into(), "project-attacker".into()),
            ("Cookie".into(), "session=attacker".into()),
            ("Content-Type".into(), "application/json".into()),
            ("x-trace-id".into(), "trace-123".into()),
        ];

        broker(
            &raw_key,
            &source,
            &request(
                &secret,
                "https://api.openai.com/v1/chat/completions",
                "POST",
            ),
            &mut headers,
        )
        .unwrap();

        assert_eq!(
            headers,
            vec![
                ("Content-Type".into(), "application/json".into()),
                ("x-trace-id".into(), "trace-123".into()),
                ("x-api-key".into(), "sk-real-value".into()),
            ]
        );
        assert_eq!(source.reveals.get(), 1);
    }

    /// The old `["%2e", "%2f", "%5c"]` denylist on this branch never covered
    /// `;` path parameters, double encoding or IIS `%u` escapes at all —
    /// each of these must fail closed with zero reveals under the new
    /// allowlist, which refuses any `%` or `;` regardless of what follows.
    #[test]
    fn percent_and_semicolon_encoded_bypasses_are_closed() {
        let source = CountingSource {
            reveals: Cell::new(0),
        };
        let secret = openai();
        for url in [
            // `;` path parameter: `..` disguised as `..;` (Tomcat/Spring).
            "https://api.openai.com/v1/chat/..;/files",
            // Encoded `;` path parameter.
            "https://api.openai.com/v1/chat/..%3b/files",
            // Double encoding: `%25` decodes to `%`, then `%2e` to `.`.
            "https://api.openai.com/v1/chat/%252e%252e/files",
            // IIS-style `%u` escape for `.`.
            "https://api.openai.com/v1/chat/%u002e%u002e/files",
            // Mixed-case percent hex, still just "a %".
            "https://api.openai.com/v1/chat/%2E%2E/files",
            // A literal `%` with no recognized escape at all must still be
            // refused — the allowlist has no escape hatch to enumerate.
            "https://api.openai.com/v1/chat/100%files",
        ] {
            let mut headers = Vec::new();
            assert!(
                broker(
                    &grants(),
                    &source,
                    &request(&secret, url, "POST"),
                    &mut headers
                )
                .is_err(),
                "{url}"
            );
            assert!(headers.is_empty(), "{url}");
        }
        assert_eq!(source.reveals.get(), 0, "no bypass may reveal");
    }

    /// Positive control: ordinary granted paths — including ones with dots
    /// inside a filename, which the allowlist must not confuse with a `..`
    /// segment — still authorize and reveal exactly once.
    #[test]
    fn ordinary_paths_with_dots_still_authorize() {
        let source = CountingSource {
            reveals: Cell::new(0),
        };
        let secret = openai();
        for url in [
            "https://api.openai.com/v1/chat/completions",
            "https://api.openai.com/v1/chat/completions.json",
            "https://api.openai.com/v1/chat/v2.1/completions",
        ] {
            let mut headers = Vec::new();
            broker(
                &grants(),
                &source,
                &request(&secret, url, "POST"),
                &mut headers,
            )
            .unwrap_or_else(|e| panic!("{url} should authorize, got {e:?}"));
        }
        assert_eq!(source.reveals.get(), 3);
    }

    /// Mutation check on the allowlist itself: a path segment that is
    /// exactly `.` or `..` must be refused even though every individual byte
    /// is allowlisted (this is the half of `path_is_safe` the byte-level
    /// check alone cannot cover; it exists only because the URL parser is
    /// trusted to resolve literal dot segments on an absolute URL — if that
    /// trust were ever wrong, this is the backstop).
    #[test]
    fn bare_dot_segments_are_refused_even_though_every_byte_is_allowlisted() {
        assert!(path_is_safe("/v1/chat/completions"));
        assert!(path_is_safe("/v1/chat/v2.1/completions"));
        assert!(!path_is_safe("/v1/chat/./files"));
        assert!(!path_is_safe("/v1/chat/../files"));
        assert!(!path_is_safe("/v1/chat/.."));
        assert!(!path_is_safe("/v1/chat/%2e%2e/files"));
        assert!(!path_is_safe("/v1/chat/a;b"));
        assert!(!path_is_safe("/v1/chat\\..\\files"));
    }

    #[test]
    fn secrets_never_print() {
        let secret = Secret::new("sk-real-value".to_string());
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
    }
}

//! Low-level HTTP plumbing for the oMLX adapter: every request goes through
//! [`get_json`], which applies a per-call timeout and turns a failure into
//! a [`BackendError`] that never carries the response body or API key —
//! every error here is built from method/path/status only, never by
//! formatting the response body or the underlying `reqwest::Error`
//! (mirroring `RuntimeAdapter::probe_ready`'s "apply your own timeout"
//! doc, and H1/H2 from the TS reference: no payload in errors/logs).
//!
//! `#![allow(dead_code)]`: the `OmlxAdapter` struct that wires `get_json`
//! into `list`/`status` lands in a follow-up PR; exercised directly by
//! this module's own tests until then — remove once `mod.rs` calls it.
#![allow(dead_code)]

use std::time::Duration;

use idoris_backend::BackendError;

use super::upstream_error;

/// Structured HTTP failure: method, path (may embed a caller-supplied model
/// id — that's the caller's own data, not upstream content) and status.
/// Never the response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmlxHttpError {
    pub method: &'static str,
    pub path: String,
    pub status: u16,
}

impl std::fmt::Display for OmlxHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "oMLX {} {} failed: HTTP {}",
            self.method, self.path, self.status
        )
    }
}

fn auth_header(builder: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(key) => builder.bearer_auth(key),
        None => builder,
    }
}

/// Classify a transport-level failure (the request never got a response at
/// all) without ever formatting `err` itself — only `err.is_timeout()`/
/// `is_connect()` are safe, structural facts about it.
fn transport_error(method: &str, path: &str, err: &reqwest::Error) -> BackendError {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "could not connect"
    } else if err.is_decode() {
        "returned an undecodable body"
    } else {
        "failed"
    };
    upstream_error(format!("oMLX {method} {path} {kind} (transport error)"))
}

/// `GET path`, parsed as JSON. `path` must start with `/`.
pub(super) async fn get_json(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    api_key: Option<&str>,
    call_timeout: Duration,
) -> Result<serde_json::Value, BackendError> {
    let url = format!("{base_url}{path}");
    let req = auth_header(client.get(&url), api_key);
    send_and_parse("GET", path, req, call_timeout).await
}

// `post_empty`/`put_json` (load/unload/pin) land in a follow-up PR, each
// with its own `tokio::time::timeout` sized to what it actually awaits
// (see `send_and_parse`'s doc: must cover body reads, not just `send()`).

fn check_status(
    method: &'static str,
    path: &str,
    resp: &reqwest::Response,
) -> Result<(), BackendError> {
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(upstream_error(
            OmlxHttpError {
                method,
                path: path.to_string(),
                status: resp.status().as_u16(),
            }
            .to_string(),
        ))
    }
}

/// **`call_timeout` covers the whole round trip, not just `req.send()`.**
/// `reqwest::RequestBuilder::send`'s future resolves once headers arrive —
/// a server that sends headers promptly and then drips the body forever
/// would make a send()-only timeout useless, since the still-unbounded
/// `resp.json()` read happens after it.
async fn send_and_parse(
    method: &'static str,
    path: &str,
    req: reqwest::RequestBuilder,
    call_timeout: Duration,
) -> Result<serde_json::Value, BackendError> {
    let attempt = async {
        let resp = req
            .send()
            .await
            .map_err(|err| transport_error(method, path, &err))?;
        check_status(method, path, &resp)?;
        // A malformed/non-JSON body is reported with only the field name
        // and the fact "not valid JSON" — never the body itself, which may
        // contain content we should not echo.
        resp.json::<serde_json::Value>().await.map_err(|_| {
            upstream_error(format!(
                "oMLX {method} {path} returned a body that was not valid JSON"
            ))
        })
    };
    tokio::time::timeout(call_timeout, attempt)
        .await
        .unwrap_or_else(|_elapsed| {
            Err(upstream_error(format!(
                "oMLX {method} {path} timed out after {call_timeout:?}"
            )))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use wiremock::matchers::{method as http_method, path as http_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn http_error_display_never_includes_api_key_or_body() {
        let err = OmlxHttpError {
            method: "GET",
            path: "/v1/models/qwen3-8b/load".to_string(),
            status: 401,
        };
        let text = err.to_string();
        assert!(text.contains("HTTP 401") && text.contains("/v1/models/qwen3-8b/load"));
        // Structurally impossible for a bearer token to appear: the type
        // has no field for it.
        assert!(!text.to_lowercase().contains("bearer"));
    }

    async fn mock(path_str: &str, resp: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(http_path(path_str))
            .respond_with(resp)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn get_json_returns_the_parsed_body_on_success() {
        let server = mock(
            "/ok",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"a": 1})),
        )
        .await;
        let client = reqwest::Client::new();
        let body = get_json(&client, &server.uri(), "/ok", None, Duration::from_secs(1))
            .await
            .expect("must succeed");
        assert_eq!(body["a"], 1);
    }

    #[tokio::test]
    async fn get_json_on_4xx_reports_upstream_error_without_the_response_body_or_key() {
        let server = mock(
            "/secret",
            ResponseTemplate::new(401).set_body_string("do-not-leak-this-body"),
        )
        .await;
        let client = reqwest::Client::new();
        let key = Some("do-not-leak-this-key");
        let err = get_json(
            &client,
            &server.uri(),
            "/secret",
            key,
            Duration::from_secs(1),
        )
        .await
        .expect_err("4xx must fail");
        assert_eq!(err.reason_code(), "upstream_error");
        let msg = err.to_string();
        assert!(
            msg.contains("401")
                && !msg.contains("do-not-leak-this-body")
                && !msg.contains("do-not-leak-this-key")
        );
    }

    /// 5xx and a non-JSON body both fail closed with a safe (no-payload)
    /// message — table-driven since both are "reached the server, response
    /// itself was the problem" cases.
    #[tokio::test]
    async fn get_json_on_5xx_or_malformed_body_fails_closed() {
        for (p, resp, want) in [
            ("/broken", ResponseTemplate::new(503), "503"),
            (
                "/junk",
                ResponseTemplate::new(200).set_body_string("not json at all"),
                "not valid JSON",
            ),
        ] {
            let server = mock(p, resp).await;
            let client = reqwest::Client::new();
            let err = get_json(&client, &server.uri(), p, None, Duration::from_secs(1))
                .await
                .expect_err("must fail, not panic or silently succeed");
            assert!(err.to_string().contains(want));
        }
    }

    #[tokio::test]
    async fn get_json_times_out_instead_of_hanging() {
        let server = mock(
            "/slow",
            ResponseTemplate::new(200).set_delay(Duration::from_secs(5)),
        )
        .await;
        let client = reqwest::Client::new();
        let err = get_json(
            &client,
            &server.uri(),
            "/slow",
            None,
            Duration::from_millis(50),
        )
        .await
        .expect_err("must time out, not hang");
        assert!(err.to_string().contains("timed out"));
    }
}

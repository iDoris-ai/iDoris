//! Low-level HTTP plumbing for the oMLX adapter: every request goes through
//! [`get_json`]/[`post_empty`]/[`put_json`]/[`post_and_parse`], each of
//! which applies a per-call timeout and turns a failure into a [`BackendError`] that never
//! carries the response body or API key — every error here is built from
//! method/path/status only, never by formatting the response body or the
//! underlying `reqwest::Error` (mirroring `RuntimeAdapter::probe_ready`'s
//! "apply your own timeout" doc, and H1/H2 from the TS reference: no
//! payload in errors/logs).

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

/// `POST path` with no body, discarding the response body — used for the
/// oMLX load/unload endpoints, which return no payload this adapter reads.
/// A send()-only timeout is correct here (unlike [`send_and_parse`]):
/// nothing reads the body afterward, so there is no unbounded read left
/// unguarded once `send()` resolves.
pub(super) async fn post_empty(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    api_key: Option<&str>,
    call_timeout: Duration,
) -> Result<(), BackendError> {
    let url = format!("{base_url}{path}");
    let req = auth_header(client.post(&url), api_key);
    send_and_discard("POST", path, req, call_timeout).await
}

/// `PUT path` with a JSON body, discarding the response body. See
/// [`post_empty`]'s doc on why a send()-only timeout is correct here too.
pub(super) async fn put_json(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    api_key: Option<&str>,
    call_timeout: Duration,
    body: &serde_json::Value,
) -> Result<(), BackendError> {
    let url = format!("{base_url}{path}");
    let req = auth_header(client.put(&url), api_key).json(body);
    send_and_discard("PUT", path, req, call_timeout).await
}

/// `POST path` with a JSON body, parsed as JSON — used for `/v1/chat/
/// completions`. Unlike [`post_empty`]/[`put_json`], the response body
/// *is* read, so this goes through [`send_and_parse`] (full-round-trip
/// timeout), not `send_and_discard`.
pub(super) async fn post_and_parse(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    api_key: Option<&str>,
    call_timeout: Duration,
    body: &serde_json::Value,
) -> Result<serde_json::Value, BackendError> {
    let url = format!("{base_url}{path}");
    let req = auth_header(client.post(&url), api_key).json(body);
    send_and_parse("POST", path, req, call_timeout).await
}

async fn send_and_discard(
    method: &'static str,
    path: &str,
    req: reqwest::RequestBuilder,
    call_timeout: Duration,
) -> Result<(), BackendError> {
    let attempt = async {
        let resp = req
            .send()
            .await
            .map_err(|err| transport_error(method, path, &err))?;
        check_status(method, path, &resp)
    };
    tokio::time::timeout(call_timeout, attempt)
        .await
        .unwrap_or_else(|_elapsed| {
            Err(upstream_error(format!(
                "oMLX {method} {path} timed out after {call_timeout:?}"
            )))
        })
}

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

    async fn mock_method(m: &'static str, path_str: &str, resp: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(http_method(m))
            .and(http_path(path_str))
            .respond_with(resp)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn post_empty_and_put_json_succeed_and_fail_closed_on_4xx() {
        let server = mock_method("POST", "/load", ResponseTemplate::new(200)).await;
        let client = reqwest::Client::new();
        post_empty(
            &client,
            &server.uri(),
            "/load",
            None,
            Duration::from_secs(1),
        )
        .await
        .expect("POST 200 must succeed");

        let server = mock_method("PUT", "/pin", ResponseTemplate::new(401)).await;
        let err = put_json(
            &client,
            &server.uri(),
            "/pin",
            Some("do-not-leak-this-key"),
            Duration::from_secs(1),
            &serde_json::json!({"is_pinned": true}),
        )
        .await
        .expect_err("PUT 401 must fail");
        let msg = err.to_string();
        assert!(msg.contains("401") && !msg.contains("do-not-leak-this-key"));
    }

    #[tokio::test]
    async fn send_and_discard_times_out_instead_of_hanging() {
        let server = mock_method(
            "POST",
            "/slow",
            ResponseTemplate::new(200).set_delay(Duration::from_secs(5)),
        )
        .await;
        let client = reqwest::Client::new();
        let err = post_empty(
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

    /// Regression test for the round-1 fix in this PR's own history
    /// (`send_and_parse`'s timeout must cover the body read, not just
    /// `send()`): `wiremock`'s `set_delay` (used by the test above) delays
    /// the *entire* response, headers included, so it can't actually catch
    /// a "headers arrive promptly, body stalls" regression — the old,
    /// broken `dispatch`-only-wraps-`send()` shape would have passed that
    /// test too. This uses a raw TCP listener instead: writes valid
    /// headers + `Content-Length: 100` immediately, then only ever sends 1
    /// body byte before stalling, so the only way this test passes is if
    /// the timeout genuinely covers the body read.
    #[tokio::test]
    async fn get_json_times_out_on_a_stalled_body_not_just_slow_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind must succeed");
        let addr = listener.local_addr().expect("local_addr must succeed");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                // Valid status line + a `Content-Length` promising 100
                // bytes, then exactly 1 of them — headers are complete,
                // the body is not.
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nX")
                    .await;
                // Never send the other 99 bytes. Sleep well past this
                // test's own timeout instead of closing the socket, so a
                // broken implementation would hang on the read, not merely
                // see a clean EOF/connection-reset shortcut it might
                // handle differently.
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });

        let client = reqwest::Client::new();
        let base_url = format!("http://{addr}");
        let start = std::time::Instant::now();
        let err = get_json(
            &client,
            &base_url,
            "/status",
            None,
            Duration::from_millis(200),
        )
        .await
        .expect_err("a stalled body must time out, not hang");
        assert!(err.to_string().contains("timed out"));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must time out promptly per call_timeout (200ms), not wait anywhere near the 30s stall"
        );
    }
}

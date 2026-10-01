//! Direct HTTP forwarding for a generic (non-oMLX) `http_service` component
//! card's `POST /v1/chat/completions` — a Rust port of
//! `packages/router/src/proxy.ts`'s `ChatProxy` (buffered/non-streaming
//! half only; streaming pass-through is a follow-up PR). The upstream
//! response body is forwarded byte-for-byte — **never** re-wrapped into the
//! local-dispatch path's `openai_chat_completion` shape, matching TS: a
//! `form: http_service` candidate is a transparent proxy, not a backend
//! `RuntimeAdapter` call.
//!
//! Non-streaming: up to 2 backoff retries (250ms, then 1000ms) on a `>=500`
//! upstream status or a transport error; the 60s-window idempotency cache
//! from the previous PR is consulted first and updated on every genuinely
//! successful (2xx) call that carries an `X-iDoris-Request-Id`.
//!
//! ## Idempotency cache key
//!
//! **Must include tenant, endpoint, and provider id — never just the
//! request id.** `X-iDoris-Request-Id` is caller-supplied; keying on it
//! alone would let two tenants that happen to collide on the same value
//! (or one tenant guessing/replaying another's) share a cached response
//! body — a real cross-tenant data leak (see `proxy.ts`'s own doc for the
//! PR #25 review finding this fixes). Endpoint is included so the same
//! request id routed to a different provider doesn't return another
//! provider's response; provider id is included *in addition to* endpoint
//! (PR #46 review finding C1 on the TS side) because two component cards
//! can share the same physical endpoint string while declaring different
//! `locality` — keying on endpoint alone would let a cache entry written by
//! one card's request be replayed under the other card's `Served-Locality`.
//!
//! `\u{0}` as the field separator: it cannot appear in an HTTP header
//! value, so there is no `tenant="a:b"+id="c"` vs `tenant="a"+id="b:c"`
//! collision ambiguity.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use idoris_contracts::common::PrivacyClass;
use idoris_contracts::provider::Locality;
use indexmap::IndexMap;
use serde_json::Value;
use tokio::time::Instant;

/// TS default (`proxy.ts`'s `ProxyDeps.idempotencyWindowMs` default).
const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
/// TS default (`proxy.ts`'s `ProxyDeps.maxCacheEntries` default) — caps the
/// cheap memory-DoS surface a caller-controlled `X-iDoris-Request-Id` would
/// otherwise open (see `proxy.ts`'s own doc on this).
const DEFAULT_MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone)]
pub(crate) struct CacheEntry {
    pub(crate) at: Instant,
    pub(crate) status: u16,
    pub(crate) body: Bytes,
    /// `X-iDoris-Record-Id` of the request that first produced this entry
    /// — replayed as `X-iDoris-Origin-Record-Id` on a cache hit.
    pub(crate) record_id: String,
    /// The `Served-Locality` actually in effect when this entry was
    /// written — replayed as-is on a cache hit (never recomputed from
    /// "whichever card is selected this time"), and used for the
    /// `local_only` fail-closed check the follow-up PR wires in (PR #46
    /// review finding C1 on the TS side).
    pub(crate) served_locality: Locality,
}

pub(crate) fn cache_key(
    tenant_scope: &str,
    endpoint: &str,
    provider_id: &str,
    request_id: &str,
) -> String {
    format!("{tenant_scope}\u{0}{endpoint}\u{0}{provider_id}\u{0}{request_id}")
}

/// Per-request options for [`ChatProxy::forward_buffered`]. Every field is
/// required on purpose (no `Default`) — a silently-omitted `provider_id`/
/// `served_locality`/`privacy` would fail *open* (PR #46 review finding M1
/// on the TS side: these three used to be optional, and a missing one
/// either shared a single sentinel cache key across every provider or
/// skipped the `local_only` fail-closed check entirely).
pub struct ForwardOpts<'a> {
    pub request_id: Option<&'a str>,
    pub tenant_id: Option<&'a str>,
    /// This request's own server-generated `X-iDoris-Record-Id` — stashed
    /// in the cache entry on a write so a *later* cache hit can report
    /// `X-iDoris-Origin-Record-Id` pointing back at it.
    pub record_id: &'a str,
    pub provider_id: &'a str,
    pub served_locality: Locality,
    pub privacy: PrivacyClass,
}

/// [`ChatProxy::forward_buffered`]'s result.
pub struct ForwardOutcome {
    pub status: u16,
    pub body: Bytes,
    pub content_type: Option<String>,
    pub cached: bool,
    /// `Some` only on a cache hit — the `X-iDoris-Record-Id` of the request
    /// that first produced this body.
    pub origin_record_id: Option<String>,
    /// `Some` only on a cache hit — the `Served-Locality` recorded when
    /// this entry was written, which the caller must use verbatim instead
    /// of whatever it computed for *this* request (see this module's cache
    /// key doc, C1).
    pub replayed_served_locality: Option<Locality>,
    pub retries: u32,
}

pub struct ChatProxy {
    pub(crate) client: reqwest::Client,
    pub(crate) window: Duration,
    max_entries: usize,
    idle_timeout: Duration,
    permits: Arc<tokio::sync::Semaphore>,
    header_timeout: Duration,
    body_timeout: Duration,
    max_body_bytes: usize,
    max_cache_bytes: usize,
    pub(crate) retry_delays: Vec<Duration>,
    pub(crate) cache: Mutex<IndexMap<String, CacheEntry>>,
}

impl ChatProxy {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            window: DEFAULT_WINDOW,
            max_entries: DEFAULT_MAX_ENTRIES,
            idle_timeout: Duration::from_secs(30),
            permits: Arc::new(tokio::sync::Semaphore::new(32)),
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(60),
            max_body_bytes: 8 * 1024 * 1024,
            max_cache_bytes: 32 * 1024 * 1024,
            // TS default (`proxy.ts`'s `ProxyDeps.retryDelaysMs` default).
            retry_delays: vec![Duration::from_millis(250), Duration::from_secs(1)],
            cache: Mutex::new(IndexMap::new()),
        }
    }

    /// Test-only: a short window (so a test can sleep past it without a
    /// real 60s wait) and/or shorter retry delays, mirroring TS's own
    /// `ProxyDeps` test-injection pattern. Not `#[cfg(test)]` itself (that
    /// would make it invisible to `crates/idoris-router/src/lib.rs`'s own
    /// non-test build, which is fine here since nothing outside `#[cfg(test)]`
    /// calls it yet) — `#[allow(dead_code)]` instead, since a real non-test
    /// caller may legitimately want a custom window/retry policy later.
    #[allow(dead_code)]
    pub(crate) fn with_config(
        client: reqwest::Client,
        window: Duration,
        retry_delays: Vec<Duration>,
    ) -> Self {
        let mut proxy = Self::new(client);
        proxy.window = window;
        proxy.retry_delays = retry_delays;
        proxy
    }

    #[cfg(test)]
    fn cache_size_for_test(&self) -> usize {
        #[allow(clippy::unwrap_used)] // test-only, poisoning would already have failed the test
        self.cache.lock().unwrap().len()
    }

    /// Writes a cache entry, then prunes — mirrors `proxy.ts`'s own
    /// `remember`/`prune` split (see that file's doc for why pruning must
    /// happen on every *write*, not only opportunistically on a read hit: a
    /// key that's never read again would otherwise never be evicted,
    /// letting a caller cheaply grow the cache unbounded by cycling through
    /// distinct `X-iDoris-Request-Id` values).
    ///
    /// `shift_remove` + `insert` (not a plain `insert`, which would leave a
    /// pre-existing key in its *original* position) keeps insertion order
    /// == expiry order, which [`Self::prune`]'s early-break scan depends on.
    pub(crate) fn remember(&self, key: String, entry: CacheEntry) {
        #[allow(clippy::unwrap_used)] // poisoning would already have failed a concurrent caller
        let mut cache = self.cache.lock().unwrap();
        cache.shift_remove(&key);
        if Self::entry_bytes(&key, &entry) <= self.max_cache_bytes {
            cache.insert(key, entry);
        }
        self.prune(&mut cache);
    }

    /// All entries share one `window`, and [`Self::remember`] maintains
    /// insertion order == expiry order, so scanning from the front and
    /// stopping at the first non-expired entry is correct and amortized
    /// O(expired-count), not O(n) per call.
    fn prune(&self, cache: &mut IndexMap<String, CacheEntry>) {
        let cutoff = Instant::now()
            .checked_sub(self.window)
            .unwrap_or_else(Instant::now);
        while let Some((_, entry)) = cache.first() {
            if entry.at > cutoff {
                break;
            }
            cache.shift_remove_index(0);
        }
        let mut bytes = cache.iter().fold(0usize, |total, (key, entry)| {
            total.saturating_add(Self::entry_bytes(key, entry))
        });
        while cache.len() > self.max_entries || bytes > self.max_cache_bytes {
            if let Some((key, entry)) = cache.shift_remove_index(0) {
                bytes = bytes.saturating_sub(Self::entry_bytes(&key, &entry));
            } else {
                break;
            }
        }
    }

    fn entry_bytes(key: &str, entry: &CacheEntry) -> usize {
        key.len()
            .saturating_add(entry.record_id.len())
            .saturating_add(entry.body.len())
    }

    fn failure(status: u16, retries: u32) -> ForwardOutcome {
        ForwardOutcome {
            status,
            body: Bytes::from_static(br#"{"error":{"type":"upstream_unavailable"}}"#),
            content_type: Some("application/json".into()),
            cached: false,
            origin_record_id: None,
            replayed_served_locality: None,
            retries,
        }
    }

    // A total deadline prevents a slow trickle from retaining a buffered
    // request forever. Check both declared length and actual chunk bytes.
    async fn read_buffered(&self, mut response: reqwest::Response) -> Result<Bytes, u16> {
        if response
            .content_length()
            .is_some_and(|n| n > self.max_body_bytes as u64)
        {
            return Err(502);
        }
        tokio::time::timeout(self.body_timeout, async {
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| 502u16)? {
                if chunk.len() > self.max_body_bytes.saturating_sub(bytes.len()) {
                    return Err(502);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(Bytes::from(bytes))
        })
        .await
        .map_err(|_| 504u16)?
    }

    async fn sleep_retry(&self, attempt: usize) {
        let delay = self
            .retry_delays
            .get(attempt)
            .copied()
            .unwrap_or(Duration::from_secs(1));
        tokio::time::sleep(delay).await;
    }

    /// Forwards `body` (with `stream: false` folded in) to
    /// `{endpoint}/v1/chat/completions`. See this module's doc for the
    /// retry/cache/pass-through contract.
    pub async fn forward_buffered(
        &self,
        endpoint: &str,
        body: &Value,
        opts: &ForwardOpts<'_>,
    ) -> ForwardOutcome {
        let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
        let tenant_scope = opts.tenant_id.unwrap_or("\u{0}personal");

        if let Some(request_id) = opts.request_id {
            let key = cache_key(tenant_scope, &url, opts.provider_id, request_id);
            #[allow(clippy::unwrap_used)]
            let hit = self.cache.lock().unwrap().get(&key).cloned();
            if let Some(entry) = hit
                && Instant::now().duration_since(entry.at) < self.window
            {
                // C1 fail-closed: a local_only request must never replay a
                // cache entry whose *recorded* Served-Locality isn't
                // loopback (a missing/foreign value is treated as unsafe,
                // not defaulted to "assume it's fine").
                let unsafe_for_local_only = opts.privacy == PrivacyClass::LocalOnly
                    && entry.served_locality != Locality::Loopback;
                if !unsafe_for_local_only {
                    return ForwardOutcome {
                        status: entry.status,
                        body: entry.body,
                        content_type: Some("application/json".to_string()),
                        cached: true,
                        origin_record_id: Some(entry.record_id),
                        replayed_served_locality: Some(entry.served_locality),
                        retries: 0,
                    };
                }
            }
        }

        let Ok(_permit) = self.permits.clone().try_acquire_owned() else {
            return Self::failure(503, 0);
        };

        let mut payload = body.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".to_string(), Value::Bool(false));
        }

        let mut attempt = 0usize;
        loop {
            // One deadline covers connection establishment and response
            // headers. An expired POST is uncertain: do not retry it.
            let sent = match tokio::time::timeout(
                self.header_timeout,
                self.client.post(&url).json(&payload).send(),
            )
            .await
            {
                Ok(sent) => sent,
                Err(_) => return Self::failure(504, attempt as u32),
            };
            match sent {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status >= 500 && attempt < self.retry_delays.len() {
                        self.sleep_retry(attempt).await;
                        attempt += 1;
                        continue;
                    }
                    let content_type = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    #[allow(clippy::cast_possible_truncation)]
                    let retries = attempt as u32;
                    // A body that fails to read (connection dropped
                    // mid-body, ...) is an upstream failure, not an empty
                    // success: never surface — let alone cache and replay
                    // for 60s — a 2xx with a truncated/empty body
                    // (prdaemon #48 round 2, Low). Not retried: the
                    // upstream already accepted and processed this call.
                    let body_bytes = match self.read_buffered(resp).await {
                        Ok(bytes) => bytes,
                        Err(status) => return Self::failure(status, retries),
                    };
                    // Only a genuinely successful (2xx) call is cached —
                    // matches TS's own `res.ok` gate on the `remember()`
                    // call site exactly (a 4xx is never retried above
                    // either, so "not currently retrying" alone isn't the
                    // right condition here; a 4xx must still reach this
                    // point without being cached).
                    if (200..300).contains(&status)
                        && let Some(request_id) = opts.request_id
                    {
                        self.remember(
                            cache_key(tenant_scope, &url, opts.provider_id, request_id),
                            CacheEntry {
                                at: Instant::now(),
                                status,
                                body: body_bytes.clone(),
                                record_id: opts.record_id.to_string(),
                                served_locality: opts.served_locality,
                            },
                        );
                    }
                    return ForwardOutcome {
                        status,
                        body: body_bytes,
                        content_type,
                        cached: false,
                        origin_record_id: None,
                        replayed_served_locality: None,
                        retries,
                    };
                }
                Err(err) => {
                    if err.is_timeout() {
                        return Self::failure(504, attempt as u32);
                    }
                    if attempt < self.retry_delays.len() {
                        self.sleep_retry(attempt).await;
                        attempt += 1;
                        continue;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let retries = attempt as u32;
                    let body = Bytes::from_static(br#"{"error":{"type":"upstream_unavailable"}}"#);
                    return ForwardOutcome {
                        status: 502,
                        body,
                        content_type: Some("application/json".to_string()),
                        cached: false,
                        origin_record_id: None,
                        replayed_served_locality: None,
                        retries,
                    };
                }
            }
        }
    }

    /// Streaming variant of [`Self::forward_buffered`]. **Never retries**
    /// (mirrors `proxy.ts`'s own `!opts.stream` guard on its retry branch —
    /// once a streaming request has started, retrying would duplicate
    /// tokens the client already received) and never touches the
    /// idempotency cache (a streamed response is not a replay candidate).
    /// A non-2xx initial response is returned **buffered**, not streamed —
    /// this matches `proxy.ts` exactly: its `if (!res.ok) { ... return
    /// {status, text, stream: null, ...} }` block runs unconditionally on
    /// `opts.stream` (only the *retry* sub-branch inside it is gated on
    /// `!opts.stream`), so a streaming request whose upstream call fails
    /// outright still gets a plain JSON/text error body, never an SSE
    /// stream carrying an error.
    pub async fn forward_stream(&self, endpoint: &str, body: &Value) -> StreamOutcome {
        // Reject immediately rather than accumulate unbounded waiters.
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            return Self::stream_failure(503);
        };
        let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
        let mut payload = body.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".to_string(), Value::Bool(true));
        }
        let sent = match tokio::time::timeout(
            self.header_timeout,
            self.client.post(&url).json(&payload).send(),
        )
        .await
        {
            Ok(sent) => sent,
            Err(_) => return Self::stream_failure(504),
        };
        match sent {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if (200..300).contains(&status) {
                    let idle = self.idle_timeout;
                    // Keep upstream reads running independently of downstream
                    // body polling. A single queued chunk bounds buffering;
                    // both reading and waiting for queue capacity have an
                    // idle deadline. Dropping the body closes the receiver,
                    // which cancels this task and drops the response/permit.
                    let (tx, rx) = tokio::sync::mpsc::channel(1);
                    let (error_tx, error_rx) = tokio::sync::oneshot::channel();
                    tokio::spawn(async move {
                        let mut resp = resp;
                        let mut terminal_error = None;
                        loop {
                            let read = tokio::select! {
                                _ = tx.closed() => break,
                                read = tokio::time::timeout(idle, resp.chunk()) => read,
                            };
                            let chunk = match read {
                                Err(_) => {
                                    terminal_error = Some(std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        "upstream stream idle timeout",
                                    ));
                                    break;
                                }
                                Ok(Err(err)) => {
                                    terminal_error = Some(std::io::Error::other(err));
                                    break;
                                }
                                Ok(Ok(None)) => break,
                                Ok(Ok(Some(chunk))) => chunk,
                            };
                            let reserve = tokio::select! {
                                _ = tx.closed() => break,
                                reserve = tokio::time::timeout(idle, tx.reserve()) => reserve,
                            };
                            let slot = match reserve {
                                Ok(Ok(slot)) => slot,
                                Ok(Err(_)) => break,
                                Err(_) => {
                                    terminal_error = Some(std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        "upstream stream idle timeout while downstream is backpressured",
                                    ));
                                    break;
                                }
                            };
                            slot.send(chunk);
                        }
                        drop(resp);
                        drop(permit);
                        if let Some(error) = terminal_error {
                            let _ = error_tx.send(error);
                        }
                    });
                    let stream = futures_util::stream::unfold(
                        (rx, Some(error_rx)),
                        |(mut rx, error_rx)| async move {
                            if let Some(chunk) = rx.recv().await {
                                return Some((Ok(chunk), (rx, error_rx)));
                            }
                            let error_rx = error_rx?;
                            match error_rx.await {
                                Ok(error) => Some((Err(error), (rx, None))),
                                Err(_) => None,
                            }
                        },
                    );
                    StreamOutcome::Stream {
                        status,
                        content_type,
                        response: axum::body::Body::from_stream(stream),
                    }
                } else {
                    match self.read_buffered(resp).await {
                        Ok(body) => StreamOutcome::Buffered {
                            status,
                            body,
                            content_type,
                        },
                        Err(status) => Self::stream_failure(status),
                    }
                }
            }
            Err(err) => Self::stream_failure(if err.is_timeout() { 504 } else { 502 }),
        }
    }

    fn stream_failure(status: u16) -> StreamOutcome {
        let failure = Self::failure(status, 0);
        StreamOutcome::Buffered {
            status,
            body: failure.body,
            content_type: failure.content_type,
        }
    }
}

/// [`ChatProxy::forward_stream`]'s result: either the initial response
/// wasn't ok (returned buffered, see that method's doc) or it was, in which
/// case a background producer owns the upstream connection and permit while
/// `response` provides bounded buffering and cancels that producer on drop.
/// Truncation or idle timeout remains visible as a body read error.
pub enum StreamOutcome {
    Buffered {
        status: u16,
        body: Bytes,
        content_type: Option<String>,
    },
    Stream {
        status: u16,
        content_type: Option<String>,
        response: axum::body::Body,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn entry(status: u16) -> CacheEntry {
        CacheEntry {
            at: Instant::now(),
            status,
            body: Bytes::from_static(b"{}"),
            record_id: "rec-1".to_string(),
            served_locality: Locality::Loopback,
        }
    }

    #[test]
    fn remember_then_lookup_round_trips() {
        let proxy = ChatProxy::new(reqwest::Client::new());
        proxy.remember("k1".to_string(), entry(200));
        assert_eq!(proxy.cache_size_for_test(), 1);
        #[allow(clippy::unwrap_used)]
        let got = proxy.cache.lock().unwrap().get("k1").cloned().unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.record_id, "rec-1");
    }

    #[test]
    fn re_remembering_the_same_key_replaces_it_and_moves_it_to_the_back() {
        let proxy = ChatProxy::new(reqwest::Client::new());
        proxy.remember("k1".to_string(), entry(200));
        proxy.remember("k2".to_string(), entry(200));
        proxy.remember("k1".to_string(), entry(201));
        assert_eq!(proxy.cache_size_for_test(), 2);
        #[allow(clippy::unwrap_used)]
        let cache = proxy.cache.lock().unwrap();
        let (last_key, last_entry) = cache.last().unwrap();
        assert_eq!(last_key, "k1");
        assert_eq!(last_entry.status, 201);
    }

    #[tokio::test]
    async fn prune_evicts_entries_older_than_the_window_on_the_next_write() {
        let proxy =
            ChatProxy::with_config(reqwest::Client::new(), Duration::from_millis(20), vec![]);
        proxy.remember("old".to_string(), entry(200));
        assert_eq!(proxy.cache_size_for_test(), 1);
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Nothing new written yet -- prune() hasn't run again, confirming
        // pruning is write-triggered, not a background sweep.
        assert_eq!(proxy.cache_size_for_test(), 1);
        proxy.remember("new".to_string(), entry(200));
        // The write above's own prune() call evicts the now-expired "old".
        assert_eq!(proxy.cache_size_for_test(), 1);
        #[allow(clippy::unwrap_used)]
        let cache = proxy.cache.lock().unwrap();
        assert!(cache.contains_key("new"));
        assert!(!cache.contains_key("old"));
    }

    #[test]
    fn prune_caps_at_max_entries_oldest_first_even_when_nothing_has_expired() {
        let proxy = ChatProxy::new(reqwest::Client::new()); // DEFAULT_MAX_ENTRIES = 1000, 60s window
        for i in 0..1005 {
            proxy.remember(format!("k{i}"), entry(200));
        }
        assert_eq!(proxy.cache_size_for_test(), DEFAULT_MAX_ENTRIES);
        #[allow(clippy::unwrap_used)]
        let cache = proxy.cache.lock().unwrap();
        // The oldest 5 (k0..k4) were evicted; the newest survive.
        assert!(!cache.contains_key("k0"));
        assert!(cache.contains_key("k1004"));
    }

    fn opts<'a>(request_id: Option<&'a str>, record_id: &'a str) -> ForwardOpts<'a> {
        ForwardOpts {
            request_id,
            tenant_id: None,
            record_id,
            provider_id: "omlx",
            served_locality: Locality::Loopback,
            privacy: PrivacyClass::Any,
        }
    }

    fn chat_body() -> Value {
        serde_json::json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "hi"}]})
    }

    #[tokio::test]
    async fn a_successful_call_is_forwarded_byte_for_byte() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"marker": "raw-passthrough"})),
            )
            .mount(&server)
            .await;
        let proxy = ChatProxy::new(reqwest::Client::new());
        let out = proxy
            .forward_buffered(&server.uri(), &chat_body(), &opts(None, "rec-1"))
            .await;
        assert_eq!(out.status, 200);
        assert!(!out.cached);
        assert_eq!(out.retries, 0);
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["marker"], "raw-passthrough");
    }

    /// prdaemon #48 round 2 (Low): a 2xx whose body fails mid-read must
    /// become a 502 and must not be cached for replay. Raw TCP because
    /// `wiremock` can't send "headers ok, body truncated".
    #[tokio::test]
    async fn a_2xx_whose_body_fails_to_read_is_a_502_and_is_not_cached() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                // Promise 100 bytes, send 2, hang up.
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{}",
                );
            }
        });
        let proxy = ChatProxy::new(reqwest::Client::new());
        let endpoint = format!("http://{addr}");
        let out = proxy
            .forward_buffered(&endpoint, &chat_body(), &opts(Some("req-1"), "rec-1"))
            .await;
        assert_eq!(out.status, 502);
        assert_eq!(
            proxy.cache_size_for_test(),
            0,
            "a failed read must not be cached"
        );
        let again = proxy
            .forward_buffered(&endpoint, &chat_body(), &opts(Some("req-1"), "rec-2"))
            .await;
        assert!(!again.cached, "nothing may be replayed from a failed read");
    }

    // K14 / M5: inject short deadlines and small byte budgets, without
    // process-global configuration or waiting for production deadlines.
    #[tokio::test]
    async fn k14_header_deadline_returns_504_without_retry() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_delay(Duration::from_millis(200)),
            )
            .mount(&server)
            .await;
        let mut proxy = fast_retry_proxy();
        proxy.header_timeout = Duration::from_millis(20);
        let out = tokio::time::timeout(
            Duration::from_millis(100),
            proxy.forward_buffered(&server.uri(), &chat_body(), &opts(Some("req"), "rec")),
        )
        .await
        .expect("response headers must have a deadline");
        assert_eq!(
            (out.status, out.retries, proxy.cache_size_for_test()),
            (504, 0, 0)
        );
    }

    #[tokio::test]
    async fn k14_buffered_body_deadline_returns_504() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0; 4096]);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nx")
                .unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let mut proxy = fast_retry_proxy();
        proxy.body_timeout = Duration::from_millis(20);
        let out = tokio::time::timeout(
            Duration::from_millis(100),
            proxy.forward_buffered(
                &format!("http://{addr}"),
                &chat_body(),
                &opts(Some("req"), "rec"),
            ),
        )
        .await
        .expect("buffered body must have a total deadline");
        assert_eq!((out.status, proxy.cache_size_for_test()), (504, 0));
    }

    #[tokio::test]
    async fn k14_oversize_body_is_502_and_never_cached() {
        use std::io::{Read, Write};
        // Cover both declared and unknown lengths (chunked).
        for headers in ["content-length: 5", "transfer-encoding: chunked"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let _ = socket.read(&mut [0; 4096]);
                let body = if headers.starts_with("content") {
                    "12345"
                } else {
                    "5\r\n12345\r\n0\r\n\r\n"
                };
                write!(socket, "HTTP/1.1 200 OK\r\n{headers}\r\n\r\n{body}").unwrap();
            });
            let mut proxy = fast_retry_proxy();
            proxy.max_body_bytes = 4;
            let out = proxy
                .forward_buffered(
                    &format!("http://{addr}"),
                    &chat_body(),
                    &opts(Some("req"), "rec"),
                )
                .await;
            assert_eq!((out.status, proxy.cache_size_for_test()), (502, 0));
        }
    }

    #[test]
    fn k14_cache_evicts_by_bytes_and_rejects_an_oversize_entry() {
        let mut proxy = ChatProxy::new(reqwest::Client::new());
        proxy.max_cache_bytes = 12;
        proxy.remember("a".into(), entry(200));
        proxy.remember("b".into(), entry(200));
        assert!(!proxy.cache.lock().unwrap().contains_key("a"));
        let mut large = entry(200);
        large.body = Bytes::from(vec![0; 17]);
        proxy.remember("huge".into(), large);
        assert!(!proxy.cache.lock().unwrap().contains_key("huge"));
    }

    #[tokio::test]
    async fn k14_stream_headers_and_error_body_are_bounded() {
        for (template, expected) in [
            (
                wiremock::ResponseTemplate::new(200).set_delay(Duration::from_millis(200)),
                504,
            ),
            (
                wiremock::ResponseTemplate::new(502).set_body_string("12345"),
                502,
            ),
        ] {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .respond_with(template)
                .expect(1)
                .mount(&server)
                .await;
            let mut proxy = fast_retry_proxy();
            proxy.header_timeout = Duration::from_millis(20);
            proxy.max_body_bytes = 4;
            let out = tokio::time::timeout(
                Duration::from_millis(100),
                proxy.forward_stream(&server.uri(), &chat_body()),
            )
            .await
            .expect("stream response headers must have a deadline");
            let StreamOutcome::Buffered { status, body, .. } = out else {
                panic!("expected a bounded error response");
            };
            assert_eq!(status, expected);
            assert_ne!(body.as_ref(), b"12345");
        }
    }

    #[tokio::test]
    async fn k14_stream_idle_timeout_errors_and_releases_permit() {
        use http_body_util::BodyExt;
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0; 4096]);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nx")
                .unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let mut proxy = fast_retry_proxy();
        proxy.idle_timeout = Duration::from_millis(20);
        proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
        let StreamOutcome::Stream { mut response, .. } = proxy
            .forward_stream(&format!("http://{addr}"), &chat_body())
            .await
        else {
            panic!("expected stream");
        };
        assert_eq!(
            response
                .frame()
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap(),
            "x"
        );
        let error = tokio::time::timeout(Duration::from_millis(100), response.frame())
            .await
            .expect("an idle stream must fail")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("idle"));
        assert_eq!(proxy.permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn k14_stream_holds_permit_until_body_drop_or_eof() {
        use http_body_util::BodyExt;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .expect(2)
            .mount(&server)
            .await;
        let mut proxy = fast_retry_proxy();
        proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
        let out = proxy.forward_stream(&server.uri(), &chat_body()).await;
        let denied = proxy
            .forward_buffered(&server.uri(), &chat_body(), &opts(None, "r"))
            .await;
        assert_eq!(denied.status, 503);
        assert!(matches!(
            proxy.forward_stream(&server.uri(), &chat_body()).await,
            StreamOutcome::Buffered { status: 503, .. }
        ));
        drop(out);
        let _released = tokio::time::timeout(
            Duration::from_millis(100),
            proxy.permits.clone().acquire_owned(),
        )
        .await
        .expect("dropping the body must cancel its producer and release the permit")
        .expect("semaphore remains open");
        drop(_released);
        assert_eq!(proxy.permits.available_permits(), 1);
        let StreamOutcome::Stream { response, .. } =
            proxy.forward_stream(&server.uri(), &chat_body()).await
        else {
            panic!("expected stream");
        };
        assert_eq!(response.collect().await.unwrap().to_bytes(), "ok");
        assert_eq!(proxy.permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn k14_buffered_holds_permit_and_cancellation_releases_it() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0; 4096]);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nx")
                .unwrap();
            let _ = sent.send(());
            std::thread::sleep(Duration::from_millis(200));
        });
        let mut proxy = fast_retry_proxy();
        proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
        let endpoint = format!("http://{addr}");
        let body = chat_body();
        let options = opts(None, "r");
        let mut pending = Box::pin(proxy.forward_buffered(&endpoint, &body, &options));
        tokio::select! {
            _ = received => {},
            _ = &mut pending => panic!("body must still be pending"),
        }
        assert_eq!(proxy.permits.available_permits(), 0);
        drop(pending);
        assert_eq!(proxy.permits.available_permits(), 1);
    }

    /// Counts requests received so the test can assert an exact retry
    /// count and return a different response per attempt.
    struct CountingRespond {
        calls: std::sync::atomic::AtomicU32,
        responses: Vec<wiremock::ResponseTemplate>,
    }

    impl wiremock::Respond for CountingRespond {
        fn respond(&self, _req: &wiremock::Request) -> wiremock::ResponseTemplate {
            let i = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as usize;
            self.responses
                .get(i)
                .cloned()
                .unwrap_or_else(|| self.responses[self.responses.len() - 1].clone())
        }
    }

    fn fast_retry_proxy() -> ChatProxy {
        ChatProxy::with_config(
            reqwest::Client::new(),
            Duration::from_secs(60),
            vec![Duration::from_millis(5), Duration::from_millis(5)],
        )
    }

    #[tokio::test]
    async fn retries_up_to_twice_on_5xx_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        let responder = CountingRespond {
            calls: std::sync::atomic::AtomicU32::new(0),
            responses: vec![
                wiremock::ResponseTemplate::new(500),
                wiremock::ResponseTemplate::new(500),
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            ],
        };
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(responder)
            .mount(&server)
            .await;
        let out = fast_retry_proxy()
            .forward_buffered(&server.uri(), &chat_body(), &opts(None, "rec-1"))
            .await;
        assert_eq!(out.status, 200);
        assert_eq!(out.retries, 2);
    }

    #[tokio::test]
    async fn persistent_5xx_is_passed_through_verbatim_after_exhausting_retries() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(503)
                    .set_body_json(serde_json::json!({"error": "always-503"})),
            )
            .mount(&server)
            .await;
        let out = fast_retry_proxy()
            .forward_buffered(&server.uri(), &chat_body(), &opts(None, "rec-1"))
            .await;
        assert_eq!(out.status, 503);
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["error"], "always-503");
    }

    #[tokio::test]
    async fn same_request_id_hits_the_cache_and_a_local_only_hit_never_replays_a_remote_entry() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"marker": "idem"})),
            )
            .expect(3) // first + remote_opts + local_only_opts (C1 forces a real call, no cache hit)
            .mount(&server)
            .await;
        let proxy = ChatProxy::new(reqwest::Client::new());
        let first = proxy
            .forward_buffered(&server.uri(), &chat_body(), &opts(Some("req-1"), "rec-1"))
            .await;
        assert!(!first.cached);
        let second = proxy
            .forward_buffered(&server.uri(), &chat_body(), &opts(Some("req-1"), "rec-2"))
            .await;
        assert!(second.cached);
        assert_eq!(second.origin_record_id.as_deref(), Some("rec-1"));
        assert_eq!(second.body, first.body);

        // PR #46 review finding C1: a local_only request must not replay a
        // cache entry whose recorded Served-Locality isn't loopback.
        let mut remote_opts = opts(Some("req-remote"), "rec-3");
        remote_opts.served_locality = Locality::Remote;
        proxy
            .forward_buffered(&server.uri(), &chat_body(), &remote_opts)
            .await;
        let mut local_only_opts = opts(Some("req-remote"), "rec-4");
        local_only_opts.privacy = PrivacyClass::LocalOnly;
        let third = proxy
            .forward_buffered(&server.uri(), &chat_body(), &local_only_opts)
            .await;
        assert!(!third.cached);
        server.verify().await;
    }

    #[tokio::test]
    async fn a_transport_error_maps_to_502_upstream_unavailable_after_retries() {
        // No mock mounted at all -- every connection attempt fails outright.
        let out = fast_retry_proxy()
            .forward_buffered("http://127.0.0.1:1", &chat_body(), &opts(None, "rec-1"))
            .await;
        assert_eq!(out.status, 502);
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["error"]["type"], "upstream_unavailable");
    }

    #[tokio::test]
    async fn forward_stream_returns_the_stream_variant_on_a_2xx_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw("data: hi\n\n", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let proxy = ChatProxy::new(reqwest::Client::new());
        match proxy.forward_stream(&server.uri(), &chat_body()).await {
            StreamOutcome::Stream {
                status,
                content_type,
                ..
            } => {
                assert_eq!(status, 200);
                assert_eq!(content_type.as_deref(), Some("text/event-stream"));
            }
            StreamOutcome::Buffered { .. } => {
                panic!("expected a Stream outcome for a 2xx response")
            }
        }
    }

    /// Conformance parity (`streaming.test.ts`'s negative control): a
    /// streaming request whose upstream call fails outright must NOT retry
    /// and must come back buffered, not as a stream.
    #[tokio::test]
    async fn forward_stream_never_retries_and_is_buffered_on_a_5xx_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(502)
                    .set_body_json(serde_json::json!({"error": "upstream-down"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let proxy = ChatProxy::new(reqwest::Client::new());
        match proxy.forward_stream(&server.uri(), &chat_body()).await {
            StreamOutcome::Buffered {
                status,
                body,
                content_type,
            } => {
                assert_eq!(status, 502);
                assert_eq!(content_type.as_deref(), Some("application/json"));
                let json: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(json["error"], "upstream-down");
            }
            StreamOutcome::Stream { .. } => panic!("a non-2xx response must not be streamed"),
        }
        server.verify().await;
    }

    #[test]
    fn cache_key_includes_all_four_components_distinctly() {
        let a = cache_key("tenant-a", "http://x", "omlx", "req-1");
        let b = cache_key("tenant-b", "http://x", "omlx", "req-1");
        let c = cache_key("tenant-a", "http://y", "omlx", "req-1");
        let d = cache_key("tenant-a", "http://x", "other", "req-1");
        let e = cache_key("tenant-a", "http://x", "omlx", "req-2");
        let all = [a, b, c, d, e];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "keys at {i} and {j} must differ");
            }
        }
    }
}

#[cfg(test)]
#[path = "proxy/stream_limits_tests.rs"]
mod stream_limits_tests;

//! Direct HTTP forwarding for a generic (non-oMLX) `http_service` component
//! card's `POST /v1/chat/completions` — a Rust port of
//! `packages/router/src/proxy.ts`'s `ChatProxy` (buffered and streaming).
//! The upstream response body is forwarded byte-for-byte — **never** re-wrapped into the
//! local-dispatch path's `openai_chat_completion` shape, matching TS: a
//! `form: http_service` candidate is a transparent proxy, not a backend
//! `RuntimeAdapter` call.
//!
//! Non-streaming: up to 2 backoff retries (250ms, then 1000ms) only on
//! connection establishment failures; the 60s-window idempotency cache
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

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use futures_util::stream;
use idoris_contracts::common::PrivacyClass;
use idoris_contracts::provider::Locality;
use indexmap::IndexMap;
use serde_json::Value;
use tokio::time::Instant;

const FINGERPRINT_BYTES: usize = 32;
struct DigestWriter(ring::digest::Context);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fingerprint(
    payload: &Value,
    privacy: PrivacyClass,
    locality: Locality,
) -> Result<[u8; 32], serde_json::Error> {
    // serde_json's default Map is key ordered (the preserve_order feature is
    // disabled), so semantically identical JSON objects hash identically.
    // Writing directly into SHA-256 avoids retaining a second serialized body.
    let mut writer = DigestWriter(ring::digest::Context::new(&ring::digest::SHA256));
    serde_json::to_writer(&mut writer, &(payload, privacy, locality))?;
    let digest = writer.0.finish();
    let mut output = [0; FINGERPRINT_BYTES];
    output.copy_from_slice(digest.as_ref());
    Ok(output)
}

/// Keeps the in-flight response permit attached to the allocation handed to
/// hyper. `Bytes` clones and slices retain this owner until their last drop.
struct PermittedBytes {
    bytes: Bytes,
    _permit: Arc<tokio::sync::OwnedSemaphorePermit>,
}

impl AsRef<[u8]> for PermittedBytes {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

fn with_permit(bytes: Bytes, permit: Arc<tokio::sync::OwnedSemaphorePermit>) -> Bytes {
    Bytes::from_owner(PermittedBytes {
        bytes,
        _permit: permit,
    })
}

/// TS default (`proxy.ts`'s `ProxyDeps.idempotencyWindowMs` default).
const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
/// TS default (`proxy.ts`'s `ProxyDeps.maxCacheEntries` default) — caps the
/// cheap memory-DoS surface a caller-controlled `X-iDoris-Request-Id` would
/// otherwise open (see `proxy.ts`'s own doc on this).
const DEFAULT_MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone)]
pub(crate) struct CacheEntry {
    pub(crate) at: Instant,
    fingerprint: [u8; 32],
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
    /// Paid direct-proxy calls require trustworthy OpenAI usage evidence
    /// before a 2xx response may be retained or replayed.
    pub require_openai_usage: bool,
}

/// [`ChatProxy::forward_buffered`]'s result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionDisposition {
    /// No upstream execution happened for this caller.
    NotExecuted,
    /// The upstream returned a complete HTTP response.
    Executed,
    /// The POST may have executed, but a trustworthy terminal response is unavailable.
    Uncertain,
    /// This caller received a retained/cache/singleflight replay.
    Replay,
}

#[derive(Clone)]
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
    pub execution: ExecutionDisposition,
}

struct Flight {
    fingerprint: [u8; 32],
    outcome: tokio::sync::Mutex<Option<ForwardOutcome>>,
    /// Set for completed successful calls, or when execution may have happened
    /// without a complete response, a complete 3xx/5xx response was received, or
    /// the leader future was dropped before forwarding completes. The registry
    /// keeps these completed/uncertain records through the idempotency window.
    cancelled_at: Mutex<Option<Instant>>,
    retained_bytes: AtomicUsize,
    flight_bytes: Arc<AtomicUsize>,
    base_bytes: usize,
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.flight_bytes.fetch_sub(
            self.retained_bytes.load(Ordering::Relaxed),
            Ordering::AcqRel,
        );
    }
}

struct FlightCancellationGuard {
    flight: Arc<Flight>,
    armed: bool,
}

impl FlightCancellationGuard {
    fn new(flight: Arc<Flight>) -> Self {
        Self {
            flight,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn retain_completed(&mut self) {
        if let Ok(mut cancelled_at) = self.flight.cancelled_at.lock() {
            *cancelled_at = Some(Instant::now());
        }
        self.disarm();
    }
}

impl Drop for FlightCancellationGuard {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut cancelled_at) = self.flight.cancelled_at.lock()
        {
            *cancelled_at = Some(Instant::now());
        }
    }
}

pub struct ChatProxy {
    pub(crate) client: reqwest::Client,
    pub(crate) window: Duration,
    max_entries: usize,
    header_timeout: Duration,
    body_timeout: Duration,
    max_body_bytes: usize,
    max_cache_bytes: usize,
    /// Independent cap for flight keys, registry records, and outcomes kept
    /// through the idempotency window.
    max_flight_bytes: usize,
    flight_bytes: Arc<AtomicUsize>,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    stream_idle_timeout: Duration,
    #[cfg(test)]
    pub(crate) stream_eof_observed: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    pub(crate) stream_read_waiting: Option<Arc<tokio::sync::Notify>>,
    pub(crate) retry_delays: Vec<Duration>,
    pub(crate) cache: Mutex<IndexMap<String, CacheEntry>>,
    flights: Mutex<IndexMap<String, Arc<Flight>>>,
}

impl ChatProxy {
    /// Creates a proxy. The injected `client` must have redirects disabled
    /// for connection-failure retries to be safe, since a redirect can obscure
    /// whether the upstream accepted the POST.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            window: DEFAULT_WINDOW,
            max_entries: DEFAULT_MAX_ENTRIES,
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(60),
            max_body_bytes: 8 * 1024 * 1024,
            max_cache_bytes: 32 * 1024 * 1024,
            // Flight records/outcomes and cache entries each have a 32 MiB pool.
            max_flight_bytes: 32 * 1024 * 1024,
            flight_bytes: Arc::new(AtomicUsize::new(0)),
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(32)),
            stream_idle_timeout: Duration::from_secs(30),
            #[cfg(test)]
            stream_eof_observed: None,
            #[cfg(test)]
            stream_read_waiting: None,
            // TS default (`proxy.ts`'s `ProxyDeps.retryDelaysMs` default).
            retry_delays: vec![Duration::from_millis(250), Duration::from_secs(1)],
            cache: Mutex::new(IndexMap::new()),
            flights: Mutex::new(IndexMap::new()),
        }
    }

    /// Test-only: a short window (so a test can sleep past it without a
    /// real 60s wait) and/or shorter retry delays, mirroring TS's own
    /// `ProxyDeps` test-injection pattern. Not `#[cfg(test)]` itself (that
    /// would make it invisible to `crates/idoris-router/src/lib.rs`'s own
    /// non-test build, which is fine here since nothing outside `#[cfg(test)]`
    /// calls it yet) — `#[allow(dead_code)]` instead, since a real non-test
    /// caller may legitimately want a custom window/retry policy later.
    /// The injected `client` must have redirects disabled for
    /// connection-failure retries to be safe, since a redirect can obscure
    /// whether the upstream accepted the POST.
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
            .saturating_add(std::mem::size_of::<CacheEntry>())
    }

    fn outcome_bytes(outcome: &ForwardOutcome) -> usize {
        // The inline outcome is already part of Flight; charge its allocations.
        outcome
            .body
            .len()
            .saturating_add(outcome.content_type.as_ref().map_or(0, String::capacity))
            .saturating_add(
                outcome
                    .origin_record_id
                    .as_ref()
                    .map_or(0, String::capacity),
            )
    }

    fn flight_base_bytes(key_capacity: usize) -> usize {
        // Include the registry slot and Arc's strong/weak counters as well as
        // the caller-controlled key allocation and the inline flight state.
        key_capacity
            .saturating_add(std::mem::size_of::<Flight>())
            .saturating_add(std::mem::size_of::<(String, Arc<Flight>)>())
            .saturating_add(2 * std::mem::size_of::<usize>())
    }

    fn reserve_flight_bytes(&self, bytes: usize) -> bool {
        let mut current = self.flight_bytes.load(Ordering::Acquire);
        loop {
            if bytes > self.max_flight_bytes.saturating_sub(current) {
                return false;
            }
            match self.flight_bytes.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn failure(status: u16, retries: u32) -> ForwardOutcome {
        Self::failure_with_execution(status, retries, ExecutionDisposition::NotExecuted)
    }

    fn failure_with_execution(
        status: u16,
        retries: u32,
        execution: ExecutionDisposition,
    ) -> ForwardOutcome {
        ForwardOutcome {
            status,
            body: Bytes::from_static(br#"{"error":{"type":"upstream_unavailable"}}"#),
            content_type: Some("application/json".into()),
            cached: false,
            origin_record_id: None,
            replayed_served_locality: None,
            retries,
            execution,
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
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| if error.is_timeout() { 504u16 } else { 502u16 })?
            {
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
        // Every caller, including a cache hit or singleflight waiter, owns a
        // permit until its final outgoing bytes are dropped. Retained results
        // below stay unbound so the registries cannot hold permits indefinitely.
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return Self::failure(503, 0),
        };
        let mut outcome = self.forward_buffered_inner(endpoint, body, opts).await;
        outcome.body = with_permit(outcome.body, Arc::new(permit));
        outcome
    }

    async fn forward_buffered_inner(
        &self,
        endpoint: &str,
        body: &Value,
        opts: &ForwardOpts<'_>,
    ) -> ForwardOutcome {
        let mut payload = body.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".to_string(), Value::Bool(false));
        }
        // Record ids differ on replay; payload and safety context must not.
        let Ok(fingerprint) = fingerprint(&payload, opts.privacy, opts.served_locality) else {
            return Self::failure(502, 0);
        };
        let Some(request_id) = opts.request_id else {
            let mut uncertain = false;
            return self
                .forward_once(endpoint, &payload, opts, &fingerprint, &mut uncertain)
                .await;
        };
        let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
        let key = cache_key(
            opts.tenant_id.unwrap_or("\u{0}personal"),
            &url,
            opts.provider_id,
            request_id,
        );
        // A cache hit remains useful even when every flight slot is occupied.
        if let Some(hit) = self.lookup_cached(&key, &fingerprint, opts) {
            return hit;
        }
        let flight = {
            let Ok(mut flights) = self.flights.lock() else {
                return Self::flight_failure(502, "upstream_unavailable");
            };
            // Successful and uncertain calls stay through the idempotency
            // window, independently of response cache eviction.
            let now = Instant::now();
            flights.retain(|_, flight| {
                if Arc::strong_count(flight) > 1 {
                    return true;
                }
                match flight.cancelled_at.lock() {
                    Ok(cancelled_at) => {
                        cancelled_at.is_some_and(|at| now.duration_since(at) < self.window)
                    }
                    // A poisoned retention marker is uncertainty too; keep
                    // it permanently rather than allowing a duplicate POST.
                    Err(_) => true,
                }
            });
            if let Some(flight) = flights.get(&key) {
                Arc::clone(flight)
            } else {
                // Bound all retained slots: active flights may become
                // uncertain if their leaders are cancelled.
                if flights.len() >= self.max_entries {
                    // A successful call may have populated the cache after
                    // the preflight lookup but before this capacity check.
                    if let Some(hit) = self.lookup_cached(&key, &fingerprint, opts) {
                        return hit;
                    }
                    return Self::flight_failure(503, "upstream_unavailable");
                }
                let fallback = Self::flight_failure(502, "upstream_unavailable");
                let base_bytes = Self::flight_base_bytes(key.capacity());
                let initial_bytes = base_bytes.saturating_add(Self::outcome_bytes(&fallback));
                if !self.reserve_flight_bytes(initial_bytes) {
                    return Self::flight_failure(503, "upstream_unavailable");
                }
                let flight = Arc::new(Flight {
                    fingerprint,
                    outcome: tokio::sync::Mutex::new(None),
                    cancelled_at: Mutex::new(None),
                    retained_bytes: AtomicUsize::new(initial_bytes),
                    flight_bytes: Arc::clone(&self.flight_bytes),
                    base_bytes,
                });
                flights.insert(key.clone(), Arc::clone(&flight));
                flight
            }
        };
        if flight.fingerprint != fingerprint {
            return Self::flight_failure(409, "request_id_conflict");
        }
        let mut result = flight.outcome.lock().await;
        if let Some(outcome) = result.as_ref() {
            // The leader may have cached a successful response but failed to
            // retain its body in the bounded flight result. Prefer that valid
            // cache entry for waiters before returning the fail-closed marker.
            if outcome.status == 502
                && !outcome.cached
                && let Some(hit) = self.lookup_cached(&key, &fingerprint, opts)
            {
                return hit;
            }
            let mut replay = outcome.clone();
            replay.execution = ExecutionDisposition::Replay;
            return replay;
        }
        // If the leader is cancelled, waiting calls fail closed instead of resending.
        *result = Some(Self::flight_failure(502, "upstream_unavailable"));
        let mut cancellation_guard = FlightCancellationGuard::new(Arc::clone(&flight));
        let mut uncertain = false;
        let outcome = self
            .forward_once(endpoint, &payload, opts, &fingerprint, &mut uncertain)
            .await;
        let mut replay = outcome.clone();
        if (200..300).contains(&replay.status) && !replay.cached {
            replay.cached = true;
            replay.origin_record_id = Some(opts.record_id.to_string());
            replay.replayed_served_locality = Some(opts.served_locality);
            replay.execution = ExecutionDisposition::Replay;
        }
        let retained_bytes = flight
            .base_bytes
            .saturating_add(Self::outcome_bytes(&replay));
        let previous_total = flight.retained_bytes.load(Ordering::Acquire);
        if retained_bytes <= previous_total {
            let release = previous_total - retained_bytes;
            self.flight_bytes.fetch_sub(release, Ordering::AcqRel);
            flight
                .retained_bytes
                .store(retained_bytes, Ordering::Release);
        } else if self.reserve_flight_bytes(retained_bytes - previous_total) {
            flight
                .retained_bytes
                .store(retained_bytes, Ordering::Release);
        } else {
            // Keep a bounded uncertainty marker until expiry. Dropping a
            // completed but uncacheable result could permit a duplicate POST.
            uncertain = true;
            replay = Self::flight_failure_with_execution(
                502,
                "upstream_unavailable",
                ExecutionDisposition::Uncertain,
            );
        }
        if !uncertain && (200..300).contains(&outcome.status) {
            // Keep the request fingerprint through the idempotency window,
            // even if independent cache pruning removes the replay body.
            cancellation_guard.retain_completed();
        } else if !uncertain {
            cancellation_guard.disarm();
        }
        *result = Some(replay);
        outcome
    }

    fn flight_failure(status: u16, kind: &str) -> ForwardOutcome {
        Self::flight_failure_with_execution(status, kind, ExecutionDisposition::NotExecuted)
    }

    fn flight_failure_with_execution(
        status: u16,
        kind: &str,
        execution: ExecutionDisposition,
    ) -> ForwardOutcome {
        ForwardOutcome {
            status,
            body: Bytes::from(serde_json::json!({"error": {"type": kind}}).to_string()),
            content_type: Some("application/json".into()),
            cached: false,
            origin_record_id: None,
            replayed_served_locality: None,
            retries: 0,
            execution,
        }
    }

    fn lookup_cached(
        &self,
        key: &str,
        fingerprint: &[u8; 32],
        opts: &ForwardOpts<'_>,
    ) -> Option<ForwardOutcome> {
        #[allow(clippy::unwrap_used)]
        let hit = self.cache.lock().unwrap().get(key).cloned();
        let entry = hit.filter(|entry| Instant::now().duration_since(entry.at) < self.window)?;
        if entry.fingerprint != *fingerprint {
            return Some(Self::flight_failure(409, "request_id_conflict"));
        }
        // A local_only request must never replay a cache entry whose recorded
        // Served-Locality isn't loopback. Let it proceed through forwarding.
        if opts.privacy == PrivacyClass::LocalOnly && entry.served_locality != Locality::Loopback {
            return None;
        }
        Some(ForwardOutcome {
            status: entry.status,
            body: entry.body,
            content_type: Some("application/json".to_string()),
            cached: true,
            origin_record_id: Some(entry.record_id),
            replayed_served_locality: Some(entry.served_locality),
            retries: 0,
            execution: ExecutionDisposition::Replay,
        })
    }

    async fn forward_once(
        &self,
        endpoint: &str,
        payload: &Value,
        opts: &ForwardOpts<'_>,
        fingerprint: &[u8; 32],
        uncertain: &mut bool,
    ) -> ForwardOutcome {
        let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
        let tenant_scope = opts.tenant_id.unwrap_or("\u{0}personal");

        if let Some(request_id) = opts.request_id {
            let key = cache_key(tenant_scope, &url, opts.provider_id, request_id);
            if let Some(hit) = self.lookup_cached(&key, fingerprint, opts) {
                return hit;
            }
        }

        let mut attempt = 0usize;
        loop {
            // Keep the transport error until retry classification: only a
            // connection failure proves that this POST was not executed.
            let sent = tokio::time::timeout(
                self.header_timeout,
                self.client.post(&url).json(payload).send(),
            )
            .await;
            match sent {
                Ok(Ok(resp)) => {
                    let status = resp.status().as_u16();
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
                        Err(status) => {
                            *uncertain = true;
                            return Self::failure_with_execution(
                                status,
                                retries,
                                ExecutionDisposition::Uncertain,
                            );
                        }
                    };
                    if (200..300).contains(&status)
                        && opts.require_openai_usage
                        && crate::budget::parse_openai_usage(&body_bytes).is_err()
                    {
                        *uncertain = true;
                        return Self::flight_failure_with_execution(
                            502,
                            "upstream_usage_invalid",
                            ExecutionDisposition::Uncertain,
                        );
                    }
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
                                fingerprint: *fingerprint,
                                status,
                                // Only outgoing bytes own the permit; cache retention must not.
                                body: body_bytes.clone(),
                                record_id: opts.record_id.to_string(),
                                served_locality: opts.served_locality,
                            },
                        );
                    }
                    // Redirects, request timeouts, and 5xx responses do not
                    // prove the POST was not executed; retain their
                    // fingerprint through the window and expose the same
                    // uncertainty to budget settlement.
                    let execution = if (300..400).contains(&status)
                        || status == 408
                        || (500..600).contains(&status)
                    {
                        *uncertain = true;
                        ExecutionDisposition::Uncertain
                    } else {
                        ExecutionDisposition::Executed
                    };
                    return ForwardOutcome {
                        status,
                        body: body_bytes,
                        content_type,
                        cached: false,
                        origin_record_id: None,
                        replayed_served_locality: None,
                        retries,
                        execution,
                    };
                }
                Ok(Err(err)) => {
                    // A send/read timeout or lost headers may follow an executed POST.
                    if err.is_connect() && attempt < self.retry_delays.len() {
                        self.sleep_retry(attempt).await;
                        attempt += 1;
                        continue;
                    }
                    *uncertain = !err.is_connect();
                    return Self::failure_with_execution(
                        if err.is_timeout() { 504 } else { 502 },
                        attempt as u32,
                        if err.is_connect() {
                            ExecutionDisposition::NotExecuted
                        } else {
                            ExecutionDisposition::Uncertain
                        },
                    );
                }
                Err(_) => {
                    *uncertain = true;
                    return Self::failure_with_execution(
                        504,
                        attempt as u32,
                        ExecutionDisposition::Uncertain,
                    );
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
        let permit = Arc::new(permit);
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
                    let idle = self.stream_idle_timeout;
                    #[cfg(test)]
                    let eof_observed = self.stream_eof_observed.clone();
                    #[cfg(test)]
                    let read_waiting = self.stream_read_waiting.clone();
                    // Keep upstream reads running independently of downstream
                    // body polling. A single queued chunk bounds buffering;
                    // both reading and waiting for queue capacity have an
                    // idle deadline. Dropping the body closes the receiver,
                    // which cancels the task and releases the permit.
                    let (tx, rx) = tokio::sync::mpsc::channel(1);
                    let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
                    let (drained_tx, drained_rx) = tokio::sync::oneshot::channel();
                    let producer_permit = Arc::clone(&permit);
                    tokio::spawn(async move {
                        let mut resp = resp;
                        let mut terminal_error = None;
                        let mut reached_eof = false;
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
                                Ok(Ok(None)) => {
                                    reached_eof = true;
                                    #[cfg(test)]
                                    if let Some(eof_observed) = &eof_observed {
                                        eof_observed.notify_one();
                                    }
                                    break;
                                }
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
                            #[cfg(test)]
                            if let Some(read_waiting) = &read_waiting {
                                read_waiting.notify_one();
                            }
                        }
                        drop(resp);
                        if let Some(error) = terminal_error {
                            let _ = terminal_tx.send(Err(error));
                        } else if reached_eof {
                            // Wait for downstream EOF too. This bounds the
                            // lifetime of an unread final chunk after upstream
                            // EOF, and lets the body observe the idle failure.
                            drop(tx);
                            match tokio::time::timeout(idle, drained_rx).await {
                                Ok(Ok(())) => {
                                    let _ = terminal_tx.send(Ok(()));
                                }
                                Ok(Err(_)) => {}
                                Err(_) => {
                                    let _ = terminal_tx.send(Err(std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        "downstream stream idle timeout after upstream EOF",
                                    )));
                                }
                            }
                        }
                        // Producer and body hold separate references: task
                        // completion releases the producer's, while the body
                        // retains its permit through EOF/error consumption or
                        // until the body is dropped.
                        drop(producer_permit);
                    });
                    let stream = stream::unfold(
                        (
                            rx,
                            Some(terminal_rx),
                            Some(drained_tx),
                            Some(Arc::clone(&permit)),
                        ),
                        |(mut rx, terminal_rx, drained_tx, permit)| async move {
                            if let Some(chunk) = rx.recv().await {
                                return Some((
                                    Ok(with_permit(chunk, Arc::clone(permit.as_ref()?))),
                                    (rx, terminal_rx, drained_tx, permit),
                                ));
                            }
                            if let Some(drained_tx) = drained_tx {
                                let _ = drained_tx.send(());
                            }
                            let terminal_rx = terminal_rx?;
                            match terminal_rx.await {
                                Ok(Ok(())) => {
                                    drop(permit);
                                    None
                                }
                                Ok(Err(error)) => {
                                    drop(permit);
                                    Some((Err(error), (rx, None, None, None)))
                                }
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
                            body: with_permit(body, Arc::clone(&permit)),
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
/// case `response` is an incremental body that enforces idle timeouts and
/// forwards upstream read errors. The stream and its emitted output `Bytes`
/// share the concurrency permit. It is released after the stream finishes or is
/// dropped and the last output owner is dropped.
pub enum StreamOutcome {
    Buffered {
        status: u16,
        body: Bytes,
        content_type: Option<String>,
    },
    Stream {
        status: u16,
        content_type: Option<String>,
        response: Body,
    },
}

#[cfg(test)]
#[path = "proxy_cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
#[path = "proxy_review_tests.rs"]
mod review_tests;

#[cfg(test)]
#[path = "proxy_singleflight_limits_tests.rs"]
mod singleflight_limits_tests;

#[cfg(test)]
#[path = "proxy_retention_tests.rs"]
mod retention_tests;

#[cfg(test)]
#[path = "proxy_limits_tests.rs"]
mod limits_tests;

#[cfg(test)]
#[path = "proxy_concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
#[path = "proxy_slow_reader_tests.rs"]
mod slow_reader_tests;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn entry(status: u16) -> CacheEntry {
        CacheEntry {
            at: Instant::now(),
            fingerprint: [0; 32],
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
            require_openai_usage: false,
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

    #[tokio::test]
    async fn k13_concurrent_same_id_is_singleflight_and_tenants_stay_isolated() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("ok")
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(2)
            .mount(&server)
            .await;
        let proxy = fast_retry_proxy();
        let endpoint = server.uri();
        let body = chat_body();
        let a = opts(Some("same-id"), "rec-a");
        let b = opts(Some("same-id"), "rec-b");
        let mut other = opts(Some("same-id"), "rec-other");
        other.tenant_id = Some("other-tenant");
        let (first, second, isolated) = tokio::join!(
            proxy.forward_buffered(&endpoint, &body, &a),
            proxy.forward_buffered(&endpoint, &body, &b),
            proxy.forward_buffered(&endpoint, &body, &other),
        );
        assert_eq!(
            (first.status, second.status, isolated.status),
            (200, 200, 200)
        );
        assert_eq!(first.body, second.body);
        assert!(second.cached);
        assert_eq!(second.origin_record_id.as_deref(), Some("rec-a"));
        assert!(!isolated.cached);
        server.verify().await;
    }

    #[tokio::test]
    async fn k13_changed_payload_conflicts_in_flight_and_after_cache_write() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("ok")
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let proxy = fast_retry_proxy();
        let endpoint = server.uri();
        let body = chat_body();
        let mut changed = body.clone();
        changed["messages"][0]["content"] = Value::String("different".into());
        let options = opts(Some("same-id"), "rec-1");
        let (first, conflict) = tokio::join!(
            proxy.forward_buffered(&endpoint, &body, &options),
            proxy.forward_buffered(&endpoint, &changed, &options),
        );
        assert_eq!(first.status, 200);
        assert_eq!(conflict.status, 409);
        let conflict = proxy.forward_buffered(&endpoint, &changed, &options).await;
        assert_eq!(conflict.status, 409);
        server.verify().await;
    }

    #[tokio::test]
    async fn k13_lost_response_headers_never_retry_an_executed_post() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let observed = calls.clone();
        let upstream = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut received = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    received.extend_from_slice(&buf[..n]);
                    if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&received[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|value| value.parse().unwrap())
                            })
                            .unwrap();
                        if received.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // The POST was consumed; close without sending response headers.
            }
        });
        let out = fast_retry_proxy()
            .forward_buffered(&endpoint, &chat_body(), &opts(Some("lost"), "rec-1"))
            .await;
        upstream.abort();
        assert_eq!(out.status, 502);
        assert_eq!(out.retries, 0);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
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
        proxy.max_cache_bytes = ChatProxy::entry_bytes("b", &entry(200));
        proxy.remember("a".into(), entry(200));
        proxy.remember("b".into(), entry(200));
        assert!(!proxy.cache.lock().unwrap().contains_key("a"));
        assert_eq!(proxy.cache.lock().unwrap()["b"].body.as_ref(), b"{}");
        let mut large = entry(200);
        large.body = Bytes::from(vec![0; 17]);
        proxy.remember("huge".into(), large);
        assert!(!proxy.cache.lock().unwrap().contains_key("huge"));
        assert_eq!(proxy.cache.lock().unwrap()["b"].body.as_ref(), b"{}");
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
    async fn k13_5xx_is_not_proof_that_a_post_was_not_executed() {
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
        assert_eq!(out.status, 500);
        assert_eq!(out.retries, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn persistent_5xx_is_passed_through_verbatim_without_retry() {
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
            .expect(2) // first + remote_opts; changed safety context is rejected
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

        // PR #46 C1 + K13/M4: changed safety context is a fingerprint conflict,
        // never a remote cache replay or a second POST under the same id.
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
        assert_eq!(third.status, 409);
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
        assert_eq!(out.retries, 2); // Connection refused: no POST was sent.
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

#[cfg(test)]
#[path = "proxy/slow_reader_tests.rs"]
mod k14_slow_reader_tests;

#[cfg(test)]
#[path = "proxy_stream_termination_tests.rs"]
mod stream_termination_tests;

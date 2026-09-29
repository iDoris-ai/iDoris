//! Direct HTTP forwarding for a generic (non-oMLX) `http_service` component
//! card's `POST /v1/chat/completions` — a Rust port of
//! `packages/router/src/proxy.ts`'s `ChatProxy`. The upstream response body
//! is forwarded byte-for-byte — **never** re-wrapped into the local-dispatch
//! path's `openai_chat_completion` shape, matching TS: a `form:
//! http_service` candidate is a transparent proxy, not a backend
//! `RuntimeAdapter` call.
//!
//! This PR lands the idempotency cache in isolation (the actual upstream
//! call — retries, the `forward_buffered` entry point — is a follow-up PR
//! built on top of it): a 60s-window cache keyed by (tenant, endpoint,
//! provider), replaying a prior response for the same
//! `X-iDoris-Request-Id`.
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
//!
//! `#![allow(dead_code)]`: this PR's only production caller of
//! `remember`/`prune` (`ChatProxy::forward_buffered`) lands in a follow-up
//! PR — exercised directly by this module's own tests until then, same
//! pattern as `idoris-upstream/src/omlx/http.rs`'s own `#![allow(dead_code)]`.
#![allow(dead_code)]

use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use idoris_contracts::provider::Locality;
use indexmap::IndexMap;
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

pub struct ChatProxy {
    pub(crate) client: reqwest::Client,
    pub(crate) window: Duration,
    max_entries: usize,
    pub(crate) retry_delays: Vec<Duration>,
    pub(crate) cache: Mutex<IndexMap<String, CacheEntry>>,
}

impl ChatProxy {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            window: DEFAULT_WINDOW,
            max_entries: DEFAULT_MAX_ENTRIES,
            // TS default (`proxy.ts`'s `ProxyDeps.retryDelaysMs` default).
            retry_delays: vec![Duration::from_millis(250), Duration::from_secs(1)],
            cache: Mutex::new(IndexMap::new()),
        }
    }

    /// Test-only: a short window (so a test can sleep past it without a
    /// real 60s wait) and/or shorter retry delays, mirroring TS's own
    /// `ProxyDeps` test-injection pattern.
    pub(crate) fn with_config(
        client: reqwest::Client,
        window: Duration,
        retry_delays: Vec<Duration>,
    ) -> Self {
        Self {
            client,
            window,
            max_entries: DEFAULT_MAX_ENTRIES,
            retry_delays,
            cache: Mutex::new(IndexMap::new()),
        }
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
        cache.insert(key, entry);
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
        while cache.len() > self.max_entries {
            cache.shift_remove_index(0);
        }
    }
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

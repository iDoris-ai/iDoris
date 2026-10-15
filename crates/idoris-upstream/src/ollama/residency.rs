//! Ollama idle timers. `keep_alive: -1` is not a variant: a negative value
//! only disables the idle unload timer, and Ollama can still evict a model
//! when it needs memory. Reporting that as [`Keepalive::Pinned`] would make
//! the Supervisor treat the model as non-evictable.

use idoris_backend::BackendError;
use idoris_contracts::load_policy::{Keepalive, LoadPolicy};

/// Ollama's documented server default when a request omits `keep_alive`.
/// Used only to replace an explicit "not pinned" policy with a finite idle
/// timer. It is not a promise that the model stays loaded for this long.
pub(super) const UNPINNED_IDLE_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdleResidency {
    /// Omit `keep_alive` and leave the server default in place.
    ServerDefault,
    /// Finite idle timer in seconds. Never negative.
    Seconds(u64),
}

pub(super) fn idle_residency(policy: Option<&LoadPolicy>) -> Result<IdleResidency, BackendError> {
    match policy.map(|item| item.keepalive) {
        Some(Keepalive::Pinned { pinned: true }) => Err(BackendError::invalid_request(
            "Ollama cannot hard-pin a model. keep_alive -1 only disables the idle timer and does not stop eviction under memory pressure. Refusing pinned keepalive instead of reporting a pin.",
        )),
        Some(Keepalive::Pinned { pinned: false }) => Ok(IdleResidency::Seconds(UNPINNED_IDLE_SECS)),
        Some(Keepalive::IdleTtl { idle_ttl_s }) => {
            if idle_ttl_s == 0 {
                return Err(BackendError::invalid_request(
                    "Ollama idle_ttl_s must be >= 1. 0 unloads, and a TTL is not a hard pin.",
                ));
            }
            Ok(IdleResidency::Seconds(u64::from(idle_ttl_s)))
        }
        None => Ok(IdleResidency::ServerDefault),
    }
}

pub(super) fn keep_alive_seconds(residency: IdleResidency) -> Option<u64> {
    match residency {
        IdleResidency::ServerDefault => None,
        IdleResidency::Seconds(seconds) => Some(seconds),
    }
}

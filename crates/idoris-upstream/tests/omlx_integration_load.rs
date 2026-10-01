//! Opt-in integration test that **loads and unloads a real model** on a
//! real oMLX instance — the mutating counterpart to the read-only
//! `omlx_integration.rs`. Gated behind its own switch, `IDORIS_OMLX_IT_LOAD=1`
//! (separate from `IDORIS_OMLX_IT`, so opting into the read-only smoke test
//! never loads anything), and only ever touches one model:
//! `IDORIS_OMLX_IT_MODEL` (default `Qwen3-0.6B-4bit`, small enough to load
//! next to whatever a developer already has resident).
//!
//! Safety rule: if that model is **already loaded** when a test starts, the
//! test skips instead of running — it would otherwise unload a model someone
//! else is using.
//!
//! Run on workstation A (see `docs/agent/COLLAB.md`):
//! ```text
//! IDORIS_OMLX_IT_LOAD=1 IDORIS_OMLX_API_KEY=... \
//!   cargo test -p idoris-upstream --test omlx_integration_load -- --test-threads=1
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use idoris_backend::{Supervisor, SupervisorConfig};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};

const DEFAULT_MODEL: &str = "Qwen3-0.6B-4bit";

fn opted_in() -> bool {
    std::env::var("IDORIS_OMLX_IT_LOAD").as_deref() == Ok("1")
}

fn model() -> String {
    std::env::var("IDORIS_OMLX_IT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
}

fn adapter() -> OmlxAdapter {
    let mut config = OmlxAdapterConfig {
        // Loading even a small model takes longer than the default 10s on a
        // cold cache; this bounds each HTTP call, not the whole test.
        call_timeout: Duration::from_secs(120),
        ..OmlxAdapterConfig::default()
    };
    if let Ok(base_url) = std::env::var("IDORIS_OMLX_BASE_URL") {
        config.base_url = base_url;
    }
    OmlxAdapter::new(config).expect("adapter must build")
}

fn on_demand() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

/// `true` iff the opt-in is set **and** the target model is not already
/// loaded (see the module doc's safety rule). Prints why it skips.
async fn may_run(adapter: &OmlxAdapter, id: &str) -> bool {
    if !opted_in() {
        eprintln!(
            "skipping: set IDORIS_OMLX_IT_LOAD=1 to load/unload {id} on a real oMLX instance"
        );
        return false;
    }
    let status = adapter
        .status()
        .await
        .expect("status() must succeed against a real oMLX instance");
    if status.loaded.iter().any(|m| m == id) {
        eprintln!(
            "skipping: {id} is already loaded — refusing to unload a model someone else may be using"
        );
        return false;
    }
    true
}

async fn wait_until_loaded(adapter: &OmlxAdapter, id: &str, want: bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let loaded = adapter
            .status()
            .await
            .expect("status() must succeed")
            .loaded
            .iter()
            .any(|m| m == id);
        if loaded == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{id} did not become loaded={want} within 120s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The adapter alone: an on-demand load really loads the model and leaves it
/// unpinned (the non-resident path's `check_not_unexpectedly_pinned` must
/// pass on a clean instance — no `LoadUnconfirmed`), `probe_ready` turns
/// true, and `unload` really releases it.
#[tokio::test]
async fn on_demand_load_then_unload_against_a_real_omlx_instance() {
    let adapter = adapter();
    let id = model();
    if !may_run(&adapter, &id).await {
        return;
    }

    adapter
        .load(&id, Some(&on_demand()))
        .await
        .expect("an on-demand load of an unpinned model must be confirmed");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !adapter.probe_ready(&id).await.unwrap() {
        assert!(Instant::now() < deadline, "{id} never became ready");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    wait_until_loaded(&adapter, &id, true).await;

    adapter.unload(&id).await.expect("unload must succeed");
    wait_until_loaded(&adapter, &id, false).await;
}

/// The same lifecycle through a real `Supervisor`: the ledger counts the
/// model while it is loaded and frees it after `unload`, and the engine's
/// own status agrees with the ledger at both points.
#[tokio::test]
async fn supervisor_ledger_tracks_a_real_load_and_unload() {
    let probe = adapter();
    let id = model();
    if !may_run(&probe, &id).await {
        return;
    }

    let handle = Supervisor::spawn(
        Arc::new(adapter()),
        SupervisorConfig {
            adapter_call_timeout: Duration::from_secs(120),
            ..SupervisorConfig::default()
        },
    )
    .expect("spawn");
    handle
        .load(id.clone(), 1.0, on_demand())
        .await
        .expect("Supervisor load must succeed against a real oMLX instance");
    assert_eq!(handle.status().await.expect("status").used_gb, 1.0);
    wait_until_loaded(&probe, &id, true).await;

    handle
        .unload(id.clone())
        .await
        .expect("Supervisor unload must succeed");
    assert_eq!(handle.status().await.expect("status").used_gb, 0.0);
    wait_until_loaded(&probe, &id, false).await;
}

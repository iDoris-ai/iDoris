//! Opt-in integration test that **loads and unloads a real model** on a
//! real oMLX instance — the mutating counterpart to the read-only
//! `omlx_integration.rs`. Gated behind its own switch, `IDORIS_OMLX_IT_LOAD=1`
//! (separate from `IDORIS_OMLX_IT`, so opting into the read-only smoke test
//! never loads anything), and only ever touches one model:
//! `IDORIS_OMLX_IT_MODEL` (default `Qwen3-0.6B-4bit`, small enough to load
//! next to whatever a developer already has resident). Never runs when `CI`
//! is set, whatever the opt-in says.
//!
//! What it does and does not guarantee:
//! - Within this test binary the whole check → load → verify → unload
//!   lifecycle is one sequential test, so the adapter phase and the
//!   Supervisor phase can never unload each other's model.
//! - If the target model is already loaded when the test starts, it skips.
//!   That is a best-effort guard, **not** exclusive ownership: another client
//!   can still load or use the model after the check. Point the test at a
//!   model nothing else uses (the default is chosen for that), or at a
//!   dedicated oMLX instance via `IDORIS_OMLX_BASE_URL`.
//! - Every phase is bounded by a lifecycle timeout, and the model is unloaded
//!   (under its own timeout) before any failure is reported — so a failed
//!   run does not leave the model resident and make the next run skip.
//!
//! Run on workstation A (see `docs/agent/COLLAB.md`):
//! ```text
//! IDORIS_OMLX_IT_LOAD=1 IDORIS_OMLX_API_KEY=... \
//!   cargo test -p idoris-upstream --test omlx_integration_load
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use idoris_backend::{Supervisor, SupervisorConfig};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use tokio::time::{Instant, sleep, timeout, timeout_at};

const DEFAULT_MODEL: &str = "Qwen3-0.6B-4bit";
/// Upper bound for one phase (load, verify, unload, verify).
const LIFECYCLE: Duration = Duration::from_secs(180);
/// Upper bound for the cleanup that always runs after a phase.
const CLEANUP: Duration = Duration::from_secs(60);
/// Bounds every single HTTP call this test makes.
const CALL: Duration = Duration::from_secs(60);

type Outcome = Result<(), String>;

struct Env {
    id: String,
    base_url: String,
    api_key: Option<String>,
}

fn env() -> Option<Env> {
    if std::env::var_os("CI").is_some() {
        eprintln!("skipping: CI is set — this test never loads models in CI");
        return None;
    }
    if std::env::var("IDORIS_OMLX_IT_LOAD").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set IDORIS_OMLX_IT_LOAD=1 to load/unload a model on a real oMLX instance"
        );
        return None;
    }
    let defaults = OmlxAdapterConfig::default();
    Some(Env {
        id: std::env::var("IDORIS_OMLX_IT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        base_url: std::env::var("IDORIS_OMLX_BASE_URL").unwrap_or(defaults.base_url),
        api_key: defaults.api_key,
    })
}

fn adapter(env: &Env) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: env.base_url.clone(),
        api_key: env.api_key.clone(),
        call_timeout: CALL,
    })
    .expect("adapter must build")
}

fn on_demand() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

/// The engine's own view of `id`, read straight from `GET
/// /v1/models/status` — independent of the adapter code under test.
/// `None` when the id is not listed at all.
async fn engine_state(env: &Env) -> Result<Option<(bool, bool)>, String> {
    let url = format!("{}/v1/models/status", env.base_url.trim_end_matches('/'));
    let mut req = reqwest::Client::new().get(url).timeout(CALL);
    if let Some(key) = &env.api_key {
        req = req.bearer_auth(key);
    }
    let body: serde_json::Value = req
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| format!("GET /v1/models/status failed: {err}"))?
        .json()
        .await
        .map_err(|err| format!("GET /v1/models/status returned non-JSON: {err}"))?;
    let entry = body["models"]
        .as_array()
        .ok_or("GET /v1/models/status has no `models` array")?
        .iter()
        .find(|m| m["id"].as_str() == Some(env.id.as_str()));
    Ok(entry.map(|m| {
        (
            m["loaded"].as_bool() == Some(true),
            m["pinned"].as_bool() == Some(true),
        )
    }))
}

async fn engine_loaded(env: &Env) -> Result<bool, String> {
    Ok(engine_state(env).await?.is_some_and(|(loaded, _)| loaded))
}

/// Polls until the engine reports `loaded == want`, bounded by `deadline`
/// (both the requests and the sleeps — a late match is still a failure).
async fn wait_engine_loaded(env: &Env, want: bool, deadline: Instant) -> Outcome {
    loop {
        let loaded = timeout_at(deadline, engine_loaded(env))
            .await
            .map_err(|_| format!("{} did not become loaded={want} in time", env.id))??;
        if loaded == want {
            return Ok(());
        }
        timeout_at(deadline, sleep(Duration::from_millis(500)))
            .await
            .map_err(|_| format!("{} did not become loaded={want} in time", env.id))?;
    }
}

/// Always runs after a phase: if the engine still has the model (including
/// after a `LoadUnconfirmed`, where it may well be resident), unload it.
async fn cleanup(env: &Env) -> Outcome {
    timeout(CLEANUP, async {
        if engine_loaded(env).await? {
            adapter(env)
                .unload(&env.id)
                .await
                .map_err(|err| format!("cleanup unload failed: {err}"))?;
            wait_engine_loaded(env, false, Instant::now() + CLEANUP).await?;
        }
        Ok(())
    })
    .await
    .map_err(|_| format!("cleanup of {} timed out", env.id))?
}

/// Runs `phase` under the lifecycle timeout, then always cleans up, then
/// reports the phase's own failure first (cleanup failures second).
async fn run_phase(env: &Env, name: &str, phase: impl std::future::Future<Output = Outcome>) {
    let outcome = timeout(LIFECYCLE, phase)
        .await
        .unwrap_or_else(|_| Err(format!("phase timed out after {LIFECYCLE:?}")));
    let cleaned = cleanup(env).await;
    if let Err(err) = outcome {
        panic!("{name}: {err} (cleanup: {cleaned:?})");
    }
    cleaned.unwrap_or_else(|err| panic!("{name}: {err}"));
}

/// Adapter alone: an on-demand load is confirmed on a clean instance (no
/// `LoadUnconfirmed`), the engine independently reports it loaded and
/// **unpinned**, `probe_ready` turns true, and `unload` really releases it.
async fn adapter_phase(env: &Env) -> Outcome {
    let adapter = adapter(env);
    let deadline = Instant::now() + LIFECYCLE;
    adapter
        .load(&env.id, Some(&on_demand()))
        .await
        .map_err(|err| format!("on-demand load was not confirmed: {err}"))?;
    while !adapter
        .probe_ready(&env.id)
        .await
        .map_err(|e| e.to_string())?
    {
        timeout_at(deadline, sleep(Duration::from_millis(500)))
            .await
            .map_err(|_| "probe_ready never turned true".to_string())?;
    }
    match engine_state(env).await? {
        Some((true, false)) => {}
        other => {
            return Err(format!(
                "engine must report loaded=true pinned=false, got {other:?}"
            ));
        }
    }
    adapter
        .unload(&env.id)
        .await
        .map_err(|err| format!("unload failed: {err}"))?;
    wait_engine_loaded(env, false, deadline).await
}

/// Through a real `Supervisor`: its own status lists the model and counts it
/// on the ledger while loaded, drops both after `unload`, and the engine's
/// independently-read state agrees at both points. (`1.0` is the ledger
/// estimate passed in, not the engine's real memory use.)
async fn supervisor_phase(env: &Env) -> Outcome {
    let deadline = Instant::now() + LIFECYCLE;
    let handle = Supervisor::spawn(
        Arc::new(adapter(env)),
        SupervisorConfig {
            adapter_call_timeout: CALL,
            probe_interval: Duration::from_millis(500),
            probe_max_attempts: 120,
            ..SupervisorConfig::default()
        },
    )
    .map_err(|err| format!("spawn failed: {err}"))?;

    handle
        .load(env.id.clone(), 1.0, on_demand())
        .await
        .map_err(|err| format!("Supervisor load failed: {err}"))?;
    let status = handle.status().await.map_err(|e| e.to_string())?;
    if status.used_gb != 1.0 || !status.loaded.contains(&env.id) {
        return Err(format!(
            "after load, Supervisor status must list {} with used_gb=1.0, got {status:?}",
            env.id
        ));
    }
    wait_engine_loaded(env, true, deadline).await?;

    handle
        .unload(env.id.clone())
        .await
        .map_err(|err| format!("Supervisor unload failed: {err}"))?;
    let status = handle.status().await.map_err(|e| e.to_string())?;
    if status.used_gb != 0.0 || status.loaded.contains(&env.id) {
        return Err(format!(
            "after unload, Supervisor status must be empty, got {status:?}"
        ));
    }
    wait_engine_loaded(env, false, deadline).await
}

/// One sequential test so the two phases can never interfere with each
/// other (see the module doc for what this does and does not guarantee).
#[tokio::test]
async fn real_omlx_load_unload_lifecycle() {
    let Some(env) = env() else { return };
    if engine_loaded(&env).await.expect("oMLX must be reachable") {
        eprintln!(
            "skipping: {} is already loaded — refusing to unload a model someone else may be using",
            env.id
        );
        return;
    }
    run_phase(&env, "adapter phase", adapter_phase(&env)).await;
    run_phase(&env, "supervisor phase", supervisor_phase(&env)).await;
}

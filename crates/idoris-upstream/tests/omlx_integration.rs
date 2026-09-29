//! Optional integration test against a **real** oMLX instance. Gated
//! behind `IDORIS_OMLX_IT=1` — not run by default, not run in CI — because
//! it needs an actual oMLX process listening on `http://127.0.0.1:8088`
//! (or `IDORIS_OMLX_BASE_URL`, if set).
//!
//! Every parsing/error-classification rule this adapter implements is
//! already covered by the `wiremock`-based unit tests in `omlx::http`/
//! `omlx::status`/`omlx::pin`/`omlx` itself — this file is a smoke test
//! that the adapter can actually talk to a real oMLX process end to end,
//! not a substitute for that coverage, and deliberately stays read-only
//! (`list`/`status`/`probe_ready`) so running it never loads/unloads/pins
//! a model on whatever real instance a developer happens to have running.
//!
//! Run with:
//! ```text
//! IDORIS_OMLX_IT=1 cargo test -p idoris-upstream --test omlx_integration
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};

/// `true` iff the caller explicitly opted in. Anything other than exactly
/// `"1"` (unset, `"0"`, `"true"`, a typo, ...) means "not opted in" — this
/// gate fails closed, matching the rest of this codebase's discipline
/// around explicit opt-in for anything that touches a real external
/// process.
fn opted_in() -> bool {
    std::env::var("IDORIS_OMLX_IT").as_deref() == Ok("1")
}

fn skip_message() -> &'static str {
    "skipping: set IDORIS_OMLX_IT=1 to run against a real oMLX instance \
     (default http://127.0.0.1:8088, override with IDORIS_OMLX_BASE_URL)"
}

fn adapter() -> OmlxAdapter {
    let mut config = OmlxAdapterConfig::default();
    if let Ok(base_url) = std::env::var("IDORIS_OMLX_BASE_URL") {
        config.base_url = base_url;
    }
    OmlxAdapter::new(config).expect("adapter must build")
}

#[tokio::test]
async fn list_and_status_against_a_real_omlx_instance() {
    if !opted_in() {
        eprintln!("{}", skip_message());
        return;
    }
    let adapter = adapter();
    let models = adapter
        .list()
        .await
        .expect("list() must succeed against a real oMLX instance");
    eprintln!("oMLX reports {} routable model(s)", models.len());

    let status = adapter
        .status()
        .await
        .expect("status() must succeed against a real oMLX instance");
    eprintln!(
        "oMLX status: pressure={:?} used_gb={:.2} model_memory_max_gb={:.2} loaded={:?}",
        status.pressure, status.used_gb, status.model_memory_max_gb, status.loaded
    );
    // Every value here already went through `omlx::status::parse_status`'s
    // fail-closed validation to get this far — a bounds/shape assertion
    // beyond that would just be re-testing that function against
    // whatever a real instance happens to report today.
}

/// `probe_ready` for an id this process never loaded must be lenient
/// (`Ok(false)`), never `Err` — see its own doc comment for why (it's
/// polled while a model may legitimately not have appeared in the status
/// list yet). A real oMLX instance is the one thing that can prove this
/// against a genuine "unknown to this engine" id, as opposed to the unit
/// tests' synthetic ones.
#[tokio::test]
async fn probe_ready_is_lenient_for_an_unknown_model_against_a_real_omlx_instance() {
    if !opted_in() {
        eprintln!("{}", skip_message());
        return;
    }
    let ready = adapter()
        .probe_ready("idoris-integration-test-model-that-does-not-exist")
        .await
        .expect("probe_ready must not error even for an unknown id");
    assert!(
        !ready,
        "a model this process never loaded must not report ready"
    );
}

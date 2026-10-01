//! `idoris` — the router binary. Reads `IDORIS_PORT`, loads components from
//! `IDORIS_COMPONENTS_DIR` (`IDORIS_ALLOW_MOCK=1` + compile-time `dev-mock`
//! feature to admit a `provider.id: mock` card), loads + fail-fast-validates
//! the routing policy from `IDORIS_ROUTING_POLICY`, binds
//! `127.0.0.1:<port>`, and serves the `/v1/chat/completions`/`/health`/
//! `/v1/models` app, logging the listening address and the registered
//! component list.
//!
//! R2-G task 1: a real oMLX-shaped card (`form: http_service`,
//! `load_policy.mode` anything other than `Resident` — see
//! `idoris_router::dispatch::is_resident_http_service`'s doc for the split
//! this mirrors) is served via a `Supervisor` wrapping
//! `idoris_upstream::OmlxAdapter`, spawned here at startup
//! ([`spawn_supervisor_for_omlx_card`]). A `LoadMode::Resident`
//! `http_service` card (a plain OpenAI-compatible backend, no model-loading
//! lifecycle) never touches this Supervisor at all — `AppState.proxy`
//! forwards to it directly per-request instead.

use idoris_backend::{Supervisor, SupervisorConfig, SupervisorHandle};
use idoris_contracts::ComponentCard;
use idoris_contracts::component_card::Form;
use idoris_router::dispatch::is_resident_http_service;
use idoris_router::{AppState, BIND_HOST, build_app, components, parse_port, routing_policy};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("[idoris] 启动失败：{message}");
        std::process::exit(1);
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

/// Finds the first registered card that needs the Supervisor's explicit
/// load/unload lifecycle (`form: http_service`, `load_policy.mode` anything
/// but `Resident` — a card with no declared `load_policy` at all also
/// counts, matching `dispatch::candidate`'s own `LoadMode::OnDemand`
/// default) and spawns one `Supervisor` wrapping an `OmlxAdapter` pointed
/// at its `endpoint`. Returns `Ok(None)` when no such card is registered —
/// every local candidate then still fails closed as
/// `local_only_unavailable`/`supervisor_unavailable`, same as before this
/// wiring existed.
///
/// **Known simplification**: today's production config has exactly one
/// such card (`config/components/omlx.yaml`); if more than one is ever
/// registered, every card past the first (in sorted-filename load order)
/// falls back to that same fail-closed behavior rather than getting its own
/// Supervisor — a documented limitation, not a silent bug, until a
/// per-card Supervisor registry exists.
fn spawn_supervisor_for_omlx_card(
    cards: &[ComponentCard],
) -> Result<Option<SupervisorHandle>, String> {
    let Some(card) = cards.iter().find(|c| {
        c.form == Form::HttpService && !is_resident_http_service(c)
        // is_resident_http_service already checks form == HttpService too;
        // the explicit form check above is just so this predicate reads as
        // "an http_service card that ISN'T the direct-forward kind" on its
        // own, without requiring the reader to know that fact about the
        // helper it calls.
    }) else {
        return Ok(None);
    };
    let adapter = OmlxAdapter::new(OmlxAdapterConfig {
        base_url: card.endpoint.clone(),
        ..OmlxAdapterConfig::default()
    })
    .map_err(|err| format!("无法构造 oMLX 适配器（{}）：{err}", card.provider.id))?;
    let handle = Supervisor::spawn(std::sync::Arc::new(adapter), SupervisorConfig::default())
        .map_err(|err| format!("无法启动 Supervisor（{}）：{err}", card.provider.id))?;
    Ok(Some(handle))
}

async fn run() -> Result<(), String> {
    let port =
        parse_port(std::env::var("IDORIS_PORT").ok().as_deref()).map_err(|err| err.to_string())?;

    let components_dir =
        components::resolve_components_dir(std::env::var("IDORIS_COMPONENTS_DIR").ok().as_deref());
    let allow_mock = env_flag("IDORIS_ALLOW_MOCK");
    let cards = components::load_components(&components_dir, allow_mock).map_err(|err| {
        format!(
            "无法加载组件目录 \"{}\"（IDORIS_COMPONENTS_DIR）：{err}",
            components_dir.display()
        )
    })?;

    // Load once and retain the validated policy for both execution paths.
    let routing_policy_path = routing_policy::resolve_routing_policy_path(
        std::env::var("IDORIS_ROUTING_POLICY").ok().as_deref(),
    );
    let routing_policy =
        routing_policy::load_routing_policy(&routing_policy_path).map_err(|err| {
            format!(
                "无法加载路由策略 \"{}\"（IDORIS_ROUTING_POLICY）：{err}",
                routing_policy_path.display()
            )
        })?;

    let supervisor = spawn_supervisor_for_omlx_card(&cards)?;

    let component_list = cards
        .iter()
        .map(|c| format!("{}({:?})", c.provider.id, c.form))
        .collect::<Vec<_>>()
        .join(", ");
    let state = AppState {
        cards,
        routing_policy,
        supervisor,
        ..AppState::default()
    };

    let app = build_app(state);
    let addr = std::net::SocketAddr::from((BIND_HOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| format!("无法绑定 {addr}：{err}"))?;

    println!("idoris listening on http://{addr}");
    println!(
        "idoris: 已注册组件 [{}]",
        if component_list.is_empty() {
            "无"
        } else {
            &component_list
        }
    );
    axum::serve(listener, app)
        .await
        .map_err(|err| format!("server error: {err}"))
}

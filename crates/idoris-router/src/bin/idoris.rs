//! `idoris` — the router binary. R2-D: reads `IDORIS_PORT`, loads
//! components from `IDORIS_COMPONENTS_DIR` (`IDORIS_ALLOW_MOCK=1` +
//! compile-time `dev-mock` feature to admit a `provider.id: mock` card),
//! loads + fail-fast-validates the routing policy from
//! `IDORIS_ROUTING_POLICY`, binds `127.0.0.1:<port>`, and serves the
//! `/v1/chat/completions`/`/health` app, logging the listening address and
//! the registered component list.
//!
//! No `Supervisor` is spawned here yet: this codebase's only
//! [`idoris_backend::RuntimeAdapter`] is `MockAdapter` (dev/test only,
//! gated the same way a mock component card is) — a real HTTP-based
//! adapter for `omlx`-shaped components is a follow-up task. Until one
//! exists, every local candidate this binary picks will fail closed as
//! `local_only_unavailable` (R2-D task 3's documented behavior for
//! `AppState.supervisor: None`), which is honest given there is nothing
//! real to dispatch to yet.

use idoris_router::{AppState, BIND_HOST, build_app, components, parse_port, routing_policy};

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

    // Fail-fast only: idoris_policy::decide() doesn't consume routing
    // policy rules yet (see routing_policy's module doc) -- this just
    // guarantees a bad/missing IDORIS_ROUTING_POLICY is caught at startup,
    // not silently ignored.
    let routing_policy_path = routing_policy::resolve_routing_policy_path(
        std::env::var("IDORIS_ROUTING_POLICY").ok().as_deref(),
    );
    routing_policy::load_routing_policy(&routing_policy_path).map_err(|err| {
        format!(
            "无法加载路由策略 \"{}\"（IDORIS_ROUTING_POLICY）：{err}",
            routing_policy_path.display()
        )
    })?;

    let component_list = cards
        .iter()
        .map(|c| format!("{}({:?})", c.provider.id, c.form))
        .collect::<Vec<_>>()
        .join(", ");
    let state = AppState {
        cards,
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

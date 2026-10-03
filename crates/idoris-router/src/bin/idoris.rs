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

use idoris_contracts::ComponentCard;
use idoris_contracts::component_card::Form;
use idoris_router::dispatch::{BoundSupervisor, is_resident_http_service};
use idoris_router::{
    AppState, BIND_HOST, build_app, components, parse_port, routing_policy,
    write_timeout::{DEFAULT_WRITE_TIMEOUT, WriteTimeoutListener},
};
use std::path::PathBuf;

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

fn env_path(name: &str) -> Result<Option<String>, String> {
    std::env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| format!("环境变量 {name} 不是有效的 Unicode 路径"))
        })
        .transpose()
}

fn resolve_bundle_path(raw: Option<&str>, default_relative: &str) -> Result<PathBuf, String> {
    if let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let executable =
        std::env::current_exe().map_err(|err| format!("无法定位当前可执行文件：{err}"))?;
    let parent = executable
        .parent()
        .ok_or_else(|| "当前可执行文件没有父目录".to_string())?;
    Ok(parent.join(default_relative))
}

/// Until there is a per-provider registry, accept at most one lifecycle
/// backend. Resident HTTP services are forwarded directly and do not count.
fn spawn_supervisor_for_omlx_card(
    cards: &[ComponentCard],
) -> Result<Option<BoundSupervisor>, String> {
    let lifecycle: Vec<_> = cards
        .iter()
        .filter(|c| c.form == Form::HttpService && !is_resident_http_service(c))
        .collect();
    if lifecycle.len() > 1 {
        let providers = lifecycle
            .iter()
            .map(|c| c.provider.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "暂不支持多个 lifecycle 后端（{providers}）；请仅配置一个非 Resident 的 http_service 后端"
        ));
    }
    lifecycle
        .first()
        .map(|card| BoundSupervisor::spawn_omlx(card))
        .transpose()
}

async fn run() -> Result<(), String> {
    let port =
        parse_port(std::env::var("IDORIS_PORT").ok().as_deref()).map_err(|err| err.to_string())?;

    let components_env = env_path("IDORIS_COMPONENTS_DIR")?;
    let components_dir = resolve_bundle_path(
        components_env.as_deref(),
        components::DEFAULT_COMPONENTS_DIR,
    )?;
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
    let policy_env = env_path("IDORIS_ROUTING_POLICY")?;
    let routing_policy_path = resolve_bundle_path(
        policy_env.as_deref(),
        routing_policy::DEFAULT_ROUTING_POLICY_PATH,
    )?;
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
    axum::serve(
        WriteTimeoutListener::new(listener, DEFAULT_WRITE_TIMEOUT),
        app,
    )
    .await
    .map_err(|err| format!("server error: {err}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn k03_missing_policy_counts_but_resident_http_does_not() {
        let mut lifecycle: ComponentCard =
            serde_yaml::from_str(include_str!("../../../../config/components/omlx.yaml")).unwrap();
        lifecycle.load_policy = None;
        let mut second = lifecycle.clone();
        second.provider.id = "second".to_string();
        assert!(spawn_supervisor_for_omlx_card(&[lifecycle.clone(), second]).is_err());
        let mut resident: ComponentCard =
            serde_yaml::from_str(include_str!("../../../../config/components/omlx.yaml")).unwrap();
        resident.provider.id = "resident".to_string();
        resident.load_policy.as_mut().unwrap().mode =
            idoris_contracts::load_policy::LoadMode::Resident;
        assert!(
            spawn_supervisor_for_omlx_card(&[lifecycle, resident.clone()])
                .unwrap()
                .is_some()
        );
        assert!(
            spawn_supervisor_for_omlx_card(&[resident])
                .unwrap()
                .is_none()
        );
        assert!(spawn_supervisor_for_omlx_card(&[]).unwrap().is_none());
    }
}

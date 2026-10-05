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

use idoris_router::{
    AppState, BIND_HOST,
    auth::VirtualKeyAuthenticator,
    build_app,
    capabilities::{CapabilitiesProvider, LiveCapabilitiesProvider},
    cli, components, config,
    connection::{ConnectionInfo, ConnectionListener},
    host_facts, parse_port, profile, routing_policy,
    runtime::RuntimeRegistry,
    storage,
    subscription::{
        config::SubscriptionConfig,
        runtime::{SubscriptionRuntimeHandle, SubscriptionRuntimeRegistry, authorize_subscription},
    },
    write_timeout::{DEFAULT_WRITE_TIMEOUT, WriteTimeoutListener},
};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    if let Err(message) = cli::parse_args(std::env::args_os().skip(1)) {
        eprintln!("[idoris] {message}");
        std::process::exit(1);
    }
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
    let deploy_mode =
        profile::deploy_mode_from_env(std::env::var("IDORIS_DEPLOY_MODE").ok().as_deref());
    let dev_no_key_enabled = env_flag("IDORIS_DEV_NO_KEY");

    let components_dir =
        config::resolve_env("IDORIS_COMPONENTS_DIR", components::DEFAULT_COMPONENTS_DIR)?;
    let allow_mock = env_flag("IDORIS_ALLOW_MOCK");
    let cards = components::load_components(&components_dir, allow_mock).map_err(|err| {
        format!(
            "无法加载组件目录 \"{}\"（IDORIS_COMPONENTS_DIR）：{err}",
            components_dir.display()
        )
    })?;
    let subscription_config = SubscriptionConfig::snapshot(
        std::env::var("IDORIS_DEPLOY_MODE").ok().as_deref(),
        std::env::var("IDORIS_ENABLE_SUBSCRIPTION").ok().as_deref(),
        std::env::var("IDORIS_DISABLE_SUBSCRIPTION").ok().as_deref(),
        std::env::var("IDORIS_SUBSCRIPTION_SANDBOX").ok().as_deref(),
        std::env::var("IDORIS_SUBSCRIPTION_CLI").ok().as_deref(),
    );
    let mut subscriptions = SubscriptionRuntimeRegistry::default();
    let mut active_cards = Vec::with_capacity(cards.len());
    for card in cards {
        if card.provider.id == idoris_policy::SUBSCRIPTION_PROVIDER_ID {
            if let Some(authorized) = authorize_subscription(&subscription_config, &card)
                .map_err(|error| format!("subscription registration failed: {error}"))?
            {
                let handle = SubscriptionRuntimeHandle::build(authorized, &card)
                    .map_err(|error| format!("subscription runtime failed: {error}"))?;
                subscriptions
                    .insert(handle)
                    .map_err(|error| format!("subscription runtime failed: {error}"))?;
                active_cards.push(card);
            }
        } else {
            active_cards.push(card);
        }
    }
    let cards = active_cards;

    // Load once and retain the validated policy for both execution paths.
    let routing_policy_path = config::resolve_env(
        "IDORIS_ROUTING_POLICY",
        routing_policy::DEFAULT_ROUTING_POLICY_PATH,
    )?;
    let routing_policy =
        routing_policy::load_routing_policy(&routing_policy_path).map_err(|err| {
            format!(
                "无法加载路由策略 \"{}\"（IDORIS_ROUTING_POLICY）：{err}",
                routing_policy_path.display()
            )
        })?;

    let runtimes = RuntimeRegistry::spawn(&cards)?;
    // Preserve startup-gate precedence: component/policy/runtime validation
    // (including subscription authorization and fixed card gates) must fail
    // before tenant storage/config bootstrap can surface a later error.
    let persistent = storage::bootstrap_process(deploy_mode)
        .map_err(|err| format!("无法初始化持久化存储：{err}"))?;
    let virtual_key_authenticator = VirtualKeyAuthenticator::new(persistent.virtual_keys.clone());

    let catalog_path = config::resolve_env("IDORIS_CATALOG", "config/catalog.yaml")?;
    let capabilities = match host_facts::current_host_facts() {
        Ok(facts) => {
            let catalog =
                idoris_recommender::catalog::load_catalog(&catalog_path).map_err(|error| {
                    format!(
                        "无法加载模型目录 \"{}\"（IDORIS_CATALOG）：{error}",
                        catalog_path.display()
                    )
                })?;
            Some(Arc::new(LiveCapabilitiesProvider::new(
                catalog,
                facts,
                runtimes.clone(),
            )) as Arc<dyn CapabilitiesProvider>)
        }
        Err(error) => {
            eprintln!("[idoris] capabilities unavailable: {error}");
            None
        }
    };

    let component_list = cards
        .iter()
        .map(|c| format!("{}({:?})", c.provider.id, c.form))
        .collect::<Vec<_>>()
        .join(", ");
    let state = AppState {
        deploy_mode,
        cards,
        routing_policy,
        runtimes,
        capabilities,
        subscriptions: subscriptions.clone(),
        budget_ledger: Some(persistent.budget),
        record_store: Some(persistent.records),
        virtual_key_authenticator: Some(virtual_key_authenticator),
        dev_no_key_enabled,
        ..AppState::default()
    };

    let app = build_app(state);
    let addr = std::net::SocketAddr::from((BIND_HOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| format!("无法绑定 {addr}：{err}"))?;

    println!("idoris listening on http://{addr}");
    if dev_no_key_enabled {
        eprintln!(
            "[idoris] IDORIS_DEV_NO_KEY=1: unauthenticated chat is limited to free loopback local_only execution"
        );
    }
    println!(
        "idoris: 已注册组件 [{}]",
        if component_list.is_empty() {
            "无"
        } else {
            &component_list
        }
    );
    let shutdown = tokio_util::sync::CancellationToken::new();
    let listener = WriteTimeoutListener::new(
        ConnectionListener::new(listener, shutdown.clone())
            .map_err(|err| format!("无法启动连接监视器：{err}"))?,
        DEFAULT_WRITE_TIMEOUT,
    );
    let signal_shutdown = shutdown.clone();
    let subscription_shutdown = subscriptions.clone();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<ConnectionInfo>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_shutdown.cancel();
        if let Err(error) = subscription_shutdown.shutdown_all().await {
            eprintln!("[idoris] subscription shutdown failed: {error}");
        }
    })
    .await
    .map_err(|err| format!("server error: {err}"))
}

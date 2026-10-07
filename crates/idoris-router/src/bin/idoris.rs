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
    admin::{ADMIN_PORT_ENV, AdminBindConfig, AdminSessionToken, build_admin_app},
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
use std::{
    future::{Future, IntoFuture},
    io::Read,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let command = match cli::parse_args(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("[idoris] {message}");
            std::process::exit(1);
        }
    };
    let cli::Command::Serve(options) = command;
    if let Err(message) = run(options).await {
        eprintln!("[idoris] 启动失败：{message}");
        std::process::exit(1);
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

async fn run(options: cli::ServeOptions) -> Result<(), String> {
    let port =
        parse_port(std::env::var("IDORIS_PORT").ok().as_deref()).map_err(|err| err.to_string())?;
    let admin_port = match std::env::var(ADMIN_PORT_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{ADMIN_PORT_ENV} 不是有效的 Unicode"));
        }
    };
    let admin_bind =
        AdminBindConfig::parse(admin_port.as_deref()).map_err(|err| err.to_string())?;
    let deploy_mode =
        profile::deploy_mode_from_env(std::env::var("IDORIS_DEPLOY_MODE").ok().as_deref());

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
        ..AppState::default()
    };

    let addr = std::net::SocketAddr::from((BIND_HOST, port));
    let data_listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| format!("无法绑定 {addr}：{err}"))?;
    let admin_listener = admin_bind
        .bind()
        .await
        .map_err(|err| format!("无法绑定 Admin {}：{err}", admin_bind.addr()))?;
    let admin_token = if options.admin_token_stdin {
        read_admin_token_from_stdin()?
    } else {
        AdminSessionToken::mint()
    };

    println!(
        "idoris: 已注册组件 [{}]",
        if component_list.is_empty() {
            "无"
        } else {
            &component_list
        }
    );
    serve_bound(data_listener, admin_listener, state, admin_token, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

fn read_admin_token_from_stdin() -> Result<AdminSessionToken, String> {
    read_admin_token(&mut std::io::stdin().lock())
}

fn read_admin_token(reader: &mut impl Read) -> Result<AdminSessionToken, String> {
    let mut framed = [0_u8; 65];
    reader
        .read_exact(&mut framed)
        .map_err(|_| "无法从 stdin 读取完整 Admin session token".to_string())?;
    if framed[64] != b'\n' {
        return Err("stdin Admin session token 缺少换行终止符".to_string());
    }
    AdminSessionToken::from_launcher_secret(&framed[..64]).map_err(|err| err.to_string())
}

async fn serve_bound<S>(
    data_listener: tokio::net::TcpListener,
    admin_listener: tokio::net::TcpListener,
    state: AppState,
    admin_token: AdminSessionToken,
    shutdown_signal: S,
) -> Result<(), String>
where
    S: Future<Output = ()>,
{
    let data_addr = data_listener
        .local_addr()
        .map_err(|err| format!("无法读取 data listener 地址：{err}"))?;
    let admin_addr = admin_listener
        .local_addr()
        .map_err(|err| format!("无法读取 Admin listener 地址：{err}"))?;
    let subscriptions = state.subscriptions.clone();
    let data_app = build_app(state.clone());
    let admin_app = build_admin_app(state, admin_token);
    let shutdown = CancellationToken::new();
    let data_listener = WriteTimeoutListener::new(
        ConnectionListener::new(data_listener, shutdown.clone())
            .map_err(|err| format!("无法启动连接监视器：{err}"))?,
        DEFAULT_WRITE_TIMEOUT,
    );
    let admin_listener = WriteTimeoutListener::new(admin_listener, DEFAULT_WRITE_TIMEOUT);
    println!("idoris listening on http://{data_addr}");
    println!("idoris admin listening on http://{admin_addr}");
    let data_shutdown = shutdown.clone();
    let admin_shutdown = shutdown.clone();
    let data_server = axum::serve(
        data_listener,
        data_app.into_make_service_with_connect_info::<ConnectionInfo>(),
    )
    .with_graceful_shutdown(async move { data_shutdown.cancelled().await })
    .into_future();
    let admin_server = axum::serve(admin_listener, admin_app)
        .with_graceful_shutdown(async move { admin_shutdown.cancelled().await })
        .into_future();
    tokio::pin!(data_server, admin_server, shutdown_signal);

    enum Exit {
        Signal,
        Data(std::io::Result<()>),
        Admin(std::io::Result<()>),
    }
    let exit = tokio::select! {
        result = &mut data_server => Exit::Data(result),
        result = &mut admin_server => Exit::Admin(result),
        _ = &mut shutdown_signal => Exit::Signal,
    };
    shutdown.cancel();

    let server_result = match exit {
        Exit::Signal => {
            let (data, admin) = tokio::join!(&mut data_server, &mut admin_server);
            match data {
                Err(err) => Err(format!("data server error: {err}")),
                Ok(()) => admin.map_err(|err| format!("admin server error: {err}")),
            }
        }
        Exit::Data(result) => {
            let _peer = admin_server.await;
            match result {
                Ok(()) => Err("data server exited unexpectedly".to_string()),
                Err(err) => Err(format!("data server error: {err}")),
            }
        }
        Exit::Admin(result) => {
            let _peer = data_server.await;
            match result {
                Ok(()) => Err("admin server exited unexpectedly".to_string()),
                Err(err) => Err(format!("admin server error: {err}")),
            }
        }
    };
    let subscription_result = subscriptions
        .shutdown_all()
        .await
        .map_err(|error| format!("subscription shutdown failed: {error}"));
    match (server_result, subscription_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(server), Ok(())) => Err(server),
        (Ok(()), Err(subscription)) => Err(subscription),
        (Err(server), Err(subscription)) => Err(format!("{server}; {subscription}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn launcher_token_validation_never_echoes_secret() {
        let secret = "A".repeat(64);
        let error = AdminSessionToken::from_launcher_secret(secret.as_bytes()).unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("Bearer"));
    }

    #[test]
    fn launcher_token_frame_is_bounded_and_newline_terminated() {
        let secret = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut valid = secret.to_vec();
        valid.push(b'\n');
        let token = read_admin_token(&mut valid.as_slice()).unwrap();
        assert!(token.matches(std::str::from_utf8(secret).unwrap()));

        for invalid in [secret[..63].to_vec(), secret.to_vec()] {
            let error = read_admin_token(&mut invalid.as_slice()).unwrap_err();
            assert!(!error.contains(std::str::from_utf8(secret).unwrap()));
        }
    }

    #[tokio::test]
    async fn bound_servers_serve_both_surfaces_and_stop_on_injected_signal() {
        let data_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_port = data_listener.local_addr().unwrap().port();
        let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin_port = admin_listener.local_addr().unwrap().port();
        let token = AdminSessionToken::mint();
        let secret = token.expose_secret().to_string();
        let signal = CancellationToken::new();
        let signal_wait = signal.clone();
        let task = tokio::spawn(serve_bound(
            data_listener,
            admin_listener,
            AppState::default(),
            token,
            async move { signal_wait.cancelled().await },
        ));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let health = client
            .get(format!("http://127.0.0.1:{data_port}/health"))
            .send()
            .await
            .unwrap();
        assert!(health.status().is_success());
        let health: serde_json::Value = health.json().await.unwrap();
        let admin = client
            .get(format!("http://127.0.0.1:{admin_port}/admin/api/v1/status"))
            .header("authorization", format!("Bearer {secret}"))
            .send()
            .await
            .unwrap();
        assert!(admin.status().is_success());
        let admin: serde_json::Value = admin.json().await.unwrap();
        assert_eq!(health["instance_id"], admin["instance_id"]);
        let data_admin = client
            .get(format!("http://127.0.0.1:{data_port}/admin/api/v1/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(data_admin.status(), reqwest::StatusCode::NOT_FOUND);

        signal.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

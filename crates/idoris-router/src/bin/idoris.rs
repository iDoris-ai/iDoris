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

use futures_util::StreamExt;
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
    match command {
        cli::Command::Serve(options) => {
            if let Err(message) = run(options).await {
                eprintln!("[idoris] 启动失败：{message}");
                std::process::exit(1);
            }
        }
        cli::Command::Admin(cli::AdminCommand::Read(resource, options)) => {
            if let Err(message) = admin_read(resource, options).await {
                eprintln!("[idoris] Admin 操作失败：{message}");
                std::process::exit(1);
            }
        }
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
        event_log: Some(persistent.event_log),
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
    read_admin_token_with_timeout(
        || read_admin_token(&mut std::io::stdin().lock()),
        std::time::Duration::from_secs(10),
    )
}

fn read_admin_token_with_timeout<F>(
    read: F,
    timeout: std::time::Duration,
) -> Result<AdminSessionToken, String>
where
    F: FnOnce() -> Result<AdminSessionToken, String> + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("idoris-admin-token".to_string())
        .spawn(move || {
            let _ = sender.send(read());
        })
        .map_err(|_| "无法启动 Admin session token 读取任务".to_string())?;
    receiver
        .recv_timeout(timeout)
        .map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => {
                "等待 launcher Admin session token 超时".to_string()
            }
            std::sync::mpsc::RecvTimeoutError::Disconnected => {
                "Admin session token 读取任务异常结束".to_string()
            }
        })?
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

const MAX_ADMIN_RESPONSE_BYTES: usize = 256 * 1024;

async fn admin_read(
    resource: cli::AdminResource,
    options: cli::AdminOptions,
) -> Result<(), String> {
    if !options.token_stdin {
        return Err("Admin CLI 必须显式使用 --token-stdin".to_string());
    }
    let token = read_admin_token_from_stdin()?;
    let admin_port = match std::env::var(ADMIN_PORT_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{ADMIN_PORT_ENV} 不是有效的 Unicode"));
        }
    };
    let bind = AdminBindConfig::parse(admin_port.as_deref()).map_err(|err| err.to_string())?;
    let value = fetch_admin_resource(bind, &token, resource).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|_| "无法格式化 Admin 响应".to_string())?
    );
    Ok(())
}

async fn fetch_admin_resource(
    bind: AdminBindConfig,
    token: &AdminSessionToken,
    resource: cli::AdminResource,
) -> Result<serde_json::Value, String> {
    let client =
        idoris_upstream::http_client().map_err(|_| "无法初始化 Admin HTTP client".to_string())?;
    let url = format!("http://{}/admin/api/v1/{}", bind.addr(), resource.path());
    let response = client
        .get(url)
        .bearer_auth(token.expose_secret())
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|_| "Admin 请求失败".to_string())?;
    if !response.status().is_success() {
        return Err(format!("Admin 返回 HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ADMIN_RESPONSE_BYTES as u64)
    {
        return Err("Admin 响应过大".to_string());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "无法读取 Admin 响应".to_string())?;
        if body.len().saturating_add(chunk.len()) > MAX_ADMIN_RESPONSE_BYTES {
            return Err("Admin 响应过大".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Admin 返回无效 JSON".to_string())?;
    Ok(value)
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
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

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

        let mut wrong_terminator = secret.to_vec();
        wrong_terminator.push(b'X');
        for invalid in [secret[..63].to_vec(), secret.to_vec(), wrong_terminator] {
            let error = read_admin_token(&mut invalid.as_slice()).unwrap_err();
            assert!(!error.contains(std::str::from_utf8(secret).unwrap()));
        }
    }

    #[test]
    fn launcher_token_read_timeout_is_bounded_and_sanitized() {
        let error = read_admin_token_with_timeout(
            || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Err("SECRET_SENTINEL".to_string())
            },
            std::time::Duration::from_millis(10),
        )
        .unwrap_err();
        assert_eq!(error, "等待 launcher Admin session token 超时");
        assert!(!error.contains("SECRET_SENTINEL"));
    }

    #[tokio::test]
    async fn admin_status_transport_rejects_error_body_redirect_and_oversize() {
        let token = AdminSessionToken::from_launcher_secret(
            b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();

        let error_server = MockServer::start().await;
        let sentinel = "UPSTREAM_SECRET_SENTINEL";
        Mock::given(method("GET"))
            .and(path("/admin/api/v1/status"))
            .respond_with(ResponseTemplate::new(401).set_body_string(sentinel))
            .mount(&error_server)
            .await;
        let error_bind =
            AdminBindConfig::parse(Some(&error_server.address().port().to_string())).unwrap();
        let error = fetch_admin_resource(error_bind, &token, cli::AdminResource::Status)
            .await
            .unwrap_err();
        assert!(error.contains("HTTP 401"));
        assert!(!error.contains(sentinel));
        assert!(!error.contains(token.expose_secret()));

        let redirect_target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/admin/api/v1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "unexpected": true
            })))
            .expect(0)
            .mount(&redirect_target)
            .await;
        let redirect_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/admin/api/v1/status"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", redirect_target.uri()),
            )
            .mount(&redirect_server)
            .await;
        let redirect_bind =
            AdminBindConfig::parse(Some(&redirect_server.address().port().to_string())).unwrap();
        let error = fetch_admin_resource(redirect_bind, &token, cli::AdminResource::Status)
            .await
            .unwrap_err();
        assert!(error.contains("HTTP 302"));
        redirect_target.verify().await;

        let large_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/admin/api/v1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
                b'x';
                MAX_ADMIN_RESPONSE_BYTES
                    + 1
            ]))
            .mount(&large_server)
            .await;
        let large_bind =
            AdminBindConfig::parse(Some(&large_server.address().port().to_string())).unwrap();
        assert_eq!(
            fetch_admin_resource(large_bind, &token, cli::AdminResource::Status)
                .await
                .unwrap_err(),
            "Admin 响应过大"
        );
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

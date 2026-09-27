//! `idoris` — the router binary. R1 skeleton: reads `IDORIS_PORT`, binds
//! `127.0.0.1:<port>`, serves `GET /health` + a `501` fallback for
//! everything else.

use idoris_router::{AppState, BIND_HOST, build_app, parse_port};

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("[idoris] 启动失败：{message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let raw_port = std::env::var("IDORIS_PORT").ok();
    let port = parse_port(raw_port.as_deref()).map_err(|err| err.to_string())?;

    let app = build_app(AppState::default());
    let addr = std::net::SocketAddr::from((BIND_HOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| format!("无法绑定 {addr}：{err}"))?;

    println!("idoris listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .map_err(|err| format!("server error: {err}"))
}

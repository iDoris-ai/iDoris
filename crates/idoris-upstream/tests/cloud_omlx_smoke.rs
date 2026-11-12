//! 可选冒烟（**默认跳过**）：对本机**已经在跑**的 oMLX（`http://127.0.0.1:8088/v1`，OpenAI 兼容）发很小的请求，
//! 看真实的 OpenAI 兼容实现下，适配器的非流式与流式解析（供应商请求号、流内 usage、`[DONE]`）站不站得住。
//!
//! - 只有设了 `IDORIS_CLOUD_SMOKE_OMLX=1` 才跑，否则立刻返回；
//! - oMLX 没在监听就跳过，**不会去拉起它**；本测试不 spawn 任何进程，不启动任何推理引擎；
//! - 密钥只从 `~/.omlx/settings.json` 的 `auth.api_key` 读，**不打印、不落盘、不进提交**（输出里只有布尔值和计数）；
//! - 模型用 `GET /v1/models` 返回的第一个（oMLX 可能按需加载，注意本机内存）。
//!
//! 手动运行：`IDORIS_CLOUD_SMOKE_OMLX=1 cargo test -p idoris-upstream --test cloud_omlx_smoke -- --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use idoris_upstream::UpstreamError;
use idoris_upstream::cloud::{
    ChatCall, ChunkKind, CloudEndpointClient, CloudEndpointSettings, CloudTimeouts, SseParser,
    build_http_client, classify, parse_completion,
};
use idoris_upstream::remote::CredentialSource;
use serde_json::{Map, Value, json};

const ADDR: &str = "127.0.0.1:8088";
const BASE_URL: &str = "http://127.0.0.1:8088/v1";

struct FileKey(String);

#[async_trait]
impl CredentialSource for FileKey {
    async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
        Ok(self.0.clone())
    }
}

/// 读本机 oMLX 的密钥（只从文件读，调用方不得打印）。读不到就返回 `None`。
fn read_key() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home).join(".omlx/settings.json");
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    settings["auth"]["api_key"]
        .as_str()
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

fn timeouts() -> CloudTimeouts {
    CloudTimeouts {
        response_head: Duration::from_secs(120),
        body_total: Duration::from_secs(120),
        stream_idle: Duration::from_secs(60),
    }
}

#[tokio::test]
async fn optional_smoke_against_the_already_running_local_omlx() {
    if std::env::var("IDORIS_CLOUD_SMOKE_OMLX").ok().as_deref() != Some("1") {
        eprintln!(
            "skipped: set IDORIS_CLOUD_SMOKE_OMLX=1 to run against the already-running local oMLX"
        );
        return;
    }
    let addr: SocketAddr = ADDR.parse().unwrap();
    if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err() {
        eprintln!("skipped: oMLX is not listening on {ADDR} (this test never starts it)");
        return;
    }
    let Some(key) = read_key() else {
        eprintln!("skipped: no auth.api_key in ~/.omlx/settings.json");
        return;
    };

    // 模型：取 /v1/models 的第一个
    let http = build_http_client(Duration::from_secs(5)).unwrap();
    let models: Value = http
        .get(format!("{BASE_URL}/models"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let Some(model) = models["data"][0]["id"].as_str().map(str::to_owned) else {
        eprintln!("skipped: oMLX lists no models");
        return;
    };

    let client = CloudEndpointClient::new(
        CloudEndpointSettings::new(BASE_URL, "omlx-smoke"),
        Arc::new(FileKey(key)),
        http,
    )
    .unwrap();
    let body = json!({"messages": [{"role": "user", "content": "Say OK."}]});
    let body = body.as_object().unwrap().clone();
    let extra = Map::new();

    // ---- 非流式 ----
    let prepared = client
        .prepare(
            &ChatCall {
                client_body: &body,
                upstream_model: &model,
                max_tokens: 8,
                stream: false,
                extra_body: &extra,
            },
            timeouts(),
        )
        .await
        .unwrap();
    let head = prepared.send().await.unwrap();
    assert!(head.is_success(), "non-stream status {}", head.status);
    let bytes = head.read_body().await.unwrap();
    let parsed = parse_completion(&bytes).expect("a parseable OpenAI-style completion");
    eprintln!(
        "non-stream: id_present={} usage_valid={} usage_verdict={}",
        parsed.id.is_some(),
        parsed.usage.valid_usage().is_some(),
        parsed.usage.verdict()
    );

    // ---- 流式（要求流内 usage）----
    let prepared = client
        .prepare(
            &ChatCall {
                client_body: &body,
                upstream_model: &model,
                max_tokens: 8,
                stream: true,
                extra_body: &extra,
            },
            timeouts(),
        )
        .await
        .unwrap();
    let mut head = prepared.send().await.unwrap();
    assert!(head.is_success(), "stream status {}", head.status);
    assert!(
        head.is_event_stream(),
        "content type {:?}",
        head.content_type
    );
    let mut parser = SseParser::new(1 << 20);
    let (mut events, mut saw_done, mut usage_valid, mut id_present) = (0, false, false, false);
    while let Some(chunk) = head.next_chunk().await.unwrap() {
        for event in parser.push(&chunk).unwrap() {
            events += 1;
            match classify(&event) {
                ChunkKind::Done => saw_done = true,
                ChunkKind::Json(info) => {
                    id_present |= info.id.is_some();
                    usage_valid |= info.usage.valid_usage().is_some();
                }
                _ => {}
            }
        }
    }
    eprintln!(
        "stream: events={events} saw_done={saw_done} id_present={id_present} usage_valid={usage_valid} partial_tail={}",
        parser.has_partial_event()
    );
    assert!(
        events > 0 && saw_done,
        "a complete SSE stream ending in [DONE]"
    );
}

//! IDORIS-CORE S6，Codex 终审 H3、M4：普通 resident 代理的请求绑定准入时的引擎代际；本地拒绝不是提供方的失败。
//!
//! 真的 Router（`build_app`）、真的代理（`ChatProxy`）；引擎管理器的「引擎被换掉」用登记表的两步代替（`block_endpoint`，再登记新的私有
//! key：代际前进），请求停在代理的同步点（`ChatProxy::with_before_outbound`，每次发送被授权之前）上：不靠墙钟到达「已经通过状态门、
//! 还没发送」的窗口（首次发送、连接失败后的每一次重试、流式的首发）。登记表是进程全局的，每个用例用自己的端口，结束时撤销登记。
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_contracts::common::Capability;
use idoris_upstream::omlx::key::{EndpointKey, block_endpoint, forget_endpoint, govern_endpoint};
use tokio::sync::Notify;
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::proxy::{BeforeOutbound, ChatProxy};
use super::*;

const PROMPT: &str = "a prompt nobody else may read";
const OLD_KEY: &str = "old-engine-key-must-not-leak";
const NEW_KEY: &str = "new-engine-key-must-not-leak";

/// 登记表是进程全局的，端口会被操作系统很快复用给同一个进程里别的用例的假上游：结束时把这个端点从登记表里整条删掉（代际也回到 0）。
struct Governed(String);

impl Drop for Governed {
    fn drop(&mut self) {
        forget_endpoint(&self.0);
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}

/// 一张能带 oMLX key 的 resident 卡（`version_pin` 的产品名是 `omlx`，loopback，不带 `idoris_upstream`），声明 `capability`。
fn omlx_card(endpoint: &str, capability: Capability) -> ComponentCard {
    let mut card = super::tests::resident_component_card("omlx", endpoint);
    card.version_pin = "omlx@0.0.0".to_owned();
    card.provider.capabilities = vec![capability];
    card
}

/// 普通 resident 代理的入口：每一个都在自己的位置读准入时的代际、绑定它（`lib.rs`）。
#[derive(Debug, Clone, Copy)]
enum Route {
    ChatBuffered,
    ChatStream,
    Embeddings,
    Rerank,
    MessagesBuffered,
    MessagesStream,
}

impl Route {
    const ALL: [Route; 6] = [
        Route::ChatBuffered,
        Route::ChatStream,
        Route::Embeddings,
        Route::Rerank,
        Route::MessagesBuffered,
        Route::MessagesStream,
    ];

    fn capability(self) -> Capability {
        match self {
            Route::ChatBuffered
            | Route::ChatStream
            | Route::MessagesBuffered
            | Route::MessagesStream => Capability::Chat,
            Route::Embeddings => Capability::Embedding,
            Route::Rerank => Capability::Rerank,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Route::ChatBuffered | Route::ChatStream => "/v1/chat/completions",
            Route::Embeddings => "/v1/embeddings",
            Route::Rerank => "/v1/rerank",
            Route::MessagesBuffered | Route::MessagesStream => "/v1/messages",
        }
    }

    fn body(self) -> serde_json::Value {
        match self {
            Route::ChatBuffered => json!({"messages": [{"role": "user", "content": PROMPT}]}),
            Route::ChatStream => {
                json!({"stream": true, "messages": [{"role": "user", "content": PROMPT}]})
            }
            Route::Embeddings => json!({"model": "embed-model", "input": PROMPT}),
            Route::Rerank => {
                json!({"model": "rerank-model", "query": PROMPT, "documents": ["alpha", "beta"]})
            }
            Route::MessagesBuffered => json!({
                "model": "claude-local", "max_tokens": 16,
                "messages": [{"role": "user", "content": [{"type": "text", "text": PROMPT}]}]
            }),
            Route::MessagesStream => json!({
                "model": "claude-local", "max_tokens": 16, "stream": true,
                "messages": [{"role": "user", "content": [{"type": "text", "text": PROMPT}]}]
            }),
        }
    }

    fn request(self) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(self.path())
            .header("content-type", "application/json")
            .header("x-idoris-privacy", "local_only")
            .body(Body::from(serde_json::to_vec(&self.body()).unwrap()))
            .unwrap()
    }
}

/// 什么都答 200 的「引擎」，记下每个请求。
async fn engine_on(listener: Option<std::net::TcpListener>) -> MockServer {
    let builder = MockServer::builder();
    let server = match listener {
        Some(listener) => builder.listener(listener).start().await,
        None => builder.start().await,
    };
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    server
}

/// 一个同步点：被停住的请求到了 `park_at` 号尝试就停在代理里，直到测试放行。
struct Park {
    reached: Arc<Notify>,
    release: Arc<Notify>,
    hook: BeforeOutbound,
}

fn park_at(attempt_to_park: usize) -> Park {
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let hook: BeforeOutbound = {
        let (reached, release) = (reached.clone(), release.clone());
        Arc::new(move |_url, attempt| {
            let (reached, release) = (reached.clone(), release.clone());
            Box::pin(async move {
                if attempt == attempt_to_park {
                    reached.notify_one();
                    release.notified().await;
                }
            })
        })
    };
    Park {
        reached,
        release,
        hook,
    }
}

/// 引擎管理器完成了一次换代：旧引擎的登记被封上，新引擎登记了它的私有 key（代际前进）。
fn the_engine_is_replaced(endpoint: &str) {
    block_endpoint(endpoint);
    govern_endpoint(endpoint, EndpointKey::Private(NEW_KEY.to_owned()));
}

async fn body_of(response: axum::response::Response) -> String {
    String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes()).into_owned()
}

/// 一个请求通过了状态门、停在代理的同步点（`park_at` 号尝试）上；这时引擎被换掉；放行。返回状态码，以及换代之后「那个端口上的东西」
/// 收到的请求。`retry` 为真时端口一开始是空的（首次发送连接失败、进入重试），换代之后才有东西绑上来。
async fn parked_then_replaced(route: Route, retry: bool) -> (StatusCode, Vec<wiremock::Request>) {
    let (endpoint, engine, held_port) = if retry {
        // 一个没人监听的端口：第一次发送连接失败（`is_connect`），代理退避之后重试。
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        (format!("http://127.0.0.1:{port}"), None, Some(port))
    } else {
        let engine = engine_on(None).await;
        (engine.uri(), Some(engine), None)
    };
    let _governed = Governed(endpoint.clone());
    govern_endpoint(&endpoint, EndpointKey::Private(OLD_KEY.to_owned()));
    let park = park_at(usize::from(retry));
    let proxy = ChatProxy::with_config(
        client(),
        Duration::from_secs(60),
        vec![Duration::from_millis(1)],
    )
    .with_before_outbound(park.hook.clone());
    let app = build_app(AppState {
        cards: vec![omlx_card(&endpoint, route.capability())],
        proxy: Arc::new(proxy),
        ..AppState::default()
    });
    let request = tokio::spawn(async move { app.oneshot(route.request()).await.unwrap() });

    park.reached.notified().await;
    // The request is past the state gate and parked right before it is sent. The engine behind the endpoint is replaced now.
    the_engine_is_replaced(&endpoint);
    let new_engine = match held_port {
        Some(port) => {
            let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            Some(engine_on(Some(listener)).await)
        }
        None => None,
    };
    park.release.notify_one();
    let response = request.await.unwrap();
    let status = response.status();
    let _ = body_of(response).await;
    let seen = match (&engine, &new_engine) {
        (Some(engine), _) => engine.received_requests().await.unwrap(),
        (None, Some(engine)) => engine.received_requests().await.unwrap(),
        (None, None) => unreachable!(),
    };
    (status, seen)
}

/// H3：请求通过状态门之后、第一次发送之前，引擎被换掉（restart 完成）；放行之后它被拒绝，**一个请求也没有发给此刻在那个端口上的新引擎**，
/// 更没有带新一代的 key。之前普通入口不绑定代际（`pinned_generation()` 是 `None`），解析器接受当前的代际：旧请求的正文连同新引擎的 key
/// 一起发给了新进程。六个普通入口各自读、各自绑，所以各测一次。
#[tokio::test]
async fn an_ordinary_request_parked_before_its_first_send_is_refused_when_the_engine_was_replaced_meanwhile()
 {
    for route in Route::ALL {
        let (status, seen) = parked_then_replaced(route, false).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{route:?}");
        assert!(
            seen.is_empty(),
            "{route:?}: the request admitted for the old engine reached the new one: {seen:?}"
        );
    }
}

/// H3 的重试：首次发送连接失败，代理退避之后重试；退避期间引擎换了、新进程绑上了同一个端口。重试被拒绝，没有请求体、没有新一代的 key
/// 发给它（这个入口的重试走的是同一个 `outbound`）。
#[tokio::test]
async fn a_retry_after_a_connection_failure_is_not_sent_to_the_engine_that_replaced_the_one_it_was_admitted_for()
 {
    let (status, seen) = parked_then_replaced(Route::ChatBuffered, true).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        seen.is_empty(),
        "the retry reached the new engine with its key: {seen:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// M4：本地的准入拒绝不是提供方的失败
// ---------------------------------------------------------------------------------------------

/// 一个在 `armed` 为真时、在发送被授权之前封上端点的同步点：预检（`may_send`）已经通过、熔断名额已经占了，之后才被拒绝。
fn block_when_armed(endpoint: String, armed: Arc<AtomicBool>) -> BeforeOutbound {
    Arc::new(move |_url, _attempt| {
        let (endpoint, armed) = (endpoint.clone(), armed.clone());
        Box::pin(async move {
            if armed.swap(false, Ordering::SeqCst) {
                block_endpoint(&endpoint);
            }
        })
    })
}

async fn chat(app: &axum::Router) -> StatusCode {
    app.clone()
        .oneshot(Route::ChatBuffered.request())
        .await
        .unwrap()
        .status()
}

/// M4：buffered 请求预检通过、占了熔断名额，之后端点被封上（重启期间的并发拒绝）：`send_buffered` 返回本地 503，外层之前把它记成提供方故障；
/// 本地阈值是 2，两次就把一个健康的新进程熔断了。现在本地拒绝释放名额、不记故障：之后的请求照常到引擎。
#[tokio::test]
async fn requests_refused_locally_after_the_precheck_never_open_the_providers_breaker() {
    let engine = engine_on(None).await;
    let endpoint = engine.uri();
    let _governed = Governed(endpoint.clone());
    let armed = Arc::new(AtomicBool::new(false));
    let proxy = ChatProxy::new(client())
        .with_before_outbound(block_when_armed(endpoint.clone(), armed.clone()));
    let app = build_app(AppState {
        cards: vec![omlx_card(&endpoint, Capability::Chat)],
        proxy: Arc::new(proxy),
        ..AppState::default()
    });
    // Five refusals (more than the local threshold of two), each one the engine being replaced after the precheck passed.
    for round in 0..5 {
        govern_endpoint(
            &endpoint,
            EndpointKey::Private(format!("engine-key-{round}")),
        );
        armed.store(true, Ordering::SeqCst);
        assert_eq!(chat(&app).await, StatusCode::SERVICE_UNAVAILABLE, "{round}");
    }
    assert!(
        engine.received_requests().await.unwrap().is_empty(),
        "nothing was sent while the endpoint was being blocked"
    );
    // The new engine is up: a request is served, not turned away by a breaker that the refusals opened.
    govern_endpoint(&endpoint, EndpointKey::Private(NEW_KEY.to_owned()));
    assert_eq!(chat(&app).await, StatusCode::OK);
    assert_eq!(engine.received_requests().await.unwrap().len(), 1);
}

/// 同上，流式：流式路径只有一次发送尝试；预检通过、熔断名额占了之后端点被封上，发送被本地拒绝：不记提供方故障（之前流式与缓冲的路径各有
/// 一处结算，只有缓冲的一处在 `claim.release()` 上有测试）。
#[tokio::test]
async fn streaming_requests_refused_locally_after_the_precheck_never_open_the_providers_breaker() {
    let engine = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&engine)
        .await;
    let endpoint = engine.uri();
    let _governed = Governed(endpoint.clone());
    let armed = Arc::new(AtomicBool::new(false));
    let proxy = ChatProxy::new(client())
        .with_before_outbound(block_when_armed(endpoint.clone(), armed.clone()));
    let app = build_app(AppState {
        cards: vec![omlx_card(&endpoint, Capability::Chat)],
        proxy: Arc::new(proxy),
        ..AppState::default()
    });
    let stream_chat = |app: &axum::Router| {
        let app = app.clone();
        async move {
            let response = app.oneshot(Route::ChatStream.request()).await.unwrap();
            let status = response.status();
            let _ = body_of(response).await;
            status
        }
    };
    // Five refusals (more than the local threshold of two), each one the engine being replaced after the precheck passed.
    for round in 0..5 {
        govern_endpoint(
            &endpoint,
            EndpointKey::Private(format!("engine-key-{round}")),
        );
        armed.store(true, Ordering::SeqCst);
        assert_eq!(
            stream_chat(&app).await,
            StatusCode::SERVICE_UNAVAILABLE,
            "{round}"
        );
    }
    assert!(
        engine.received_requests().await.unwrap().is_empty(),
        "nothing was sent while the endpoint was being blocked"
    );
    // The new engine is up: the stream is served, not turned away by a breaker that the refusals opened.
    govern_endpoint(&endpoint, EndpointKey::Private(NEW_KEY.to_owned()));
    assert_eq!(stream_chat(&app).await, StatusCode::OK);
    assert_eq!(engine.received_requests().await.unwrap().len(), 1);
}

/// 同上，半开探针：断路器真的开过（引擎连续两次 500），窗口过去之后的那一个探针请求在发送前被本地拒绝：探针的名额还回去，下一个请求接着探测、
/// 成功闭合；之前它被记成探针失败、以新的完整窗口重新断开，健康的新进程又被挡了一个窗口。
#[tokio::test]
async fn a_half_open_probe_that_is_refused_locally_hands_its_slot_back() {
    let engine = MockServer::start().await;
    let endpoint = engine.uri();
    let _governed = Governed(endpoint.clone());
    govern_endpoint(&endpoint, EndpointKey::Private(OLD_KEY.to_owned()));
    // Two 500s open the local breaker (threshold 2); afterwards the engine answers 200.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&engine)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .with_priority(2)
        .mount(&engine)
        .await;
    let armed = Arc::new(AtomicBool::new(false));
    let proxy = Arc::new(
        ChatProxy::new(client())
            .with_before_outbound(block_when_armed(endpoint.clone(), armed.clone())),
    );
    proxy.set_clock_ms_for_test(1_000);
    let app = build_app(AppState {
        cards: vec![omlx_card(&endpoint, Capability::Chat)],
        proxy: proxy.clone(),
        ..AppState::default()
    });
    // Two upstream 500s (passed through verbatim) open the breaker; the third request is turned away without reaching the engine.
    assert_eq!(chat(&app).await, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(chat(&app).await, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(chat(&app).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(engine.received_requests().await.unwrap().len(), 2);

    // The recovery window (15 s for a local provider) passes: the next request is the half-open probe. The engine is replaced after
    // the precheck passed, so the probe is refused locally before anything is written.
    proxy.set_clock_ms_for_test(1_000 + 15_000 + 1);
    govern_endpoint(
        &endpoint,
        EndpointKey::Private("probe-engine-key".to_owned()),
    );
    armed.store(true, Ordering::SeqCst);
    assert_eq!(chat(&app).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        engine.received_requests().await.unwrap().len(),
        2,
        "the refused probe sent nothing"
    );

    // The new engine is registered: the very next request is the probe again (the slot was handed back, the breaker did not
    // reopen for a full window), it succeeds and the breaker closes.
    govern_endpoint(&endpoint, EndpointKey::Private(NEW_KEY.to_owned()));
    assert_eq!(chat(&app).await, StatusCode::OK);
    assert_eq!(chat(&app).await, StatusCode::OK);
    assert_eq!(engine.received_requests().await.unwrap().len(), 4);
}

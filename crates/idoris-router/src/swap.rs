//! 可原子替换的服务体（接口规范 §2.7.6「与 Supervisor、围栏、`/health` 的关系」；IDORIS-CORE S5）。
//!
//! 托管引擎要先就绪，完整的 Router 才能创建（它的 Supervisor 在创建时对引擎采样一次，引擎还没好就会把启动错误粘滞住，
//! 之后一直 503）；而引擎可能要 90 秒才好，期间 iDoris 要能答 `/health`。做法：数据面与 Admin 两个监听器照旧最先绑定，
//! 各自服务一个 [`SwapApp`]：启动期里面是「starting 体」（[`crate::build_starting_app`]、
//! [`crate::admin::build_admin_starting_app`]），引擎就绪之后把完整的 Router [`SwapApp::swap`] 进去。
//!
//! # 替换的语义
//!
//! - **按请求取，不按连接取**：每个请求开始时读一次当前的体，所以一条 keep-alive 连接上、替换之后的请求用新体，
//!   替换之前已经在处理的请求在旧体上跑完（旧体靠 `Arc` 活到最后一个在途请求结束，自然释放）。
//! - 请求扩展原样透传：`ConnectInfo<ConnectionInfo>` 之类由监听器放进请求的东西，新体的处理函数照样取得到。
//! - 替换是一次写锁里换一个 `Router`（克隆很便宜，内部是 `Arc`），读者不会读到半个状态。
//!
//! 没有托管卡时不使用本模块：服务体直接是 `build_app` 的结果，行为字节级不变。

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use tower::{Service, ServiceExt};

/// 一个可以在运行期整体替换的 Router。克隆共享同一个槽。
#[derive(Clone)]
pub struct SwapApp {
    current: Arc<RwLock<Router>>,
}

impl std::fmt::Debug for SwapApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SwapApp { .. }")
    }
}

impl SwapApp {
    pub fn new(initial: Router) -> Self {
        Self {
            current: Arc::new(RwLock::new(initial)),
        }
    }

    /// 之后开始的请求用 `next`；已经在处理的请求不受影响。
    pub fn swap(&self, next: Router) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = next;
    }

    fn current(&self) -> Router {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 交给 `axum::serve` 的 Router：什么路径、什么方法都转给当前的体（它自己的路由、中间件、回退照常生效）。
    pub fn router(&self) -> Router {
        Router::new().fallback_service(self.clone())
    }
}

impl Service<Request<Body>> for SwapApp {
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        // 在这里（请求开始时）取体，不是在 future 被 poll 时：「之后开始的请求用新体」按调用的顺序算。
        let body = self.current();
        Box::pin(body.oneshot(request))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::extract::ConnectInfo;
    use axum::http::StatusCode;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn body(text: &'static str) -> Router {
        Router::new().route("/x", get(move || async move { text }))
    }

    async fn fetch(app: &Router, path: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn requests_after_the_swap_see_the_new_body_and_requests_before_it_the_old_one() {
        let swap = SwapApp::new(body("starting"));
        let app = swap.router();
        assert_eq!(fetch(&app, "/x").await, (StatusCode::OK, "starting".into()));
        swap.swap(body("ready"));
        assert_eq!(fetch(&app, "/x").await, (StatusCode::OK, "ready".into()));
        // The new body's own fallback applies: an unknown path is the new body's 404.
        assert_eq!(fetch(&app, "/y").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_request_already_in_flight_finishes_on_the_body_it_started_on() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let slow = Router::new().route(
            "/slow",
            get(move || {
                let entered_tx = entered_tx.clone();
                let release_rx = release_rx.clone();
                async move {
                    if let Some(tx) = entered_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    "old body, finished"
                }
            }),
        );
        let swap = SwapApp::new(slow);
        let app = swap.router();
        let in_flight = tokio::spawn({
            let app = app.clone();
            async move { fetch(&app, "/slow").await }
        });
        entered_rx.await.unwrap();
        swap.swap(body("new"));
        // The slow request is still running on the old body; a new request already sees the new body.
        assert_eq!(fetch(&app, "/x").await, (StatusCode::OK, "new".into()));
        release_tx.send(()).unwrap();
        assert_eq!(
            in_flight.await.unwrap(),
            (StatusCode::OK, "old body, finished".into())
        );
    }

    #[tokio::test]
    async fn request_extensions_reach_the_current_body() {
        // The listener puts `ConnectInfo` into the request; the swapped-in body's handlers must still get it.
        let seen = Router::new().route(
            "/who",
            get(|ConnectInfo(peer): ConnectInfo<String>| async move { peer }),
        );
        let swap = SwapApp::new(Router::new());
        swap.swap(seen);
        let mut request = Request::builder().uri("/who").body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo("peer-address".to_owned()));
        let response = swap.router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"peer-address");
    }

    #[tokio::test]
    async fn swapping_is_visible_to_every_clone_and_never_blocks_readers() {
        let swap = SwapApp::new(body("a"));
        let clone = swap.clone();
        let app = clone.router();
        assert_eq!(fetch(&app, "/x").await.1, "a");
        swap.swap(body("b"));
        assert_eq!(fetch(&app, "/x").await.1, "b");
        // Many concurrent readers while another task keeps swapping: every answer is one of the two bodies.
        let swapper = tokio::spawn({
            let swap = swap.clone();
            async move {
                for index in 0..50 {
                    swap.swap(body(if index % 2 == 0 { "a" } else { "b" }));
                    tokio::task::yield_now().await;
                }
            }
        });
        let readers: Vec<_> = (0..20)
            .map(|_| {
                let app = app.clone();
                tokio::spawn(async move {
                    for _ in 0..10 {
                        let (status, text) = fetch(&app, "/x").await;
                        assert_eq!(status, StatusCode::OK);
                        assert!(text == "a" || text == "b", "{text}");
                    }
                })
            })
            .collect();
        swapper.await.unwrap();
        for reader in readers {
            reader.await.unwrap();
        }
    }
}

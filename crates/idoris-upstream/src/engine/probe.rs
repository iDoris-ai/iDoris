//! 引擎就绪探测的通用部分（设计稿 §2.7.6）。
//!
//! 哪个路径、响应体长什么样才算「这个引擎就绪」是**引擎相关**的（oMLX：`GET /v1/models` 的 JSON 里有
//! `data` 数组；llama.cpp：`GET /health`；Ollama：`GET /`），由 [`super::EngineRecipe::probe_ready`]
//! 回答。**状态码与传输错误怎么分类**是通用的，在这里：配方只要提供一个「响应体对不对」的判断，
//! 调用 [`probe_http`]。
//!
//! 这里的「就绪」是**引擎**（进程在服务）就绪，不是某个**模型**就绪（oMLX 的后者是
//! [`crate::OmlxAdapter::probe_ready`]）。
//!
//! 探测结果分得比「好 / 不好」细，因为启动决策（设计稿 §2.7.2）要靠它：连接被拒 = 没有引擎，可以拉起；
//! 401/403 = 有引擎但我们进不去（不拉起第二个）；形状不对 = 端口被别的服务占了。
//!
//! 请求走 [`crate::http_client`]（不走代理、不跟随重定向）；key 只经 `Authorization` 头发出；
//! 响应体最多读 1 MiB，错误里不带响应内容。key 是普通字符串：自己拉起的引擎用它的私有
//! [`super::EngineKey`]，探测外部引擎（用户自己的那一个）时用调用方解析出的 key，两者都能传。

use std::time::Duration;

/// 响应体上限；就绪探测的响应远小于它。
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 一次探测的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineProbe {
    /// 200，且响应体被配方认作「这个引擎」。
    Ready,
    /// 连接被拒：这个端口上没有任何服务在监听。
    ConnectionRefused,
    /// TCP connection failed without proving the port was unbound.
    ConnectionFailed,
    /// 401 / 403：有服务，但不认我们给的 key（或我们没有 key）。
    AuthRejected(u16),
    /// 有服务在答，但不像这个引擎（响应体不对、过大、其他 4xx、重定向）。
    NotThisEngine,
    /// 还不能用：超时、5xx、408/429、其他传输错误。再探一次可能就好了。
    Unavailable,
}

/// `GET url`，按状态码与传输错误分类；200 时读响应体（最多 1 MiB），交给 `accepts` 判断是不是这个引擎。
/// `bearer` 是要带的 API key（没有就不带 `Authorization` 头）。`timeout` 覆盖整个往返（含读响应体）。
pub async fn probe_http(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    timeout: Duration,
    accepts: impl Fn(&[u8]) -> bool,
) -> EngineProbe {
    let mut request = client.get(url);
    if let Some(key) = bearer {
        request = request.bearer_auth(key);
    }
    let attempt = async {
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) => return classify_transport_error(&error),
        };
        match response.status().as_u16() {
            200 => {}
            code @ (401 | 403) => return EngineProbe::AuthRejected(code),
            408 | 429 | 500..=599 => return EngineProbe::Unavailable,
            _ => return EngineProbe::NotThisEngine,
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    body.extend_from_slice(&chunk);
                    if body.len() > MAX_BODY_BYTES {
                        return EngineProbe::NotThisEngine;
                    }
                }
                Ok(None) => break,
                Err(_) => return EngineProbe::Unavailable,
            }
        }
        if accepts(&body) {
            EngineProbe::Ready
        } else {
            EngineProbe::NotThisEngine
        }
    };
    tokio::time::timeout(timeout, attempt)
        .await
        .unwrap_or(EngineProbe::Unavailable)
}

/// 一次探测，**经登记表的授权路径**（IDORIS-CORE S6，Codex 复审 L2 的 M2'）。给引擎管理器**接管的端点**（已经登记过口径与守卫）用：外部引擎的接管与
/// 监督，以及第 3 层引擎换代之后的探测。探测带着调用方**事先解析好**的 `bearer`，但发出去的路径与数据面的每个请求一样：
///
/// 1. 调用方先取 key（`bearer`），**之后**才有这里的核对：key 不是在核对之后才去读文件；
/// 2. 守卫在发送它的这个任务里**重新确认监听者**（[`crate::omlx::key::authorize_omlx_request_without_key`]：代际、封上、守卫、在途登记），确认不了或已经不是
///    登记时认的那个监听者就拒绝，什么都不发；
/// 3. 探测只拨**核验过的地址**（[`crate::omlx::key::Authorization::dial_url`]，不是卡里写的 `localhost`、`[::1]`），走授权给的**不保留空闲连接**的客户端，所以带 key
///    的请求只走授权之后新开的连接；
/// 4. 发出之前再同步看一眼端点（[`crate::omlx::key::Authorization::ensure_current`]）；授权在探测读完之前一直握着，是端点上的一个在途操作，排空会等它。
///
/// 之前的做法是在别处核对监听者，再用裸的探测把 key 发给 `endpoint` 这个写法：核对与发送是两件事，探测拨的也不是核验过的那个地址。授权被拒绝时返回 `Err`
/// （什么都没有发出去）：调用方把它当作「监听者此刻确认不了」。
///
/// **没有核验过的拨号地址就不发**（Codex 复审 L2 的终检）：端点没登记过、或登记时没带守卫，授权什么都没核验过（`Authorization::dial` 是 `None`），这时探测会照卡里写的 URL、
/// 用调用方的客户端发出去，bearer 也跟着出去：帮手自己守住这条线，返回 `Err(KeyRefusal::Unverified)`，不管有没有 `bearer`，什么都不发。它只给被管理、带守卫的端点用。
///
/// **发出前的最后一眼在调配方之前，不在请求建好之后**：配方（[`super::EngineRecipe::probe_ready`]）自己建请求并发出，这里能做的是授权之后、调它之前同步看一眼端点（`ensure_current`）；
/// 适配器、代理、模型列表、就绪探测那几条路径才是请求建好之后、发出前再看一眼。
pub async fn probe_authorized(
    recipe: &dyn super::EngineRecipe,
    endpoint: &str,
    shared_client: &reqwest::Client,
    bearer: Option<&str>,
    timeout: Duration,
) -> Result<EngineProbe, crate::omlx::key::KeyRefusal> {
    let authorization =
        crate::omlx::key::authorize_omlx_request_without_key(endpoint, None).await?;
    if authorization.dial().is_none() {
        return Err(crate::omlx::key::KeyRefusal::Unverified);
    }
    let client = authorization.client(shared_client);
    let base = authorization.dial_url(endpoint);
    authorization.ensure_current()?;
    let probe = recipe.probe_ready(&client, &base, bearer, timeout).await;
    if matches!(
        probe,
        EngineProbe::ConnectionRefused | EngineProbe::ConnectionFailed
    ) {
        authorization.connection_failed();
    }
    Ok(probe)
}

fn classify_transport_error(error: &reqwest::Error) -> EngineProbe {
    if error.is_timeout() {
        return if error.is_connect() {
            EngineProbe::ConnectionFailed
        } else {
            EngineProbe::Unavailable
        };
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if current
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
        {
            return EngineProbe::ConnectionRefused;
        }
        source = current.source();
    }
    if error.is_connect() {
        EngineProbe::ConnectionFailed
    } else {
        EngineProbe::Unavailable
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn is_ok(body: &[u8]) -> bool {
        body == b"ok"
    }

    async fn probe(server: &MockServer, bearer: Option<&str>) -> EngineProbe {
        let client = crate::http_client().unwrap();
        probe_http(
            &client,
            &format!("{}/health", server.uri()),
            bearer,
            WAIT,
            is_ok,
        )
        .await
    }

    async fn serving(template: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(template)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_recipe_decides_what_a_ready_body_looks_like() {
        let server = serving(ResponseTemplate::new(200).set_body_string("ok")).await;
        assert_eq!(probe(&server, None).await, EngineProbe::Ready);
        for body in ["", "OK", "ok ", "{\"status\":\"ok\"}", "<html>hello</html>"] {
            let server = serving(ResponseTemplate::new(200).set_body_string(body)).await;
            assert_eq!(
                probe(&server, None).await,
                EngineProbe::NotThisEngine,
                "{body:?}"
            );
        }
        let huge =
            serving(ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_BODY_BYTES + 1]))
                .await;
        assert_eq!(
            probe(&huge, None).await,
            EngineProbe::NotThisEngine,
            "over the body cap"
        );
    }

    #[tokio::test]
    async fn the_key_goes_out_as_a_bearer_token_and_only_there() {
        // An engine's private key, or whatever key an external engine was configured with.
        for key in ["cd".repeat(32), "sk-user-chosen-key".to_owned()] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/health"))
                .and(header("authorization", format!("Bearer {key}").as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
                .expect(1)
                .mount(&server)
                .await;
            assert_eq!(probe(&server, Some(&key)).await, EngineProbe::Ready);
            // Without the key the same mock does not match: wiremock answers 404.
            assert_eq!(probe(&server, None).await, EngineProbe::NotThisEngine);
        }
    }

    #[tokio::test]
    async fn the_outcomes_the_startup_decision_needs_are_told_apart() {
        for (status, expected) in [
            (401, EngineProbe::AuthRejected(401)),
            (403, EngineProbe::AuthRejected(403)),
            (404, EngineProbe::NotThisEngine),
            (302, EngineProbe::NotThisEngine),
            (503, EngineProbe::Unavailable),
            (500, EngineProbe::Unavailable),
            (429, EngineProbe::Unavailable),
        ] {
            let server = serving(ResponseTemplate::new(status)).await;
            assert_eq!(probe(&server, None).await, expected, "HTTP {status}");
        }
    }

    #[tokio::test]
    async fn a_closed_port_is_connection_refused_and_a_stalled_server_is_unavailable() {
        let spare = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = spare.local_addr().unwrap().port();
        drop(spare);
        let client = crate::http_client().unwrap();
        assert_eq!(
            probe_http(
                &client,
                &format!("http://127.0.0.1:{port}/health"),
                None,
                WAIT,
                is_ok
            )
            .await,
            EngineProbe::ConnectionRefused
        );

        let server = serving(ResponseTemplate::new(200).set_delay(Duration::from_secs(5))).await;
        let started = std::time::Instant::now();
        assert_eq!(
            probe_http(
                &client,
                &format!("{}/health", server.uri()),
                None,
                Duration::from_millis(200),
                is_ok
            )
            .await,
            EngineProbe::Unavailable
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

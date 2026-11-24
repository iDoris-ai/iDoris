//! IDORIS-CORE S6：代理在每一次发送前按引擎管理器登记的口径核对（Codex 预审 3、1）。
//!
//! 端点被封上（没有一个通过校验的引擎在它后面）、或者请求绑定的引擎代际已经被换掉时，代理什么都不发：不带新一代的 key，请求体
//! （用户的提示词）也不发给此刻占着那个端口的东西，也不去碰提供方的熔断。登记表是进程全局的，每个用例用自己的 wiremock 端口
//! 作端点，彼此不相干。
#![allow(clippy::unwrap_used)]

use idoris_backend::generation;
use idoris_contracts::common::PrivacyClass;
use idoris_contracts::provider::Locality;
use idoris_upstream::omlx::key::{
    EndpointKey, block_endpoint, endpoint_generation, govern_endpoint, release_endpoint,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn opts() -> ForwardOpts<'static> {
    ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "record-governance",
        provider_id: "omlx",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::LocalOnly,
        require_openai_usage: false,
        // 一张 loopback 的 oMLX 卡：代理会按登记表给它取 key。
        attach_omlx_key: true,
    }
}

/// 登记表是进程全局的，而端口会被操作系统很快复用给同一个进程里别的用例的假上游：每个用例结束时撤销自己的登记，不留一个
/// 封上的（或登记着私有 key 的）端点给之后碰巧拿到同一个端口的用例。
struct Governed(String);

impl Drop for Governed {
    fn drop(&mut self) {
        release_endpoint(&self.0);
    }
}

fn body() -> Value {
    serde_json::json!({"messages": [{"role": "user", "content": "a prompt nobody else may read"}]})
}

async fn engine() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices": []})))
        .mount(&server)
        .await;
    server
}

async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap()
}

fn authorization(request: &wiremock::Request) -> Option<String> {
    request
        .headers
        .get("authorization")
        .map(|value| value.to_str().unwrap().to_owned())
}

#[tokio::test]
async fn a_blocked_endpoint_gets_no_request_at_all_and_the_provider_breaker_never_hears_of_it() {
    let server = engine().await;
    let _governed = Governed(server.uri());
    block_endpoint(&server.uri());
    let proxy = ChatProxy::new(reqwest::Client::new());
    // 比熔断的上限（5 次连续失败）多得多：被拒绝的发送不是提供方的失败。
    for _ in 0..12 {
        let outcome = proxy
            .forward_buffered(&server.uri(), &body(), &opts())
            .await;
        assert_eq!(outcome.status, 503);
        assert_eq!(outcome.execution, ExecutionDisposition::NotExecuted);
        assert_eq!(
            outcome.body,
            Bytes::from_static(br#"{"error":{"type":"upstream_unavailable"}}"#)
        );
    }
    assert_eq!(received(&server).await.len(), 0, "not a byte was sent");
    // 新引擎通过校验、登记了私有 key：下一个请求直接发出去（不是 `provider_circuit_open`），带的是它的 key。
    govern_endpoint(
        &server.uri(),
        EndpointKey::Private("key-of-the-new-engine".into()),
    );
    let outcome = proxy
        .forward_buffered(&server.uri(), &body(), &opts())
        .await;
    assert_eq!(outcome.status, 200);
    let requests = received(&server).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        authorization(&requests[0]).as_deref(),
        Some("Bearer key-of-the-new-engine")
    );
}

#[tokio::test]
async fn a_blocked_endpoint_gets_no_streaming_request_either() {
    let server = engine().await;
    let _governed = Governed(server.uri());
    block_endpoint(&server.uri());
    let proxy = ChatProxy::new(reqwest::Client::new());
    let provider = ProviderRef {
        id: "omlx",
        omlx_key: true,
    };
    for _ in 0..12 {
        let outcome = proxy
            .forward_stream_path(
                &server.uri(),
                StreamingUpstreamPath::CHAT_COMPLETIONS,
                &body(),
                provider,
            )
            .await;
        match outcome {
            StreamOutcome::Buffered {
                status, execution, ..
            } => {
                assert_eq!(status, 503);
                assert_eq!(execution, ExecutionDisposition::NotExecuted);
            }
            StreamOutcome::Stream { .. } => panic!("a blocked endpoint must not stream"),
        }
    }
    assert_eq!(received(&server).await.len(), 0, "not a byte was sent");
    govern_endpoint(
        &server.uri(),
        EndpointKey::Private("key-of-the-new-engine".into()),
    );
    let outcome = proxy
        .forward_stream_path(
            &server.uri(),
            StreamingUpstreamPath::CHAT_COMPLETIONS,
            &body(),
            provider,
        )
        .await;
    assert!(
        matches!(outcome, StreamOutcome::Stream { .. }),
        "the breaker was never involved: the request goes through once the engine is verified"
    );
    assert_eq!(received(&server).await.len(), 1);
}

#[tokio::test]
async fn a_request_bound_to_a_replaced_engine_is_refused_and_never_carries_the_new_key() {
    let server = engine().await;
    let uri = server.uri();
    let _governed = Governed(uri.clone());
    govern_endpoint(&uri, EndpointKey::Private("key-of-the-old-engine".into()));
    let admitted = endpoint_generation(&uri);
    // 引擎被换掉：封上，新的进程通过校验之后登记它的私有 key。
    block_endpoint(&uri);
    govern_endpoint(&uri, EndpointKey::Private("key-of-the-new-engine".into()));
    let proxy = ChatProxy::new(reqwest::Client::new());

    let stale = generation::scoped(
        Some(admitted),
        proxy.forward_buffered(&uri, &body(), &opts()),
    )
    .await;
    assert_eq!(stale.status, 503);
    assert_eq!(stale.execution, ExecutionDisposition::NotExecuted);
    assert_eq!(
        received(&server).await.len(),
        0,
        "neither the new key nor the prompt reached the new engine"
    );

    // 准入时就是当前这一代的请求照常发出去，带当前的 key。
    let current = endpoint_generation(&uri);
    let outcome = generation::scoped(
        Some(current),
        proxy.forward_buffered(&uri, &body(), &opts()),
    )
    .await;
    assert_eq!(outcome.status, 200);
    // 没有绑定代际的请求（聊天的 resident 路径）按此刻的登记走，行为与以前一致。
    let unbound = proxy.forward_buffered(&uri, &body(), &opts()).await;
    assert_eq!(unbound.status, 200);
    let requests = received(&server).await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            authorization(request).as_deref(),
            Some("Bearer key-of-the-new-engine")
        );
    }
}

#[tokio::test]
async fn a_request_without_the_omlx_key_is_still_refused_by_a_blocked_endpoint() {
    // `attach_omlx_key: false`（共用同一个托管端点的别的 resident 卡）：没有 key 可言，但端点被封上、或请求绑定的代际已经被换掉
    // 时，请求体同样不发（Codex 预审 M1）；端点没有被接管时行为不变（见下一个用例）。
    let server = engine().await;
    let _governed = Governed(server.uri());
    govern_endpoint(&server.uri(), EndpointKey::Private("first".into()));
    let admitted = endpoint_generation(&server.uri());
    let proxy = ChatProxy::new(reqwest::Client::new());
    let mut plain = opts();
    plain.attach_omlx_key = false;
    // 引擎在，代际对：发出去，不带 key。
    let outcome = generation::scoped(Some(admitted), async {
        proxy.forward_buffered(&server.uri(), &body(), &plain).await
    })
    .await;
    assert_eq!(outcome.status, 200);
    assert_eq!(authorization(&received(&server).await[0]), None);
    // 换代：绑定旧代际的请求什么都不发。
    govern_endpoint(&server.uri(), EndpointKey::Private("second".into()));
    let outcome = generation::scoped(Some(admitted), async {
        proxy.forward_buffered(&server.uri(), &body(), &plain).await
    })
    .await;
    assert_eq!(outcome.status, 503);
    assert_eq!(outcome.execution, ExecutionDisposition::NotExecuted);
    // 封上：不绑定也不发。
    block_endpoint(&server.uri());
    let outcome = proxy.forward_buffered(&server.uri(), &body(), &plain).await;
    assert_eq!(outcome.status, 503);
    assert_eq!(
        received(&server).await.len(),
        1,
        "only the first request was sent"
    );
}

#[tokio::test]
async fn a_request_to_an_endpoint_nobody_governs_is_not_subject_to_the_registry() {
    let server = engine().await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let mut plain = opts();
    plain.attach_omlx_key = false;
    let outcome = proxy.forward_buffered(&server.uri(), &body(), &plain).await;
    assert_eq!(outcome.status, 200);
    let requests = received(&server).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(authorization(&requests[0]), None);
}

/// Codex 预审 M2：重试的间隙里端点被封上，发送被本地拒绝（503，没发出去任何东西）——不是提供方的错，不能计入熔断。
/// 连着发生比熔断上限（本地类 2 次）多的次数之后，端点恢复，下一个请求直接发出去，而不是 `provider_circuit_open`。
#[tokio::test]
async fn a_refusal_between_connection_retries_does_not_count_against_the_provider_breaker() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let uri = format!("http://127.0.0.1:{port}");
    let _governed = Governed(uri.clone());
    govern_endpoint(&uri, EndpointKey::Private("old".into()));
    let proxy = Arc::new(ChatProxy::with_config(
        reqwest::Client::new(),
        DEFAULT_WINDOW,
        vec![Duration::from_millis(500)],
    ));
    // 本地类提供方的熔断上限是 2 次连续失败；4 次本地拒绝远超它。退避 500 ms、40 ms 处封上：给慢机器留 400 ms 以上的余量。
    for _ in 0..4 {
        // 第一次发送连接被拒、在等重试；这时端点被封上，重试被本地拒绝。
        govern_endpoint(&uri, EndpointKey::Private("old".into()));
        let request = {
            let (proxy, uri) = (Arc::clone(&proxy), uri.clone());
            tokio::spawn(async move { proxy.forward_buffered(&uri, &body(), &opts()).await })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        block_endpoint(&uri);
        let outcome = request.await.unwrap();
        assert_eq!(outcome.status, 503);
        assert_eq!(outcome.execution, ExecutionDisposition::NotExecuted);
        assert_eq!(
            outcome.retries, 1,
            "refused at the retry, not before the first send"
        );
    }
    // 端点上起了一个新引擎：请求直接发出去。
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let server = MockServer::builder().listener(listener).start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices": []})))
        .mount(&server)
        .await;
    govern_endpoint(&uri, EndpointKey::Private("new".into()));
    let outcome = proxy.forward_buffered(&uri, &body(), &opts()).await;
    assert_eq!(
        outcome.status, 200,
        "the breaker never heard of the refusals"
    );
}

/// 一次连接失败的重试，落在引擎已经被换掉之后：不发。第一次发送（还是准入时的那一代）连接被拒；等重试的时候引擎换了，端口上
/// 起来了新引擎，重试**不能**把请求（与新引擎的 key）发给它。对照：引擎没换，同一个重试正常到达。
#[tokio::test]
async fn a_connection_failure_retry_is_not_sent_to_an_engine_that_replaced_the_one_it_was_admitted_for()
 {
    for replaced in [false, true] {
        // 先占一个端口再放掉：第一次发送连接被拒。
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let uri = format!("http://127.0.0.1:{port}");
        let _governed = Governed(uri.clone());
        govern_endpoint(&uri, EndpointKey::Private("key-of-the-old-engine".into()));
        let admitted = endpoint_generation(&uri);

        let proxy = ChatProxy::with_config(
            reqwest::Client::new(),
            DEFAULT_WINDOW,
            vec![Duration::from_millis(400)],
        );
        let request = {
            let uri = uri.clone();
            tokio::spawn(async move {
                generation::scoped(Some(admitted), async {
                    proxy.forward_buffered(&uri, &body(), &opts()).await
                })
                .await
            })
        };
        // 第一次发送已经失败、正在等重试：这时端口上起来了一个引擎。
        tokio::time::sleep(Duration::from_millis(150)).await;
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let server = MockServer::builder().listener(listener).start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices": []})),
            )
            .mount(&server)
            .await;
        if replaced {
            block_endpoint(&uri);
            govern_endpoint(&uri, EndpointKey::Private("key-of-the-new-engine".into()));
        }
        let outcome = request.await.unwrap();
        let requests = received(&server).await;
        if replaced {
            assert_eq!(outcome.status, 503, "refused");
            assert_eq!(outcome.execution, ExecutionDisposition::NotExecuted);
            assert_eq!(
                requests.len(),
                0,
                "the retry was not sent to the new engine"
            );
        } else {
            assert_eq!(
                outcome.status, 200,
                "the same retry arrives when nothing changed"
            );
            assert_eq!(outcome.retries, 1);
            assert_eq!(requests.len(), 1);
            assert_eq!(
                authorization(&requests[0]).as_deref(),
                Some("Bearer key-of-the-old-engine")
            );
        }
    }
}

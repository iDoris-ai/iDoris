//! 云上游适配器的传输层测试：全部对本地假上游（`127.0.0.1`）进行，**不连接任何真实供应商**。
//!
//! 覆盖：成功、各种状态码、不跟随重定向、连接被拒、响应头超时、连接被接受后直接关闭、响应体被截断、
//! 响应体过大、慢响应体、流（正常、静默超时、截断、客户端取消），以及密钥与请求内容不出现在任何输出里。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/fake_upstream.rs"]
mod fake_upstream;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use idoris_upstream::UpstreamError;
use idoris_upstream::cloud::{
    ChatCall, ChunkKind, CloudEndpointClient, CloudEndpointSettings, CloudFailure, CloudTimeouts,
    SseParser, build_http_client, classify, parse_completion,
};
use idoris_upstream::remote::CredentialSource;
use serde_json::{Map, Value, json};

use fake_upstream::{
    Behavior, FakeUpstream, SseStep, completion, content_event, done_event, usage, usage_event,
};

const KEY: &str = "sk-test-SENTINEL-0123456789";

struct Key;

#[async_trait]
impl CredentialSource for Key {
    async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
        Ok(KEY.to_string())
    }
}

fn timeouts() -> CloudTimeouts {
    CloudTimeouts {
        response_head: Duration::from_secs(5),
        body_total: Duration::from_secs(5),
        stream_idle: Duration::from_secs(5),
    }
}

fn client_with(
    base_url: String,
    tweak: impl FnOnce(&mut CloudEndpointSettings),
) -> CloudEndpointClient {
    let mut settings = CloudEndpointSettings::new(base_url, "example-provider");
    tweak(&mut settings);
    CloudEndpointClient::new(
        settings,
        Arc::new(Key),
        build_http_client(Duration::from_secs(2)).unwrap(),
    )
    .unwrap()
}

fn client(base_url: String) -> CloudEndpointClient {
    client_with(base_url, |_| {})
}

fn body() -> Map<String, Value> {
    json!({
        "model": "idoris/agent",
        "messages": [{"role": "user", "content": "PROMPT-SENTINEL"}],
        "user": "student-4711"
    })
    .as_object()
    .unwrap()
    .clone()
}

async fn send(
    client: &CloudEndpointClient,
    stream: bool,
    timeouts: CloudTimeouts,
) -> Result<idoris_upstream::cloud::ResponseHead, CloudFailure> {
    let body = body();
    let extra = Map::new();
    let prepared = client
        .prepare(
            &ChatCall {
                client_body: &body,
                upstream_model: "example-org/chat-70b",
                max_tokens: 256,
                stream,
                extra_body: &extra,
            },
            timeouts,
        )
        .await
        .unwrap();
    prepared.send().await
}

#[tokio::test]
async fn a_successful_call_sends_exactly_the_intended_request_and_exposes_id_and_usage() {
    let fake = FakeUpstream::start(vec![Behavior::json_with_header(
        200,
        completion("chatcmpl-1", "hi", Some(usage(10, 5))),
        "x-request-id",
        "req-from-header",
    )])
    .await;
    let client = client(fake.url());
    let head = send(&client, false, timeouts()).await.unwrap();
    assert_eq!(head.status, 200);
    assert!(head.is_success());
    assert_eq!(
        head.supplier_request_id_header.as_deref(),
        Some("req-from-header")
    );
    let bytes = head.read_body().await.unwrap();
    let parsed = parse_completion(&bytes).unwrap();
    assert_eq!(parsed.id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(parsed.usage.valid_usage().unwrap().completion_tokens, 5);

    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("accept"), Some("application/json"));
    // 只发这几个头：不转发客户端的头，明确要求不压缩
    let mut names: Vec<&str> = request.headers.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "accept",
            "accept-encoding",
            "authorization",
            "content-length",
            "content-type",
            "host",
            "user-agent"
        ]
    );
    assert_eq!(request.header("accept-encoding"), Some("identity"));
    let sent = request.json();
    assert_eq!(sent["model"], "example-org/chat-70b");
    assert_eq!(sent["max_tokens"], 256);
    assert_eq!(sent["stream"], false);
    assert!(
        sent.get("user").is_none(),
        "identifying fields are not forwarded"
    );
    assert_eq!(sent["messages"][0]["content"], "PROMPT-SENTINEL");
}

#[tokio::test]
async fn non_2xx_statuses_are_returned_for_the_caller_to_classify() {
    for status in [400u16, 401, 408, 429, 500, 503] {
        let fake = FakeUpstream::start(vec![Behavior::json(
            status,
            json!({"error": {"message": "nope"}}),
        )])
        .await;
        let head = send(&client(fake.url()), false, timeouts()).await.unwrap();
        assert_eq!(head.status, status);
        assert!(!head.is_success());
    }
}

#[tokio::test]
async fn redirects_are_not_followed_and_the_key_never_reaches_the_target() {
    let target =
        FakeUpstream::start(vec![Behavior::json(200, completion("x", "leaked", None))]).await;
    let origin = FakeUpstream::start(vec![Behavior::Json {
        status: 302,
        body: String::new(),
        headers: vec![(
            "Location".into(),
            format!("http://{}/v1/chat/completions", target.addr()),
        )],
        delay: Duration::ZERO,
    }])
    .await;
    let head = send(&client(origin.url()), false, timeouts())
        .await
        .unwrap();
    assert_eq!(
        head.status, 302,
        "a 3xx is a failure for the caller, never followed"
    );
    assert_eq!(target.request_count(), 0);
    assert_eq!(target.connection_count(), 0);
}

#[tokio::test]
async fn connection_refused_is_classified_as_a_connect_failure() {
    let url = FakeUpstream::refused_url().await;
    let failure = send(&client(url), false, timeouts()).await.unwrap_err();
    assert_eq!(failure, CloudFailure::ConnectFailed);
    assert_eq!(failure.code(), "connect_failed");
}

#[tokio::test]
async fn a_silent_upstream_hits_the_response_head_timeout() {
    let fake = FakeUpstream::start(vec![Behavior::Hang]).await;
    let started = std::time::Instant::now();
    let failure = send(
        &client(fake.url()),
        false,
        CloudTimeouts {
            response_head: Duration::from_millis(200),
            ..timeouts()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(failure, CloudFailure::ResponseHeadTimeout);
    assert!(failure.is_timeout());
    assert!(started.elapsed() < Duration::from_secs(3));
    // 请求已经到了上游（上游可能已经开始处理）
    assert_eq!(fake.request_count(), 1);
}

#[tokio::test]
async fn closing_the_connection_before_any_response_byte_is_a_send_failure_not_a_connect_failure() {
    let fake = FakeUpstream::start(vec![Behavior::CloseBeforeResponse]).await;
    let failure = send(&client(fake.url()), false, timeouts())
        .await
        .unwrap_err();
    assert_eq!(failure, CloudFailure::SendFailed);
    assert_eq!(
        fake.request_count(),
        1,
        "the request did reach the upstream"
    );
}

#[tokio::test]
async fn a_truncated_body_is_a_body_failure() {
    let fake = FakeUpstream::start(vec![Behavior::TruncatedBody {
        status: 200,
        declared_len: 1000,
        partial: r#"{"id":"c","choi"#.to_string(),
    }])
    .await;
    let head = send(&client(fake.url()), false, timeouts()).await.unwrap();
    assert_eq!(head.status, 200);
    assert_eq!(
        head.read_body().await.unwrap_err(),
        CloudFailure::BodyReadFailed
    );
}

#[tokio::test]
async fn oversized_bodies_are_refused_whether_declared_or_chunked() {
    // 声明的长度就超限：读之前就拒绝
    let big = "x".repeat(2000);
    let fake = FakeUpstream::start(vec![Behavior::json(200, json!({"pad": big}))]).await;
    let client_small = client_with(fake.url(), |s| s.max_response_bytes = 1000);
    let head = send(&client_small, false, timeouts()).await.unwrap();
    assert_eq!(
        head.read_body().await.unwrap_err(),
        CloudFailure::BodyTooLarge
    );
    // 没有声明长度（chunked）：读到超限为止
    let fake = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send("a".repeat(600)),
        SseStep::Send("b".repeat(600)),
        SseStep::Finish,
    ])])
    .await;
    let client_small = client_with(fake.url(), |s| s.max_response_bytes = 1000);
    let head = send(&client_small, false, timeouts()).await.unwrap();
    assert_eq!(
        head.read_body().await.unwrap_err(),
        CloudFailure::BodyTooLarge
    );
}

#[tokio::test]
async fn a_body_that_never_finishes_hits_the_total_body_timeout() {
    let fake = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send("{\"id\":".to_string()),
        SseStep::Hang,
    ])])
    .await;
    let head = send(
        &client(fake.url()),
        false,
        CloudTimeouts {
            body_total: Duration::from_millis(200),
            ..timeouts()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        head.read_body().await.unwrap_err(),
        CloudFailure::BodyTimeout
    );
}

async fn read_stream(
    mut head: idoris_upstream::cloud::ResponseHead,
) -> (Vec<ChunkKind>, Result<(), CloudFailure>, bool) {
    let mut parser = SseParser::new(1 << 20);
    let mut kinds = Vec::new();
    let outcome = loop {
        match head.next_chunk().await {
            Ok(Some(chunk)) => {
                for event in parser.push(&chunk).unwrap() {
                    kinds.push(classify(&event));
                }
            }
            Ok(None) => break Ok(()),
            Err(failure) => break Err(failure),
        }
    };
    (kinds, outcome, parser.has_partial_event())
}

#[tokio::test]
async fn a_stream_is_read_event_by_event_with_usage_in_the_last_chunk() {
    let fake = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("chatcmpl-s", "Hel")),
        SseStep::Send(content_event("chatcmpl-s", "lo")),
        SseStep::Send(usage_event("chatcmpl-s", usage(12, 2))),
        SseStep::Send(done_event()),
        SseStep::Finish,
    ])])
    .await;
    let head = send(&client(fake.url()), true, timeouts()).await.unwrap();
    assert!(head.is_event_stream());
    let (kinds, outcome, partial) = read_stream(head).await;
    outcome.unwrap();
    assert!(!partial);
    assert_eq!(kinds.len(), 4);
    let ChunkKind::Json(first) = &kinds[0] else {
        panic!("{kinds:?}")
    };
    assert_eq!(first.id.as_deref(), Some("chatcmpl-s"));
    let ChunkKind::Json(last) = &kinds[2] else {
        panic!("{kinds:?}")
    };
    assert_eq!(last.usage.valid_usage().unwrap().completion_tokens, 2);
    assert_eq!(kinds[3], ChunkKind::Done);
    // 上游收到的是流式请求，并且要求了流内 usage
    let sent = fake.requests()[0].json();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"], json!({"include_usage": true}));
    assert_eq!(
        fake.requests()[0].header("accept"),
        Some("text/event-stream")
    );
}

#[tokio::test]
async fn a_stream_that_goes_quiet_hits_the_idle_timeout() {
    let fake = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("c", "a")),
        SseStep::Sleep(Duration::from_secs(10)),
    ])])
    .await;
    let head = send(
        &client(fake.url()),
        true,
        CloudTimeouts {
            stream_idle: Duration::from_millis(150),
            ..timeouts()
        },
    )
    .await
    .unwrap();
    let (kinds, outcome, _) = read_stream(head).await;
    assert_eq!(kinds.len(), 1, "the event before the silence was delivered");
    assert_eq!(outcome.unwrap_err(), CloudFailure::StreamIdleTimeout);
}

#[tokio::test]
async fn a_stream_cut_without_the_terminating_chunk_is_truncated_not_finished() {
    let fake = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("c", "a")),
        SseStep::Send("data: {\"id\":\"c\",\"choices\":[{\"del".to_string()),
        SseStep::Close,
    ])])
    .await;
    let head = send(&client(fake.url()), true, timeouts()).await.unwrap();
    let (kinds, outcome, _) = read_stream(head).await;
    assert_eq!(kinds.len(), 1);
    assert_eq!(outcome.unwrap_err(), CloudFailure::StreamTruncated);
}

#[tokio::test]
async fn dropping_the_response_cancels_the_upstream_call() {
    let fake = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("c", "a")),
        SseStep::Hang,
    ])])
    .await;
    let mut head = send(&client(fake.url()), true, timeouts()).await.unwrap();
    let first = head.next_chunk().await.unwrap();
    assert!(first.is_some());
    assert!(!fake.client_closed(0, Duration::from_millis(100)).await);
    drop(head);
    assert!(
        fake.client_closed(0, Duration::from_secs(3)).await,
        "the upstream observed the client disconnect"
    );
}

#[tokio::test]
async fn the_key_and_the_prompt_appear_in_no_debug_or_error_text() {
    let fake = FakeUpstream::start(vec![
        Behavior::json(500, json!({"error": {"message": format!("echo {KEY}")}})),
        Behavior::CloseBeforeResponse,
    ])
    .await;
    let client = client(fake.url());
    let mut texts = vec![format!("{client:?}")];
    let head = send(&client, false, timeouts()).await.unwrap();
    texts.push(format!("{head:?}"));
    let failure = send(&client, false, timeouts()).await.unwrap_err();
    texts.push(format!("{failure} {failure:?} {}", failure.code()));
    for text in texts {
        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("SENTINEL"), "{text}");
        assert!(!text.contains("PROMPT"), "{text}");
    }
}

#[tokio::test]
async fn a_fixed_extra_body_reaches_the_upstream_and_the_client_cannot_override_it() {
    let fake = FakeUpstream::start(vec![Behavior::json(200, completion("c", "x", None))]).await;
    let client = client_with(fake.url(), |s| {
        s.extra_body = json!({"provider": {"zdr": true}})
            .as_object()
            .unwrap()
            .clone();
    });
    let mut client_body = body();
    client_body.insert("provider".into(), json!({"zdr": false}));
    let extra = Map::new();
    let prepared = client
        .prepare(
            &ChatCall {
                client_body: &client_body,
                upstream_model: "m",
                max_tokens: 8,
                stream: false,
                extra_body: &extra,
            },
            timeouts(),
        )
        .await
        .unwrap();
    prepared.send().await.unwrap();
    assert_eq!(fake.requests()[0].json()["provider"], json!({"zdr": true}));
}

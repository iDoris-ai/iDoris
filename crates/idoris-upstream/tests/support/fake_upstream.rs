//! 脚本化的假 OpenAI 兼容上游（裸 TCP 上的 HTTP/1.1），给计算服务 S4 的测试用。
//!
//! **不连接任何真实的推理供应商**：只在 `127.0.0.1` 上监听。为什么不用 `wiremock`：它只能给出「一个完整的响应」，
//! 而 S4 要测的恰恰是各种不完整的响应——连接被接受后直接关闭、响应头发出后断开、流发到一半断开、
//! 一直不回应、慢响应、客户端中途断开时上游是否看到连接被关。
//!
//! 每个请求从脚本队列里取下一个 [`Behavior`]；队列耗尽后回 500（`script exhausted`），这样「不该发生的第二次调用」
//! 会被计数看见。每个请求一个连接（响应带 `Connection: close`），不涉及 keep-alive。
//!
//! 这个文件同时被 `idoris-upstream` 与 `idoris-router` 的测试用 `#[path]` 引入，所以只依赖 std、tokio 和 serde_json。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// 一个响应脚本。
#[derive(Debug, Clone)]
pub enum Behavior {
    /// 一个完整的响应（`Content-Length`），先睡 `delay`。
    Json {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
        delay: Duration,
    },
    /// 流式（chunked）响应：按步骤执行。
    Sse {
        status: u16,
        headers: Vec<(String, String)>,
        steps: Vec<SseStep>,
    },
    /// 接受连接、读完请求，**不写任何字节**就关闭。
    CloseBeforeResponse,
    /// 接受连接、读完请求，永远不回应（直到客户端关闭）。
    Hang,
    /// 响应头声明了更长的 `Content-Length`，只写一部分正文就关闭。
    TruncatedBody {
        status: u16,
        declared_len: usize,
        partial: String,
    },
}

/// 流式响应的一个步骤。
#[derive(Debug, Clone)]
pub enum SseStep {
    /// 写一个 chunked 数据块（调用方自己带好 `data: ...\n\n`）。
    Send(String),
    /// 停一会儿。
    Sleep(Duration),
    /// 正常结束（写终结块 `0\r\n\r\n`）并关闭。
    Finish,
    /// 不写终结块，直接关闭连接（截断）。
    Close,
    /// 不再写，也不关闭，直到客户端关闭。
    Hang,
}

/// 记录下来的一个请求。
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// 头名小写。
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

struct Shared {
    script: std::sync::Mutex<VecDeque<Behavior>>,
    requests: std::sync::Mutex<Vec<RecordedRequest>>,
    closed: std::sync::Mutex<Vec<Arc<ClientClosed>>>,
    connections: AtomicUsize,
}

/// 客户端是否已经关闭了连接（读半边读到 EOF 或出错）。
pub struct ClientClosed {
    flag: AtomicBool,
    notify: Notify,
}

impl ClientClosed {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    fn mark(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_closed(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn wait(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let notified = self.notify.notified();
            if self.is_closed() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.is_closed();
            }
        }
    }
}

pub struct FakeUpstream {
    addr: SocketAddr,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeUpstream {
    /// 启动，脚本按顺序消费。
    pub async fn start(script: Vec<Behavior>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            script: std::sync::Mutex::new(script.into()),
            requests: std::sync::Mutex::new(Vec::new()),
            closed: std::sync::Mutex::new(Vec::new()),
            connections: AtomicUsize::new(0),
        });
        let task = tokio::spawn({
            let shared = shared.clone();
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    shared.connections.fetch_add(1, Ordering::SeqCst);
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let _ = serve(socket, shared).await;
                    });
                }
            }
        });
        Self { addr, shared, task }
    }

    /// 一个绑定后立刻关闭的端口：连接会被拒绝。
    pub async fn refused_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}/v1")
    }

    /// 基址（不含 `/chat/completions`）。
    pub fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 收到并记录了几个完整的请求。
    pub fn request_count(&self) -> usize {
        self.shared.requests.lock().unwrap().len()
    }

    /// 接受了几个 TCP 连接（连上但没发完请求的也算）。
    pub fn connection_count(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared.requests.lock().unwrap().clone()
    }

    /// 等到至少收到 `n` 个请求。
    pub async fn wait_for_requests(&self, n: usize, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while self.request_count() < n {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }

    /// 第 `index` 个请求的客户端是否在 `within` 内关闭了连接（取消）。
    pub async fn client_closed(&self, index: usize, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let handle = loop {
            if let Some(handle) = self.shared.closed.lock().unwrap().get(index).cloned() {
                break handle;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        handle.wait(left).await
    }
}

// ---------------------------------------------------------------------------
// 便捷构造
// ---------------------------------------------------------------------------

impl Behavior {
    pub fn json(status: u16, body: Value) -> Self {
        Behavior::Json {
            status,
            body: body.to_string(),
            headers: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    pub fn json_with_header(status: u16, body: Value, name: &str, value: &str) -> Self {
        Behavior::Json {
            status,
            body: body.to_string(),
            headers: vec![(name.to_string(), value.to_string())],
            delay: Duration::ZERO,
        }
    }

    pub fn slow_json(status: u16, body: Value, delay: Duration) -> Self {
        Behavior::Json {
            status,
            body: body.to_string(),
            headers: Vec::new(),
            delay,
        }
    }

    pub fn sse(steps: Vec<SseStep>) -> Self {
        Behavior::Sse {
            status: 200,
            headers: Vec::new(),
            steps,
        }
    }
}

/// 一个 OpenAI 风格的非流式响应。
pub fn completion(id: &str, content: &str, usage: Option<Value>) -> Value {
    let mut body = serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "model": "upstream-model",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": content}}],
    });
    if let Some(usage) = usage {
        body["usage"] = usage;
    }
    body
}

/// 流里一个内容 chunk（`data: ...\n\n`）。
pub fn content_event(id: &str, content: &str) -> String {
    let chunk = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": content}}],
    });
    format!("data: {chunk}\n\n")
}

/// 流里的 usage chunk（`choices` 为空）。
pub fn usage_event(id: &str, usage: Value) -> String {
    let chunk = serde_json::json!({"id": id, "object": "chat.completion.chunk", "choices": [], "usage": usage});
    format!("data: {chunk}\n\n")
}

pub fn done_event() -> String {
    "data: [DONE]\n\n".to_string()
}

pub fn usage(prompt: u64, completion: u64) -> Value {
    serde_json::json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    })
}

// ---------------------------------------------------------------------------
// 服务端实现
// ---------------------------------------------------------------------------

async fn read_request(socket: &mut TcpStream) -> std::io::Result<Option<RecordedRequest>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > 8 * 1024 * 1024 {
            return Ok(None);
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    }))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

async fn serve(mut socket: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let Some(request) = read_request(&mut socket).await? else {
        return Ok(());
    };
    let closed = ClientClosed::new();
    {
        // 先登记请求与关闭句柄，再取脚本：测试里「请求数 == n」之后一定能查到对应的关闭句柄
        shared.closed.lock().unwrap().push(closed.clone());
        shared.requests.lock().unwrap().push(request);
    }
    let behavior = shared.script.lock().unwrap().pop_front();
    let (mut read_half, mut write_half) = socket.into_split();
    // 请求已经读完，之后读半边再有任何结果（EOF / 错误 / 数据）都说明客户端关了连接（或发了不该发的东西）
    let watcher = tokio::spawn({
        let closed = closed.clone();
        async move {
            let mut scratch = [0u8; 64];
            loop {
                match read_half.read(&mut scratch).await {
                    Ok(0) | Err(_) => {
                        closed.mark();
                        return;
                    }
                    Ok(_) => {}
                }
            }
        }
    });
    let result = respond(&mut write_half, behavior, &closed).await;
    watcher.abort();
    result
}

async fn write_head(
    out: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    headers: &[(String, String)],
    extra: &[&str],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nConnection: close\r\n",
        reason(status)
    );
    for line in extra {
        head.push_str(line);
        head.push_str("\r\n");
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes()).await?;
    out.flush().await
}

/// 睡 `d`，但客户端关闭就提前返回 `true`。
async fn sleep_or_closed(d: Duration, closed: &ClientClosed) -> bool {
    if d.is_zero() {
        return closed.is_closed();
    }
    tokio::select! {
        () = tokio::time::sleep(d) => closed.is_closed(),
        was_closed = closed.wait(d) => was_closed,
    }
}

async fn respond(
    out: &mut tokio::net::tcp::OwnedWriteHalf,
    behavior: Option<Behavior>,
    closed: &ClientClosed,
) -> std::io::Result<()> {
    let behavior = behavior.unwrap_or_else(|| Behavior::Json {
        status: 500,
        body: r#"{"error":{"message":"script exhausted"}}"#.to_string(),
        headers: Vec::new(),
        delay: Duration::ZERO,
    });
    match behavior {
        Behavior::Json {
            status,
            body,
            headers,
            delay,
        } => {
            if sleep_or_closed(delay, closed).await {
                return Ok(());
            }
            let len = format!("Content-Length: {}", body.len());
            write_head(
                out,
                status,
                &headers,
                &["Content-Type: application/json", &len],
            )
            .await?;
            out.write_all(body.as_bytes()).await?;
            out.flush().await?;
            out.shutdown().await
        }
        Behavior::Sse {
            status,
            headers,
            steps,
        } => {
            write_head(
                out,
                status,
                &headers,
                &[
                    "Content-Type: text/event-stream",
                    "Cache-Control: no-cache",
                    "Transfer-Encoding: chunked",
                ],
            )
            .await?;
            for step in steps {
                match step {
                    SseStep::Send(data) => {
                        if closed.is_closed() {
                            return Ok(());
                        }
                        out.write_all(format!("{:x}\r\n", data.len()).as_bytes())
                            .await?;
                        out.write_all(data.as_bytes()).await?;
                        out.write_all(b"\r\n").await?;
                        out.flush().await?;
                    }
                    SseStep::Sleep(d) => {
                        if sleep_or_closed(d, closed).await {
                            return Ok(());
                        }
                    }
                    SseStep::Finish => {
                        out.write_all(b"0\r\n\r\n").await?;
                        out.flush().await?;
                        return out.shutdown().await;
                    }
                    SseStep::Close => return Ok(()),
                    SseStep::Hang => {
                        closed.wait(Duration::from_secs(3600)).await;
                        return Ok(());
                    }
                }
            }
            // 脚本写完了没有 Finish：按「没有终结块的关闭」处理
            Ok(())
        }
        Behavior::CloseBeforeResponse => Ok(()),
        Behavior::Hang => {
            closed.wait(Duration::from_secs(3600)).await;
            Ok(())
        }
        Behavior::TruncatedBody {
            status,
            declared_len,
            partial,
        } => {
            let len = format!("Content-Length: {declared_len}");
            write_head(out, status, &[], &["Content-Type: application/json", &len]).await?;
            out.write_all(partial.as_bytes()).await?;
            out.flush().await?;
            Ok(())
        }
    }
}

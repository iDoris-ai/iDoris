//! IDORIS-CORE 切片 S2（接口规范 §2.10，修订 1.6）：向 agent24d 登记与心跳（UDS 客户端）。
//!
//! 两层，都带一个假 agent24d（测试里的 Rust 服务端，在临时目录里监听 UDS，用测试自己按规范文字写的 HMAC 校验）：
//!
//! - **进程内**：直接用 `register::Registration`（二进制 `bin/idoris.rs` 用的同一个入口），时间常量缩短，
//!   断言握手与测试向量、先说话的顺序、心跳与失联、退避序列（观察「安排了多长的等待」，不靠墙钟猜）、各错误码、
//!   `instance_conflict`、孤儿计时、`set_admin_token`、`bye`、发现文件自愈、fd 不被子进程继承。
//! - **进程级**：起真二进制（规范时间常量，二进制没有改时间的入口），断言它在 `challenge` 之前一个字节不写、
//!   `register` 的字段与 `proof`、`/health.registered` 随会话变化、SIGTERM 时的 `bye`、`set_admin_token` 之后真实的
//!   Admin 监听器旧 token 401 新 token 200、被拒时数据面照常、`instance_conflict` 退出码 3、孤儿超时退出码 4。
//!
//! 测试环境是密封的：`HOME`、`IDORIS_STATE_DIR` 指向临时目录，组件卡的 endpoint 指向一个刚释放的本机端口，不连外网，
//! 不起任何推理进程。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

// 起的真二进制用 `spawn_guarded`：断言失败或 panic 时守卫的 `Drop` 杀掉并收走子进程，不会留下占着端口的孤儿。
#[path = "support/child_guard.rs"]
mod child_guard;

use std::{
    fs,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use child_guard::{ChildGuard, SpawnGuarded};
use idoris_router::{
    AppState,
    admin::{AdminSessionToken, SharedAdminToken, build_admin_app_with_token},
    discovery::{DiscoveryRecord, DiscoveryTarget},
    instance_nonce::InstanceNonce,
    readiness::{HealthState, Observation, UpstreamState, UpstreamStatus},
    register::{
        self, Event, Identity, JitterSource, LogSink, Observer, Registration, RegistrationConfig,
        RetryReason, SelfHeal, ServerError, StopCause, Timings, TokenRejection,
    },
};
use ring::hmac;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};
use tower::ServiceExt;

// 规范 §2.10.5 的测试向量。
const VECTOR_NONCE: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const VECTOR_CONN_ID: &str = "0123456789abcdef";
const VECTOR_CHALLENGE_NONCE: &str = "00112233445566778899aabbccddeeff";
const VECTOR_INSTANCE_ID: &str = "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11";
const VECTOR_SESSION_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
const VECTOR_REGISTER_PROOF: &str =
    "386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884";
const VECTOR_ACK_PROOF: &str = "099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967";
/// 规范 §2.10.3 里 `register` 的示例（含结尾的 LF）。
const REGISTER_EXAMPLE: &str = "{\"v\":1,\"op\":\"register\",\"nonce\":\"00112233445566778899aabbccddeeff\",\"instance_id\":\"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\",\"pid\":41234,\"version\":\"0.6.0\",\"contract_version\":\"1.3.0\",\"data_port\":51234,\"admin_port\":51235,\"started_at_ms\":1763625330123,\"phase\":\"ready\",\"resume\":false,\"proof\":\"386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884\"}\n";
/// 规范 §2.10.3 里 `heartbeat` 的示例（含结尾的 LF）。
const HEARTBEAT_EXAMPLE: &str =
    "{\"v\":1,\"op\":\"heartbeat\",\"seq\":1,\"phase\":\"ready\",\"ready\":true}\n";
const BYE_LINE: &str = "{\"v\":1,\"op\":\"bye\",\"reason\":\"shutdown\"}\n";
const ADMIN_TOKEN_ACK_LINE: &str = "{\"v\":1,\"op\":\"admin_token_ack\"}\n";

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const NEW_TOKEN: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const OTHER_TOKEN: &str = "aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbccccccccccccccccdddddddddddddddd";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn hmac_key(key_hex: &str) -> hmac::Key {
    let bytes: Vec<u8> = (0..key_hex.len() / 2)
        .map(|i| u8::from_str_radix(&key_hex[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    assert_eq!(bytes.len(), 32, "the HMAC key is the 32 decoded raw bytes");
    hmac::Key::new(hmac::HMAC_SHA256, &bytes)
}

fn hmac_hex(key_hex: &str, transcript: &str) -> String {
    hex(hmac::sign(&hmac_key(key_hex), transcript.as_bytes()).as_ref())
}

/// 假 agent24d 校验 `register.proof`：按规范文字，用 `register` 帧自己带的字段重建 transcript。
fn expected_register_proof(key_hex: &str, conn_id: &str, nonce: &str, register: &Value) -> String {
    let transcript = format!(
        "idoris-register/v1\n{conn_id}\n{nonce}\n{}\n{}\n{}\n{}\n{}",
        register["instance_id"].as_str().unwrap(),
        register["pid"],
        register["data_port"],
        register["admin_port"],
        register["started_at_ms"],
    );
    hmac_hex(key_hex, &transcript)
}

fn expected_ack_proof(key_hex: &str, conn_id: &str, nonce: &str, session_id: &str) -> String {
    hmac_hex(
        key_hex,
        &format!("idoris-ack/v1\n{conn_id}\n{nonce}\n{session_id}"),
    )
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 每次握手一个新的 `conn_id`（16 个 hex）、挑战 nonce（32 个 hex）、`session_id`（32 个 hex）。
fn fresh_ids() -> (String, String, String) {
    let n = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    (
        format!("{n:016x}"),
        format!("{:032x}", u128::from(n) * 0x0001_0000_0001 + 0xabcd),
        format!("{:032x}", u128::from(n) * 0x0002_0000_0003 + 0x1234),
    )
}

// ---------------------------------------------------------------------------------------------------------------
// 假 agent24d
// ---------------------------------------------------------------------------------------------------------------

enum Got {
    /// 一行（含结尾的 LF）。
    Line(Vec<u8>),
    Eof,
    Timeout,
}

/// 服务端看到的一条连接。
struct Peer {
    stream: UnixStream,
    /// 已经读到、还没有凑成一行的字节。
    buf: Vec<u8>,
}

struct Handshaken {
    conn_id: String,
    nonce: String,
    session_id: String,
    register: Value,
}

impl Peer {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    async fn read_line(&mut self, within: Duration) -> Got {
        let deadline = Instant::now() + within;
        loop {
            if let Some(pos) = self.buf.iter().position(|byte| *byte == b'\n') {
                return Got::Line(self.buf.drain(..=pos).collect());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let mut chunk = [0_u8; 4096];
            match tokio::time::timeout(left, self.stream.read(&mut chunk)).await {
                Err(_) => return Got::Timeout,
                Ok(Ok(0) | Err(_)) => return Got::Eof,
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    async fn recv_line(&mut self) -> Vec<u8> {
        match self.read_line(secs(15)).await {
            Got::Line(line) => line,
            Got::Eof => panic!(
                "the connection closed while waiting for a frame (leftover bytes: {:?})",
                self.buf
            ),
            Got::Timeout => panic!("no frame within 15 s (buffered: {:?})", self.buf),
        }
    }

    async fn recv(&mut self) -> Value {
        let line = self.recv_line().await;
        serde_json::from_slice(&line).unwrap_or_else(|error| {
            panic!("not JSON: {error}: {:?}", String::from_utf8_lossy(&line))
        })
    }

    /// 读到对端关闭为止，返回关闭之前收到的每一行；`within` 内没关闭就失败。
    async fn drain_until_eof(&mut self, within: Duration) -> Vec<Vec<u8>> {
        let deadline = Instant::now() + within;
        let mut lines = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.read_line(left).await {
                Got::Line(line) => lines.push(line),
                Got::Eof => {
                    assert!(
                        self.buf.is_empty(),
                        "the peer closed in the middle of a frame: {:?}",
                        self.buf
                    );
                    return lines;
                }
                Got::Timeout => {
                    panic!(
                        "the connection was not closed within {within:?} (frames so far: {lines:?})"
                    )
                }
            }
        }
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.unwrap();
    }

    /// 对端可能已经关闭时用：写失败不算测试失败。
    async fn try_send_raw(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes).await;
    }

    async fn send(&mut self, frame: &Value) {
        let mut line = frame.to_string().into_bytes();
        line.push(b'\n');
        self.send_raw(&line).await;
    }

    /// 服务端的前半：发 `challenge`，读 `register`（原始行也返回）。
    async fn challenge(&mut self, conn_id: &str, nonce: &str) -> (Value, Vec<u8>) {
        self.send(&json!({"v": 1, "op": "challenge", "conn_id": conn_id, "nonce": nonce}))
            .await;
        let line = self.recv_line().await;
        (serde_json::from_slice(&line).unwrap(), line)
    }

    async fn ack(
        &mut self,
        key_hex: &str,
        conn_id: &str,
        nonce: &str,
        session_id: &str,
        heartbeat_ms: Option<u64>,
    ) {
        let mut frame = json!({
            "v": 1,
            "op": "ack",
            "session_id": session_id,
            "proof": expected_ack_proof(key_hex, conn_id, nonce, session_id),
        });
        if let Some(heartbeat_ms) = heartbeat_ms {
            frame["heartbeat_ms"] = json!(heartbeat_ms);
        }
        self.send(&frame).await;
    }

    /// 一次完整的、正确的握手；按规范校验 `register`（`nonce` 回显、`proof`），再回 `ack`。
    async fn handshake(&mut self, key_hex: &str, heartbeat_ms: Option<u64>) -> Handshaken {
        let (conn_id, nonce, session_id) = fresh_ids();
        let (register, _) = self.challenge(&conn_id, &nonce).await;
        assert_eq!(register["v"], 1);
        assert_eq!(register["op"], "register");
        assert_eq!(
            register["nonce"],
            nonce.as_str(),
            "the challenge nonce is echoed"
        );
        assert_eq!(
            register["proof"],
            expected_register_proof(key_hex, &conn_id, &nonce, &register).as_str(),
            "register.proof must verify"
        );
        self.ack(key_hex, &conn_id, &nonce, &session_id, heartbeat_ms)
            .await;
        Handshaken {
            conn_id,
            nonce,
            session_id,
            register,
        }
    }

    /// 在已建立的会话上应答心跳，直到 `until` 到（至少应答一个）。每个心跳读到都回 `heartbeat_ack`。
    async fn serve_heartbeats_until(&mut self, until: Instant) -> usize {
        let mut answered = 0;
        while Instant::now() < until {
            let left = until.saturating_duration_since(Instant::now());
            match self.read_line(left.min(ms(100))).await {
                Got::Line(line) => {
                    let beat: Value = serde_json::from_slice(&line).unwrap();
                    assert_eq!(beat["op"], "heartbeat", "{beat}");
                    self.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": beat["seq"]}))
                        .await;
                    answered += 1;
                }
                Got::Timeout => {}
                Got::Eof => panic!("the client closed a session that should have stayed up"),
            }
        }
        answered
    }
}

struct FakeServer {
    listener: UnixListener,
    path: PathBuf,
}

impl FakeServer {
    fn bind(path: &Path) -> Self {
        let _ = fs::remove_file(path);
        Self {
            listener: UnixListener::bind(path).unwrap(),
            path: path.to_path_buf(),
        }
    }

    async fn accept(&self) -> Peer {
        match tokio::time::timeout(secs(30), self.listener.accept()).await {
            Ok(Ok((stream, _))) => Peer::new(stream),
            other => panic!("no connection from iDoris within 30 s: {other:?}"),
        }
    }

    async fn try_accept(&self, within: Duration) -> Option<Peer> {
        match tokio::time::timeout(within, self.listener.accept()).await {
            Ok(Ok((stream, _))) => Some(Peer::new(stream)),
            _ => None,
        }
    }

    /// 服务端「关掉」：不再监听，socket 文件也没了（agent24d 停了）。
    fn shut_down(self) {
        let path = self.path.clone();
        drop(self);
        let _ = fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 进程内的装置
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<Event>>>);

impl Events {
    fn observer(&self) -> Observer {
        let events = self.0.clone();
        Arc::new(move |event: &Event| events.lock().unwrap().push(event.clone()))
    }

    fn snapshot(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }

    fn count(&self, matching: impl Fn(&Event) -> bool) -> usize {
        self.snapshot()
            .iter()
            .filter(|event| matching(event))
            .count()
    }

    /// 第 `after` 个事件之后第一个满足条件的事件。
    async fn wait_after(
        &self,
        after: usize,
        what: &str,
        matching: impl Fn(&Event) -> bool,
    ) -> Event {
        let deadline = Instant::now() + secs(20);
        loop {
            if let Some(event) = self
                .snapshot()
                .into_iter()
                .skip(after)
                .find(|e| matching(e))
            {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; events: {:#?}",
                self.snapshot()
            );
            tokio::time::sleep(ms(5)).await;
        }
    }

    async fn wait(&self, what: &str, matching: impl Fn(&Event) -> bool) -> Event {
        self.wait_after(0, what, matching).await
    }

    /// 到目前为止安排过的所有重连等待（按顺序）。
    fn retry_delays(&self) -> Vec<Duration> {
        self.snapshot()
            .into_iter()
            .filter_map(|event| match event {
                Event::RetryScheduled { delay, .. } => Some(delay),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<String>>>);

impl Logs {
    fn sink(&self) -> LogSink {
        let lines = self.0.clone();
        Arc::new(move |line: &str| lines.lock().unwrap().push(line.to_string()))
    }

    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn assert_no_secret(&self, secrets: &[&str]) {
        let joined = self.lines().join("\n");
        for secret in secrets {
            assert!(!secret.is_empty());
            assert!(
                !joined.contains(secret),
                "a secret leaked into the log: {joined}"
            );
        }
    }
}

/// 缩短后的时间：握手各步的超时放得很宽（机器忙时不会误判），重试的等待很短，抖动关闭。
fn fast() -> Timings {
    Timings {
        connect: secs(3),
        challenge_wait: secs(5),
        register_within: secs(2),
        ack_wait: secs(5),
        heartbeat_default: ms(50),
        heartbeat_min: ms(5),
        heartbeat_max: secs(2),
        heartbeat_jitter: 0.0,
        heartbeat_ack_wait: secs(2),
        heartbeat_misses: 3,
        backoff_steps: [
            Duration::ZERO,
            ms(5),
            ms(10),
            ms(20),
            ms(40),
            ms(80),
            ms(160),
        ],
        backoff_cap: ms(200),
        backoff_jitter: 0.2,
        backoff_reset_after: secs(600),
        wait_identity: ms(300),
        wait_proof_invalid: ms(150),
        wait_busy: ms(60),
        bye_budget: ms(500),
        admin_token_interval: ms(800),
        self_heal_interval: ms(40),
        jitter: JitterSource::None,
    }
}

/// 握手各步的超时拉到 60 秒：一条路径是「立即关闭」还是「等超时才关闭」，在 [`PROMPTLY`]（5 秒）的期限里就分得开了。
/// （`fast()` 的 5 秒超时配上 10 秒的 drain 期限分不开：去掉握手阶段的某个守卫，连接仍然会在 5 秒后被超时关掉。）
fn patient() -> Timings {
    Timings {
        challenge_wait: secs(60),
        ack_wait: secs(60),
        ..fast()
    }
}

/// 「立即」的期限：远短于 `patient()` 的 60 秒，远长于一次本机往返（机器忙时也不会误判）。
const PROMPTLY: Duration = Duration::from_secs(5);

fn ready_upstream() -> UpstreamStatus {
    UpstreamStatus {
        id: "omlx".to_string(),
        engine_kind: "omlx".to_string(),
        state: UpstreamState::Ready,
        reason: None,
        managed: false,
        owned: false,
    }
}

fn starting_upstream() -> UpstreamStatus {
    UpstreamStatus {
        state: UpstreamState::Starting,
        ..ready_upstream()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    dir_path: PathBuf,
    socket: PathBuf,
    nonce_hex: String,
    identity: Identity,
    health: HealthState,
    admin_token: SharedAdminToken,
    old_secret: String,
    events: Events,
    logs: Logs,
}

struct StartOptions {
    timings: Timings,
    orphan: Option<Duration>,
    /// 为 false 就不配 `register_socket`（只有发现文件自愈）。
    with_socket: bool,
    discovery: Option<SelfHeal>,
    /// 为 true 就在启动之后立刻开发布闸门（没有发现文件的装置没有别的开闸者）。
    open_gate: bool,
}

impl StartOptions {
    fn new(timings: Timings) -> Self {
        Self {
            timings,
            orphan: None,
            with_socket: true,
            discovery: None,
            open_gate: true,
        }
    }
}

impl Rig {
    fn new() -> Self {
        Self::with(
            Identity {
                instance_id: "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11".to_string(),
                pid: 4242,
                version: "0.6.0".to_string(),
                contract_version: "1.3.0".to_string(),
                data_port: 51234,
                admin_port: 51235,
                started_at_ms: 1_763_625_330_123,
            },
            &"a5".repeat(32),
            vec![ready_upstream()],
        )
    }

    fn with(identity: Identity, nonce_hex: &str, upstreams: Vec<UpstreamStatus>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = fs::canonicalize(dir.path()).unwrap();
        fs::set_permissions(&dir_path, fs::Permissions::from_mode(0o700)).unwrap();
        let admin = AdminSessionToken::from_launcher_secret(TOKEN.as_bytes()).unwrap();
        Self {
            socket: dir_path.join("r.sock"),
            dir_path,
            _dir: dir,
            nonce_hex: nonce_hex.to_string(),
            health: HealthState::with_upstreams(identity.started_at_ms, upstreams),
            identity,
            admin_token: SharedAdminToken::new(admin),
            old_secret: TOKEN.to_string(),
            events: Events::default(),
            logs: Logs::default(),
        }
    }

    fn server(&self) -> FakeServer {
        FakeServer::bind(&self.socket)
    }

    fn start(&self, timings: Timings) -> Registration {
        self.start_with(StartOptions::new(timings))
    }

    fn start_with(&self, options: StartOptions) -> Registration {
        let registration = Registration::start(RegistrationConfig {
            socket: options.with_socket.then(|| self.socket.clone()),
            orphan_timeout: options.orphan,
            identity: self.identity.clone(),
            nonce: InstanceNonce::from_hex(&self.nonce_hex).unwrap(),
            health: self.health.clone(),
            admin_token: self.admin_token.clone(),
            discovery: options.discovery,
            timings: options.timings,
            observer: Some(self.events.observer()),
            log: self.logs.sink(),
        });
        if options.open_gate {
            registration.publish_gate().open();
        }
        registration
    }

    async fn wait_registered(&self, expected: bool) {
        let deadline = Instant::now() + secs(20);
        while self.health.registered() != expected {
            assert!(
                Instant::now() < deadline,
                "/health.registered never became {expected}; events: {:#?}",
                self.events.snapshot()
            );
            tokio::time::sleep(ms(2)).await;
        }
    }
}

async fn wait_until(what: &str, within: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(ms(5)).await;
    }
}

async fn still_pending(registration: &Registration, window: Duration) -> bool {
    tokio::time::timeout(window, registration.wants_stop())
        .await
        .is_err()
}

// ---------------------------------------------------------------------------------------------------------------
// 握手、测试向量、先说话
// ---------------------------------------------------------------------------------------------------------------

/// 规范 §2.10.5 的测试向量，端到端：用向量里的身份与 nonce 起登记客户端，服务端发向量里的 `challenge`，
/// 客户端发出的 `register` 逐字节等于规范 §2.10.3 的示例（含 `proof` `386409d0…`）；服务端回向量里的 `ack`
/// （`proof` `099e173f…`），客户端接受，`/health.registered` 变 true，紧接着发 `heartbeat` seq=1，也逐字节等于示例。
#[tokio::test]
async fn the_spec_test_vectors_produce_the_exact_register_and_heartbeat_bytes() {
    let rig = Rig::with(
        Identity {
            instance_id: VECTOR_INSTANCE_ID.to_string(),
            pid: 41234,
            version: "0.6.0".to_string(),
            contract_version: "1.3.0".to_string(),
            data_port: 51234,
            admin_port: 51235,
            started_at_ms: 1_763_625_330_123,
        },
        VECTOR_NONCE,
        vec![ready_upstream()],
    );
    let server = rig.server();
    // 心跳间隔用规范值（ack 里 10000），所以第二个心跳在大约 10 秒之后；其余常量缩短与否都不影响本测试。
    let registration = rig.start(Timings {
        heartbeat_default: secs(10),
        heartbeat_min: secs(2),
        heartbeat_max: secs(60),
        ..fast()
    });
    let mut peer = server.accept().await;
    peer.send(&json!({
        "v": 1, "op": "challenge", "conn_id": VECTOR_CONN_ID, "nonce": VECTOR_CHALLENGE_NONCE
    }))
    .await;
    let register_line = peer.recv_line().await;
    assert_eq!(
        String::from_utf8(register_line.clone()).unwrap(),
        REGISTER_EXAMPLE,
        "register must be byte-identical to the specified example"
    );
    let register: Value = serde_json::from_slice(&register_line).unwrap();
    assert_eq!(register["proof"], VECTOR_REGISTER_PROOF);
    assert!(!rig.health.registered(), "not registered before the ack");

    let acked_at = Instant::now();
    peer.send(&json!({
        "v": 1, "op": "ack", "session_id": VECTOR_SESSION_ID,
        "heartbeat_ms": 10000, "proof": VECTOR_ACK_PROOF
    }))
    .await;
    rig.wait_registered(true).await;
    let first_beat = peer.recv_line().await;
    assert_eq!(String::from_utf8(first_beat).unwrap(), HEARTBEAT_EXAMPLE);
    assert!(
        acked_at.elapsed() < secs(3),
        "the first heartbeat follows the ack at once, not a full interval later: {:?}",
        acked_at.elapsed()
    );
    // 「ack 后立即发，之后每 heartbeat_ms」：第二个心跳还在大约 10 秒之外。
    assert!(
        matches!(peer.read_line(ms(400)).await, Got::Timeout),
        "the next heartbeat is a full interval away"
    );
    assert!(peer.buf.is_empty());
    assert_eq!(
        rig.events
            .count(|event| matches!(event, Event::Registered { resume: false, .. })),
        1
    );

    registration.finish().await;
    // 日志里没有 nonce、proof。
    rig.logs
        .assert_no_secret(&[VECTOR_NONCE, VECTOR_REGISTER_PROOF, VECTOR_ACK_PROOF]);
}

/// 服务端先说话：它沉默的这段时间里，客户端一个字节都不写，也不关闭连接；服务端开口之后才有 `register`。
#[tokio::test]
async fn the_client_writes_nothing_until_the_server_has_sent_the_challenge() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    assert!(
        matches!(peer.read_line(ms(800)).await, Got::Timeout),
        "the client must neither write nor hang up while waiting for the challenge"
    );
    assert!(
        peer.buf.is_empty(),
        "stray bytes before the challenge: {:?}",
        peer.buf
    );
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::ChallengeReceived)),
        0
    );

    let (conn_id, nonce, session_id) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    assert_eq!(register["op"], "register");
    // 第一个字节出现在 challenge 之后。
    peer.ack(&rig.nonce_hex, &conn_id, &nonce, &session_id, Some(50))
        .await;
    rig.wait_registered(true).await;
    registration.finish().await;
}

/// 包括 `error`：challenge 之前收到服务端的 `error`（例如 `busy`），客户端也不回任何字节，直接关闭连接，
/// 并按 `busy` 的固定等待安排重连。
#[tokio::test]
async fn an_error_before_the_challenge_gets_no_reply_bytes_at_all() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    peer.send(&json!({"v": 1, "op": "error", "code": "busy", "message": "full"}))
        .await;
    let frames = peer.drain_until_eof(secs(10)).await;
    assert!(
        frames.is_empty(),
        "nothing may be written before the challenge: {frames:?}"
    );
    let event = rig
        .events
        .wait("the scheduled retry", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    assert_eq!(
        event,
        Event::RetryScheduled {
            delay: fast().wait_busy,
            fixed: true,
            reason: RetryReason::Server(ServerError::Busy)
        }
    );
    registration.finish().await;
}

/// 同样：challenge 之前的坏帧（乱码、非对象、`v` 不对、`set_admin_token`）只会让客户端关闭连接，不写任何东西
/// （收到 challenge 之前不写任何字节，所以连 `error bad_frame` 也不发）。握手各步的超时是 60 秒（`patient()`），而关闭必须在
/// 5 秒内发生、安排的重连原因必须是 `bad_frame`：「立即关闭」与「忽略这一帧、等 `challenge_wait` 到了再关」靠这个分得开。
#[tokio::test]
async fn garbage_before_the_challenge_closes_the_connection_without_writing() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(patient());
    let bad_frames: [&[u8]; 4] = [
        b"this is not json\n",
        b"[1,2,3]\n",
        b"{\"v\":2,\"op\":\"challenge\",\"conn_id\":\"0123456789abcdef\",\"nonce\":\"00112233445566778899aabbccddeeff\"}\n",
        // 没有 ack 的会话上的 `set_admin_token`：不处理并关闭连接。
        b"{\"v\":1,\"op\":\"set_admin_token\",\"token\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"}\n",
    ];
    for bad in bad_frames {
        let seen = rig.events.snapshot().len();
        let mut peer = server.accept().await;
        peer.send_raw(bad).await;
        let frames = peer.drain_until_eof(PROMPTLY).await;
        assert!(
            frames.is_empty(),
            "no byte may be written before the challenge: {frames:?} (after {:?})",
            String::from_utf8_lossy(bad)
        );
        let scheduled = rig
            .events
            .wait_after(seen, "the retry scheduled after the bad frame", |e| {
                matches!(e, Event::RetryScheduled { .. })
            })
            .await;
        assert!(
            matches!(
                scheduled,
                Event::RetryScheduled {
                    reason: RetryReason::BadFrame,
                    fixed: false,
                    ..
                }
            ),
            "{scheduled:?} (after {:?})",
            String::from_utf8_lossy(bad)
        );
    }
    // 那个 `set_admin_token` 没有被处理。
    assert!(rig.admin_token.matches(&rig.old_secret));
    registration.finish().await;
}

/// 收到 challenge 之后、`ack` 之前的坏帧：客户端尽力发 `error bad_frame` 再关闭连接，然后重连。握手各步的超时是 60 秒
/// （`patient()`），关闭必须在 5 秒内发生，原因是 `bad_frame`：分得开「立即关闭」与「`error` 发完之后继续等 `ack` 到超时」。
#[tokio::test]
async fn a_bad_frame_after_the_challenge_is_answered_with_bad_frame_and_the_connection_is_closed() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(patient());
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    assert_eq!(register["op"], "register");
    peer.send_raw(b"{\"v\":1,\"op\":\"ack\",\"session_id\":\"SHORT\",\"proof\":\"x\"}\n")
        .await;
    let frames = peer.drain_until_eof(PROMPTLY).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    let error: Value = serde_json::from_slice(&frames[0]).unwrap();
    assert_eq!(error["op"], "error");
    assert_eq!(error["code"], "bad_frame");
    assert!(error["message"].as_str().unwrap().chars().count() <= 200);
    assert!(!rig.health.registered());
    let scheduled = rig
        .events
        .wait("the retry scheduled after the bad frame", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    assert_eq!(
        scheduled,
        Event::RetryScheduled {
            delay: Duration::ZERO,
            fixed: false,
            reason: RetryReason::BadFrame
        }
    );
    // 重连（默认退避，第一次立即）。
    let _second = server.accept().await;
    registration.finish().await;
}

/// challenge 之前服务端就关闭了连接（EOF）：客户端立即按 `disconnected` 重连（退避的第 1 次，等待为零），而不是在已经没有
/// 对端的连接上空等到 `challenge_wait`（这里是 60 秒）。
#[tokio::test]
async fn an_eof_before_the_challenge_means_a_reconnect_at_once_not_a_wait_for_the_timeout() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(patient());
    drop(server.accept().await);
    let scheduled = tokio::time::timeout(
        PROMPTLY,
        rig.events.wait("the retry after the EOF", |e| {
            matches!(e, Event::RetryScheduled { .. })
        }),
    )
    .await
    .expect(
        "an EOF before the challenge must schedule the reconnect at once, not after challenge_wait",
    );
    assert_eq!(
        scheduled,
        Event::RetryScheduled {
            delay: Duration::ZERO,
            fixed: false,
            reason: RetryReason::Disconnected
        }
    );
    // 重连真的发生了，而且新连接上仍然什么都没写。
    let mut again = server
        .try_accept(PROMPTLY)
        .await
        .expect("the client must reconnect");
    assert!(matches!(again.read_line(ms(300)).await, Got::Timeout));
    registration.finish().await;
}

/// `register` 发出之后、`ack` 之前服务端关闭了连接（EOF）：同样立即按 `disconnected` 重连，不是等到 `ack_wait`（这里是 60 秒）。
#[tokio::test]
async fn an_eof_after_register_means_a_reconnect_at_once_not_a_wait_for_the_ack_timeout() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(patient());
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    assert_eq!(register["op"], "register");
    drop(peer);
    let scheduled = tokio::time::timeout(
        PROMPTLY,
        rig.events.wait("the retry after the EOF", |e| {
            matches!(e, Event::RetryScheduled { .. })
        }),
    )
    .await
    .expect(
        "an EOF while waiting for the ack must schedule the reconnect at once, not after ack_wait",
    );
    assert_eq!(
        scheduled,
        Event::RetryScheduled {
            delay: Duration::ZERO,
            fixed: false,
            reason: RetryReason::Disconnected
        }
    );
    assert!(!rig.health.registered());
    // 重连之后能照常完成握手。
    let mut again = server
        .try_accept(PROMPTLY)
        .await
        .expect("the client must reconnect");
    again.handshake(&rig.nonce_hex, Some(50)).await;
    rig.wait_registered(true).await;
    registration.finish().await;
}

// ---------------------------------------------------------------------------------------------------------------
// 服务端证明：ack.proof
// ---------------------------------------------------------------------------------------------------------------

/// `ack.proof` 不符：客户端不登记、不发心跳，尽力发 `error proof_invalid` 后关闭，按默认退避重连，数据面照常。
/// 覆盖三种不符：错的 proof、别的连接的 proof 被重放（`conn_id` 与挑战 nonce 绑进了证明）、不知道 nonce 的冒充者算的 proof。
#[tokio::test]
async fn a_wrong_or_replayed_ack_proof_is_refused_with_proof_invalid_and_never_registers() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());

    // 先让一个连接的握手正常走完，留下它的（合法的）证明用来重放。
    let mut first = server.accept().await;
    let old = first.handshake(&rig.nonce_hex, Some(50)).await;
    rig.wait_registered(true).await;
    drop(first);
    rig.wait_registered(false).await;

    // 每个坏证明都在拿到本次连接的 conn_id、挑战 nonce、session_id 之后才算出来。
    let key = rig.nonce_hex.clone();
    let replayed = expected_ack_proof(&key, &old.conn_id, &old.nonce, &old.session_id);
    type ProofMaker = Box<dyn Fn(&str, &str, &str) -> String>;
    let cases: Vec<(&str, ProofMaker)> = vec![
        ("a flipped digit", {
            let key = key.clone();
            Box::new(move |conn, nonce, session| {
                let mut proof = expected_ack_proof(&key, conn, nonce, session);
                let first = if proof.starts_with('0') { "1" } else { "0" };
                proof.replace_range(..1, first);
                proof
            })
        }),
        (
            "a proof replayed from an earlier connection",
            Box::new(move |_, _, _| replayed.clone()),
        ),
        ("a proof computed with a key that is not the nonce", {
            Box::new(|conn, nonce, session| {
                expected_ack_proof(&"11".repeat(32), conn, nonce, session)
            })
        }),
        ("a proof that binds a different conn_id", {
            let key = key.clone();
            Box::new(move |_, nonce, session| {
                expected_ack_proof(&key, "ffffffffffffffff", nonce, session)
            })
        }),
        ("a proof that binds a different challenge nonce", {
            let key = key.clone();
            Box::new(move |conn, _, session| {
                expected_ack_proof(&key, conn, "ffffffffffffffffffffffffffffffff", session)
            })
        }),
        ("a proof that binds a different session_id", {
            let key = key.clone();
            Box::new(move |conn, nonce, _| {
                expected_ack_proof(&key, conn, nonce, "ffffffffffffffffffffffffffffffff")
            })
        }),
    ];
    let case_count = cases.len();
    for (label, make_proof) in cases {
        let mut peer = server.accept().await;
        let (conn_id, nonce, session_id) = fresh_ids();
        let (register, _) = peer.challenge(&conn_id, &nonce).await;
        // 客户端自己的 proof 是对的（服务端能验）。
        assert_eq!(
            register["proof"],
            expected_register_proof(&rig.nonce_hex, &conn_id, &nonce, &register).as_str()
        );
        peer.send(&json!({
            "v": 1, "op": "ack", "session_id": session_id, "heartbeat_ms": 50,
            "proof": make_proof(&conn_id, &nonce, &session_id)
        }))
        .await;
        let frames = peer.drain_until_eof(secs(10)).await;
        assert_eq!(frames.len(), 1, "{label}: {frames:?}");
        let error: Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(error["op"], "error", "{label}");
        assert_eq!(error["code"], "proof_invalid", "{label}");
        assert!(
            !frames
                .iter()
                .any(|frame| String::from_utf8_lossy(frame).contains("heartbeat")),
            "{label}: no heartbeat may follow a refused ack"
        );
        assert!(!rig.health.registered(), "{label}");
    }
    // 只有第一个合法的握手建立过会话。
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::Registered { .. })),
        1
    );
    // 被拒的这几次都按默认退避（不是固定等待）重连。
    let refused = rig
        .events
        .snapshot()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::AttemptFailed {
                    reason: RetryReason::AckProofInvalid,
                    ..
                }
            )
        })
        .count();
    assert_eq!(refused, case_count);
    assert!(
        rig.events
            .snapshot()
            .iter()
            .all(|event| !matches!(event, Event::RetryScheduled { fixed: true, .. }))
    );
    registration.finish().await;
}

/// 服务端不认 `register.proof`：回 `error proof_invalid`，客户端关闭、固定等 30 秒（缩短后的值）再重连，不退出。
/// 同一张表里的其余错误码：`identity_rejected`、`unsupported_version`、`contract_incompatible` 等 60 秒，`busy` 5 秒，
/// `bad_frame`、`timeout`、`internal`、不认识的码按默认退避（第一次立即）。**用规范的时间常量**：只读事件里安排的等待，
/// 不真的等。
#[tokio::test]
async fn each_error_code_closes_the_connection_and_schedules_the_specified_wait() {
    let table: [(&str, bool, Duration, ServerError); 9] = [
        (
            "identity_rejected",
            true,
            secs(60),
            ServerError::IdentityRejected,
        ),
        (
            "unsupported_version",
            true,
            secs(60),
            ServerError::UnsupportedVersion,
        ),
        (
            "contract_incompatible",
            true,
            secs(60),
            ServerError::ContractIncompatible,
        ),
        ("proof_invalid", true, secs(30), ServerError::ProofInvalid),
        ("busy", true, secs(5), ServerError::Busy),
        ("bad_frame", false, Duration::ZERO, ServerError::BadFrame),
        ("timeout", false, Duration::ZERO, ServerError::Timeout),
        ("internal", false, Duration::ZERO, ServerError::Internal),
        (
            "some_future_code",
            false,
            Duration::ZERO,
            ServerError::Unknown,
        ),
    ];
    for (code, fixed, delay, parsed) in table {
        let rig = Rig::new();
        let server = rig.server();
        let registration = rig.start(Timings {
            jitter: JitterSource::None,
            ..Timings::default()
        });
        let mut peer = server.accept().await;
        let (conn_id, nonce, _) = fresh_ids();
        let (register, _) = peer.challenge(&conn_id, &nonce).await;
        assert_eq!(register["op"], "register", "{code}");
        peer.send(&json!({"v": 1, "op": "error", "code": code, "message": "no"}))
            .await;
        // 关闭连接，而且不再多写什么（agent24d 收到 error 之后本来就结束了会话）。
        let frames = peer.drain_until_eof(secs(10)).await;
        assert!(frames.is_empty(), "{code}: {frames:?}");
        let event = rig
            .events
            .wait("the scheduled retry", |e| {
                matches!(e, Event::RetryScheduled { .. })
            })
            .await;
        assert_eq!(
            event,
            Event::RetryScheduled {
                delay,
                fixed,
                reason: RetryReason::Server(parsed)
            },
            "{code}"
        );
        assert!(!rig.health.registered(), "{code}");
        // 登记被拒永远不使 iDoris 退出。
        assert!(still_pending(&registration, ms(100)).await, "{code}");
        registration.finish().await;
    }
}

/// 服务端不认 `register.proof`（它用的 nonce 与我们的不同，回 `error proof_invalid`）：客户端关闭连接，**固定等待**
/// `proof_invalid` 的时间（规范 30 秒，这里缩短成 400ms，不是默认退避的立即重连）之后才重连，不退出，重新握手成功。
#[tokio::test]
async fn a_register_proof_the_server_rejects_means_a_fixed_wait_before_the_next_attempt_and_no_exit()
 {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(Timings {
        wait_proof_invalid: ms(400),
        ..fast()
    });
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    // 服务端按它以为的 nonce 验证：对不上。
    assert_ne!(
        register["proof"],
        expected_register_proof(&"22".repeat(32), &conn_id, &nonce, &register).as_str()
    );
    peer.send(&json!({
        "v": 1, "op": "error", "code": "proof_invalid", "message": "register.proof mismatch"
    }))
    .await;
    let rejected_at = Instant::now();
    assert!(peer.drain_until_eof(secs(10)).await.is_empty());
    let scheduled = rig
        .events
        .wait("the scheduled retry", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    assert_eq!(
        scheduled,
        Event::RetryScheduled {
            delay: ms(400),
            fixed: true,
            reason: RetryReason::Server(ServerError::ProofInvalid)
        }
    );
    assert!(!rig.health.registered());
    // 固定等待期间没有新连接，也没有退出请求。
    assert!(
        server.try_accept(ms(250)).await.is_none(),
        "proof_invalid means a fixed wait, not an immediate reconnect"
    );
    assert!(still_pending(&registration, ms(10)).await);
    // 等待结束之后重连，这次握手正常。
    let mut again = server.accept().await;
    assert!(
        rejected_at.elapsed() >= ms(350),
        "the next attempt came too early: {:?}",
        rejected_at.elapsed()
    );
    again.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    registration.finish().await;
}

// ---------------------------------------------------------------------------------------------------------------
// 严格的帧
// ---------------------------------------------------------------------------------------------------------------

/// 会话里服务端发了不合规的帧：超长（> 16384 字节）、非 UTF-8、空行、非对象、`v` 不对、缺 `op`、必填字段形状不对。
/// 客户端发 `error bad_frame` 并关闭连接，`registered` 变 false，然后重连。
#[tokio::test]
async fn a_malformed_frame_in_a_session_gets_bad_frame_and_a_reconnect() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut oversized = vec![b'x'; 16_385];
    oversized.push(b'\n');
    // 刚好 16384 字节（含 LF）的合规帧不算超长：它是一个未知 op 加填充，必须被忽略而不是被拒绝。
    let mut at_limit = b"{\"v\":1,\"op\":\"future\",\"pad\":\"".to_vec();
    let pad = 16_384 - at_limit.len() - "\"}\n".len();
    at_limit.extend(std::iter::repeat_n(b'p', pad));
    at_limit.extend_from_slice(b"\"}\n");
    assert_eq!(at_limit.len(), 16_384);
    // 比上限多 1 字节（含 LF 共 16385）的合规 JSON 帧：超长，必须按 bad_frame 拒绝。
    let mut one_over = at_limit.clone();
    one_over.insert(one_over.len() - 3, b'p');
    assert_eq!(one_over.len(), 16_385);

    let bad: Vec<(&str, Vec<u8>)> = vec![
        ("an oversized frame", oversized),
        ("a well-formed frame one byte over the limit", one_over),
        (
            "a frame without any LF within 16384 bytes",
            vec![b'y'; 20_000],
        ),
        (
            "invalid UTF-8",
            b"{\"v\":1,\"op\":\"x\",\"z\":\"\xff\xfe\"}\n".to_vec(),
        ),
        ("an empty line", b"\n".to_vec()),
        ("a JSON array", b"[1,2]\n".to_vec()),
        (
            "v=2",
            b"{\"v\":2,\"op\":\"heartbeat_ack\",\"seq\":1}\n".to_vec(),
        ),
        ("no v", b"{\"op\":\"heartbeat_ack\",\"seq\":1}\n".to_vec()),
        ("no op", b"{\"v\":1}\n".to_vec()),
        ("a CR", b"{\"v\":1,\"op\":\"future\"}\r\n".to_vec()),
        (
            "a BOM",
            b"\xEF\xBB\xBF{\"v\":1,\"op\":\"future\"}\n".to_vec(),
        ),
        (
            "a non-numeric seq",
            b"{\"v\":1,\"op\":\"heartbeat_ack\",\"seq\":\"1\"}\n".to_vec(),
        ),
    ];
    for (label, bytes) in bad {
        let mut peer = server.accept().await;
        peer.handshake(&rig.nonce_hex, Some(50)).await;
        rig.wait_registered(true).await;
        // 先确认这个会话是好的：能收到心跳。
        assert_eq!(peer.recv().await["op"], "heartbeat", "{label}");
        peer.try_send_raw(&bytes).await;
        // 之后可能还有几个在途的心跳，最后才是 error 与关闭。
        let frames = peer.drain_until_eof(secs(10)).await;
        let errors: Vec<Value> = frames
            .iter()
            .map(|frame| serde_json::from_slice::<Value>(frame).unwrap())
            .filter(|frame| frame["op"] == "error")
            .collect();
        assert_eq!(errors.len(), 1, "{label}: {frames:?}");
        assert_eq!(errors[0]["code"], "bad_frame", "{label}");
        rig.wait_registered(false).await;
    }
    // 一个刚好 16384 字节（含 LF）的未知 op 帧被接受（然后被忽略），会话不受影响；前向兼容的未知字段同理。
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(50)).await;
    rig.wait_registered(true).await;
    peer.send_raw(&at_limit).await;
    peer.send(&json!({"v": 1, "op": "future_op", "x": [1, 2, 3]}))
        .await;
    peer.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": 1, "unknown_field": {"a": 1}}))
        .await;
    // 重复的 `challenge`、`ack` 在会话里被忽略。
    peer.send(&json!({"v": 1, "op": "challenge", "conn_id": VECTOR_CONN_ID, "nonce": VECTOR_CHALLENGE_NONCE}))
        .await;
    peer.send(
        &json!({"v": 1, "op": "ack", "session_id": VECTOR_SESSION_ID, "proof": VECTOR_ACK_PROOF}),
    )
    .await;
    peer.serve_heartbeats_until(Instant::now() + ms(400)).await;
    assert!(
        rig.health.registered(),
        "tolerated frames must not end the session"
    );
    registration.finish().await;
}

/// `ack` 里的未知字段被忽略、`heartbeat_ms` 缺省走默认值：前向兼容。
#[tokio::test]
async fn an_ack_with_unknown_fields_and_no_heartbeat_interval_is_accepted() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(Timings {
        heartbeat_default: ms(30),
        ..fast()
    });
    let mut peer = server.accept().await;
    let (conn_id, nonce, session_id) = fresh_ids();
    peer.challenge(&conn_id, &nonce).await;
    peer.send(&json!({
        "v": 1, "op": "ack", "session_id": session_id,
        "proof": expected_ack_proof(&rig.nonce_hex, &conn_id, &nonce, &session_id),
        "from_the_future": {"x": true}
    }))
    .await;
    rig.wait_registered(true).await;
    // 缺省的间隔（这里缩短成 30ms）：很快就有第二个心跳。
    assert_eq!(peer.recv().await["seq"], 1);
    peer.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": 1}))
        .await;
    assert_eq!(peer.recv().await["seq"], 2);
    registration.finish().await;
}

// ---------------------------------------------------------------------------------------------------------------
// 心跳、失联、重连
// ---------------------------------------------------------------------------------------------------------------

/// 心跳 `seq` 从 1 起每次加 1；`phase`、`ready` 取自当下的 `/health` 状态。
#[tokio::test]
async fn heartbeats_count_up_from_one_and_carry_the_live_phase_and_ready() {
    let rig = Rig::with(
        Identity {
            instance_id: VECTOR_INSTANCE_ID.to_string(),
            pid: 1,
            version: "0.6.0".to_string(),
            contract_version: "1.3.0".to_string(),
            data_port: 1,
            admin_port: 2,
            started_at_ms: 1,
        },
        &"c3".repeat(32),
        vec![starting_upstream()],
    );
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    let handshaken = peer.handshake(&rig.nonce_hex, Some(20)).await;
    // register 里的 phase 是当时的 /health.phase。
    assert_eq!(handshaken.register["phase"], "starting");
    rig.wait_registered(true).await;

    let mut last_seq = 0;
    let mut saw_starting = false;
    let mut flipped = false;
    for _ in 0..200 {
        let beat = peer.recv().await;
        assert_eq!(beat["v"], 1);
        assert_eq!(beat["op"], "heartbeat");
        let seq = beat["seq"].as_u64().unwrap();
        assert_eq!(seq, last_seq + 1, "seq counts up by exactly one");
        last_seq = seq;
        let phase = beat["phase"].as_str().unwrap();
        let ready = beat["ready"].as_bool().unwrap();
        assert_eq!(
            ready,
            phase == "ready",
            "ready equals phase == ready: {beat}"
        );
        if phase == "starting" {
            assert!(!flipped, "phase went back to starting");
            saw_starting = true;
        }
        if seq == 3 && !flipped {
            // 上游就绪了：之后的心跳报 ready。
            rig.health.apply("omlx", Observation::Ready);
            flipped = true;
        }
        peer.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": seq}))
            .await;
        if flipped && phase == "ready" {
            break;
        }
    }
    assert!(
        saw_starting,
        "the first heartbeats report the starting phase"
    );
    assert!(flipped && last_seq >= 4);
    registration.finish().await;
}

/// 连续 3 次心跳没有应答：客户端关闭连接、`registered` 变 false、重连重新握手，`resume` 为 true。
#[tokio::test]
async fn three_unanswered_heartbeats_drop_the_session_and_the_client_reconnects() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(Timings {
        heartbeat_ack_wait: ms(80),
        ..fast()
    });
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    // 服务端读心跳但从不应答：客户端必须自己发现失联并关闭连接。
    let frames = peer.drain_until_eof(secs(10)).await;
    let beats = frames
        .iter()
        .map(|frame| serde_json::from_slice::<Value>(frame).unwrap())
        .filter(|frame| frame["op"] == "heartbeat")
        .count();
    assert!(
        beats >= 3,
        "it takes at least three unanswered heartbeats to give up, saw {beats}"
    );
    rig.wait_registered(false).await;
    assert_eq!(
        rig.events.count(|e| matches!(
            e,
            Event::SessionLost {
                reason: RetryReason::HeartbeatLost
            }
        )),
        1
    );
    // 重连并重新握手；这是同一个进程的第二次登记，`resume` 为 true。
    let mut second = server.accept().await;
    let handshaken = second.handshake(&rig.nonce_hex, Some(20)).await;
    assert_eq!(handshaken.register["resume"], true);
    rig.wait_registered(true).await;
    registration.finish().await;
}

/// 应答了一部分（每隔一个）的服务端不算失联：任何一次应答都把「连续未应答」清零；累积的应答代表更早的心跳也到了。
#[tokio::test]
async fn a_server_that_answers_only_some_heartbeats_is_not_dropped() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(Timings {
        heartbeat_ack_wait: ms(500),
        ..fast()
    });
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    let mut answered = 0;
    for _ in 0..40 {
        let beat = peer.recv().await;
        let seq = beat["seq"].as_u64().unwrap();
        // 只应答奇数 seq：连续未应答最多 1 次，永远到不了 3 次。
        if seq % 2 == 1 {
            peer.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": seq}))
                .await;
            answered += 1;
        }
    }
    assert!(answered >= 20);
    assert!(rig.health.registered());
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::SessionLost { .. })),
        0
    );
    registration.finish().await;
}

/// 过期的、重复的、没发过的 `seq` 的应答不算数：只有比已确认的更新、且我们发过的 `seq` 才算活着。
#[tokio::test]
async fn stale_duplicate_and_unsent_heartbeat_acks_do_not_keep_a_session_alive() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(Timings {
        heartbeat_ack_wait: ms(80),
        ..fast()
    });
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    // 只应答一次 seq=1，之后不停地重复它，并夹着一个没发过的 seq。
    assert_eq!(peer.recv().await["seq"], 1);
    peer.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": 1}))
        .await;
    let started = Instant::now();
    let mut closed = false;
    while started.elapsed() < secs(10) {
        peer.try_send_raw(b"{\"v\":1,\"op\":\"heartbeat_ack\",\"seq\":1}\n")
            .await;
        peer.try_send_raw(b"{\"v\":1,\"op\":\"heartbeat_ack\",\"seq\":999999}\n")
            .await;
        peer.try_send_raw(b"{\"v\":1,\"op\":\"heartbeat_ack\",\"seq\":0}\n")
            .await;
        match peer.read_line(ms(20)).await {
            Got::Eof => {
                closed = true;
                break;
            }
            Got::Line(_) | Got::Timeout => {}
        }
    }
    assert!(
        closed,
        "a session kept alive only by stale acks must be dropped"
    );
    rig.wait_registered(false).await;
    registration.finish().await;
}

/// 服务端关闭连接（EOF）：客户端立即重连（退避的第一次是立即），`registered` 在两次会话之间是 false；
/// 服务端「重启」（监听没了、socket 文件没了，之后又起来）：客户端按退避重试，起来之后重新握手，新 `session_id`，`resume` 为 true。
#[tokio::test]
async fn a_closed_connection_is_retried_at_once_and_a_restarted_server_gets_a_fresh_handshake() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());

    let mut first = server.accept().await;
    let first_session = first.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    assert_eq!(first_session.register["resume"], false);

    // 服务端关掉连接（EOF）：客户端立即重连，新会话。
    let before = rig.events.snapshot().len();
    drop(first);
    rig.wait_registered(false).await;
    let lost = rig
        .events
        .wait_after(before, "the session loss", |e| {
            matches!(e, Event::SessionLost { .. })
        })
        .await;
    // 对端关闭连接有两种合法的观察方式：读到 EOF（`Disconnected`），或恰好到点的心跳先写到了已关闭的 socket
    // （`WriteFailed`；会话循环里 `Beat` 排在读之前，谁先被看到取决于调度，不取决于被测行为）。两者都走同一条
    // 「首次立即重连」的计划；除此之外的原因（心跳超时、IO 错误、握手类原因）仍然不被接受。
    assert!(
        matches!(
            lost,
            Event::SessionLost {
                reason: RetryReason::Disconnected | RetryReason::WriteFailed
            }
        ),
        "a closed connection ends the session as disconnected or write_failed: {lost:?}"
    );
    let scheduled = rig
        .events
        .wait_after(before, "the retry after EOF", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    assert!(
        matches!(scheduled, Event::RetryScheduled { delay, fixed: false, .. } if delay.is_zero()),
        "the first reconnect is immediate: {scheduled:?}"
    );
    let mut second = server.accept().await;
    let second_session = second.handshake(&rig.nonce_hex, Some(20)).await;
    assert_ne!(first_session.session_id, second_session.session_id);
    assert_eq!(second_session.register["resume"], true);
    rig.wait_registered(true).await;

    // 服务端整个停掉：连接断开，之后连不上。
    drop(second);
    server.shut_down();
    rig.wait_registered(false).await;
    let failed_before = rig
        .events
        .count(|e| matches!(e, Event::AttemptFailed { .. }));
    wait_until("three failed reconnect attempts", secs(10), || {
        rig.events.count(|e| {
            matches!(
                e,
                Event::AttemptFailed {
                    reason: RetryReason::ConnectFailed,
                    ..
                }
            )
        }) >= failed_before + 3
    })
    .await;
    assert!(!rig.health.registered());
    assert!(
        still_pending(&registration, ms(50)).await,
        "an absent agent24d never makes iDoris exit by itself"
    );

    // agent24d 被重启（launchd 拉起）：同一个路径上又有了服务端，客户端重新握手，新的 session_id。
    let restarted = rig.server();
    let mut third = restarted.accept().await;
    let third_session = third.handshake(&rig.nonce_hex, Some(20)).await;
    assert_eq!(third_session.register["resume"], true);
    assert!(
        ![&first_session.session_id, &second_session.session_id]
            .contains(&&third_session.session_id)
    );
    rig.wait_registered(true).await;
    registration.finish().await;
}

// ---------------------------------------------------------------------------------------------------------------
// 退避序列
// ---------------------------------------------------------------------------------------------------------------

/// 没有人在监听：安排的等待依次是第 1 次立即、0.25、0.5、1、2、4、8 秒，再之后固定 10 秒（缩短比例：
/// 0、5、10、20、40、80、160、200 毫秒；抖动关闭所以精确）。每一次失败的原因都是 `connect_failed`，连续失败数递增。
#[tokio::test]
async fn with_nobody_listening_the_scheduled_waits_follow_the_specified_sequence() {
    let rig = Rig::new();
    let registration = rig.start(fast());
    wait_until("ten scheduled retries", secs(20), || {
        rig.events.retry_delays().len() >= 10
    })
    .await;
    let delays = rig.events.retry_delays();
    assert_eq!(
        delays[..10],
        [0, 5, 10, 20, 40, 80, 160, 200, 200, 200].map(ms)
    );
    let failures: Vec<(RetryReason, u64)> = rig
        .events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            Event::AttemptFailed {
                reason,
                consecutive,
            } => Some((reason, consecutive)),
            _ => None,
        })
        .collect();
    for (index, (reason, consecutive)) in failures.iter().take(10).enumerate() {
        assert_eq!(*reason, RetryReason::ConnectFailed);
        assert_eq!(*consecutive, index as u64 + 1);
    }
    assert!(!rig.health.registered());
    assert!(still_pending(&registration, ms(50)).await);
    registration.finish().await;
}

/// 带抖动时每个等待都在名义值的 ±20% 之内（第 1 次立即不会被抖动成非零）。
#[tokio::test]
async fn with_jitter_every_scheduled_wait_stays_within_20_percent() {
    let rig = Rig::new();
    let timings = Timings {
        jitter: JitterSource::System,
        ..fast()
    };
    let registration = rig.start(timings.clone());
    wait_until("ten scheduled retries", secs(20), || {
        rig.events.retry_delays().len() >= 10
    })
    .await;
    let delays = rig.events.retry_delays();
    for (index, delay) in delays.iter().take(10).enumerate() {
        let nominal = timings
            .backoff_steps
            .get(index)
            .copied()
            .unwrap_or(timings.backoff_cap);
        if nominal.is_zero() {
            assert_eq!(*delay, Duration::ZERO);
        } else {
            assert!(
                *delay >= nominal.mul_f64(0.8) && *delay <= nominal.mul_f64(1.2),
                "retry {index}: {delay:?} vs nominal {nominal:?}"
            );
        }
    }
    registration.finish().await;
}

/// 会话持续满 `backoff_reset_after` 才清零：一个很快就断开的会话不清零（等待继续往上数，不是每次立即重连），
/// 一个持续足够久的会话断开之后，下一次又是立即。
#[tokio::test]
async fn the_backoff_resets_only_after_a_session_has_lasted_long_enough() {
    let rig = Rig::new();
    let timings = Timings {
        backoff_reset_after: ms(1_500),
        ..fast()
    };
    let registration = rig.start(timings.clone());
    // 先让服务端不在，攒下几次失败（n >= 1）。
    rig.events
        .wait("a first scheduled retry", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    let server = rig.server();

    // 第一个会话很短（远小于 1.5 秒）就被服务端关掉。
    let mut first = server.accept().await;
    first.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    let retries_before_first_session = rig
        .events
        .snapshot()
        .iter()
        .take_while(|event| !matches!(event, Event::Registered { .. }))
        .filter(|event| matches!(event, Event::RetryScheduled { .. }))
        .count();
    assert!(retries_before_first_session >= 1);
    let at = rig.events.snapshot().len();
    drop(first);
    let after_short = rig
        .events
        .wait_after(at, "the retry after the short session", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    // 不清零：等待是名义序列里的下一项（已经安排过 n 次，所以这是第 n+1 项），不是 0。
    let nominal = |index: usize| {
        timings
            .backoff_steps
            .get(index)
            .copied()
            .unwrap_or(timings.backoff_cap)
    };
    let expected = nominal(retries_before_first_session);
    assert!(!expected.is_zero());
    assert!(
        matches!(after_short, Event::RetryScheduled { delay, fixed: false, .. } if delay == expected),
        "a short session must not reset the backoff: expected {expected:?}, got {after_short:?}"
    );

    // 第二个会话持续超过 1.5 秒，再被关掉：清零，下一次立即。
    let mut second = server.accept().await;
    second.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    second
        .serve_heartbeats_until(Instant::now() + ms(1_800))
        .await;
    let at = rig.events.snapshot().len();
    drop(second);
    let after_long = rig
        .events
        .wait_after(at, "the retry after the long session", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    assert!(
        matches!(after_long, Event::RetryScheduled { delay, fixed: false, .. } if delay.is_zero()),
        "a session that lasted at least the reset period resets the backoff: {after_long:?}"
    );
    registration.finish().await;
}

/// 登记失败的日志：只记状态变化，以及首次失败和之后每第 10 次失败，不刷屏。
#[tokio::test]
async fn failure_logs_are_throttled_to_the_first_and_every_tenth_failure() {
    let rig = Rig::new();
    let registration = rig.start(Timings {
        backoff_steps: [Duration::ZERO, ms(1), ms(1), ms(1), ms(1), ms(1), ms(1)],
        backoff_cap: ms(1),
        ..fast()
    });
    wait_until("31 failed attempts", secs(30), || {
        rig.events
            .count(|e| matches!(e, Event::AttemptFailed { .. }))
            >= 31
    })
    .await;
    registration.finish().await;
    let numbers: Vec<u64> = rig
        .logs
        .lines()
        .iter()
        .filter(|line| line.contains("登记失败"))
        .map(|line| {
            let rest = line.split("连续第 ").nth(1).unwrap();
            rest.split(' ').next().unwrap().parse().unwrap()
        })
        .collect();
    // 失败 1、10、20、30 各一行（可能在 31 之后 `finish` 之前又多出几次失败，但不会多出别的编号）。
    assert!(numbers.starts_with(&[1, 10, 20, 30]), "{numbers:?}");
    for number in &numbers {
        assert!(*number == 1 || number.is_multiple_of(10), "{numbers:?}");
    }
    // 没有一行带路径或任何对端文本。
    for line in rig.logs.lines() {
        assert!(line.starts_with("[idoris] "), "{line}");
        assert!(!line.contains("r.sock"), "{line}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// instance_conflict
// ---------------------------------------------------------------------------------------------------------------

/// agent24d 以 `instance_conflict` 拒绝（`register` 之后、`ack` 之前）：客户端不重连，要求 iDoris 以退出码 3 停止，不发 `bye`
/// （`bye` 只在信号停止与退出码 5 的停止时发，规范 §2.10.10 第 10 条）。`ack` 之后的会话里收到的情形见
/// `an_instance_conflict_inside_an_acked_session_also_requests_exit_3_without_bye_or_reconnect`。
#[tokio::test]
async fn instance_conflict_requests_exit_3_and_the_client_never_reconnects_or_says_bye() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    assert_eq!(register["op"], "register");
    peer.send(&json!({
        "v": 1, "op": "error", "code": "instance_conflict",
        "message": "another healthy instance is registered"
    }))
    .await;

    let cause = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("instance_conflict must request a stop");
    assert_eq!(cause, StopCause::InstanceConflict);
    assert_eq!(cause.exit_code(), 3);
    // 已经发布的原因在第一次被 poll 时就就绪，不用等下一次变化（见孤儿超时那条同样的断言）。
    assert_eq!(
        futures_util::FutureExt::now_or_never(registration.wants_stop()),
        Some(StopCause::InstanceConflict)
    );
    // 连接被关闭，而且之后什么也没写（没有 ack 的会话，所以没有 bye）。
    let frames = peer.drain_until_eof(secs(10)).await;
    assert!(frames.is_empty(), "{frames:?}");
    // 不重连：之后再也没有新的连接（等的时间远超过所有退避）。
    assert!(
        server.try_accept(ms(700)).await.is_none(),
        "instance_conflict means: do not reconnect"
    );
    assert!(!rig.health.registered());
    assert_eq!(
        rig.events
            .count(|e| matches!(e, Event::RetryScheduled { .. })),
        0,
        "no retry is ever scheduled after instance_conflict"
    );
    assert_eq!(
        rig.events
            .count(|e| matches!(e, Event::ExitRequested(StopCause::InstanceConflict))),
        1
    );
    registration.finish().await;
}

/// 规范勘误十二（1）（H4）：登记任务已经**发布**了停止原因（孤儿超时 4 / `instance_conflict` 3），同一轮里信号也就绪：停止原因是登记的
/// 那个，不是信号（此前无 `biased` 的 `select!` 随机挑，信号能把它们盖成退出码 0）。真的登记客户端、真的发布出来的原因、
/// 真的 `StopSources`；每次结果都一样（随机挑的话，64 次里总有几次挑中信号）。
#[tokio::test]
async fn a_published_registration_cause_outranks_a_signal_that_is_ready_in_the_same_round() {
    use idoris_router::stop::{StopSources, StopVerdict};

    async fn verdict_with_a_ready_signal(registration: &Registration) -> StopVerdict {
        StopSources {
            engine_failed: std::future::pending::<()>(),
            registration: registration.wants_stop(),
            gate_failed: std::future::pending::<String>(),
            signal: std::future::ready(()),
        }
        .first()
        .await
    }

    // 孤儿超时（4）：没有任何会话，超时之后发布。
    let rig = Rig::new();
    let registration = rig.start_with(StartOptions {
        orphan: Some(ms(100)),
        ..StartOptions::new(fast())
    });
    // 等它发布（按事件等，不断言墙钟）；发布之后信号才「到」。
    let published = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("the orphan timeout must fire");
    assert_eq!(published, StopCause::OrphanTimeout);
    for round in 0..64 {
        assert_eq!(
            verdict_with_a_ready_signal(&registration).await,
            StopVerdict::Stop(StopCause::OrphanTimeout),
            "round {round}"
        );
    }
    registration.finish().await;

    // `instance_conflict`（3）：agent24d 在 `register` 之后拒绝。
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    assert_eq!(register["op"], "register");
    peer.send(&json!({
        "v": 1, "op": "error", "code": "instance_conflict",
        "message": "another healthy instance is registered"
    }))
    .await;
    let published = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("instance_conflict must request a stop");
    assert_eq!(published, StopCause::InstanceConflict);
    for round in 0..64 {
        assert_eq!(
            verdict_with_a_ready_signal(&registration).await,
            StopVerdict::Stop(StopCause::InstanceConflict),
            "round {round}"
        );
    }
    registration.finish().await;
}

/// `instance_conflict` 是对 `register` 的回答；还没发 `register`（challenge 之前）就收到它，按普通错误处理，
/// 不因此让 iDoris 退出（规范没写清，取更保守的一边：不让一个无关的早到帧终止进程）。
#[tokio::test]
async fn an_instance_conflict_that_arrives_before_register_is_not_a_reason_to_exit() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    peer.send(&json!({"v": 1, "op": "error", "code": "instance_conflict", "message": "early"}))
        .await;
    assert!(peer.drain_until_eof(secs(10)).await.is_empty());
    assert!(still_pending(&registration, ms(200)).await);
    // 客户端照常重连。
    let mut again = server.accept().await;
    again.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    registration.finish().await;
}

/// 收到 `instance_conflict` 之后不再自愈重写发现文件（规范 §2.7.3、§2.10.4）：自愈在冲突的那一刻立即停止，
/// 即使停止流程（排空等）还要很久。做法：冲突之前自愈在工作（对照：删掉的文件会被写回）；冲突之后（等在途的那一次
/// 重写落地）再删一次文件，自愈每 20ms 一轮，之后再也不会把它写回来。
#[tokio::test]
async fn instance_conflict_stops_the_discovery_self_heal_for_good() {
    let rig = Rig::new();
    let server = rig.server();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let original = fs::read(&path).unwrap();
    let registration = rig.start_with(StartOptions {
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(Timings {
            self_heal_interval: ms(20),
            ..fast()
        })
    });
    // 对照：冲突之前自愈在工作。
    fs::remove_file(&path).unwrap();
    wait_for_file(&path, &original).await;

    // 握手走到 register，然后冲突。
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    peer.challenge(&conn_id, &nonce).await;
    peer.send(&json!({"v": 1, "op": "error", "code": "instance_conflict", "message": "x"}))
        .await;
    assert_eq!(
        tokio::time::timeout(secs(10), registration.wants_stop())
            .await
            .unwrap(),
        StopCause::InstanceConflict
    );
    // 给已经在途的那一次重写（如果有）一点时间落地，再删文件。
    tokio::time::sleep(ms(150)).await;
    let _ = fs::remove_file(&path);
    tokio::time::sleep(ms(700)).await;
    assert!(
        !path.exists(),
        "the self-heal must not rewrite the file after instance_conflict (it ticks every 20 ms)"
    );
    registration.finish().await;
}

/// 会话中途（`ack` 之后）收到 `instance_conflict`：规范 §2.10.10 第 5 条是「发出 `register` 之后收到就令进程退出」，会话里收到
/// 也一样：退出码 3、不重连、不再自愈重写发现文件；`bye` 只在信号停止与退出码 5 的停止时发（第 10 条），所以这里也不发——
/// 连接关闭之前只可能有在途的心跳。
#[tokio::test]
async fn an_instance_conflict_inside_an_acked_session_also_requests_exit_3_without_bye_or_reconnect()
 {
    let rig = Rig::new();
    let server = rig.server();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let original = fs::read(&path).unwrap();
    let registration = rig.start_with(StartOptions {
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(Timings {
            self_heal_interval: ms(20),
            ..fast()
        })
    });
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(2_000)).await;
    rig.wait_registered(true).await;
    // 对照：会话建立之后自愈在工作。
    fs::remove_file(&path).unwrap();
    wait_for_file(&path, &original).await;

    peer.send(&json!({"v": 1, "op": "error", "code": "instance_conflict", "message": "x"}))
        .await;
    let cause = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("an instance_conflict inside a session must request a stop");
    assert_eq!(cause, StopCause::InstanceConflict);
    assert_eq!(cause.exit_code(), 3);
    // 连接被关闭；关闭之前最多有在途的心跳，没有 `bye`，也没有别的帧。
    let frames = peer.drain_until_eof(secs(10)).await;
    for frame in &frames {
        let frame: Value = serde_json::from_slice(frame).unwrap();
        assert_eq!(
            frame["op"], "heartbeat",
            "only heartbeats may precede the close"
        );
    }
    // 不重连，也没有安排任何重连。
    assert!(
        server.try_accept(ms(700)).await.is_none(),
        "instance_conflict means: do not reconnect"
    );
    assert!(!rig.health.registered());
    assert_eq!(
        rig.events
            .count(|e| matches!(e, Event::RetryScheduled { .. })),
        0,
        "no retry is ever scheduled after instance_conflict"
    );
    assert_eq!(
        rig.events
            .count(|e| matches!(e, Event::ExitRequested(StopCause::InstanceConflict))),
        1
    );
    // 自愈在冲突的那一刻就停了：之后删掉的文件不会再被写回来（它每 20 毫秒一轮）。
    tokio::time::sleep(ms(150)).await;
    let _ = fs::remove_file(&path);
    tokio::time::sleep(ms(700)).await;
    assert!(
        !path.exists(),
        "the self-heal must not rewrite the file after instance_conflict"
    );
    registration.finish().await;
}

/// 日志不刷屏：一次误删只记一行；两个实例共用一个路径、各自的自愈互相覆盖（这里用一个后台线程不停地删文件来模拟）时，
/// 连续的重写只记第 1、10、20… 次。
#[tokio::test]
async fn repeated_self_heal_rewrites_are_logged_for_the_first_and_every_tenth_time_only() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(Timings {
            self_heal_interval: ms(20),
            ..fast()
        })
    });
    let stop = Arc::new(AtomicUsize::new(0));
    let deleter = {
        let (path, stop) = (path.clone(), stop.clone());
        std::thread::spawn(move || {
            while stop.load(Ordering::SeqCst) == 0 {
                let _ = fs::remove_file(&path);
                std::thread::sleep(ms(1));
            }
        })
    };
    let rewrite_lines = {
        let logs = rig.logs.clone();
        move || {
            logs.lines()
                .into_iter()
                .filter(|line| line.contains("已按原子发布重写"))
                .collect::<Vec<_>>()
        }
    };
    wait_until("ten consecutive rewrites to be logged", secs(60), || {
        rewrite_lines()
            .iter()
            .any(|line| line.contains("连续第 10 次"))
    })
    .await;
    stop.store(1, Ordering::SeqCst);
    deleter.join().unwrap();
    registration.finish().await;

    let lines = rewrite_lines();
    // 按连续段校验：自愈核对到文件完好就把计数清零（生产语义），负载高时删除线程可能被拖过一轮，
    // 所以日志里会出现多段。每条不带次数的行开启新的一段；段内后续行的次数必须依次是 10、20、30…
    assert!(!lines.is_empty(), "{lines:?}");
    assert!(!lines[0].contains("连续第"), "{lines:?}");
    let mut expected_next: u64 = 10;
    let mut longest_run: u64 = 1;
    for line in &lines {
        match line.split("连续第 ").nth(1) {
            None => {
                expected_next = 10;
            }
            Some(rest) => {
                let number: u64 = rest
                    .split(' ')
                    .next()
                    .and_then(|number| number.parse().ok())
                    .unwrap_or_else(|| panic!("unparsable repeat count: {line}"));
                assert!(number.is_multiple_of(10), "{lines:?}");
                assert_eq!(number, expected_next, "{lines:?}");
                longest_run = number;
                expected_next += 10;
            }
        }
    }
    assert!(longest_run >= 10, "{lines:?}");
    // 路径与 nonce 不在日志里。
    rig.logs
        .assert_no_secret(&[rig.nonce_hex.as_str(), "idoris.json"]);
}

/// 确定性：核对到文件完好就把连续计数清零。删一次、等它被重写，再等若干轮（至少一轮核对到完好），再删一次：
/// 两次重写都是各自的“第 1 次”，所以是两条不带次数的行（不清零的话第二次是第 2 次，根本不记）。
#[tokio::test]
async fn self_heal_rewrite_count_resets_after_an_intact_check() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(Timings {
            self_heal_interval: ms(20),
            ..fast()
        })
    });
    let rewrite_count = {
        let logs = rig.logs.clone();
        move || {
            logs.lines()
                .into_iter()
                .filter(|line| line.contains("已按原子发布重写"))
                .count()
        }
    };
    fs::remove_file(&path).unwrap();
    wait_until("the first rewrite to be logged", secs(30), || {
        rewrite_count() == 1
    })
    .await;
    // 等 10 轮自愈间隔：其间必有核对到完好的一轮。
    tokio::time::sleep(ms(200)).await;
    fs::remove_file(&path).unwrap();
    wait_until("the second rewrite to be logged", secs(30), || {
        rewrite_count() == 2
    })
    .await;
    registration.finish().await;
    let lines: Vec<String> = rig
        .logs
        .lines()
        .into_iter()
        .filter(|line| line.contains("已按原子发布重写"))
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().all(|l| !l.contains("连续第")), "{lines:?}");
}

fn discovery_for(rig: &Rig) -> (Arc<DiscoveryTarget>, Arc<DiscoveryRecord>) {
    let target = Arc::new(DiscoveryTarget::open(&rig.dir_path.join("idoris.json")).unwrap());
    let record = Arc::new(DiscoveryRecord {
        instance_id: rig.identity.instance_id.clone(),
        instance_nonce: InstanceNonce::from_hex(&rig.nonce_hex).unwrap(),
        pid: rig.identity.pid,
        data_port: rig.identity.data_port,
        admin_port: rig.identity.admin_port,
        version: rig.identity.version.clone(),
        contract_version: rig.identity.contract_version.clone(),
        started_at_ms: rig.identity.started_at_ms,
        launch: None,
    });
    (target, record)
}

// ---------------------------------------------------------------------------------------------------------------
// 孤儿计时
// ---------------------------------------------------------------------------------------------------------------

/// 从来没有登记上（包含进程启动后一直没登记上的情形）：超过 `orphan_timeout_s` 要求停止，退出码 4。
#[tokio::test]
async fn without_any_session_the_orphan_timeout_requests_exit_4() {
    let rig = Rig::new();
    let started = Instant::now();
    let registration = rig.start_with(StartOptions {
        orphan: Some(ms(300)),
        ..StartOptions::new(fast())
    });
    let cause = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("the orphan timeout must fire");
    assert_eq!(cause, StopCause::OrphanTimeout);
    assert_eq!(cause.exit_code(), 4);
    assert!(
        started.elapsed() >= ms(280),
        "it must not fire early: {:?}",
        started.elapsed()
    );
    // 已经发布的原因在第一次被 poll 时就就绪，不用等下一次变化：停止流程（`stop::StopSources`，规范勘误十二）靠这一点让已经成立的
    // 退出原因排在同一轮到达的信号前面。
    assert_eq!(
        futures_util::FutureExt::now_or_never(registration.wants_stop()),
        Some(StopCause::OrphanTimeout)
    );
    registration.finish().await;
}

/// 有已 `ack` 的会话时不超时：会话建立之后的计时清零，会话在的时候不计时；会话结束的这一刻才重新开始计
/// （不是从进程启动算）：会话比超时更长，服务端关掉之后，也要再等一整个超时才退出。
#[tokio::test]
async fn a_live_session_never_times_out_and_the_clock_restarts_when_it_ends() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start_with(StartOptions {
        orphan: Some(ms(700)),
        ..StartOptions::new(fast())
    });
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    // 会话比超时（700ms）长得多：一直没有退出请求。
    peer.serve_heartbeats_until(Instant::now() + ms(1_800))
        .await;
    assert!(
        still_pending(&registration, ms(10)).await,
        "a registered iDoris is never an orphan"
    );
    assert!(rig.health.registered());

    // 服务端关掉会话并且不再监听。
    drop(peer);
    server.shut_down();
    let closed_at = Instant::now();
    rig.wait_registered(false).await;
    // 刚断开：计时刚刚重新开始，不能立刻就退（进程已经启动了 1.5 秒以上，从启动算的话早就该退了）。
    assert!(
        still_pending(&registration, ms(150)).await,
        "the orphan clock must restart when the session ends, not run from process start"
    );
    let cause = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("the orphan timeout must fire after the session is gone");
    assert_eq!(cause, StopCause::OrphanTimeout);
    assert!(
        closed_at.elapsed() >= ms(650),
        "a full timeout must pass after the session ended: {:?}",
        closed_at.elapsed()
    );
    registration.finish().await;
}

/// 超时之前握手成功：`ack` 把计时清零，之后即使过了原本的截止时间也不退出。
#[tokio::test]
async fn an_ack_before_the_deadline_clears_the_orphan_clock() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start_with(StartOptions {
        orphan: Some(ms(500)),
        ..StartOptions::new(fast())
    });
    let mut peer = server.accept().await;
    // 服务端拖到 250ms 才说话，ack 在大约 300ms；会话再保持到 1.2 秒（远过了原本的截止时间）。
    tokio::time::sleep(ms(250)).await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    peer.serve_heartbeats_until(Instant::now() + ms(1_200))
        .await;
    assert!(still_pending(&registration, ms(10)).await);
    registration.finish().await;
}

/// 孤儿计时在握手阶段也在走：服务端接受连接但永远不发 challenge，超时到了就要求停止，不等握手超时。
#[tokio::test]
async fn the_orphan_clock_runs_during_a_stalled_handshake() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start_with(StartOptions {
        orphan: Some(ms(300)),
        ..StartOptions::new(fast())
    });
    let mut peer = server.accept().await;
    assert!(matches!(peer.read_line(ms(100)).await, Got::Timeout));
    let cause = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("a stalled handshake does not stop the orphan clock");
    assert_eq!(cause, StopCause::OrphanTimeout);
    registration.finish().await;
}

// ---------------------------------------------------------------------------------------------------------------
// bye
// ---------------------------------------------------------------------------------------------------------------

/// 优雅停止（信号）时，在已建立的会话上尽力发 `bye`（`reason:"shutdown"`），之后关闭连接。
#[tokio::test]
async fn a_graceful_stop_says_bye_on_the_live_session() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    registration.begin_stop(StopCause::Signal);
    let frames = peer.drain_until_eof(secs(10)).await;
    let tail = frames
        .iter()
        .map(|frame| String::from_utf8(frame.clone()).unwrap())
        .collect::<Vec<_>>();
    // 之前可能还有几个在途的心跳，但 bye 在，而且是最后一个。
    assert_eq!(tail.last().map(String::as_str), Some(BYE_LINE), "{tail:?}");
    assert_eq!(
        tail.iter().filter(|line| line.as_str() == BYE_LINE).count(),
        1
    );
    registration.finish().await;
    assert!(!rig.health.registered());
    assert_eq!(rig.events.count(|e| matches!(e, Event::ByeSent)), 1);
    // 停止之后不再重连。
    assert!(server.try_accept(ms(300)).await.is_none());
}

/// 孤儿超时、`instance_conflict` 的停止不发 `bye`（规范 §2.10.10 第 10 条：只在信号停止与退出码 5 的停止、且有已 `ack` 的会话时发）；
/// 直接 `finish`（出错路径）也不发。
#[tokio::test]
async fn only_a_signal_stop_says_bye_and_only_when_a_session_exists() {
    // 没有会话的时候收到信号：什么都不发（这时连接可能还没建立好，或者在等 challenge）。
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    registration.begin_stop(StopCause::Signal);
    let frames = peer.drain_until_eof(secs(10)).await;
    assert!(frames.is_empty(), "no session yet, so no bye: {frames:?}");
    registration.finish().await;

    // 有会话，但停止的原因不是信号（孤儿超时之类，调用方不会在会话还在时这样停，这里直接验证指令的语义）。
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    registration.begin_stop(StopCause::OrphanTimeout);
    let frames = peer.drain_until_eof(secs(10)).await;
    assert!(
        frames
            .iter()
            .all(|frame| String::from_utf8_lossy(frame) != BYE_LINE),
        "{frames:?}"
    );
    registration.finish().await;

    // 出错路径：直接 finish，不发 bye。
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    registration.finish().await;
    let frames = peer.drain_until_eof(secs(10)).await;
    assert!(
        frames
            .iter()
            .all(|frame| String::from_utf8_lossy(frame) != BYE_LINE),
        "{frames:?}"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// set_admin_token
// ---------------------------------------------------------------------------------------------------------------

async fn admin_status(app: &axum::Router, secret: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .uri("/admin/api/v1/status")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// 会话内轮换 Admin token（规范 §2.10.6）：合规的 `set_admin_token` 得到 `admin_token_ack`，之后旧 token 401、新 token 200
/// （Admin router 没有重建，用的是同一个共享的槽）。
#[tokio::test]
async fn set_admin_token_replaces_the_token_the_old_one_stops_working_at_once() {
    let rig = Rig::new();
    let app = build_admin_app_with_token(AppState::default(), rig.admin_token.clone());
    let server = rig.server();
    let registration = rig.start(fast());
    assert_eq!(admin_status(&app, TOKEN).await, StatusCode::OK);
    assert_eq!(
        admin_status(&app, NEW_TOKEN).await,
        StatusCode::UNAUTHORIZED
    );

    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN}))
        .await;
    // 之间可能夹着心跳：找到 admin_token_ack。
    let ack = loop {
        let line = String::from_utf8(peer.recv_line().await).unwrap();
        if !line.contains("\"heartbeat\"") {
            break line;
        }
    };
    assert_eq!(ack, ADMIN_TOKEN_ACK_LINE);
    assert_eq!(
        admin_status(&app, TOKEN).await,
        StatusCode::UNAUTHORIZED,
        "the old token is dead"
    );
    assert_eq!(admin_status(&app, NEW_TOKEN).await, StatusCode::OK);
    assert!(
        rig.health.registered(),
        "accepting a token does not end the session"
    );
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::AdminTokenReplaced)),
        1
    );
    registration.finish().await;
    // token 不进日志。
    rig.logs
        .assert_no_secret(&[TOKEN, NEW_TOKEN, rig.nonce_hex.as_str()]);
}

/// 不合规的 token（不是 64 位小写 hex：大写、63 或 65 位、非 hex、空串、数字、null、缺字段）：不替换，旧 token 保持有效，
/// 先尽力发 `error bad_frame` 再关闭该会话。校验失败不消耗 10 秒的名额。
#[tokio::test]
async fn an_invalid_token_is_refused_the_old_one_stays_valid_and_the_session_is_closed() {
    let rig = Rig::new();
    let app = build_admin_app_with_token(AppState::default(), rig.admin_token.clone());
    let server = rig.server();
    let registration = rig.start(fast());
    let upper = NEW_TOKEN.to_uppercase();
    let sixty_three = &NEW_TOKEN[..63];
    let sixty_five = format!("{NEW_TOKEN}0");
    let non_hex = format!("{}g", &NEW_TOKEN[..63]);
    let with_space = format!(" {}", &NEW_TOKEN[..63]);
    let with_lf = format!("{}\n", &NEW_TOKEN[..63]);
    let bad: Vec<(&str, Value)> = vec![
        ("uppercase", json!({"token": upper})),
        ("63 chars", json!({"token": sixty_three})),
        ("65 chars", json!({"token": sixty_five})),
        ("non-hex", json!({"token": non_hex})),
        ("leading space", json!({"token": with_space})),
        ("trailing LF", json!({"token": with_lf})),
        ("empty", json!({"token": ""})),
        ("a number", json!({"token": 12345})),
        ("null", json!({"token": null})),
        ("missing", json!({})),
    ];
    for (label, fields) in bad {
        let mut peer = server.accept().await;
        peer.handshake(&rig.nonce_hex, Some(20)).await;
        rig.wait_registered(true).await;
        let mut frame = json!({"v": 1, "op": "set_admin_token"});
        for (key, value) in fields.as_object().unwrap() {
            frame[key] = value.clone();
        }
        peer.send(&frame).await;
        let frames = peer.drain_until_eof(secs(10)).await;
        let errors: Vec<Value> = frames
            .iter()
            .map(|frame| serde_json::from_slice::<Value>(frame).unwrap())
            .filter(|frame| frame["op"] != "heartbeat")
            .collect();
        assert_eq!(errors.len(), 1, "{label}: {frames:?}");
        assert_eq!(errors[0]["op"], "error", "{label}");
        assert_eq!(errors[0]["code"], "bad_frame", "{label}");
        assert_eq!(
            admin_status(&app, TOKEN).await,
            StatusCode::OK,
            "{label}: the old token must stay valid"
        );
        assert_eq!(
            admin_status(&app, NEW_TOKEN).await,
            StatusCode::UNAUTHORIZED,
            "{label}"
        );
        rig.wait_registered(false).await;
    }
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::AdminTokenReplaced)),
        0
    );
    assert_eq!(
        rig.events.count(|e| matches!(
            e,
            Event::AdminTokenRejected {
                reason: TokenRejection::Invalid
            }
        )),
        10
    );
    // 失败的校验没有消耗名额：紧接着一个合规的 token 立即被接受。
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN}))
        .await;
    wait_until("the token to be replaced", secs(10), || {
        rig.admin_token.matches(NEW_TOKEN)
    })
    .await;
    registration.finish().await;
    rig.logs
        .assert_no_secret(&[TOKEN, NEW_TOKEN, &NEW_TOKEN.to_uppercase()]);
}

/// 每 10 秒（缩短成 1.5 秒）至多接受一次：第二个合规的 token 在窗口内被拒（`error bad_frame`、会话关闭、token 保持第一次设的值），
/// 窗口过后再设就被接受。
#[tokio::test]
async fn a_second_token_inside_the_rate_window_is_refused_and_one_after_it_is_accepted() {
    let rig = Rig::new();
    let app = build_admin_app_with_token(AppState::default(), rig.admin_token.clone());
    let server = rig.server();
    let timings = Timings {
        admin_token_interval: ms(1_500),
        ..fast()
    };
    let window = timings.admin_token_interval;
    let registration = rig.start(timings);

    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN}))
        .await;
    wait_until("the first token", secs(10), || {
        rig.admin_token.matches(NEW_TOKEN)
    })
    .await;
    let accepted_at = Instant::now();

    // 立即再设一个：被拒。
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": OTHER_TOKEN}))
        .await;
    let frames = peer.drain_until_eof(secs(10)).await;
    let errors: Vec<Value> = frames
        .iter()
        .map(|frame| serde_json::from_slice::<Value>(frame).unwrap())
        .filter(|frame| frame["op"] == "error")
        .collect();
    assert_eq!(errors.len(), 1, "{frames:?}");
    assert_eq!(errors[0]["code"], "bad_frame");
    assert_eq!(
        admin_status(&app, NEW_TOKEN).await,
        StatusCode::OK,
        "the first new token stays"
    );
    assert_eq!(
        admin_status(&app, OTHER_TOKEN).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(admin_status(&app, TOKEN).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        rig.events.count(|e| matches!(
            e,
            Event::AdminTokenRejected {
                reason: TokenRejection::RateLimited
            }
        )),
        1
    );

    // 窗口过后（限制跨会话：它挡的是 token 的更换频率，不是某一条连接）。
    let mut again = server.accept().await;
    again.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    tokio::time::sleep(window.saturating_sub(accepted_at.elapsed()) + ms(50)).await;
    again
        .send(&json!({"v": 1, "op": "set_admin_token", "token": OTHER_TOKEN}))
        .await;
    wait_until("the token after the window", secs(10), || {
        rig.admin_token.matches(OTHER_TOKEN)
    })
    .await;
    assert_eq!(
        admin_status(&app, NEW_TOKEN).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(admin_status(&app, OTHER_TOKEN).await, StatusCode::OK);
    registration.finish().await;
}

/// 限频是进程级的（规范 §2.10.10 第 6 条）：每个进程每 10 秒（这里 30 秒，远长于测试）至多接受一次，跨会话、跨重连合计，
/// 重连不重置。第一个会话里设了 token 之后，服务端结束这个会话；agent24d「重连」得到的新会话里，窗口之内再设一个合规的 token，
/// 仍然被拒（`error bad_frame`、关闭该会话），token 保持第一次设的值。
#[tokio::test]
async fn the_token_rate_limit_counts_across_sessions_and_reconnects() {
    let rig = Rig::new();
    let app = build_admin_app_with_token(AppState::default(), rig.admin_token.clone());
    let server = rig.server();
    let registration = rig.start(Timings {
        admin_token_interval: secs(30),
        ..fast()
    });

    let mut first = server.accept().await;
    first.handshake(&rig.nonce_hex, Some(2_000)).await;
    rig.wait_registered(true).await;
    first
        .send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN}))
        .await;
    wait_until("the first token", secs(10), || {
        rig.admin_token.matches(NEW_TOKEN)
    })
    .await;
    // 服务端结束这个会话；客户端重连，新会话。
    drop(first);
    rig.wait_registered(false).await;
    let mut second = server.accept().await;
    second.handshake(&rig.nonce_hex, Some(2_000)).await;
    rig.wait_registered(true).await;

    // 新会话里、窗口之内再设一个：被拒，token 保持第一次设的值。
    second
        .send(&json!({"v": 1, "op": "set_admin_token", "token": OTHER_TOKEN}))
        .await;
    let frames = second.drain_until_eof(PROMPTLY).await;
    let errors: Vec<Value> = frames
        .iter()
        .map(|frame| serde_json::from_slice::<Value>(frame).unwrap())
        .filter(|frame| frame["op"] == "error")
        .collect();
    assert_eq!(errors.len(), 1, "{frames:?}");
    assert_eq!(errors[0]["code"], "bad_frame");
    assert_eq!(
        admin_status(&app, NEW_TOKEN).await,
        StatusCode::OK,
        "the first new token stays"
    );
    assert_eq!(
        admin_status(&app, OTHER_TOKEN).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        rig.events.count(|e| matches!(
            e,
            Event::AdminTokenRejected {
                reason: TokenRejection::RateLimited
            }
        )),
        1
    );
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::AdminTokenReplaced)),
        1
    );
    registration.finish().await;
}

/// 替换先于 `admin_token_ack` 的写：写失败（对端已经没了）时会话断开，但新 token 已经生效，不会因为 ack 没发出去就
/// 退回旧值。做法：服务端读走第一个心跳，发 `set_admin_token` 之后立刻关闭连接，客户端读到这一帧、替换、再写 ack 时对端已经
/// 不在了。（只看「收到 ack 时新 token 已生效」分不出先后：写完立刻替换，测试任务在客户端让出之前看不到差别。）
#[tokio::test]
async fn the_token_is_replaced_before_the_ack_is_written_even_when_that_write_then_fails() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(2_000)).await;
    rig.wait_registered(true).await;
    // ack 之后立即发的第一个心跳读走，这样对端关闭时没有未读的数据。
    assert_eq!(peer.recv().await["op"], "heartbeat");
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN}))
        .await;
    drop(peer);

    wait_until("the token to be replaced", secs(10), || {
        rig.admin_token.matches(NEW_TOKEN)
    })
    .await;
    assert!(!rig.admin_token.matches(TOKEN), "the old token is dead");
    let lost = rig
        .events
        .wait("the lost session", |e| {
            matches!(e, Event::SessionLost { .. })
        })
        .await;
    assert_eq!(
        lost,
        Event::SessionLost {
            reason: RetryReason::WriteFailed
        },
        "the ack could not be written"
    );
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::AdminTokenReplaced)),
        1
    );
    registration.finish().await;
}

/// 只在 `ack.proof` 已通过的会话上接受：`ack` 之前（`register` 之后）收到的 `set_admin_token`，一律不处理并关闭连接；
/// 证明没过的连接上更不会处理。握手各步的超时是 60 秒（`patient()`），关闭必须在 5 秒内发生：分得开「立即关闭」与「`error`
/// 发完之后继续等 `ack` 到超时」。
#[tokio::test]
async fn set_admin_token_before_the_ack_is_never_applied() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(patient());
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    assert_eq!(register["op"], "register");
    // 不回 ack，直接设 token。
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN}))
        .await;
    let frames = peer.drain_until_eof(PROMPTLY).await;
    assert!(
        frames
            .iter()
            .all(|frame| !String::from_utf8_lossy(frame).contains("admin_token_ack")),
        "{frames:?}"
    );
    // 客户端尽力发了一个 `error bad_frame` 再关闭，原因记作 `bad_frame`。
    assert_eq!(frames.len(), 1, "{frames:?}");
    let error: Value = serde_json::from_slice(&frames[0]).unwrap();
    assert_eq!(error["op"], "error");
    assert_eq!(error["code"], "bad_frame");
    let scheduled = rig
        .events
        .wait("the retry scheduled after the refused token", |e| {
            matches!(e, Event::RetryScheduled { .. })
        })
        .await;
    assert!(
        matches!(
            scheduled,
            Event::RetryScheduled {
                reason: RetryReason::BadFrame,
                fixed: false,
                ..
            }
        ),
        "{scheduled:?}"
    );
    assert!(
        rig.admin_token.matches(TOKEN),
        "the token must be untouched"
    );
    assert!(!rig.admin_token.matches(NEW_TOKEN));
    assert!(!rig.health.registered());

    // `ack.proof` 没过的连接：客户端已经关了，之后补发的 set_admin_token 无处可去，token 同样不变。
    let mut peer = server.accept().await;
    let (conn_id, nonce, session_id) = fresh_ids();
    peer.challenge(&conn_id, &nonce).await;
    peer.send(&json!({
        "v": 1, "op": "ack", "session_id": session_id, "proof": "0".repeat(64)
    }))
    .await;
    peer.try_send_raw(
        format!("{{\"v\":1,\"op\":\"set_admin_token\",\"token\":\"{NEW_TOKEN}\"}}\n").as_bytes(),
    )
    .await;
    peer.drain_until_eof(PROMPTLY).await;
    assert!(rig.admin_token.matches(TOKEN));
    assert_eq!(
        rig.events.count(|e| matches!(e, Event::AdminTokenReplaced)),
        0
    );
    registration.finish().await;
}

// ---------------------------------------------------------------------------------------------------------------
// fd 不被子进程继承
// ---------------------------------------------------------------------------------------------------------------

/// 子进程里 fd `fd` 是不是开着：`/dev/fd/<n>`（macOS 的 fdesc 只列出打开的 fd；Linux 是 `/proc/self/fd` 的符号链接）。
fn open_in_a_child(fd: i32) -> bool {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("[ -e /dev/fd/{fd} ] || [ -L /dev/fd/{fd} ]"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn cloexec_flag(fd: &impl std::os::fd::AsFd) -> bool {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    FdFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFD).unwrap()).contains(FdFlag::FD_CLOEXEC)
}

/// UDS 的 fd 带 close-on-exec，不被任何子进程继承（含以后拉起的引擎进程）。用客户端真正用的连接函数建立连接，
/// 检查 `FD_CLOEXEC`，并且真的起一个子进程看它能不能看到这个 fd。对照组：一个清掉了 CLOEXEC 的 socket 子进程看得到，
/// 说明这个检查方法本身是灵敏的。
#[tokio::test]
async fn the_register_socket_fd_is_close_on_exec_and_never_reaches_a_child_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(dir.path()).unwrap().join("c.sock");
    let _listener = UnixListener::bind(&path).unwrap();

    let stream = register::connect_register_socket(&path).await.unwrap();
    assert!(
        cloexec_flag(&stream),
        "the connection must carry FD_CLOEXEC"
    );
    assert!(
        !open_in_a_child(stream.as_raw_fd()),
        "a child process inherited the register socket"
    );

    // 对照：清掉 CLOEXEC 的 socket 会被继承。
    let control = std::os::unix::net::UnixStream::connect(&path).unwrap();
    assert!(cloexec_flag(&control), "std sets CLOEXEC itself");
    nix::fcntl::fcntl(
        &control,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .unwrap();
    assert!(!cloexec_flag(&control));
    assert!(
        open_in_a_child(control.as_raw_fd()),
        "the harness must be able to see an inherited fd (control group)"
    );

    // `ensure_close_on_exec` 自己会补上缺失的标志，并核对。
    register::ensure_close_on_exec(&control).unwrap();
    assert!(cloexec_flag(&control));
    assert!(!open_in_a_child(control.as_raw_fd()));
}

// ---------------------------------------------------------------------------------------------------------------
// 发现文件自愈
// ---------------------------------------------------------------------------------------------------------------

async fn wait_for_file(path: &Path, expected: &[u8]) {
    let deadline = Instant::now() + secs(20);
    loop {
        if fs::read(path).is_ok_and(|bytes| bytes == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the file {} was never restored",
            path.display()
        );
        tokio::time::sleep(ms(5)).await;
    }
}

/// 自愈与是否登记无关（没有 `register_socket`、没有任何会话也照常）：被删、被换成别的实例的文件、被改成读不懂的内容，
/// 都在下一轮核对时按原子发布重写成与原来逐字节相同的内容（权限 0600）。
#[tokio::test]
async fn the_self_heal_restores_a_deleted_replaced_or_garbled_file_without_any_registration() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let original = fs::read(&path).unwrap();
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(fast())
    });

    // 误删。
    fs::remove_file(&path).unwrap();
    wait_for_file(&path, &original).await;
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // 被换成别的实例的文件（规范：不一致就重写）。
    let foreign = String::from_utf8(original.clone()).unwrap().replace(
        &rig.identity.instance_id,
        "11111111-2222-4333-8444-555555555555",
    );
    fs::write(&path, foreign).unwrap();
    wait_for_file(&path, &original).await;

    // 读不懂的内容。
    fs::write(&path, b"{\"truncated\":").unwrap();
    wait_for_file(&path, &original).await;

    // 完好的文件不被碰：一段时间里（远多于几个周期）inode 与 mtime 不变。
    let before = fs::metadata(&path).unwrap();
    tokio::time::sleep(ms(300)).await;
    let after = fs::metadata(&path).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec()),
        "an intact file must not be rewritten"
    );
    registration.finish().await;
    // 日志里没有 nonce，没有路径。
    rig.logs
        .assert_no_secret(&[rig.nonce_hex.as_str(), "idoris.json"]);
}

/// 目标被换成符号链接、目录：自愈只报错（日志节流），不删除、不覆盖。
#[tokio::test]
async fn the_self_heal_never_replaces_a_symlink_or_a_directory() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let outside = rig.dir_path.join("outside.txt");
    fs::write(&outside, b"precious").unwrap();
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(fast())
    });
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    wait_until("the self-heal to report the refusal", secs(10), || {
        rig.logs
            .lines()
            .iter()
            .any(|line| line.contains("target_not_regular"))
    })
    .await;
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&outside).unwrap(), b"precious");

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), b"keep").unwrap();
    tokio::time::sleep(ms(200)).await;
    assert!(path.is_dir());
    assert_eq!(fs::read(path.join("keep")).unwrap(), b"keep");
    // 不刷屏：报错的日志只记第 1、10、20…次。
    let refusals = rig
        .logs
        .lines()
        .iter()
        .filter(|line| line.contains("自愈失败"))
        .count();
    assert!((1..=4).contains(&refusals), "{refusals}");
    registration.finish().await;
}

/// 发布闸门没开之前不核对、不写：发现文件在 `/health` 能答之后才出现（规范 §2.7.3「何时写」）。闸门开了才开始。
#[tokio::test]
async fn the_self_heal_waits_for_the_publish_gate() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    let path = rig.dir_path.join("idoris.json");
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        open_gate: false,
        ..StartOptions::new(fast())
    });
    // 间隔是 40ms：400ms 里有十个周期，文件仍然不出现。
    tokio::time::sleep(ms(400)).await;
    assert!(
        !path.exists(),
        "the self-heal must not publish before the gate opens"
    );
    registration.publish_gate().open();
    wait_until("the file after the gate opened", secs(10), || path.exists()).await;
    registration.finish().await;
}

/// 登记客户端也等发布闸门（规范 §2.7.3「何时写」、§2.10：登记在写发现文件之后，agent24d 要读这份文件才拿得到 nonce）：闸门没开
/// 之前不连（服务端在监听，第一次连接尝试本来是立即的）；闸门一开就连，握手照常完成。
#[tokio::test]
async fn the_client_does_not_connect_until_the_publish_gate_opens() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start_with(StartOptions {
        open_gate: false,
        ..StartOptions::new(fast())
    });
    assert!(
        server.try_accept(ms(600)).await.is_none(),
        "the client must not connect before the discovery file is published"
    );
    assert_eq!(rig.events.count(|e| matches!(e, Event::Connecting)), 0);

    registration.publish_gate().open();
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(50)).await;
    rig.wait_registered(true).await;
    registration.finish().await;
}

/// 孤儿计时在等发布闸门期间也在走（规范 §2.10.10 第 7 条：从「没有已 `ack` 的登记会话」起算，进程启动之后一直没登记上也算）：
/// 闸门一直没开、一次连接都没发生，超时照样到点要求停止（退出码 4）。
#[tokio::test]
async fn the_orphan_clock_runs_while_the_client_waits_for_the_publish_gate() {
    let rig = Rig::new();
    let server = rig.server();
    let started = Instant::now();
    let registration = rig.start_with(StartOptions {
        orphan: Some(ms(300)),
        open_gate: false,
        ..StartOptions::new(fast())
    });
    let cause = tokio::time::timeout(secs(10), registration.wants_stop())
        .await
        .expect("the orphan clock must run while the client waits for the publish gate");
    assert_eq!(cause, StopCause::OrphanTimeout);
    assert_eq!(cause.exit_code(), 4);
    assert!(
        started.elapsed() >= ms(280),
        "it must not fire early: {:?}",
        started.elapsed()
    );
    // 它从来没有连过。
    assert!(server.try_accept(ms(50)).await.is_none());
    assert_eq!(rig.events.count(|e| matches!(e, Event::Connecting)), 0);
    registration.finish().await;
}

/// 开始停止之后自愈立即停止：之后删掉的文件不会再被写回。
#[tokio::test]
async fn the_self_heal_stops_the_moment_a_stop_begins() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let original = fs::read(&path).unwrap();
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(fast())
    });
    fs::remove_file(&path).unwrap();
    wait_for_file(&path, &original).await;

    registration.begin_stop(StopCause::Signal);
    registration.finish().await;
    fs::remove_file(&path).unwrap();
    tokio::time::sleep(ms(300)).await;
    assert!(!path.exists(), "no rewrite after the stop began");
}

/// 启动失败的静默停止（`begin_silent_stop`，L2）与信号停止一样**立即**停下自愈：排空期间（`finish` 还没调）发现文件被删了也不再写回。
/// 不调 `finish`：它自己也会停自愈，那样就分不出是不是开始停止的那一刻就停了。
#[tokio::test]
async fn the_self_heal_stops_the_moment_a_silent_stop_begins_without_waiting_for_finish() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let original = fs::read(&path).unwrap();
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(Timings {
            self_heal_interval: ms(20),
            ..fast()
        })
    });
    // 对照：停止之前自愈在工作。
    fs::remove_file(&path).unwrap();
    wait_for_file(&path, &original).await;

    registration.begin_silent_stop();
    fs::remove_file(&path).unwrap();
    // 二十轮自愈间隔：停下了就一直不回来。
    tokio::time::sleep(ms(400)).await;
    assert!(
        !path.exists(),
        "no rewrite after a silent stop began, even before finish"
    );
    registration.finish().await;
}

/// 启动失败的静默停止不发 `bye`，也不再重连：与孤儿超时一样直接结束登记客户端，已建立的会话被关掉。
#[tokio::test]
async fn a_silent_stop_sends_no_bye_closes_the_session_and_never_reconnects() {
    let rig = Rig::new();
    let server = rig.server();
    let registration = rig.start(fast());
    let mut peer = server.accept().await;
    peer.handshake(&rig.nonce_hex, Some(20)).await;
    rig.wait_registered(true).await;
    registration.begin_silent_stop();
    let frames = peer.drain_until_eof(secs(10)).await;
    assert!(
        frames
            .iter()
            .all(|frame| String::from_utf8_lossy(frame) != BYE_LINE),
        "a startup failure is not an expected stop: no bye: {frames:?}"
    );
    registration.finish().await;
    assert_eq!(rig.events.count(|e| matches!(e, Event::ByeSent)), 0);
    assert!(server.try_accept(ms(300)).await.is_none());
}

/// 进行中的重写一定做完，`finish` 要等到它结束：删除发现文件在 `finish` 之后才做，否则自愈可能在删除之后又把文件写回来
/// （规范 §2.7.3「何时删」：监听关闭之后、实例锁释放之前，仅当 `instance_id` 是自己）。做法：消费者持着旁路锁，
/// 自愈的发布被挡在 `renameat` 之前（临时文件已经写好、出现在目录里）；这时开始停止并 `finish`，它必须还在等；
/// 消费者放锁之后重写落地，`finish` 才返回。
#[tokio::test]
async fn finish_waits_for_a_self_heal_rewrite_that_is_already_in_flight() {
    let rig = Rig::new();
    let (target, record) = discovery_for(&rig);
    target.publish(&record).unwrap();
    let path = rig.dir_path.join("idoris.json");
    let consumer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(rig.dir_path.join("idoris.json.lock"))
        .unwrap();
    let registration = rig.start_with(StartOptions {
        with_socket: false,
        discovery: Some(SelfHeal {
            target: target.clone(),
            record: record.clone(),
        }),
        ..StartOptions::new(Timings {
            self_heal_interval: ms(20),
            ..fast()
        })
    });
    // 消费者（agent24d）正持着旁路锁清理陈旧文件，同时文件不在了：自愈的重写会被挡在 rename 之前。
    consumer.lock().unwrap();
    fs::remove_file(&path).unwrap();
    wait_until("the in-flight rewrite (its temp file)", secs(10), || {
        fs::read_dir(&rig.dir_path)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".idoris-discovery.")
            })
    })
    .await;

    registration.begin_stop(StopCause::Signal);
    let finishing = {
        let registration = registration.clone();
        tokio::spawn(async move { registration.finish().await })
    };
    tokio::time::sleep(ms(400)).await;
    assert!(
        !finishing.is_finished(),
        "finish must wait for the rewrite that is already in flight"
    );
    assert!(
        !path.exists(),
        "the rewrite is still blocked on the sidecar lock"
    );

    consumer.unlock().unwrap();
    tokio::time::timeout(secs(10), finishing)
        .await
        .unwrap()
        .unwrap();
    assert!(
        path.exists(),
        "the in-flight rewrite is completed, not abandoned"
    );
    // 之后没有新的重写：文件删掉就不会再回来。
    fs::remove_file(&path).unwrap();
    tokio::time::sleep(ms(200)).await;
    assert!(!path.exists());
}

// ---------------------------------------------------------------------------------------------------------------
// 进程级：起真二进制
// ---------------------------------------------------------------------------------------------------------------

const FILE_NAME: &str = "idoris.json";

fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn supervised_omlx_card(endpoint: &str) -> String {
    format!(
        r#"provider:
  id: omlx
  family: local
  tier: local
  capabilities: [chat, embedding]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: on_demand, keepalive: {{ idle_ttl_s: 300 }}, admission: requires_eviction }}
"#
    )
}

/// 进程级测试默认的孤儿计时（秒）：见 `Env::frame`。
const ORPHAN_NET_S: u64 = 600;

struct Env {
    repo: PathBuf,
    components: tempfile::TempDir,
    state: tempfile::TempDir,
    home: tempfile::TempDir,
    _run: tempfile::TempDir,
    run_dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let components = tempfile::tempdir().unwrap();
        fs::write(
            components.path().join("omlx.yaml"),
            supervised_omlx_card(&format!("http://127.0.0.1:{}", closed_port())),
        )
        .unwrap();
        let run = tempfile::tempdir().unwrap();
        let run_dir = fs::canonicalize(run.path()).unwrap();
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            repo,
            components,
            state: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            _run: run,
            run_dir,
        }
    }

    fn discovery_path(&self) -> PathBuf {
        self.run_dir.join(FILE_NAME)
    }

    fn socket_path(&self) -> PathBuf {
        self.run_dir.join("r.sock")
    }

    /// 一帧：Admin token、发现文件、登记 socket、兜底的孤儿计时；`extra` 里的字段覆盖或追加。
    ///
    /// 孤儿计时（`ORPHAN_NET_S`）是兜底：测试进程被 SIGKILL 时守卫的 `Drop` 不会跑，漏下的二进制没有登记会话，
    /// 到时间自己以退出码 4 退出，不会永远占着端口。它比任何一条进程级测试都长得多，不影响它们断言的东西；
    /// 要测孤儿计时的两条用 `extra` 覆盖。
    fn frame(&self, extra: Value) -> Value {
        let mut frame = json!({
            "v": 1,
            "admin_token": TOKEN,
            "discovery_path": self.discovery_path(),
            "register_socket": self.socket_path(),
            "orphan_timeout_s": ORPHAN_NET_S,
        });
        for (key, value) in extra.as_object().unwrap() {
            frame[key] = value.clone();
        }
        frame
    }

    fn spawn(&self, frame: &Value) -> ChildGuard {
        let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
        {
            command.env_remove(key);
        }
        command
            .current_dir(&self.repo)
            .env("HOME", self.home.path())
            .env("IDORIS_STATE_DIR", self.state.path().join("engine-state"))
            .env("IDORIS_COMPONENTS_DIR", self.components.path())
            .env(
                "IDORIS_ROUTING_POLICY",
                self.repo.join("config/routing-policy.yaml"),
            )
            .env("IDORIS_CATALOG", self.repo.join("config/catalog.yaml"))
            .env("IDORIS_DB_PATH", self.state.path().join("state.sqlite3"))
            .env("IDORIS_PORT", "0")
            .env("IDORIS_ADMIN_PORT", "0")
            .env("IDORIS_DEV_NO_KEY", "1")
            .args(["serve", "--launcher-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn_guarded().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // launcher 写完就关 stdin（预期行为：EOF 不代表 launcher 不在）。
        let _ = stdin.write_all(format!("{frame}\n").as_bytes());
        drop(stdin);
        child
    }
}

fn send_signal(signal: &str, child: &Child) {
    let status = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {signal} {}", child.id());
}

fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the process did not exit in time"
        );
        std::thread::sleep(ms(20));
    }
}

fn read_all(mut pipe: impl Read) -> String {
    let mut out = String::new();
    pipe.read_to_string(&mut out).unwrap();
    out
}

fn output_of(child: &mut Child) -> (String, String) {
    (
        read_all(child.stdout.take().unwrap()),
        read_all(child.stderr.take().unwrap()),
    )
}

/// 进程还活着时收走它已经写出的 stderr/stdout 是不安全的（会阻塞），所以只在退出之后读。
fn stop_and_collect(child: &mut Child) -> (std::process::ExitStatus, String, String) {
    send_signal("-TERM", child);
    let status = wait_exit(child, secs(20));
    let (stdout, stderr) = output_of(child);
    (status, stdout, stderr)
}

fn wait_discovery(path: &Path, child: &mut Child) -> (Value, Vec<u8>) {
    let deadline = Instant::now() + secs(60);
    loop {
        if let Ok(bytes) = fs::read(path) {
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            return (value, bytes);
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the server exited before it wrote the discovery file"
        );
        assert!(
            Instant::now() < deadline,
            "the discovery file never appeared"
        );
        std::thread::sleep(ms(10));
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn health(client: &reqwest::Client, port: u64) -> Value {
    client
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(secs(5))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn health_until(client: &reqwest::Client, port: u64, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + secs(30);
    loop {
        let body = health(client, port).await;
        if done(&body) {
            return body;
        }
        assert!(Instant::now() < deadline, "/health never converged: {body}");
        tokio::time::sleep(ms(20)).await;
    }
}

async fn admin_http_status(client: &reqwest::Client, port: u64, secret: &str) -> StatusCode {
    let status = client
        .get(format!("http://127.0.0.1:{port}/admin/api/v1/status"))
        .header("authorization", format!("Bearer {secret}"))
        .timeout(secs(5))
        .send()
        .await
        .unwrap()
        .status();
    StatusCode::from_u16(status.as_u16()).unwrap()
}

fn assert_no_secrets(output: &str, secrets: &[&str]) {
    for secret in secrets {
        assert!(
            !output.contains(secret),
            "a secret leaked into the output: {output}"
        );
    }
}

/// 进程级的主线：服务端沉默 800ms 期间进程一个字节都不写；`challenge` 之后才发 `register`，字段与发现文件一致，
/// `proof` 按发现文件里的 nonce 能验；`ack` 之后 `/health.registered` 为 true，立即有 `heartbeat` seq=1，之后按
/// `ack.heartbeat_ms`（夹到 2 秒）发；SIGTERM 时在会话上发 `bye`，退出码 0，发现文件被删。
#[tokio::test]
async fn the_binary_registers_after_the_challenge_heartbeats_and_says_bye_on_sigterm() {
    let env = Env::new();
    let server = FakeServer::bind(&env.socket_path());
    let mut child = env.spawn(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let nonce = file["instance_nonce"].as_str().unwrap().to_string();
    let data_port = file["data_port"].as_u64().unwrap();
    let client = http();
    assert_eq!(health(&client, data_port).await["registered"], false);

    let mut peer = server.accept().await;
    assert!(
        matches!(peer.read_line(ms(800)).await, Got::Timeout),
        "the process must not write or hang up before the challenge"
    );
    assert!(
        peer.buf.is_empty(),
        "stray bytes before the challenge: {:?}",
        peer.buf
    );
    assert_eq!(health(&client, data_port).await["registered"], false);

    let handshaken = peer.handshake(&nonce, Some(2_000)).await;
    let register = &handshaken.register;
    for field in [
        "instance_id",
        "version",
        "contract_version",
        "data_port",
        "admin_port",
        "started_at_ms",
        "pid",
    ] {
        assert_eq!(
            register[field], file[field],
            "{field} must equal the discovery file"
        );
    }
    assert_eq!(register["pid"].as_u64().unwrap(), u64::from(child.id()));
    assert_eq!(register["resume"], false);
    assert!(["starting", "ready", "degraded"].contains(&register["phase"].as_str().unwrap()));
    let mut keys: Vec<&str> = register
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "admin_port",
            "contract_version",
            "data_port",
            "instance_id",
            "nonce",
            "op",
            "phase",
            "pid",
            "proof",
            "resume",
            "started_at_ms",
            "v",
            "version"
        ]
    );

    health_until(&client, data_port, |body| body["registered"] == true).await;
    let first_beat = peer.recv().await;
    assert_eq!(first_beat["op"], "heartbeat");
    assert_eq!(first_beat["seq"], 1);
    peer.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": 1}))
        .await;
    // 第二个心跳在 heartbeat_ms（2000，±10%）之后。
    let waited = Instant::now();
    let second_beat = peer.recv().await;
    assert_eq!(second_beat["seq"], 2);
    assert!(
        waited.elapsed() >= ms(1_500) && waited.elapsed() <= secs(6),
        "{:?}",
        waited.elapsed()
    );
    peer.send(&json!({"v": 1, "op": "heartbeat_ack", "seq": 2}))
        .await;

    send_signal("-TERM", &child);
    let mut tail = Vec::new();
    loop {
        match peer.read_line(secs(15)).await {
            Got::Line(line) => tail.push(String::from_utf8(line).unwrap()),
            Got::Eof => break,
            Got::Timeout => panic!("the process neither said bye nor closed: {tail:?}"),
        }
    }
    assert_eq!(tail.last().map(String::as_str), Some(BYE_LINE), "{tail:?}");
    let status = wait_exit(&mut child, secs(20));
    let (stdout, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(stdout.contains("idoris: 已停止"), "{stdout}");
    assert!(
        !env.discovery_path().exists(),
        "a graceful stop removes the discovery file"
    );
    // stdout 与 stderr 里没有 nonce、token、proof。
    let proof = register["proof"].as_str().unwrap().to_string();
    let ack_proof = expected_ack_proof(
        &nonce,
        &handshaken.conn_id,
        &handshaken.nonce,
        &handshaken.session_id,
    );
    assert_no_secrets(
        &format!("{stdout}\n{stderr}"),
        &[&nonce, TOKEN, &proof, &ack_proof],
    );
}

/// 进程级的 `set_admin_token`：不合规的先被拒（旧 token 在真实的 Admin 监听器上仍是 200），合规的被接受之后，
/// 旧 token 401、新 token 200，数据面不受影响。
#[tokio::test]
async fn the_binary_rotates_the_real_admin_token_in_a_registered_session() {
    let env = Env::new();
    let server = FakeServer::bind(&env.socket_path());
    let mut child = env.spawn(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let nonce = file["instance_nonce"].as_str().unwrap().to_string();
    let (data_port, admin_port) = (
        file["data_port"].as_u64().unwrap(),
        file["admin_port"].as_u64().unwrap(),
    );
    let client = http();
    assert_eq!(
        admin_http_status(&client, admin_port, TOKEN).await,
        StatusCode::OK
    );

    // 不合规：大写 hex。被拒，会话关闭，旧 token 照常。
    let mut peer = server.accept().await;
    peer.handshake(&nonce, Some(2_000)).await;
    health_until(&client, data_port, |body| body["registered"] == true).await;
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN.to_uppercase()}))
        .await;
    let frames = peer.drain_until_eof(secs(15)).await;
    let error = frames
        .iter()
        .map(|frame| serde_json::from_slice::<Value>(frame).unwrap())
        .find(|frame| frame["op"] == "error")
        .expect("an error frame");
    assert_eq!(error["code"], "bad_frame");
    assert_eq!(
        admin_http_status(&client, admin_port, TOKEN).await,
        StatusCode::OK
    );
    assert_eq!(
        admin_http_status(&client, admin_port, NEW_TOKEN).await,
        StatusCode::UNAUTHORIZED
    );
    health_until(&client, data_port, |body| body["registered"] == false).await;

    // 进程重连（第一次重连立即），合规的 token 被接受。
    let mut peer = server.accept().await;
    peer.handshake(&nonce, Some(2_000)).await;
    health_until(&client, data_port, |body| body["registered"] == true).await;
    peer.send(&json!({"v": 1, "op": "set_admin_token", "token": NEW_TOKEN}))
        .await;
    let ack = loop {
        let line = String::from_utf8(peer.recv_line().await).unwrap();
        if !line.contains("\"heartbeat\"") {
            break line;
        }
    };
    assert_eq!(ack, ADMIN_TOKEN_ACK_LINE);
    assert_eq!(
        admin_http_status(&client, admin_port, TOKEN).await,
        StatusCode::UNAUTHORIZED,
        "the launcher's original token is dead"
    );
    assert_eq!(
        admin_http_status(&client, admin_port, NEW_TOKEN).await,
        StatusCode::OK
    );
    assert_eq!(health(&client, data_port).await["registered"], true);

    let (status, stdout, stderr) = stop_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_no_secrets(&format!("{stdout}\n{stderr}"), &[TOKEN, NEW_TOKEN, &nonce]);
}

/// agent24d 拒绝（`bad_frame`：默认退避，几秒内连来好几次；`identity_rejected`：固定等 60 秒）：
/// 数据面、Admin 照常，`/health.registered` 为 false，进程不退出。
#[tokio::test]
async fn when_agent24d_refuses_the_binary_keeps_serving() {
    let env = Env::new();
    let server = FakeServer::bind(&env.socket_path());
    let mut child = env.spawn(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let (data_port, admin_port) = (
        file["data_port"].as_u64().unwrap(),
        file["admin_port"].as_u64().unwrap(),
    );
    let client = http();

    // 默认退避：第 1 次立即，之后 0.25、0.5、1 秒……几秒内至少连来 4 次。
    let started = Instant::now();
    for attempt in 0..4 {
        let mut peer = server.accept().await;
        let (conn_id, nonce, _) = fresh_ids();
        let (register, _) = peer.challenge(&conn_id, &nonce).await;
        assert_eq!(register["op"], "register", "attempt {attempt}");
        peer.send(&json!({"v": 1, "op": "error", "code": "bad_frame", "message": "no"}))
            .await;
        assert!(peer.drain_until_eof(secs(10)).await.is_empty());
        let body = health(&client, data_port).await;
        assert_eq!(body["registered"], false);
        assert_eq!(body["status"], "ok");
        assert_eq!(
            admin_http_status(&client, admin_port, TOKEN).await,
            StatusCode::OK
        );
    }
    assert!(
        started.elapsed() < secs(25),
        "the default backoff reconnects within seconds: {:?}",
        started.elapsed()
    );

    // identity_rejected：固定 60 秒，期间没有新连接，但仍在服务。
    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    peer.challenge(&conn_id, &nonce).await;
    peer.send(&json!({"v": 1, "op": "error", "code": "identity_rejected", "message": "no"}))
        .await;
    assert!(peer.drain_until_eof(secs(10)).await.is_empty());
    assert!(
        server.try_accept(secs(2)).await.is_none(),
        "identity_rejected means: wait 60 seconds"
    );
    let body = health(&client, data_port).await;
    assert_eq!(body["registered"], false);
    assert!(
        child.try_wait().unwrap().is_none(),
        "a refusal never stops iDoris"
    );

    let (status, _, stderr) = stop_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    // 登记失败在日志里是节流的：这几次失败不会刷屏。
    assert!(
        stderr
            .lines()
            .filter(|line| line.contains("登记失败"))
            .count()
            <= 3,
        "{stderr}"
    );
}

/// 进程级：agent24d 回 `instance_conflict`，进程优雅停止并以退出码 3 退出，固定日志，不重连、不继续服务、
/// 不重写发现文件（这里文件在冲突之前被服务端删掉了，退出之后它仍然不在：没有被写回，也没有别的东西出现）。
#[tokio::test]
async fn the_binary_exits_3_on_instance_conflict_and_does_not_rewrite_the_discovery_file() {
    let env = Env::new();
    let server = FakeServer::bind(&env.socket_path());
    let mut child = env.spawn(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let data_port = file["data_port"].as_u64().unwrap();

    let mut peer = server.accept().await;
    let (conn_id, nonce, _) = fresh_ids();
    let (register, _) = peer.challenge(&conn_id, &nonce).await;
    assert_eq!(register["op"], "register");
    // 模拟 `rm -rf` 之后冲突：文件不在了，自愈（若还在跑）会把它写回。
    fs::remove_file(env.discovery_path()).unwrap();
    peer.send(&json!({"v": 1, "op": "error", "code": "instance_conflict", "message": "x"}))
        .await;

    let status = wait_exit(&mut child, secs(30));
    let (stdout, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(3), "{stderr}");
    assert!(
        stderr.lines().any(|line| line
            == "idoris: 登记被 agent24d 以 instance_conflict 拒绝（已有另一个健康实例），已停止"),
        "{stderr}"
    );
    assert!(!stdout.contains("idoris: 已停止"), "{stdout}");
    assert!(
        !env.discovery_path().exists(),
        "the discovery file must not be rewritten"
    );
    // 没有已 ack 的会话：不发 bye，也没有别的帧；不重连。
    assert!(peer.drain_until_eof(secs(5)).await.is_empty());
    assert!(server.try_accept(ms(500)).await.is_none());
    // 不继续服务：端口上没有人再以这个实例答话。
    if let Ok(response) = http()
        .get(format!("http://127.0.0.1:{data_port}/health"))
        .timeout(secs(2))
        .send()
        .await
    {
        let answered: Value = response.json().await.unwrap_or_default();
        assert_ne!(answered["instance_id"], file["instance_id"]);
    }
}

/// 进程级孤儿计时（`orphan_timeout_s` 取 2 秒）：从来没登记上，进程照常服务，到时优雅停止，退出码 4，固定日志，
/// 发现文件被删（走的是同一条优雅停止路径）。
#[tokio::test]
async fn the_binary_exits_4_when_it_never_gets_registered_within_the_orphan_timeout() {
    let env = Env::new();
    // 登记 socket 路径上没有任何人监听。
    let mut child = env.spawn(&env.frame(json!({"orphan_timeout_s": 2})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let started = Instant::now();
    // 超时之前它在服务。
    let body = health(&http(), file["data_port"].as_u64().unwrap()).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["registered"], false);
    let status = wait_exit(&mut child, secs(30));
    let (stdout, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(4), "{stderr}");
    assert!(
        stderr
            .lines()
            .any(|line| line == "idoris: 无启动器登记超过 2 秒，已停止"),
        "{stderr}"
    );
    assert!(!stdout.contains("idoris: 已停止"), "{stdout}");
    assert!(
        started.elapsed() <= secs(25),
        "the orphan exit must come right after the timeout: {:?}",
        started.elapsed()
    );
    assert!(
        !env.discovery_path().exists(),
        "the graceful path removes the discovery file"
    );
}

/// 进程级：有会话时不超时；会话结束之后计时重新开始（不是从进程启动算）。会话比超时（3 秒）长，服务端关掉会话并停止监听，
/// 进程要再等一个整的超时才以退出码 4 退出，而不是立刻退出。
#[tokio::test]
async fn the_binary_with_a_live_session_is_not_an_orphan_and_the_clock_restarts_at_session_end() {
    let env = Env::new();
    let server = FakeServer::bind(&env.socket_path());
    let mut child = env.spawn(&env.frame(json!({"orphan_timeout_s": 3})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let nonce = file["instance_nonce"].as_str().unwrap().to_string();
    let data_port = file["data_port"].as_u64().unwrap();
    let client = http();

    let mut peer = server.accept().await;
    peer.handshake(&nonce, Some(2_000)).await;
    health_until(&client, data_port, |body| body["registered"] == true).await;
    // 会话保持 4.5 秒（> 3 秒）：进程没有退出，仍是已登记。
    peer.serve_heartbeats_until(Instant::now() + ms(4_500))
        .await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "a registered iDoris is not an orphan"
    );
    assert_eq!(health(&client, data_port).await["registered"], true);

    // 服务端消失。
    drop(peer);
    server.shut_down();
    let closed_at = Instant::now();
    health_until(&client, data_port, |body| body["registered"] == false).await;
    // 刚断开：还没到一个整的超时，进程仍在服务。
    assert!(
        child.try_wait().unwrap().is_none(),
        "the clock restarts when the session ends"
    );
    let status = wait_exit(&mut child, secs(30));
    let (_, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(4), "{stderr}");
    assert!(
        closed_at.elapsed() >= ms(2_300),
        "a full orphan timeout must pass after the session ended: {:?}",
        closed_at.elapsed()
    );
    assert!(
        stderr
            .lines()
            .any(|line| line == "idoris: 无启动器登记超过 3 秒，已停止"),
        "{stderr}"
    );
}

/// 进程级：服务端（agent24d）被重启：进程按退避重连，重新握手，新的 `session_id`，`resume` 为 true，
/// `/health.registered` 在两次会话之间是 false。
#[tokio::test]
async fn the_binary_re_registers_with_resume_true_after_agent24d_restarts() {
    let env = Env::new();
    let server = FakeServer::bind(&env.socket_path());
    let mut child = env.spawn(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let nonce = file["instance_nonce"].as_str().unwrap().to_string();
    let data_port = file["data_port"].as_u64().unwrap();
    let client = http();

    let mut first = server.accept().await;
    let first_session = first.handshake(&nonce, Some(2_000)).await;
    assert_eq!(first_session.register["resume"], false);
    health_until(&client, data_port, |body| body["registered"] == true).await;

    drop(first);
    server.shut_down();
    health_until(&client, data_port, |body| body["registered"] == false).await;
    // agent24d 停了一会儿（进程在退避里重试），然后被拉起。
    tokio::time::sleep(ms(1_200)).await;
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(health(&client, data_port).await["registered"], false);
    let restarted = FakeServer::bind(&env.socket_path());
    let mut second = restarted.accept().await;
    let second_session = second.handshake(&nonce, Some(2_000)).await;
    assert_eq!(second_session.register["resume"], true);
    assert_ne!(first_session.session_id, second_session.session_id);
    health_until(&client, data_port, |body| body["registered"] == true).await;

    let (status, _, stderr) = stop_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

/// 进程级：管道的读端（agent24d 或者起它的测试进程）先走了，stdout、stderr 没人读。会话断开那一行日志写不出去，不能让登记
/// 客户端死掉：agent24d 重启之后它照常重连、重新握手（`resume` 为 true），SIGTERM 的退出码仍是 0（`已停止` 那一行也写不出去）。
/// 日志用 `eprintln!` 写的时候，这里 panic，客户端任务悄悄死掉，再也不重连，退出码是 101。
#[tokio::test]
async fn the_binary_re_registers_and_exits_0_even_when_nobody_reads_its_stdout_and_stderr_any_more()
{
    let env = Env::new();
    let server = FakeServer::bind(&env.socket_path());
    let mut child = env.spawn(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let nonce = file["instance_nonce"].as_str().unwrap().to_string();
    let data_port = file["data_port"].as_u64().unwrap();
    let client = http();

    let mut first = server.accept().await;
    let first_session = first.handshake(&nonce, Some(2_000)).await;
    health_until(&client, data_port, |body| body["registered"] == true).await;

    // 读端没了：之后它写 stdout、stderr 都是 EPIPE。
    drop(child.stdout.take());
    drop(child.stderr.take());
    drop(first);
    server.shut_down();
    health_until(&client, data_port, |body| body["registered"] == false).await;
    // 给客户端时间把「会话断开」那一行写出去（写不出去）。
    tokio::time::sleep(ms(500)).await;
    let restarted = FakeServer::bind(&env.socket_path());
    let mut second = restarted.accept().await;
    let second_session = second.handshake(&nonce, Some(2_000)).await;
    assert_eq!(second_session.register["resume"], true);
    assert_ne!(first_session.session_id, second_session.session_id);
    health_until(&client, data_port, |body| body["registered"] == true).await;

    send_signal("-TERM", &child);
    let status = wait_exit(&mut child, secs(20));
    assert_eq!(status.code(), Some(0));
}

/// 进程级：同上，孤儿超时的退出码也不受影响：启动之后读端没了，到时优雅停止，退出码是 4，不是 `eprintln!` panic 之后的 101。
#[tokio::test]
async fn the_binary_exits_4_on_the_orphan_timeout_even_when_nobody_reads_its_stdout_and_stderr() {
    let env = Env::new();
    // 登记 socket 路径上没有任何人监听。
    let mut child = env.spawn(&env.frame(json!({"orphan_timeout_s": 3})));
    wait_discovery(&env.discovery_path(), &mut child);
    drop(child.stdout.take());
    drop(child.stderr.take());
    let status = wait_exit(&mut child, secs(30));
    assert_eq!(status.code(), Some(4));
}

/// 进程级自愈（规范间隔 10 秒，真实等待）：运行期间发现文件被删（`rm -rf` 误删），十几秒内被原子重写，
/// 内容与原来逐字节相同（同一个实例、同一个 nonce）；与是否登记无关（这里没有 `register_socket`）。
#[tokio::test]
async fn the_binary_restores_a_deleted_discovery_file_within_the_ten_second_interval() {
    let env = Env::new();
    let frame = json!({
        "v": 1,
        "admin_token": TOKEN,
        "discovery_path": env.discovery_path(),
    });
    let mut child = env.spawn(&frame);
    let (_, original) = wait_discovery(&env.discovery_path(), &mut child);
    fs::remove_file(env.discovery_path()).unwrap();
    let deleted_at = Instant::now();
    let deadline = deleted_at + secs(40);
    loop {
        if let Ok(bytes) = fs::read(env.discovery_path()) {
            assert_eq!(bytes, original, "the restored file must be the same record");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the discovery file was never restored"
        );
        std::thread::sleep(ms(50));
    }
    assert!(
        deleted_at.elapsed() <= secs(15),
        "the self-heal runs every 10 seconds: {:?}",
        deleted_at.elapsed()
    );
    assert_eq!(
        fs::metadata(env.discovery_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let (status, _, stderr) = stop_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("发现文件被删除或改动，已按原子发布重写"),
        "{stderr}"
    );
    assert!(!env.discovery_path().exists());
}

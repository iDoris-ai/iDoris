//! 向 agent24d 登记与心跳（接口规范 §2.10，修订 1.6；IDORIS-CORE 切片 S2）。
//!
//! iDoris 是 UDS 客户端：launcher 在 `--launcher-stdin` 帧里给 `register_socket`，iDoris 连上去，**等服务端先发
//! `challenge`**，回带 HMAC 证明的 `register`，收到带服务端证明的 `ack` 才算「已登记」（`/health.registered`
//! 为 true），之后定期发 `heartbeat`。帧是 NDJSON（一行一帧，每帧含 LF ≤ 16384 字节）。
//!
//! # 不变量（规范逐条写明的，测试逐条钉住）
//!
//! - **收到 `challenge` 之前不写任何字节**，包括收到服务端 `error` 的时候：提前到达的字节可能被 agent24d 按
//!   `identity_rejected` 拒绝并关闭连接。
//! - **UDS 的 fd 带 close-on-exec**（[`connect_register_socket`] 显式设置并核对），不得被任何子进程继承。
//! - **活性只看 `/health`**：心跳只用来发现连接断开并重新握手。登记失败（连不上、身份或证明被拒、版本不兼容……）
//!   永远不使 iDoris 退出、也不影响数据面与 Admin；仅有的两个例外是孤儿超时（[`StopCause::OrphanTimeout`]，退出码 4）
//!   与 agent24d 回 `instance_conflict`（[`StopCause::InstanceConflict`]，退出码 3）。
//! - **秘密不出现在日志里**：`instance_nonce`、`proof`、`set_admin_token` 的 token 都不进日志；服务端 `error` 帧的
//!   `message` 文本（对端提供的字符串）也不进日志，只记它的稳定 `code`。
//!
//! # 组成
//!
//! - 纯函数与类型：[`register_proof`]、[`ack_proof`]（测试向量见规范 §2.10.5）、[`parse_server_frame`]、[`Backoff`]、
//!   [`Timings`]（规范数值，测试可缩短）、[`StopCause`]。
//! - [`Registration`]：运行期的整体（登记客户端任务 + 发现文件自愈任务 + 孤儿计时）。`bin/idoris.rs` 用它接线
//!   （进程级测试验证接线、退出码与日志，时间常量只能是规范值）；`tests/register_uds.rs` 也直接用它做进程内测试
//!   （缩短时间常量，观察 [`Event`]），所以进程内测试验证的客户端行为就是二进制里跑的那份代码。
//!
//! # 发现文件自愈
//!
//! 规范 §2.7.3「自愈」：**每 10 秒（与是否已登记无关）**核对目标，缺失或 `instance_id` 不一致就按原子发布重写；
//! 收到 `instance_conflict` 之后不再重写（§2.10.4，否则两个共用路径的实例会互相覆盖）。发布之前不核对（发现文件在
//! `/health` 能答之后才出现，§2.7.3「何时写」），所以自愈任务与登记客户端一样等 [`PublishGate`]。

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::admin::SharedAdminToken;
use crate::discovery::{DiscoveryRecord, DiscoveryTarget, HealOutcome, heal_once};
use crate::instance_nonce::{InstanceNonce, decode_lower_hex, hex_lower, is_lower_hex};
use crate::launcher_frame::StopOnExit;
use crate::readiness::HealthState;

/// 单帧上限（含结尾的 LF）。
pub const MAX_FRAME_BYTES: usize = 16 * 1024;
/// 帧版本：服务端的每一帧都必须带 `"v":1`，我们发出的也是。
pub const FRAME_VERSION: u64 = 1;
/// agent24d 以 `instance_conflict` 拒绝之后的退出码（复用「同一数据库已有实例」的 3，规范 §2.7.5）。
pub const EXIT_INSTANCE_CONFLICT: u8 = 3;
/// 孤儿超时的退出码（规范 §2.7.5 新增）。
pub const EXIT_ORPHAN_TIMEOUT: u8 = 4;
/// iDoris 自己拉起的引擎失效、进程内无法恢复的退出码（规范 §2.7.5 新增，设计稿 D4 的 B）。
pub const EXIT_ENGINE_FAILED: u8 = 5;

const REGISTER_LABEL: &str = "idoris-register/v1";
const ACK_LABEL: &str = "idoris-ack/v1";

// ---------------------------------------------------------------------------------------------------------------
// 停止原因与退出码
// ---------------------------------------------------------------------------------------------------------------

/// 停止的原因。四者走同一条优雅停止路径（规范 §2.7.5）；区别在退出码、是否发 `bye`、结束时打印的日志（信号写 stdout，
/// 另三种写 stderr），以及 S3、S5 接入引擎之后是否停掉自己拉起的引擎（[`StopCause::stops_owned_engines`]）。
///
/// **同一轮里几个原因同时成立时的优先级是 5（`EngineFailed`）> 4（`OrphanTimeout`）> 3（`InstanceConflict`）> 信号（0）**，
/// 先到先得地只锁存一次，退出码、`bye`、引擎处置都从这一个结果推出（`crate::stop`，规范勘误十二）。登记客户端发布一个原因就结束，
/// 所以 3 与 4 不会同时发布。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// SIGTERM、SIGINT：退出码 0，在已有的登记会话上尽力发 `bye`。
    Signal,
    /// agent24d 回了 `instance_conflict`（发出 `register` 之后：等 `ack` 期间，或 `ack` 之后的会话里，规范 §2.10.10 第 5 条）：
    /// 退出码 3，不重连、不再自愈重写发现文件，不发 `bye`（规范 §2.10.10 第 10 条：`bye` 只在信号停止与退出码 5 的停止时发，
    /// 这里不是这两种；会话里收到时，服务端也刚用 `error` 结束了它）。
    InstanceConflict,
    /// 超过 `orphan_timeout_s` 没有已 `ack` 的登记会话：退出码 4，不发 `bye`（同一条规则；按定义也没有会话可发）。
    OrphanTimeout,
    /// iDoris 自己拉起的引擎失效，v0.6.0 无法在进程内恢复（热恢复是 v0.6.1）：优雅停止，退出码 5，由 agent24d 重启走全新的
    /// 启动门控。信号以外唯一**发 `bye`** 的停止（规范 §2.10.8、§2.10.10 第 10 条）；一律停掉自己拥有的引擎。原因来自引擎管理器
    /// （`lifecycle::EngineManager::engine_failed`），不来自登记客户端，所以走 `bin` 里与信号并列的那条路，不经 `wants_stop`。
    EngineFailed,
}

impl StopCause {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Signal => 0,
            Self::InstanceConflict => EXIT_INSTANCE_CONFLICT,
            Self::OrphanTimeout => EXIT_ORPHAN_TIMEOUT,
            Self::EngineFailed => EXIT_ENGINE_FAILED,
        }
    }

    /// 停止时要不要在已建立的登记会话上发 `bye`（规范 §2.10.8、§2.10.10 第 10 条：信号停止，或退出码 5 的停止，并且当时存在
    /// 已 `ack` 的会话，才尽力发，让 agent24d 区分「预期停止」与「崩溃」；其他退出路径不发）。信号停止与自有引擎失效
    /// （退出码 5）发；孤儿超时与 `instance_conflict`（包括会话里收到的）不是这两种停止，不发。
    pub fn says_bye(self) -> bool {
        match self {
            Self::Signal | Self::EngineFailed => true,
            Self::InstanceConflict | Self::OrphanTimeout => false,
        }
    }

    /// 停止原因一确定，要不要**立刻**开始停掉自己拉起的引擎（与 HTTP 排空并行，规范 §2.7.5）：`engine.stop_on_exit:"never"`
    /// **只对退出码 0 有效**（正常停止，升级重启时保留引擎给下一个 iDoris 接管）；孤儿超时（4）、因 `instance_conflict` 退出的 3
    /// 与 5（自有引擎失效）一律停，否则会留下没有任何管理者的引擎，正是孤儿策略要避免的「孤儿占内存」。
    ///
    /// 返回 `false`（只有 `never` 的信号停止）**不等于放手**：此刻不停，也不放下所有权句柄；排空期间引擎照常跑，排空结束、
    /// 退出结果出来之后才定：以 0 退出才移交，排空失败（退出码 1）就停止并回收（`crate::stop::EnginePlan::DecideAfterDrain`，
    /// 规范勘误十二（2））。
    pub fn stops_owned_engines(self, stop_on_exit: StopOnExit) -> bool {
        match self {
            Self::Signal => stop_on_exit == StopOnExit::Owned,
            Self::InstanceConflict | Self::OrphanTimeout | Self::EngineFailed => true,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 计时与退避
// ---------------------------------------------------------------------------------------------------------------

/// 抖动的来源。生产用 [`JitterSource::System`]；测试要精确序列时用 [`JitterSource::None`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitterSource {
    System,
    None,
}

impl JitterSource {
    /// `base` 乘以 `1 ± fraction` 里的一个随机系数。零等待保持为零（「第 1 次立即」不能被抖动成非零）。
    pub fn apply(self, base: Duration, fraction: f64) -> Duration {
        if base.is_zero() || self == Self::None {
            return base;
        }
        let factor = 1.0 + fraction * (2.0 * random_unit() - 1.0);
        base.mul_f64(factor.clamp(0.0, 4.0))
    }
}

/// `[0, 1)` 里的随机数；系统随机数源不可用时取中点（等于不抖动）。
fn random_unit() -> f64 {
    let mut bytes = [0_u8; 8];
    if SystemRandom::new().fill(&mut bytes).is_err() {
        return 0.5;
    }
    // 取高 53 位：恰好是 f64 能精确表示的整数范围。
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1_u64 << 53) as f64
}

/// 规范 §2.10.2、§2.10.7 的全部时间常量。[`Timings::default`] 就是规范里的值；测试可以缩短（进程内测试直接构造），
/// 进程级测试只能用规范值（二进制不提供任何改时间的入口）。
#[derive(Debug, Clone)]
pub struct Timings {
    /// 连接超时：2 秒。
    pub connect: Duration,
    /// 等 `challenge`：5 秒。
    pub challenge_wait: Duration,
    /// 收到 `challenge` 之后发出 `register`：1 秒内。
    pub register_within: Duration,
    /// 等 `ack`：5 秒。
    pub ack_wait: Duration,
    /// `ack` 没给 `heartbeat_ms` 时的心跳间隔：10 秒。
    pub heartbeat_default: Duration,
    /// `ack.heartbeat_ms` 夹到的下限：2 秒。
    pub heartbeat_min: Duration,
    /// `ack.heartbeat_ms` 夹到的上限：60 秒。
    pub heartbeat_max: Duration,
    /// 心跳间隔的抖动：±10%。
    pub heartbeat_jitter: f64,
    /// 等 `heartbeat_ack`：5 秒。
    pub heartbeat_ack_wait: Duration,
    /// 连续几次心跳未应答判失联：3 次。
    pub heartbeat_misses: u32,
    /// 重连退避，第 1 次立即，之后 0.25、0.5、1、2、4、8 秒。
    pub backoff_steps: [Duration; 7],
    /// 退避用完之后的固定等待：10 秒。
    pub backoff_cap: Duration,
    /// 退避的抖动：±20%。
    pub backoff_jitter: f64,
    /// 会话持续满这么久，退避清零：60 秒。
    pub backoff_reset_after: Duration,
    /// `identity_rejected`、`unsupported_version`、`contract_incompatible` 之后的固定等待：60 秒。
    pub wait_identity: Duration,
    /// 收到 `proof_invalid` 之后的固定等待：30 秒。
    pub wait_proof_invalid: Duration,
    /// 收到 `busy` 之后的固定等待：5 秒。
    pub wait_busy: Duration,
    /// 优雅停止时 `bye`（以及尽力发出的 `error` 帧）的预算：0.5 秒。
    pub bye_budget: Duration,
    /// `set_admin_token` 至多每多久接受一次：10 秒。
    pub admin_token_interval: Duration,
    /// 发现文件自愈的间隔：10 秒。
    pub self_heal_interval: Duration,
    pub jitter: JitterSource,
}

impl Default for Timings {
    fn default() -> Self {
        let ms = Duration::from_millis;
        let secs = Duration::from_secs;
        Self {
            connect: secs(2),
            challenge_wait: secs(5),
            register_within: secs(1),
            ack_wait: secs(5),
            heartbeat_default: secs(10),
            heartbeat_min: secs(2),
            heartbeat_max: secs(60),
            heartbeat_jitter: 0.10,
            heartbeat_ack_wait: secs(5),
            heartbeat_misses: 3,
            backoff_steps: [
                Duration::ZERO,
                ms(250),
                ms(500),
                secs(1),
                secs(2),
                secs(4),
                secs(8),
            ],
            backoff_cap: secs(10),
            backoff_jitter: 0.20,
            backoff_reset_after: secs(60),
            wait_identity: secs(60),
            wait_proof_invalid: secs(30),
            wait_busy: secs(5),
            bye_budget: ms(500),
            admin_token_interval: secs(10),
            self_heal_interval: secs(10),
            jitter: JitterSource::System,
        }
    }
}

/// 重连退避：第 1 次立即；之后 0.25、0.5、1、2、4、8 秒，再之后固定 10 秒；每次 ±20% 抖动；会话持续满 60 秒后清零。
///
/// 计数的是「按默认退避安排的重连」：连接失败、没有固定等待的握手失败、会话断开都算一次。对端 `error` 码带固定等待的握手失败
/// （`identity_rejected`、`unsupported_version`、`contract_incompatible`、`proof_invalid`、`busy`，见 [`ServerError`]）不经过这里：
/// 固定等待只替代当次的退避等待，不推进计数，也不加抖动（规范 §2.10.10 第 3 条）。只有持续满 [`Timings::backoff_reset_after`]
/// 的会话才清零（一个建立后很快又断开的会话不清零，所以反复抖动的服务端得到的是逐步变长的等待，不是每次立即重连，
/// §2.10.10 第 4 条）。
#[derive(Debug, Clone, Default)]
pub struct Backoff {
    retries: usize,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// 下一次重连之前的等待。
    pub fn next_delay(&mut self, timings: &Timings) -> Duration {
        let base = timings
            .backoff_steps
            .get(self.retries)
            .copied()
            .unwrap_or(timings.backoff_cap);
        self.retries = self.retries.saturating_add(1);
        timings.jitter.apply(base, timings.backoff_jitter)
    }

    pub fn reset(&mut self) {
        self.retries = 0;
    }

    /// 自上次清零以来已经安排过几次重连。
    pub fn retries(&self) -> usize {
        self.retries
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 实例标识与 HMAC 证明（规范 §2.10.5）
// ---------------------------------------------------------------------------------------------------------------

/// `register` 帧里描述本实例的那些字段，取值与发现文件相同（agent24d 会交叉核对）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub instance_id: String,
    pub pid: u32,
    pub version: String,
    pub contract_version: String,
    pub data_port: u16,
    pub admin_port: u16,
    pub started_at_ms: u64,
}

impl Identity {
    /// 直接取自发现文件的内容：两处永远是同一份事实。
    pub fn from_record(record: &DiscoveryRecord) -> Self {
        Self {
            instance_id: record.instance_id.clone(),
            pid: record.pid,
            version: record.version.clone(),
            contract_version: record.contract_version.clone(),
            data_port: record.data_port,
            admin_port: record.admin_port,
            started_at_ms: record.started_at_ms,
        }
    }
}

/// `register.proof` 的 transcript：`idoris-register/v1`、`conn_id`、`challenge_nonce`、`instance_id`、`pid`、
/// `data_port`、`admin_port`、`started_at_ms`，ASCII，项与项之间只有一个 LF，没有空格，末尾没有 LF，数字十进制无前导零。
pub fn register_transcript(conn_id: &str, challenge_nonce: &str, identity: &Identity) -> Vec<u8> {
    format!(
        "{REGISTER_LABEL}\n{conn_id}\n{challenge_nonce}\n{}\n{}\n{}\n{}\n{}",
        identity.instance_id,
        identity.pid,
        identity.data_port,
        identity.admin_port,
        identity.started_at_ms
    )
    .into_bytes()
}

/// `ack.proof` 的 transcript：`idoris-ack/v1`、`conn_id`、`challenge_nonce`、`session_id`，同样以单个 LF 分隔、末尾没有 LF。
pub fn ack_transcript(conn_id: &str, challenge_nonce: &str, session_id: &str) -> Vec<u8> {
    format!("{ACK_LABEL}\n{conn_id}\n{challenge_nonce}\n{session_id}").into_bytes()
}

/// `register.proof = HMAC-SHA256(key, transcript)`，64 位小写 hex。`key` 来自 [`InstanceNonce::hmac_key`]。
pub fn register_proof(
    key: &hmac::Key,
    conn_id: &str,
    challenge_nonce: &str,
    identity: &Identity,
) -> String {
    hex_lower(
        hmac::sign(
            key,
            &register_transcript(conn_id, challenge_nonce, identity),
        )
        .as_ref(),
    )
}

/// `ack.proof = HMAC-SHA256(key, transcript)`：服务端对 iDoris 的证明（它读得到发现文件才算得出来）。
pub fn ack_proof(
    key: &hmac::Key,
    conn_id: &str,
    challenge_nonce: &str,
    session_id: &str,
) -> String {
    hex_lower(hmac::sign(key, &ack_transcript(conn_id, challenge_nonce, session_id)).as_ref())
}

/// 常量时间校验 `ack.proof`（`ring::hmac::verify`）。`claimed` 必须是 64 位小写 hex，否则一律 `false`。
pub fn ack_proof_matches(
    key: &hmac::Key,
    conn_id: &str,
    challenge_nonce: &str,
    session_id: &str,
    claimed: &str,
) -> bool {
    let Some(tag) = decode_lower_hex::<32>(claimed) else {
        return false;
    };
    hmac::verify(
        key,
        &ack_transcript(conn_id, challenge_nonce, session_id),
        &tag,
    )
    .is_ok()
}

// ---------------------------------------------------------------------------------------------------------------
// 帧
// ---------------------------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct RegisterFrame<'a> {
    v: u64,
    op: &'static str,
    nonce: &'a str,
    instance_id: &'a str,
    pid: u32,
    version: &'a str,
    contract_version: &'a str,
    data_port: u16,
    admin_port: u16,
    started_at_ms: u64,
    phase: &'a str,
    resume: bool,
    proof: &'a str,
}

#[derive(Serialize)]
struct HeartbeatFrame<'a> {
    v: u64,
    op: &'static str,
    seq: u64,
    phase: &'a str,
    ready: bool,
}

#[derive(Serialize)]
struct ByeFrame {
    v: u64,
    op: &'static str,
    reason: &'static str,
}

#[derive(Serialize)]
struct ErrorFrame {
    v: u64,
    op: &'static str,
    code: &'static str,
    message: &'static str,
}

#[derive(Serialize)]
struct AdminTokenAckFrame {
    v: u64,
    op: &'static str,
}

/// 序列化一帧并加 LF。超过 [`MAX_FRAME_BYTES`]（我们发的帧最大约 350 字节，不会发生）或序列化失败时为 `None`。
fn encode_frame<T: Serialize>(frame: &T) -> Option<Vec<u8>> {
    let mut line = serde_json::to_vec(frame).ok()?;
    line.push(b'\n');
    (line.len() <= MAX_FRAME_BYTES).then_some(line)
}

fn error_frame(code: &'static str, message: &'static str) -> Option<Vec<u8>> {
    encode_frame(&ErrorFrame {
        v: FRAME_VERSION,
        op: "error",
        code,
        message,
    })
}

/// 服务端帧不合规的原因。全部按 `bad_frame` 处理（规范 §2.10.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// 一帧（含 LF）超过 16384 字节。
    TooLong,
    NotUtf8,
    /// 空行、含 CR 或 BOM、不是 JSON、不是对象。
    Malformed,
    /// `v` 缺失或不等于 1。
    BadVersion,
    /// 必填字段缺失或形状不对（字段名）。
    BadField(&'static str),
}

/// 不进日志的字符串（`set_admin_token` 带来的 token）：`Debug` 只输出 `Secret([REDACTED])`，所以解析出来的帧即使被
/// `{:?}` 写进日志或错误信息，也不会带出 token；取原文只有一个显式入口 [`Secret::expose`]。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

/// iDoris 处理的五种服务端帧（规范 §2.10.4「会话规则」）；其余 `op` 与未知字段一律忽略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame {
    Challenge {
        conn_id: String,
        nonce: String,
    },
    Ack {
        session_id: String,
        proof: String,
        /// 没给就是 `None`（用缺省的 10 秒）；夹取在调用处做。
        heartbeat_ms: Option<u64>,
    },
    HeartbeatAck {
        seq: u64,
    },
    /// `token` 不是字符串（或缺失）时为 `None`：同样是校验失败。格式校验（64 位小写 hex）在应用时做。
    SetAdminToken {
        token: Option<Secret>,
    },
    Error {
        code: ServerError,
    },
    /// 其他 `op`，或 iDoris 自己才会发的 `op`：忽略。
    Ignored,
}

/// 解析并严格校验一帧（不含结尾的 LF）：非 UTF-8、空行、含 CR 或 BOM、非 JSON、非对象、`v` 不是 1、`op` 不是字符串、
/// 必填字段缺失或形状不对，一律是 [`Fault`]；未知字段忽略（前向兼容）。
pub fn parse_server_frame(line: &[u8]) -> Result<ServerFrame, Fault> {
    // 空行与以 BOM 开头的输入，下面的 `serde_json` 同样得到 `Fault::Malformed`（单元测试与集成测试各有一行钉住）；CR 它当作
    // 空白放过去，而规范要求拒绝（§2.10.1），所以这里先拦的是 CR。
    if line.contains(&b'\r') {
        return Err(Fault::Malformed);
    }
    let text = std::str::from_utf8(line).map_err(|_| Fault::NotUtf8)?;
    let Value::Object(object) =
        serde_json::from_str::<Value>(text).map_err(|_| Fault::Malformed)?
    else {
        return Err(Fault::Malformed);
    };
    match object.get("v").and_then(Value::as_u64) {
        Some(FRAME_VERSION) => {}
        _ => return Err(Fault::BadVersion),
    }
    let Some(Value::String(op)) = object.get("op") else {
        return Err(Fault::BadField("op"));
    };
    let string_field = |name: &'static str, len: usize| -> Result<String, Fault> {
        match object.get(name) {
            Some(Value::String(value)) if is_lower_hex(value, len) => Ok(value.clone()),
            _ => Err(Fault::BadField(name)),
        }
    };
    match op.as_str() {
        "challenge" => Ok(ServerFrame::Challenge {
            conn_id: string_field("conn_id", 16)?,
            nonce: string_field("nonce", 32)?,
        }),
        "ack" => {
            let heartbeat_ms = match object.get("heartbeat_ms") {
                None => None,
                Some(value) => Some(value.as_u64().ok_or(Fault::BadField("heartbeat_ms"))?),
            };
            Ok(ServerFrame::Ack {
                session_id: string_field("session_id", 32)?,
                proof: string_field("proof", 64)?,
                heartbeat_ms,
            })
        }
        "heartbeat_ack" => Ok(ServerFrame::HeartbeatAck {
            seq: object
                .get("seq")
                .and_then(Value::as_u64)
                .ok_or(Fault::BadField("seq"))?,
        }),
        "set_admin_token" => Ok(ServerFrame::SetAdminToken {
            token: object.get("token").and_then(Value::as_str).map(Secret::new),
        }),
        "error" => Ok(ServerFrame::Error {
            code: object
                .get("code")
                .and_then(Value::as_str)
                .map_or(ServerError::Unknown, ServerError::parse),
        }),
        _ => Ok(ServerFrame::Ignored),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 错误码与重试计划
// ---------------------------------------------------------------------------------------------------------------

/// 服务端 `error.code`（规范 §2.10.4 表）。不认识的取值是 [`ServerError::Unknown`]，按默认退避处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerError {
    IdentityRejected,
    ProofInvalid,
    UnsupportedVersion,
    ContractIncompatible,
    BadFrame,
    Timeout,
    Busy,
    InstanceConflict,
    Internal,
    Unknown,
}

impl ServerError {
    pub fn parse(code: &str) -> Self {
        match code {
            "identity_rejected" => Self::IdentityRejected,
            "proof_invalid" => Self::ProofInvalid,
            "unsupported_version" => Self::UnsupportedVersion,
            "contract_incompatible" => Self::ContractIncompatible,
            "bad_frame" => Self::BadFrame,
            "timeout" => Self::Timeout,
            "busy" => Self::Busy,
            "instance_conflict" => Self::InstanceConflict,
            "internal" => Self::Internal,
            _ => Self::Unknown,
        }
    }

    /// 稳定的原因码：写日志只用它，不用对端给的任何文本。
    pub fn code(self) -> &'static str {
        match self {
            Self::IdentityRejected => "identity_rejected",
            Self::ProofInvalid => "proof_invalid",
            Self::UnsupportedVersion => "unsupported_version",
            Self::ContractIncompatible => "contract_incompatible",
            Self::BadFrame => "bad_frame",
            Self::Timeout => "timeout",
            Self::Busy => "busy",
            Self::InstanceConflict => "instance_conflict",
            Self::Internal => "internal",
            Self::Unknown => "unknown",
        }
    }

    /// 收到之后关闭连接，下一次重连之前的等待（规范 §2.10.4 表与 §2.10.7）。
    /// `instance_conflict` 不重连（iDoris 退出），由调用方先行处理；这里按默认退避兜底。
    fn wait(self) -> Wait {
        match self {
            Self::IdentityRejected | Self::UnsupportedVersion | Self::ContractIncompatible => {
                Wait::Identity
            }
            Self::ProofInvalid => Wait::ProofInvalid,
            Self::Busy => Wait::Busy,
            Self::BadFrame
            | Self::Timeout
            | Self::Internal
            | Self::InstanceConflict
            | Self::Unknown => Wait::Backoff,
        }
    }
}

/// 下一次重连之前怎么等。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// 默认退避（[`Backoff`]）。
    Backoff,
    /// 60 秒。
    Identity,
    /// 30 秒。
    ProofInvalid,
    /// 5 秒。
    Busy,
}

impl Wait {
    /// 固定等待的时长；默认退避没有固定值。
    fn fixed(self, timings: &Timings) -> Option<Duration> {
        match self {
            Self::Backoff => None,
            Self::Identity => Some(timings.wait_identity),
            Self::ProofInvalid => Some(timings.wait_proof_invalid),
            Self::Busy => Some(timings.wait_busy),
        }
    }
}

/// 一次握手或会话为什么结束、要重连（稳定的原因码，进事件与日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    ConnectFailed,
    ChallengeTimeout,
    AckTimeout,
    WriteFailed,
    /// 服务端关闭了连接：EOF，或对端带着没读的数据关闭时 Linux 给的 `ECONNRESET`（同样处理）。
    Disconnected,
    IoError,
    /// 服务端的帧不合规（[`Fault`]）。
    BadFrame,
    /// `ack.proof` 校验失败：服务端读不到发现文件（或是冒充者）。
    AckProofInvalid,
    /// 连续 3 次心跳没有应答。
    HeartbeatLost,
    /// 对 `set_admin_token` 的校验失败或超频：会话被关闭。
    AdminTokenRejected,
    Server(ServerError),
}

impl RetryReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::ConnectFailed => "connect_failed",
            Self::ChallengeTimeout => "challenge_timeout",
            Self::AckTimeout => "ack_timeout",
            Self::WriteFailed => "write_failed",
            Self::Disconnected => "disconnected",
            Self::IoError => "io_error",
            Self::BadFrame => "bad_frame",
            Self::AckProofInvalid => "ack_proof_invalid",
            Self::HeartbeatLost => "heartbeat_lost",
            Self::AdminTokenRejected => "admin_token_rejected",
            Self::Server(error) => error.code(),
        }
    }
}

/// 重试计划：原因加等待方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    reason: RetryReason,
    wait: Wait,
}

impl Plan {
    fn backoff(reason: RetryReason) -> Self {
        Self {
            reason,
            wait: Wait::Backoff,
        }
    }

    fn server(error: ServerError) -> Self {
        Self {
            reason: RetryReason::Server(error),
            wait: error.wait(),
        }
    }
}

/// `set_admin_token` 被拒的原因（进事件；日志也只写这个原因码，不写 token）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRejection {
    /// 不是 64 位小写 hex 的字符串（缺失、类型不对也算）。
    Invalid,
    /// 距上一次接受不足 10 秒。
    RateLimited,
}

impl TokenRejection {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_token",
            Self::RateLimited => "rate_limited",
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 事件与日志
// ---------------------------------------------------------------------------------------------------------------

/// 登记客户端的状态变化，给日志与测试用（测试据此断言「发生了什么」，而不是靠墙钟猜）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 开始一次连接尝试。
    Connecting,
    ChallengeReceived,
    RegisterSent {
        resume: bool,
    },
    /// 收到并校验通过 `ack`：会话建立，`/health.registered` 变 true。
    Registered {
        session_id: String,
        resume: bool,
    },
    HeartbeatSent {
        seq: u64,
    },
    HeartbeatAcked {
        seq: u64,
    },
    /// 已建立的会话结束，`/health.registered` 变 false。
    SessionLost {
        reason: RetryReason,
    },
    /// 一次连接或握手没有建立会话；`consecutive` 是连续失败次数（建立会话后清零）。
    AttemptFailed {
        reason: RetryReason,
        consecutive: u64,
    },
    /// 安排下一次重连。`fixed` 为 true 表示是错误码规定的固定等待（60、30、5 秒），否则是默认退避。
    RetryScheduled {
        delay: Duration,
        fixed: bool,
        reason: RetryReason,
    },
    AdminTokenReplaced,
    AdminTokenRejected {
        reason: TokenRejection,
    },
    ByeSent,
    /// 登记客户端要求 iDoris 停止（`instance_conflict` 或孤儿超时）。
    ExitRequested(StopCause),
}

pub type Observer = Arc<dyn Fn(&Event) + Send + Sync>;
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// 默认日志：写 stderr（agent24d 会把子进程输出管道读走）。写不出去就算了：agent24d 重启或退出之后，管道的另一头没人读，
/// `eprintln!` 会 panic，登记客户端任务随之悄悄死掉：再也不重连，孤儿计时也跟着没了。
pub fn stderr_log() -> LogSink {
    Arc::new(|line| {
        let _ = writeln!(std::io::stderr(), "{line}");
    })
}

/// 重复发生的事情的日志节流（规范 §2.10.7「日志」：只记状态变化，以及首次失败和之后每第 10 次失败，不刷屏）：
/// 连续第 1、10、20、30… 次才记。登记失败与发现文件的连续重写都用它。
pub fn should_log_repeat(consecutive: u64) -> bool {
    consecutive == 1 || (consecutive > 0 && consecutive.is_multiple_of(10))
}

// ---------------------------------------------------------------------------------------------------------------
// 整体：登记客户端 + 自愈 + 孤儿计时
// ---------------------------------------------------------------------------------------------------------------

/// 发现文件的自愈目标：与启动时发布的是同一个目标与同一份内容。
#[derive(Clone)]
pub struct SelfHeal {
    pub target: Arc<DiscoveryTarget>,
    pub record: Arc<DiscoveryRecord>,
}

pub struct RegistrationConfig {
    /// launcher 帧的 `register_socket`；没有就不登记（但发现文件自愈照常）。
    pub socket: Option<PathBuf>,
    /// launcher 帧的 `orphan_timeout_s`（非 0）；没有就不因「无登记会话」退出。孤儿计时跟着登记客户端走，
    /// 所以没有 `socket` 时不起作用（帧校验保证 `orphan_timeout_s` 非 0 则 `register_socket` 必填）。
    pub orphan_timeout: Option<Duration>,
    pub identity: Identity,
    pub nonce: InstanceNonce,
    pub health: HealthState,
    pub admin_token: SharedAdminToken,
    /// 有发现文件才有自愈。
    pub discovery: Option<SelfHeal>,
    pub timings: Timings,
    pub observer: Option<Observer>,
    pub log: LogSink,
}

/// 「发现文件已经首次发布」的闸门：登记与自愈都等它（规范 §2.7.3「何时写」：登记在写文件之后）。
/// 启动时的发布钩子在发布成功之后调用 [`PublishGate::open`]。
#[derive(Clone)]
pub struct PublishGate(Arc<watch::Sender<bool>>);

impl PublishGate {
    pub fn open(&self) {
        self.0.send_replace(true);
    }
}

/// 给登记客户端的停止指令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMode {
    Run,
    /// 优雅停止：已建立的会话上尽力发 `bye`（至多 0.5 秒），然后结束。
    Bye,
    /// 直接结束，不发 `bye`。
    Silent,
}

struct Inner {
    gate: PublishGate,
    stop: watch::Sender<StopMode>,
    exit: Arc<watch::Sender<Option<StopCause>>>,
    heal_cancel: CancellationToken,
    heal_task: Mutex<Option<JoinHandle<()>>>,
    client_task: Mutex<Option<JoinHandle<()>>>,
    bye_budget: Duration,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // 没有调用 `finish` 就丢弃：不让自愈任务在没人管的情况下继续重写文件。
        self.heal_cancel.cancel();
    }
}

/// 运行期的登记整体。克隆很便宜，所有克隆指向同一个整体。
#[derive(Clone)]
pub struct Registration {
    inner: Arc<Inner>,
}

impl Registration {
    /// 启动登记客户端任务与自愈任务（必须在 tokio 运行时里调用）。两者都等 [`PublishGate`]。
    /// 没有 `socket` 就没有客户端任务，没有 `discovery` 就没有自愈任务。
    pub fn start(config: RegistrationConfig) -> Self {
        let RegistrationConfig {
            socket,
            orphan_timeout,
            identity,
            nonce,
            health,
            admin_token,
            discovery,
            timings,
            observer,
            log,
        } = config;
        let (stop_tx, stop_rx) = watch::channel(StopMode::Run);
        let exit = Arc::new(watch::channel(None::<StopCause>).0);
        let (gate_tx, gate_rx) = watch::channel(false);
        let heal_cancel = CancellationToken::new();
        let bye_budget = timings.bye_budget;

        let heal_task = discovery.map(|heal| {
            tokio::spawn(run_self_heal(
                heal,
                timings.self_heal_interval,
                gate_rx.clone(),
                heal_cancel.clone(),
                log.clone(),
            ))
        });
        let client_task = socket.map(|socket| {
            spawn_client(ClientParts {
                socket,
                orphan_timeout,
                identity,
                nonce,
                health,
                admin_token,
                timings,
                observer,
                log,
                stop: stop_rx,
                exit: exit.clone(),
                heal_cancel: heal_cancel.clone(),
                published: gate_rx,
            })
        });
        Self {
            inner: Arc::new(Inner {
                gate: PublishGate(Arc::new(gate_tx)),
                stop: stop_tx,
                exit,
                heal_cancel,
                heal_task: Mutex::new(heal_task),
                client_task: Mutex::new(client_task),
                bye_budget,
            }),
        }
    }

    /// 发布闸门：启动时的发布钩子在发现文件首次发布成功之后调用 [`PublishGate::open`]。
    pub fn publish_gate(&self) -> PublishGate {
        self.inner.gate.clone()
    }

    /// 登记客户端要求 iDoris 停止时返回原因（`instance_conflict` 或孤儿超时）。没有客户端、或客户端没有这样的要求时
    /// 永不返回，所以可以直接放进 `select!` 与信号并列。
    ///
    /// **已经发布的原因在第一次被 poll 时就就绪**（读 `watch` 里的当前值，不等下一次变化）：`crate::stop::StopSources` 靠这一点
    /// 让已经成立的退出原因排在信号前面（同一轮里两者都就绪时，不让信号盖掉它们）。
    pub async fn wants_stop(&self) -> StopCause {
        let mut exit = self.inner.exit.subscribe();
        loop {
            if let Some(cause) = *exit.borrow_and_update() {
                return cause;
            }
            if exit.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// iDoris 开始停止（任何预期的原因）时调用，立即返回：自愈立刻停止（之后不再重写发现文件）；原因是信号或自有引擎失效就让
    /// 登记客户端在已建立的会话上尽力发 `bye`（至多 0.5 秒，与 HTTP 排空并行），其余原因不发（见 [`StopCause::says_bye`]）。
    pub fn begin_stop(&self, cause: StopCause) {
        self.begin(if cause.says_bye() {
            StopMode::Bye
        } else {
            StopMode::Silent
        });
    }

    /// iDoris 因启动失败停止（不是任何一种预期的停止，没有 [`StopCause`]）时调用，立即返回：自愈立刻停止，登记客户端直接结束，
    /// **不发 `bye`**。排空期间不能让一个已经在退出的进程继续往发现文件里写「我是活的」，也不能继续与 agent24d 保持登记会话。
    pub fn begin_silent_stop(&self) {
        self.begin(StopMode::Silent);
    }

    fn begin(&self, mode: StopMode) {
        self.inner.heal_cancel.cancel();
        self.inner.stop.send_if_modified(|current| {
            let change = *current == StopMode::Run;
            if change {
                *current = mode;
            }
            change
        });
    }

    /// 等自愈与登记客户端都结束，之后才能删发现文件（否则自愈可能在删除之后又把它写回来）。自愈任务会被等到底
    /// （一次进行中的发布至多等旁路锁 5 秒）；登记客户端的 `bye` 有 0.5 秒预算，另给一点余量，超过就中止。
    pub async fn finish(&self) {
        self.inner.heal_cancel.cancel();
        self.inner.stop.send_if_modified(|current| {
            let change = *current == StopMode::Run;
            if change {
                *current = StopMode::Silent;
            }
            change
        });
        let heal = self
            .inner
            .heal_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(heal) = heal {
            let _ = heal.await;
        }
        let client = self
            .inner
            .client_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut client) = client {
            let budget = self.inner.bye_budget + Duration::from_millis(250);
            if tokio::time::timeout(budget, &mut client).await.is_err() {
                client.abort();
            }
        }
    }
}

/// 发现文件自愈（规范 §2.7.3）：发布之后每 `interval` 核对一次，缺失或不一致就重写。
async fn run_self_heal(
    heal: SelfHeal,
    interval: Duration,
    mut published: watch::Receiver<bool>,
    cancel: CancellationToken,
    log: LogSink,
) {
    tokio::select! {
        _ = cancel.cancelled() => return,
        opened = published.wait_for(|opened| *opened) => {
            if opened.is_err() {
                return;
            }
        }
    }
    let mut failures: u64 = 0;
    // 连续被重写的次数：核对到完好的文件就清零。一次误删只记一行；两个实例共用一个路径、各自的自愈互相覆盖时，
    // 每 10 秒都会重写一次，这时只记第 1、10、20… 次，不刷屏。
    let mut rewrites: u64 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        if cancel.is_cancelled() {
            return;
        }
        let (target, record) = (heal.target.clone(), heal.record.clone());
        // 阻塞的文件系统调用：不在这里被取消，进行中的重写一定做完（`Registration::finish` 会等到它结束，
        // 删除发现文件才不会和它赛跑）。
        let outcome = tokio::task::spawn_blocking(move || heal_once(&target, &record)).await;
        match outcome {
            Ok(HealOutcome::Intact) => {
                failures = 0;
                rewrites = 0;
            }
            Ok(HealOutcome::Rewritten) => {
                failures = 0;
                rewrites += 1;
                if rewrites == 1 {
                    log("[idoris] 发现文件被删除或改动，已按原子发布重写");
                } else if should_log_repeat(rewrites) {
                    log(&format!(
                        "[idoris] 发现文件被删除或改动，已按原子发布重写（连续第 {rewrites} 次：有别的实例或进程在反复改动它）"
                    ));
                }
            }
            Ok(HealOutcome::Failed(error)) => {
                failures += 1;
                if should_log_repeat(failures) {
                    log(&format!(
                        "[idoris] 发现文件自愈失败（{}，连续第 {failures} 次）；文件保持原样，下一轮再试",
                        error.code()
                    ));
                }
            }
            Err(_) => {
                failures += 1;
                if should_log_repeat(failures) {
                    log(&format!(
                        "[idoris] 发现文件自愈任务异常结束（连续第 {failures} 次）"
                    ));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 登记客户端：UDS（仅 unix）
// ---------------------------------------------------------------------------------------------------------------

/// 启动登记客户端需要的全部东西（`Registration::start` 拆开 `RegistrationConfig` 后交给它）。
#[cfg_attr(not(unix), allow(dead_code))]
struct ClientParts {
    socket: PathBuf,
    orphan_timeout: Option<Duration>,
    identity: Identity,
    nonce: InstanceNonce,
    health: HealthState,
    admin_token: SharedAdminToken,
    timings: Timings,
    observer: Option<Observer>,
    log: LogSink,
    stop: watch::Receiver<StopMode>,
    exit: Arc<watch::Sender<Option<StopCause>>>,
    heal_cancel: CancellationToken,
    published: watch::Receiver<bool>,
}

#[cfg(unix)]
fn spawn_client(parts: ClientParts) -> JoinHandle<()> {
    tokio::spawn(client::Supervisor::new(parts).run())
}

/// 非 unix 平台没有 Unix domain socket：不登记（launcher 要求发现文件的路径在更早处已经启动失败，
/// 到不了这里；这只是让别的平台能编译）。
#[cfg(not(unix))]
fn spawn_client(parts: ClientParts) -> JoinHandle<()> {
    (parts.log)("[idoris] 本平台没有 Unix domain socket，不向 agent24d 登记");
    tokio::spawn(async {})
}

#[cfg(unix)]
pub use client::{connect_register_socket, ensure_close_on_exec};

/// 等到 `mode` 不再是 [`StopMode::Run`]。发送端被丢弃（整体已经被丢弃）按「直接结束」处理。可取消。
async fn stop_requested(stop: &mut watch::Receiver<StopMode>) -> StopMode {
    loop {
        let mode = *stop.borrow_and_update();
        if mode != StopMode::Run {
            return mode;
        }
        if stop.changed().await.is_err() {
            return StopMode::Silent;
        }
    }
}

/// 到 `deadline` 才返回；没有截止时间就永不返回。
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(unix)]
mod client {
    use std::collections::VecDeque;
    use std::os::fd::AsFd;
    use std::path::Path;

    use nix::fcntl::{self, FcntlArg, FdFlag};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::time::{Instant, sleep, sleep_until, timeout};

    use super::*;
    use crate::admin::AdminSessionToken;

    /// 连接 `register_socket`，并确保这个 fd 带 close-on-exec：不得被任何子进程（含以后拉起的引擎进程）继承
    /// （规范 §2.10.1）。tokio、mio 在创建 socket 时已经设置了它（Linux 是创建时原子设置，macOS 是创建之后立刻 `fcntl`）；
    /// 这里仍然显式设置并**核对**，不依赖那条依赖栈的实现细节：核对不过就不用这个连接。
    pub async fn connect_register_socket(path: &Path) -> std::io::Result<UnixStream> {
        let stream = UnixStream::connect(path).await?;
        ensure_close_on_exec(&stream)?;
        Ok(stream)
    }

    /// 设置并核对 `FD_CLOEXEC`。
    pub fn ensure_close_on_exec(fd: &impl AsFd) -> std::io::Result<()> {
        let flags = FdFlag::from_bits_retain(
            fcntl::fcntl(fd, FcntlArg::F_GETFD).map_err(std::io::Error::from)?,
        );
        if !flags.contains(FdFlag::FD_CLOEXEC) {
            fcntl::fcntl(fd, FcntlArg::F_SETFD(flags | FdFlag::FD_CLOEXEC))
                .map_err(std::io::Error::from)?;
        }
        let verified = FdFlag::from_bits_retain(
            fcntl::fcntl(fd, FcntlArg::F_GETFD).map_err(std::io::Error::from)?,
        );
        if verified.contains(FdFlag::FD_CLOEXEC) {
            Ok(())
        } else {
            Err(std::io::Error::other("无法确认登记连接的 close-on-exec"))
        }
    }

    /// 读到的一帧，或读不下去的原因。
    enum Read {
        Frame(ServerFrame),
        /// 对端关闭了连接：EOF，或 `ECONNRESET`（对端带着没读的数据关闭，见 [`FrameReader::next`]），两者同样处理。
        Eof,
        /// 其余的读错误。
        Io,
        /// 帧不合规（超长、非 UTF-8、非对象、`v` 不对、必填字段不对……）：全部按 `bad_frame` 处理，不区分细节。
        Bad,
    }

    /// 带缓冲的帧读取器：一行一帧，每帧（含 LF）≤ 16384 字节。`next` 可取消（半帧留在缓冲里，下次接着读）。
    ///
    /// 对读端泛型：生产里就是 UDS 的读半边；单元测试（`register_tests.rs`）注入一个会出错的读端。真实的 UDS 上没法在 macOS 上
    /// 造出读错误：`ECONNRESET` 要 Linux 上对端带着没读的数据关闭才有（见 `next`），而 `register` 发出之前我们没写过字节，
    /// 对端也没有未读的数据；其余的读错误没有已知的触发办法。每一种读错误在握手里的两个读点都要有测试钉住，所以注入读端。
    pub(super) struct FrameReader<R = OwnedReadHalf> {
        half: R,
        buf: Vec<u8>,
    }

    impl<R: AsyncRead + Unpin> FrameReader<R> {
        pub(super) fn new(half: R) -> Self {
            Self {
                half,
                buf: Vec::with_capacity(512),
            }
        }

        /// 读下一帧。`ECONNRESET` 与 EOF 同样处理：Linux 的 AF_UNIX 在对端带着没读的数据关闭时，让这一端读到 `ECONNRESET`
        /// 而不是 EOF（macOS 给 EOF）。规范 §2.10.7 对「任何读写错误，或 EOF」的处理本来就一样（判失联、重连），原因码记
        /// `disconnected`，比 `io_error` 对运维更准确：对端确实是自己关掉的。读端见不到 `EPIPE`（那是写的错误），所以只认
        /// `ConnectionReset`。
        async fn next(&mut self) -> Read {
            loop {
                if let Some(end) = self.buf.iter().position(|byte| *byte == b'\n') {
                    let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                    line.pop();
                    if end + 1 > MAX_FRAME_BYTES {
                        return Read::Bad;
                    }
                    return match parse_server_frame(&line) {
                        Ok(frame) => Read::Frame(frame),
                        Err(_) => Read::Bad,
                    };
                }
                // 已经读满一帧的上限还没有 LF：超长。
                if self.buf.len() >= MAX_FRAME_BYTES {
                    self.buf.clear();
                    return Read::Bad;
                }
                match self.half.read_buf(&mut self.buf).await {
                    Ok(0) => return Read::Eof,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                        return Read::Eof;
                    }
                    Err(_) => return Read::Io,
                }
            }
        }
    }

    pub(super) struct Conn<R = OwnedReadHalf> {
        pub(super) reader: FrameReader<R>,
        pub(super) writer: OwnedWriteHalf,
    }

    impl<R> Conn<R> {
        /// 在 `within` 内写完一帧。失败（超时、对端已关闭）返回 `false`。
        async fn send(&mut self, bytes: &[u8], within: Duration) -> bool {
            let write = async {
                self.writer.write_all(bytes).await?;
                self.writer.flush().await
            };
            matches!(timeout(within, write).await, Ok(Ok(())))
        }
    }

    /// 握手成功得到的会话。
    pub(super) struct Session {
        conn: Conn,
        heartbeat_every: Duration,
    }

    pub(super) enum Handshake {
        Established(Session),
        Retry(Plan),
        /// 服务端回了 `instance_conflict`。
        Conflict,
    }

    enum SessionEnd {
        /// iDoris 自己要停（`bye` 已经尽力发过）。
        Stopped,
        Conflict,
        Lost {
            plan: Plan,
            lasted: Duration,
        },
    }

    pub(super) struct Supervisor {
        socket: PathBuf,
        orphan_timeout: Option<Duration>,
        identity: Identity,
        key: hmac::Key,
        health: HealthState,
        admin_token: SharedAdminToken,
        t: Timings,
        observer: Option<Observer>,
        log: LogSink,
        stop: watch::Receiver<StopMode>,
        exit: Arc<watch::Sender<Option<StopCause>>>,
        heal_cancel: CancellationToken,
        published: watch::Receiver<bool>,
        ever_registered: bool,
        backoff: Backoff,
        consecutive_failures: u64,
        last_token_set: Option<Instant>,
    }

    impl Supervisor {
        pub(super) fn new(parts: ClientParts) -> Self {
            Self {
                socket: parts.socket,
                orphan_timeout: parts.orphan_timeout,
                identity: parts.identity,
                key: parts.nonce.hmac_key(),
                health: parts.health,
                admin_token: parts.admin_token,
                t: parts.timings,
                observer: parts.observer,
                log: parts.log,
                stop: parts.stop,
                exit: parts.exit,
                heal_cancel: parts.heal_cancel,
                published: parts.published,
                ever_registered: false,
                backoff: Backoff::new(),
                consecutive_failures: 0,
                last_token_set: None,
            }
        }

        fn emit(&self, event: Event) {
            if let Some(observer) = &self.observer {
                observer(&event);
            }
            let line = match &event {
                Event::Registered { .. } => Some("[idoris] 已向 agent24d 登记".to_string()),
                Event::SessionLost { reason } => Some(format!(
                    "[idoris] 登记会话已断开（{}），将重连；数据面与 Admin 不受影响",
                    reason.code()
                )),
                Event::AttemptFailed {
                    reason,
                    consecutive,
                } if should_log_repeat(*consecutive) => Some(format!(
                    "[idoris] 登记失败（{}，连续第 {consecutive} 次）；iDoris 继续服务，按退避重试",
                    reason.code()
                )),
                Event::AdminTokenReplaced => {
                    Some("[idoris] Admin token 已在登记会话内轮换，旧 token 已失效".to_string())
                }
                Event::AdminTokenRejected { reason } => Some(format!(
                    "[idoris] 拒绝 agent24d 的 set_admin_token（{}），token 保持不变",
                    reason.code()
                )),
                _ => None,
            };
            if let Some(line) = line {
                (self.log)(&line);
            }
        }

        /// 登记客户端要求 iDoris 停止：先停自愈（收到 `instance_conflict` 之后不再重写发现文件），再通知。
        fn request_exit(&self, cause: StopCause) {
            self.heal_cancel.cancel();
            self.exit.send_replace(Some(cause));
            self.emit(Event::ExitRequested(cause));
        }

        pub(super) async fn run(mut self) {
            let mut stop = self.stop.clone();
            let mut published = self.published.clone();
            // 孤儿计时从「没有已 ack 的登记会话」开始，包含进程启动后一直没登记上的情形。
            let mut orphan_deadline = self.orphan_timeout.map(|after| Instant::now() + after);

            // 发现文件首次发布之前不连：agent24d 要读这份文件才拿得到 `instance_nonce`（规范：登记在写文件之后）。
            tokio::select! {
                biased;
                _ = stop_requested(&mut stop) => return,
                _ = until(orphan_deadline) => {
                    self.request_exit(StopCause::OrphanTimeout);
                    return;
                }
                opened = published.wait_for(|opened| *opened) => {
                    if opened.is_err() {
                        return;
                    }
                }
            }

            loop {
                enum Step {
                    Stopped,
                    Orphan,
                    Handshake(Handshake),
                }
                let step = tokio::select! {
                    biased;
                    _ = stop_requested(&mut stop) => Step::Stopped,
                    _ = until(orphan_deadline) => Step::Orphan,
                    result = self.handshake() => Step::Handshake(result),
                };
                let plan = match step {
                    Step::Stopped => return,
                    Step::Orphan => {
                        self.request_exit(StopCause::OrphanTimeout);
                        return;
                    }
                    Step::Handshake(Handshake::Conflict) => {
                        self.request_exit(StopCause::InstanceConflict);
                        return;
                    }
                    Step::Handshake(Handshake::Retry(plan)) => {
                        self.consecutive_failures += 1;
                        self.emit(Event::AttemptFailed {
                            reason: plan.reason,
                            consecutive: self.consecutive_failures,
                        });
                        plan
                    }
                    Step::Handshake(Handshake::Established(session)) => {
                        self.consecutive_failures = 0;
                        self.health.set_registered(true);
                        let ended = self.run_session(session, &mut stop).await;
                        self.health.set_registered(false);
                        match ended {
                            SessionEnd::Stopped => return,
                            SessionEnd::Conflict => {
                                self.request_exit(StopCause::InstanceConflict);
                                return;
                            }
                            SessionEnd::Lost { plan, lasted } => {
                                // 会话持续满 60 秒，退避清零；否则继续往上数。
                                if lasted >= self.t.backoff_reset_after {
                                    self.backoff.reset();
                                }
                                // 孤儿计时在会话结束的这一刻重新开始，不是从进程启动算。
                                orphan_deadline =
                                    self.orphan_timeout.map(|after| Instant::now() + after);
                                self.emit(Event::SessionLost {
                                    reason: plan.reason,
                                });
                                plan
                            }
                        }
                    }
                };

                let fixed = plan.wait.fixed(&self.t);
                let delay = match fixed {
                    Some(delay) => delay,
                    None => self.backoff.next_delay(&self.t),
                };
                self.emit(Event::RetryScheduled {
                    delay,
                    fixed: fixed.is_some(),
                    reason: plan.reason,
                });
                tokio::select! {
                    biased;
                    _ = stop_requested(&mut stop) => return,
                    _ = until(orphan_deadline) => {
                        self.request_exit(StopCause::OrphanTimeout);
                        return;
                    }
                    _ = sleep(delay) => {}
                }
            }
        }

        /// 一次完整的连接与握手：连接 → 等 `challenge`（什么都不写）→ `register` → 等 `ack` 并校验服务端的证明。
        async fn handshake(&mut self) -> Handshake {
            self.emit(Event::Connecting);
            let stream = match timeout(self.t.connect, connect_register_socket(&self.socket)).await
            {
                Ok(Ok(stream)) => stream,
                _ => return Handshake::Retry(Plan::backoff(RetryReason::ConnectFailed)),
            };
            let (read_half, write_half) = stream.into_split();
            let mut conn = Conn {
                reader: FrameReader::new(read_half),
                writer: write_half,
            };

            // 第一步：服务端先说话。这一步没有任何写操作（包括收到 `error` 的时候）。
            let (conn_id, challenge_nonce) =
                match timeout(self.t.challenge_wait, Self::read_challenge(&mut conn)).await {
                    Err(_) => {
                        return Handshake::Retry(Plan::backoff(RetryReason::ChallengeTimeout));
                    }
                    Ok(Err(plan)) => return Handshake::Retry(plan),
                    Ok(Ok(challenge)) => challenge,
                };
            self.emit(Event::ChallengeReceived);

            // 第二步：`register`，收到 `challenge` 之后 1 秒内发出。
            let phase = self.health.view().phase.as_str();
            let proof = register_proof(&self.key, &conn_id, &challenge_nonce, &self.identity);
            let resume = self.ever_registered;
            let Some(register) = encode_frame(&RegisterFrame {
                v: FRAME_VERSION,
                op: "register",
                nonce: &challenge_nonce,
                instance_id: &self.identity.instance_id,
                pid: self.identity.pid,
                version: &self.identity.version,
                contract_version: &self.identity.contract_version,
                data_port: self.identity.data_port,
                admin_port: self.identity.admin_port,
                started_at_ms: self.identity.started_at_ms,
                phase,
                resume,
                proof: &proof,
            }) else {
                return Handshake::Retry(Plan::backoff(RetryReason::WriteFailed));
            };
            if !conn.send(&register, self.t.register_within).await {
                return Handshake::Retry(Plan::backoff(RetryReason::WriteFailed));
            }
            self.emit(Event::RegisterSent { resume });

            // 第三步：`ack`，5 秒内。
            let wait_for_ack = self.read_ack(&mut conn, &conn_id, &challenge_nonce);
            let (session_id, heartbeat_ms) = match timeout(self.t.ack_wait, wait_for_ack).await {
                // 等 `ack` 超时：不发 `error`，直接关闭连接。
                Err(_) => return Handshake::Retry(Plan::backoff(RetryReason::AckTimeout)),
                Ok(Err(handshake)) => return handshake,
                Ok(Ok(acked)) => acked,
            };
            let heartbeat_every = heartbeat_ms
                .map_or(self.t.heartbeat_default, Duration::from_millis)
                .clamp(self.t.heartbeat_min, self.t.heartbeat_max);
            self.ever_registered = true;
            self.emit(Event::Registered { session_id, resume });
            Handshake::Established(Session {
                conn,
                heartbeat_every,
            })
        }

        /// 等 `challenge`。**不写任何东西**：服务端先发的 `error`（例如 `busy`）、别的帧、读错误都只决定怎么重连。
        pub(super) async fn read_challenge<R: AsyncRead + Unpin>(
            conn: &mut Conn<R>,
        ) -> Result<(String, String), Plan> {
            loop {
                match conn.reader.next().await {
                    Read::Frame(ServerFrame::Challenge { conn_id, nonce }) => {
                        return Ok((conn_id, nonce));
                    }
                    Read::Frame(ServerFrame::Error { code }) => {
                        // `instance_conflict` 只可能是对 `register` 的回答；还没发 `register` 就收到，按普通错误处理，
                        // 不因此退出。
                        return Err(Plan::server(code));
                    }
                    // 没有 ack 的会话上收到 `set_admin_token`：不处理并关闭连接（不写：还没收到 challenge）。
                    Read::Frame(ServerFrame::SetAdminToken { .. }) => {
                        return Err(Plan::backoff(RetryReason::BadFrame));
                    }
                    Read::Frame(_) => {}
                    Read::Eof => return Err(Plan::backoff(RetryReason::Disconnected)),
                    Read::Io => return Err(Plan::backoff(RetryReason::IoError)),
                    // 收到 `challenge` 之前不写任何字节：连 `error` 也不发。
                    Read::Bad => return Err(Plan::backoff(RetryReason::BadFrame)),
                }
            }
        }

        /// 等 `ack` 并校验 `ack.proof`。返回 `(session_id, heartbeat_ms)`；其余结局都是 [`Handshake`]。
        pub(super) async fn read_ack<R: AsyncRead + Unpin>(
            &self,
            conn: &mut Conn<R>,
            conn_id: &str,
            challenge_nonce: &str,
        ) -> Result<(String, Option<u64>), Handshake> {
            loop {
                match conn.reader.next().await {
                    Read::Frame(ServerFrame::Ack {
                        session_id,
                        proof,
                        heartbeat_ms,
                    }) => {
                        if ack_proof_matches(
                            &self.key,
                            conn_id,
                            challenge_nonce,
                            &session_id,
                            &proof,
                        ) {
                            return Ok((session_id, heartbeat_ms));
                        }
                        // 服务端读不到发现文件（或是冒充者）：尽力发 `proof_invalid`，关闭，不发心跳，按默认退避重连，
                        // 仍继续服务。
                        self.send_error(conn, "proof_invalid", "ack proof mismatch")
                            .await;
                        return Err(Handshake::Retry(Plan::backoff(
                            RetryReason::AckProofInvalid,
                        )));
                    }
                    Read::Frame(ServerFrame::Error { code }) => {
                        return Err(if code == ServerError::InstanceConflict {
                            Handshake::Conflict
                        } else {
                            Handshake::Retry(Plan::server(code))
                        });
                    }
                    // `ack` 之前的 `set_admin_token`：不处理并关闭连接。
                    Read::Frame(ServerFrame::SetAdminToken { .. }) => {
                        self.send_error(conn, "bad_frame", "set_admin_token before ack")
                            .await;
                        return Err(Handshake::Retry(Plan::backoff(RetryReason::BadFrame)));
                    }
                    Read::Frame(_) => {}
                    Read::Eof => {
                        return Err(Handshake::Retry(Plan::backoff(RetryReason::Disconnected)));
                    }
                    Read::Io => return Err(Handshake::Retry(Plan::backoff(RetryReason::IoError))),
                    Read::Bad => {
                        self.send_error(conn, "bad_frame", "invalid frame").await;
                        return Err(Handshake::Retry(Plan::backoff(RetryReason::BadFrame)));
                    }
                }
            }
        }

        /// 尽力发一个 `error` 帧（至多 `bye_budget`，不等应答）。只在已经收到过 `challenge` 之后调用。
        async fn send_error<R>(
            &self,
            conn: &mut Conn<R>,
            code: &'static str,
            message: &'static str,
        ) {
            if let Some(bytes) = error_frame(code, message) {
                let _ = conn.send(&bytes, self.t.bye_budget).await;
            }
        }

        /// 已建立的会话：每 `heartbeat_every`（±10%）一个 `heartbeat`，立即发第一个；处理 `heartbeat_ack`、
        /// `set_admin_token`、`error`。失联判定：连续 `heartbeat_misses` 次心跳没有在 `heartbeat_ack_wait` 内得到应答，
        /// 或任何读写错误、EOF。
        async fn run_session(
            &mut self,
            session: Session,
            stop: &mut watch::Receiver<StopMode>,
        ) -> SessionEnd {
            let Session {
                mut conn,
                heartbeat_every,
            } = session;
            let started = Instant::now();
            let lost = |reason: RetryReason, wait: Wait| SessionEnd::Lost {
                plan: Plan { reason, wait },
                lasted: started.elapsed(),
            };
            let mut seq: u64 = 0;
            let mut highest_acked: u64 = 0;
            // 还没有得到应答的心跳：(seq, 应答截止时间)。
            let mut outstanding: VecDeque<(u64, Instant)> = VecDeque::new();
            let mut misses: u32 = 0;
            let mut next_beat = started;
            loop {
                enum Wake {
                    Stop(StopMode),
                    Beat,
                    AckDue,
                    Frame(Read),
                }
                let ack_due = outstanding.front().map(|(_, due)| *due);
                let wake = tokio::select! {
                    biased;
                    mode = stop_requested(stop) => Wake::Stop(mode),
                    _ = sleep_until(next_beat) => Wake::Beat,
                    _ = until(ack_due) => Wake::AckDue,
                    frame = conn.reader.next() => Wake::Frame(frame),
                };
                match wake {
                    Wake::Stop(mode) => {
                        if mode == StopMode::Bye {
                            self.send_bye(&mut conn).await;
                        }
                        return SessionEnd::Stopped;
                    }
                    Wake::Beat => {
                        seq += 1;
                        let view = self.health.view();
                        let Some(beat) = encode_frame(&HeartbeatFrame {
                            v: FRAME_VERSION,
                            op: "heartbeat",
                            seq,
                            phase: view.phase.as_str(),
                            ready: view.ready,
                        }) else {
                            return lost(RetryReason::WriteFailed, Wait::Backoff);
                        };
                        if !conn.send(&beat, self.t.heartbeat_ack_wait).await {
                            return lost(RetryReason::WriteFailed, Wait::Backoff);
                        }
                        let now = Instant::now();
                        outstanding.push_back((seq, now + self.t.heartbeat_ack_wait));
                        next_beat = now
                            + self
                                .t
                                .jitter
                                .apply(heartbeat_every, self.t.heartbeat_jitter);
                        self.emit(Event::HeartbeatSent { seq });
                    }
                    Wake::AckDue => {
                        outstanding.pop_front();
                        misses += 1;
                        if misses >= self.t.heartbeat_misses {
                            return lost(RetryReason::HeartbeatLost, Wait::Backoff);
                        }
                    }
                    Wake::Frame(Read::Frame(frame)) => match frame {
                        ServerFrame::HeartbeatAck { seq: acked } => {
                            // 只认我们发过、且比已确认的更新的 seq；它也代表更早的心跳已经到达对端。
                            if acked > highest_acked && acked <= seq {
                                highest_acked = acked;
                                outstanding.retain(|(pending, _)| *pending > acked);
                                misses = 0;
                                self.emit(Event::HeartbeatAcked { seq: acked });
                            }
                        }
                        ServerFrame::Error { code } => {
                            if code == ServerError::InstanceConflict {
                                return SessionEnd::Conflict;
                            }
                            return lost(RetryReason::Server(code), code.wait());
                        }
                        ServerFrame::SetAdminToken { token } => {
                            if let Err(reason) = self.apply_admin_token(&mut conn, token).await {
                                return lost(reason, Wait::Backoff);
                            }
                        }
                        // 会话里多出来的 `challenge`、`ack` 与未知 `op`：忽略。
                        ServerFrame::Challenge { .. }
                        | ServerFrame::Ack { .. }
                        | ServerFrame::Ignored => {}
                    },
                    Wake::Frame(Read::Eof) => {
                        return lost(RetryReason::Disconnected, Wait::Backoff);
                    }
                    Wake::Frame(Read::Io) => return lost(RetryReason::IoError, Wait::Backoff),
                    Wake::Frame(Read::Bad) => {
                        self.send_error(&mut conn, "bad_frame", "invalid frame")
                            .await;
                        return lost(RetryReason::BadFrame, Wait::Backoff);
                    }
                }
            }
        }

        /// `set_admin_token`（规范 §2.10.6）：只在 `ack.proof` 已通过的会话上（`run_session` 就是）。`token` 必须是 64 位小写
        /// hex，每 10 秒至多接受一次；通过就原子替换（旧 token 立即失效）并回 `admin_token_ack`。校验失败或超频：
        /// 不替换，token 保持原值，先尽力发 `error`（`bad_frame`）再关闭该会话。
        ///
        /// 替换先于 `admin_token_ack` 的写：写失败时会话断开，token 已经是新的（agent24d 重连之后可以再设，
        /// 但受同一个 10 秒限制）。
        async fn apply_admin_token(
            &mut self,
            conn: &mut Conn,
            token: Option<Secret>,
        ) -> Result<(), RetryReason> {
            let parsed = token.as_ref().and_then(|token| {
                AdminSessionToken::from_launcher_secret(token.expose().as_bytes()).ok()
            });
            let now = Instant::now();
            let rate_ok = self.last_token_set.is_none_or(|last| {
                now.saturating_duration_since(last) >= self.t.admin_token_interval
            });
            let rejection = match (&parsed, rate_ok) {
                (None, _) => Some(TokenRejection::Invalid),
                (Some(_), false) => Some(TokenRejection::RateLimited),
                (Some(_), true) => None,
            };
            if let (Some(new_token), None) = (parsed, rejection) {
                self.admin_token.replace(new_token);
                self.last_token_set = Some(now);
                self.emit(Event::AdminTokenReplaced);
                let acked = match encode_frame(&AdminTokenAckFrame {
                    v: FRAME_VERSION,
                    op: "admin_token_ack",
                }) {
                    Some(bytes) => conn.send(&bytes, self.t.heartbeat_ack_wait).await,
                    None => false,
                };
                return if acked {
                    Ok(())
                } else {
                    Err(RetryReason::WriteFailed)
                };
            }
            if let Some(reason) = rejection {
                self.emit(Event::AdminTokenRejected { reason });
            }
            self.send_error(conn, "bad_frame", "invalid set_admin_token")
                .await;
            Err(RetryReason::AdminTokenRejected)
        }

        /// 优雅停止时在已有的会话上尽力发 `bye`：至多 `bye_budget`，不等应答。
        async fn send_bye(&self, conn: &mut Conn) {
            let Some(bytes) = encode_frame(&ByeFrame {
                v: FRAME_VERSION,
                op: "bye",
                reason: "shutdown",
            }) else {
                return;
            };
            if conn.send(&bytes, self.t.bye_budget).await {
                self.emit(Event::ByeSent);
            }
        }
    }
}

#[cfg(test)]
#[path = "register_tests.rs"]
mod tests;

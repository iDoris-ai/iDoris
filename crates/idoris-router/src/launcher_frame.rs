//! `idoris serve --launcher-stdin` 的帧（接口规范 §2.7.1，修订 1.6）。
//!
//! 字节层：一行 UTF-8 JSON，以 LF 结尾，总长（含 LF）≤ 4096 字节，10 秒内读完；读到第一个 LF 就停，
//! 之后不再读 stdin（launcher 写完就关 stdin 是预期行为，EOF 不代表 launcher 已不在）。
//!
//! 解析严格：未知字段一律拒绝（launcher 与 iDoris 锁版本一起发，字段拼错而静默失效比直接失败更危险）。
//! 任何错误都只给出固定文案
//! `launcher 帧无效（<原因码>[：<字段名>]）`，**绝不回显字段值**：
//! 解析库的默认错误信息会带输入片段（里面有 Admin token），所以这里不把任何 `serde_json::Error` 带出去，
//! 一律映射成下面的 [`FrameErrorCode`]。
//!
//! 这个模块只负责读、校验、存进配置结构：`register_socket`、`orphan_timeout_s` 由 `register`（S2，向 agent24d 登记、
//! 孤儿计时）使用，`engine.*` 要等 S3/S5 才使用。规范要求的跨字段校验（`register_socket` 出现则 `discovery_path`
//! 必填、`orphan_timeout_s` 非 0 则 `register_socket` 必填）在读帧时就做，所以登记与孤儿计时只会在发现文件存在时启动。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::admin::AdminSessionToken;
use crate::engine_kind::SUPPORTED_MANAGED_ENGINES;
use crate::instance_nonce::InstanceNonce;

/// 帧总长上限（含结尾的 LF）。
pub const MAX_FRAME_BYTES: usize = 4096;
/// 帧版本，`v` 必须等于它。
pub const FRAME_VERSION: u64 = 1;
/// 读完整帧的时限。
pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// `discovery_path` 的字节上限（与发现文件模块校验路径用的是同一个常量）。
pub const MAX_DISCOVERY_PATH_BYTES: usize = crate::discovery::MAX_PATH_BYTES;
/// `register_socket` 的字节上限（macOS `sun_path` 为 104 字节含 NUL）。
pub const MAX_REGISTER_SOCKET_BYTES: usize = 103;
/// `orphan_timeout_s` 的上限（一天）。
pub const MAX_ORPHAN_TIMEOUT_S: u64 = 86_400;
/// `engine.executables` 里每个路径的字节上限。规范只说「绝对路径」，没有给上限；这里取与 `discovery_path` 相同。
pub const MAX_EXECUTABLE_PATH_BYTES: usize = 1024;

const TOP_LEVEL_FIELDS: &[&str] = &[
    "v",
    "admin_token",
    "discovery_path",
    "register_socket",
    "orphan_timeout_s",
    "instance_nonce",
    "engine",
];
const ENGINE_FIELDS: &[&str] = &["executables", "stop_on_exit"];

/// 规范 §2.7.1 列出的六个原因码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameErrorCode {
    FrameTooLong,
    FrameNotJson,
    UnsupportedFrameVersion,
    FieldInvalid,
    UnknownField,
    Timeout,
}

impl FrameErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FrameTooLong => "frame_too_long",
            Self::FrameNotJson => "frame_not_json",
            Self::UnsupportedFrameVersion => "unsupported_frame_version",
            Self::FieldInvalid => "field_invalid",
            Self::UnknownField => "unknown_field",
            Self::Timeout => "timeout",
        }
    }
}

/// 帧无效。`Display` 就是规范要求的固定文案；字段名只来自本模块的字面量，
/// 或经 [`plain_field_name`] 过滤的未知字段名，**从不**包含字段值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    code: FrameErrorCode,
    field: Option<Cow<'static, str>>,
}

impl FrameError {
    fn new(code: FrameErrorCode) -> Self {
        Self { code, field: None }
    }

    fn at(code: FrameErrorCode, field: &'static str) -> Self {
        Self {
            code,
            field: Some(Cow::Borrowed(field)),
        }
    }

    /// 未知字段：名字只在是「普通名字」时才带出（小写字母开头，之后小写字母、数字、下划线，≤ 32 个字符），
    /// 不是的话（控制字符、过长、像一个 token、以数字或下划线开头）只给原因码。
    fn unknown(name: &str, parent: Option<&'static str>) -> Self {
        let field = plain_field_name(name).map(|name| match parent {
            Some(parent) => Cow::Owned(format!("{parent}.{name}")),
            None => Cow::Owned(name.to_string()),
        });
        Self {
            code: FrameErrorCode::UnknownField,
            field,
        }
    }

    pub fn code(&self) -> FrameErrorCode {
        self.code
    }

    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.field {
            Some(field) => write!(f, "launcher 帧无效（{}：{field}）", self.code.as_str()),
            None => write!(f, "launcher 帧无效（{}）", self.code.as_str()),
        }
    }
}

impl std::error::Error for FrameError {}

/// 小写字母开头、之后小写字母数字下划线、≤ 32 字符。64 位 hex 的 token 当字段名也过不了这一关。
fn plain_field_name(name: &str) -> Option<&str> {
    crate::engine_kind::is_valid_engine_kind(name).then_some(name)
}

/// `engine.stop_on_exit`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopOnExit {
    /// 退出时停掉自己拉起的引擎（缺省）。
    #[default]
    Owned,
    /// 正常退出也保留引擎与所有权记录，由下一个 iDoris 接管。
    Never,
}

/// 帧里的 `engine` 对象。S3/S5 才使用。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EngineFrame {
    /// 引擎种类到 launcher 已校验过的可执行文件（视为可信）。键只可能是 [`SUPPORTED_MANAGED_ENGINES`] 里的值。
    pub executables: BTreeMap<String, PathBuf>,
    pub stop_on_exit: StopOnExit,
}

/// 校验过的 launcher 帧。`Debug` 不会渲染 token 与 nonce（两个类型各自脱敏）。
#[derive(Debug)]
pub struct LauncherFrame {
    pub admin_token: AdminSessionToken,
    /// 缺省则不写发现文件。
    pub discovery_path: Option<PathBuf>,
    /// 登记用的 UDS 路径（`register` 使用）；缺省则不登记。
    pub register_socket: Option<PathBuf>,
    /// 0 = 不因「无登记会话」退出；非 0 则超过这么多秒没有已 `ack` 的登记会话就以退出码 4 优雅停止（`register` 使用）。
    pub orphan_timeout_s: u32,
    /// 规范约定只给确定性测试用（生产不给这个字段，代码不拒绝）；缺省由 iDoris 自己生成。
    pub instance_nonce: Option<InstanceNonce>,
    pub engine: EngineFrame,
}

/// 启动输入：帧里除了 Admin token 之外的全部字段。不是 launcher 托管的启动（旧的 `--admin-token-stdin` 或没有
/// stdin 帧）时是 [`Default`]：没有发现文件、不登记、没有孤儿计时、引擎配置缺省。
#[derive(Debug, Default)]
pub struct LauncherConfig {
    pub discovery_path: Option<PathBuf>,
    pub register_socket: Option<PathBuf>,
    pub orphan_timeout_s: u32,
    pub instance_nonce: Option<InstanceNonce>,
    pub engine: EngineFrame,
}

impl LauncherFrame {
    /// 取出 Admin token，其余字段作为启动输入。
    pub fn split(self) -> (AdminSessionToken, LauncherConfig) {
        (
            self.admin_token,
            LauncherConfig {
                discovery_path: self.discovery_path,
                register_socket: self.register_socket,
                orphan_timeout_s: self.orphan_timeout_s,
                instance_nonce: self.instance_nonce,
                engine: self.engine,
            },
        )
    }
}

/// 读到第一个 LF 为止，不含 LF。总长（含 LF）≤ [`MAX_FRAME_BYTES`]。
///
/// - 4096 字节内没有 LF：`frame_too_long`；
/// - 读到 EOF 还没有 LF（含空输入）：规范没有给这种情况专门的原因码，归入 `frame_not_json`
///   （这一行不是一个完整的 JSON 帧）。
pub fn read_frame_line(reader: &mut impl BufRead) -> Result<Vec<u8>, FrameError> {
    let mut line = Vec::new();
    let mut limited = (&mut *reader).take(MAX_FRAME_BYTES as u64);
    limited
        .read_until(b'\n', &mut line)
        .map_err(|_| FrameError::new(FrameErrorCode::FrameNotJson))?;
    if line.last() == Some(&b'\n') {
        line.pop();
        return Ok(line);
    }
    if line.len() >= MAX_FRAME_BYTES {
        return Err(FrameError::new(FrameErrorCode::FrameTooLong));
    }
    Err(FrameError::new(FrameErrorCode::FrameNotJson))
}

/// 读一帧并校验。
pub fn read_launcher_frame(reader: &mut impl BufRead) -> Result<LauncherFrame, FrameError> {
    parse_frame_line(&read_frame_line(reader)?)
}

/// 等取消的间隔：阻塞的读不能被打断，所以每隔这么久看一眼取消标志（H1：停止信号不能等读帧的 10 秒自己结束，launcher 17 秒就 SIGKILL）。
pub const CANCEL_POLL: Duration = Duration::from_millis(50);

/// [`wait_on_thread`] 的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Waited<T> {
    /// 读完了。
    Done(T),
    /// 超过了时限。
    TimedOut,
    /// 被取消（停止信号到了）。
    Cancelled,
    /// 读线程起不来，或没给结果就结束了（panic）。
    Disconnected,
}

/// 在独立线程里做一次阻塞的读（阻塞的 stdin 读不能被打断），等它做完、超过 `timeout`，或 `cancel()` 为真（每 [`CANCEL_POLL`] 看一次，
/// 进入时先看一次）。超时或取消之后读线程留在后台，随进程退出一起结束（调用方马上退出）：它不会再被读到，也不会再碰 stdin 以外的东西。
pub fn wait_on_thread<T, F>(
    name: &str,
    read: F,
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
) -> Waited<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if cancel() {
        return Waited::Cancelled;
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _ = sender.send(read());
        });
    if spawned.is_err() {
        return Waited::Disconnected;
    }
    let started = std::time::Instant::now();
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        match receiver.recv_timeout(remaining.min(CANCEL_POLL)) {
            Ok(result) => return Waited::Done(result),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Waited::Disconnected,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel() {
                    return Waited::Cancelled;
                }
                if started.elapsed() >= timeout {
                    return Waited::TimedOut;
                }
            }
        }
    }
}

/// [`read_with_timeout`]，可以被取消：`Ok(None)` 是被取消（停止信号到了，没有读到帧也不是帧有问题），其余同它。
pub fn read_with_timeout_or_cancel<F>(
    read: F,
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
) -> Result<Option<LauncherFrame>, FrameError>
where
    F: FnOnce() -> Result<LauncherFrame, FrameError> + Send + 'static,
{
    match wait_on_thread("idoris-launcher-frame", read, timeout, cancel) {
        Waited::Done(result) => result.map(Some),
        Waited::Cancelled => Ok(None),
        Waited::TimedOut => Err(FrameError::new(FrameErrorCode::Timeout)),
        Waited::Disconnected => Err(FrameError::new(FrameErrorCode::FrameNotJson)),
    }
}

/// 在独立线程里读（阻塞的 stdin 读不能被打断），超过 `timeout` 报 `timeout`。读线程留在后台，
/// 随进程退出一起结束（调用方马上以退出码 1 退出）。
pub fn read_with_timeout<F>(read: F, timeout: Duration) -> Result<LauncherFrame, FrameError>
where
    F: FnOnce() -> Result<LauncherFrame, FrameError> + Send + 'static,
{
    // 永不取消，所以 `Ok(None)` 不会出现；真出现了就当作超时，不 panic。
    read_with_timeout_or_cancel(read, timeout, &|| false)?
        .ok_or_else(|| FrameError::new(FrameErrorCode::Timeout))
}

/// 生产入口：从进程的 stdin 读一帧，10 秒内；`cancel()` 为真（停止信号到了）就放弃，返回 `Ok(None)`。
pub fn read_launcher_frame_from_stdin_or_cancel(
    cancel: &dyn Fn() -> bool,
) -> Result<Option<LauncherFrame>, FrameError> {
    read_with_timeout_or_cancel(
        || read_launcher_frame(&mut std::io::stdin().lock()),
        FRAME_READ_TIMEOUT,
        cancel,
    )
}

fn valid_absolute_path(value: &str, max_bytes: usize) -> bool {
    value.starts_with('/') && value.len() <= max_bytes && !value.contains('\0')
}

fn optional_path(
    object: &Map<String, Value>,
    name: &'static str,
    max_bytes: usize,
) -> Result<Option<PathBuf>, FrameError> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::String(path)) if valid_absolute_path(path, max_bytes) => {
            Ok(Some(PathBuf::from(path)))
        }
        Some(_) => Err(FrameError::at(FrameErrorCode::FieldInvalid, name)),
    }
}

fn parse_engine(engine: &Map<String, Value>) -> Result<EngineFrame, FrameError> {
    if let Some(unknown) = engine
        .keys()
        .find(|key| !ENGINE_FIELDS.contains(&key.as_str()))
    {
        return Err(FrameError::unknown(unknown, Some("engine")));
    }
    let invalid = |field: &'static str| FrameError::at(FrameErrorCode::FieldInvalid, field);
    let mut executables = BTreeMap::new();
    match engine.get("executables") {
        None => {}
        Some(Value::Object(map)) => {
            for (kind, path) in map {
                let supported = SUPPORTED_MANAGED_ENGINES.contains(&kind.as_str());
                match path {
                    Value::String(path)
                        if supported && valid_absolute_path(path, MAX_EXECUTABLE_PATH_BYTES) =>
                    {
                        executables.insert(kind.clone(), PathBuf::from(path));
                    }
                    _ => return Err(invalid("engine.executables")),
                }
            }
        }
        Some(_) => return Err(invalid("engine.executables")),
    }
    let stop_on_exit = match engine.get("stop_on_exit") {
        None => StopOnExit::default(),
        Some(Value::String(value)) if value == "owned" => StopOnExit::Owned,
        Some(Value::String(value)) if value == "never" => StopOnExit::Never,
        Some(_) => return Err(invalid("engine.stop_on_exit")),
    };
    Ok(EngineFrame {
        executables,
        stop_on_exit,
    })
}

/// 解析并校验一帧（不含结尾的 LF）。错误顺序固定：JSON 形状 → `v` → 未知字段 → 各字段（按规范表顺序）→ 跨字段规则。
pub fn parse_frame_line(line: &[u8]) -> Result<LauncherFrame, FrameError> {
    // serde_json 的错误信息会带输入片段，这里一律丢掉，只留原因码。
    let value: Value =
        serde_json::from_slice(line).map_err(|_| FrameError::new(FrameErrorCode::FrameNotJson))?;
    let Value::Object(object) = value else {
        return Err(FrameError::new(FrameErrorCode::FrameNotJson));
    };

    match object.get("v") {
        None => return Err(FrameError::at(FrameErrorCode::FieldInvalid, "v")),
        Some(version) => match version.as_u64() {
            Some(FRAME_VERSION) => {}
            Some(_) => return Err(FrameError::new(FrameErrorCode::UnsupportedFrameVersion)),
            None if version.is_i64() => {
                return Err(FrameError::new(FrameErrorCode::UnsupportedFrameVersion));
            }
            None => return Err(FrameError::at(FrameErrorCode::FieldInvalid, "v")),
        },
    }
    if let Some(unknown) = object
        .keys()
        .find(|key| !TOP_LEVEL_FIELDS.contains(&key.as_str()))
    {
        return Err(FrameError::unknown(unknown, None));
    }

    let admin_token = match object.get("admin_token") {
        Some(Value::String(token)) => AdminSessionToken::from_launcher_secret(token.as_bytes())
            .map_err(|_| FrameError::at(FrameErrorCode::FieldInvalid, "admin_token"))?,
        _ => return Err(FrameError::at(FrameErrorCode::FieldInvalid, "admin_token")),
    };
    let discovery_path = optional_path(&object, "discovery_path", MAX_DISCOVERY_PATH_BYTES)?;
    let register_socket = optional_path(&object, "register_socket", MAX_REGISTER_SOCKET_BYTES)?;
    let orphan_timeout_s = match object.get("orphan_timeout_s") {
        None => 0,
        Some(value) => match value.as_u64() {
            Some(seconds) if seconds <= MAX_ORPHAN_TIMEOUT_S => seconds as u32,
            _ => {
                return Err(FrameError::at(
                    FrameErrorCode::FieldInvalid,
                    "orphan_timeout_s",
                ));
            }
        },
    };
    let instance_nonce = match object.get("instance_nonce") {
        None => None,
        Some(Value::String(nonce)) => Some(
            InstanceNonce::from_hex(nonce)
                .ok_or_else(|| FrameError::at(FrameErrorCode::FieldInvalid, "instance_nonce"))?,
        ),
        Some(_) => {
            return Err(FrameError::at(
                FrameErrorCode::FieldInvalid,
                "instance_nonce",
            ));
        }
    };
    let engine = match object.get("engine") {
        None => EngineFrame::default(),
        Some(Value::Object(engine)) => parse_engine(engine)?,
        Some(_) => return Err(FrameError::at(FrameErrorCode::FieldInvalid, "engine")),
    };

    // 跨字段规则：报的是「出现了、但前提不满足」的那个字段。
    if register_socket.is_some() && discovery_path.is_none() {
        return Err(FrameError::at(
            FrameErrorCode::FieldInvalid,
            "register_socket",
        ));
    }
    if orphan_timeout_s != 0 && register_socket.is_none() {
        return Err(FrameError::at(
            FrameErrorCode::FieldInvalid,
            "orphan_timeout_s",
        ));
    }

    Ok(LauncherFrame {
        admin_token,
        discovery_path,
        register_socket,
        orphan_timeout_s,
        instance_nonce,
        engine,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const NONCE: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn parse(value: serde_json::Value) -> Result<LauncherFrame, FrameError> {
        parse_frame_line(value.to_string().as_bytes())
    }

    fn minimal() -> serde_json::Value {
        json!({"v": 1, "admin_token": TOKEN})
    }

    fn with(extra: serde_json::Value) -> serde_json::Value {
        let mut base = minimal();
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        base
    }

    #[test]
    fn the_spec_example_frame_parses_and_every_field_lands_in_the_struct() {
        let line = format!(
            "{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"discovery_path\":\"/Users/x/.agent24/run/idoris.json\",\"register_socket\":\"/Users/x/.agent24/run/idoris-register.sock\",\"orphan_timeout_s\":900}}"
        );
        let frame = parse_frame_line(line.as_bytes()).unwrap();
        assert!(frame.admin_token.matches(TOKEN));
        assert_eq!(
            frame.discovery_path.as_deref(),
            Some(std::path::Path::new("/Users/x/.agent24/run/idoris.json"))
        );
        assert_eq!(
            frame.register_socket.as_deref(),
            Some(std::path::Path::new(
                "/Users/x/.agent24/run/idoris-register.sock"
            ))
        );
        assert_eq!(frame.orphan_timeout_s, 900);
        assert!(frame.instance_nonce.is_none());
        assert_eq!(frame.engine, EngineFrame::default());
        assert_eq!(frame.engine.stop_on_exit, StopOnExit::Owned);
    }

    #[test]
    fn a_minimal_frame_needs_only_v_and_the_admin_token() {
        let frame = parse(minimal()).unwrap();
        assert!(frame.discovery_path.is_none());
        assert!(frame.register_socket.is_none());
        assert_eq!(frame.orphan_timeout_s, 0);
    }

    #[test]
    fn the_full_engine_object_and_a_test_nonce_are_accepted() {
        let frame = parse(with(json!({
            "instance_nonce": NONCE,
            "engine": {"executables": {"omlx": "/Applications/oMLX.app/Contents/MacOS/omlx-cli"}, "stop_on_exit": "never"}
        })))
        .unwrap();
        assert_eq!(frame.instance_nonce.unwrap().expose_hex(), NONCE);
        assert_eq!(frame.engine.stop_on_exit, StopOnExit::Never);
        assert_eq!(
            frame.engine.executables.get("omlx").unwrap(),
            std::path::Path::new("/Applications/oMLX.app/Contents/MacOS/omlx-cli")
        );
        let owned = parse(with(json!({"engine": {"stop_on_exit": "owned"}}))).unwrap();
        assert_eq!(owned.engine.stop_on_exit, StopOnExit::Owned);
        let empty = parse(with(json!({"engine": {}}))).unwrap();
        assert_eq!(empty.engine, EngineFrame::default());
    }

    fn assert_error(
        result: Result<LauncherFrame, FrameError>,
        code: FrameErrorCode,
        field: Option<&str>,
    ) {
        let error = result.unwrap_err();
        assert_eq!(error.code(), code, "{error}");
        assert_eq!(error.field(), field, "{error}");
    }

    #[test]
    fn not_json_and_non_object_json_are_frame_not_json() {
        for line in [
            b"".as_slice(),
            b"not json".as_slice(),
            b"{\"v\":1".as_slice(),
            b"[]".as_slice(),
            b"\"x\"".as_slice(),
            b"7".as_slice(),
            b"null".as_slice(),
            b"\xff\xfe{}".as_slice(),
        ] {
            assert_error(parse_frame_line(line), FrameErrorCode::FrameNotJson, None);
        }
    }

    #[test]
    fn the_version_is_checked_before_anything_else_in_the_object() {
        assert_error(
            parse(json!({"admin_token": TOKEN})),
            FrameErrorCode::FieldInvalid,
            Some("v"),
        );
        for version in [json!(2), json!(0), json!(-1), json!(99999)] {
            assert_error(
                parse(json!({"v": version, "admin_token": TOKEN, "future_field": 1})),
                FrameErrorCode::UnsupportedFrameVersion,
                None,
            );
        }
        for version in [json!("1"), json!(1.0), json!(true), json!(null), json!([1])] {
            assert_error(
                parse(json!({"v": version, "admin_token": TOKEN})),
                FrameErrorCode::FieldInvalid,
                Some("v"),
            );
        }
    }

    #[test]
    fn unknown_fields_are_rejected_at_the_top_level_and_inside_engine() {
        assert_error(
            parse(with(json!({"discovery_pth": "/a/b"}))),
            FrameErrorCode::UnknownField,
            Some("discovery_pth"),
        );
        assert_error(
            parse(with(json!({"engine": {"executable": {}}}))),
            FrameErrorCode::UnknownField,
            Some("engine.executable"),
        );
    }

    #[test]
    fn an_unknown_field_name_that_is_not_plain_is_not_echoed() {
        // 过长、含大写或标点、或者整个就是一个 token：只给原因码，不带名字。
        for name in [
            TOKEN.to_string(),
            "Admin-Token".to_string(),
            "x".repeat(33),
            "bad\u{1b}[31mname".to_string(),
            "".to_string(),
        ] {
            let mut frame = minimal();
            frame[&name] = json!(1);
            let error = parse(frame).unwrap_err();
            assert_eq!(error.code(), FrameErrorCode::UnknownField);
            assert_eq!(error.field(), None, "{name:?}");
            assert_eq!(error.to_string(), "launcher 帧无效（unknown_field）");
        }
    }

    #[test]
    fn admin_token_must_be_exactly_64_lowercase_hex() {
        for token in [
            json!("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeF"),
            json!(&TOKEN[..63]),
            json!(format!("{TOKEN}0")),
            json!(format!("{TOKEN}\n")),
            json!(7),
            json!(null),
            json!(["x"]),
        ] {
            assert_error(
                parse(json!({"v": 1, "admin_token": token})),
                FrameErrorCode::FieldInvalid,
                Some("admin_token"),
            );
        }
    }

    #[test]
    fn path_fields_must_be_absolute_short_and_nul_free() {
        let long = format!("/{}", "a".repeat(MAX_DISCOVERY_PATH_BYTES));
        let exact = format!("/{}", "a".repeat(MAX_DISCOVERY_PATH_BYTES - 1));
        assert!(parse(with(json!({"discovery_path": exact}))).is_ok());
        for path in [
            json!("relative/idoris.json"),
            json!(""),
            json!(long),
            json!("/a\u{0}b"),
            json!(7),
            json!(null),
        ] {
            assert_error(
                parse(with(json!({"discovery_path": path}))),
                FrameErrorCode::FieldInvalid,
                Some("discovery_path"),
            );
        }
        let socket_103 = format!("/{}", "s".repeat(102));
        let socket_104 = format!("/{}", "s".repeat(103));
        assert!(
            parse(with(
                json!({"discovery_path": "/a/b", "register_socket": socket_103})
            ))
            .is_ok()
        );
        assert_error(
            parse(with(
                json!({"discovery_path": "/a/b", "register_socket": socket_104}),
            )),
            FrameErrorCode::FieldInvalid,
            Some("register_socket"),
        );
        assert_error(
            parse(with(
                json!({"discovery_path": "/a/b", "register_socket": "rel.sock"}),
            )),
            FrameErrorCode::FieldInvalid,
            Some("register_socket"),
        );
    }

    #[test]
    fn register_socket_requires_a_discovery_path_and_a_nonzero_orphan_timeout_requires_a_socket() {
        assert_error(
            parse(with(json!({"register_socket": "/a/b.sock"}))),
            FrameErrorCode::FieldInvalid,
            Some("register_socket"),
        );
        assert_error(
            parse(with(json!({"orphan_timeout_s": 900}))),
            FrameErrorCode::FieldInvalid,
            Some("orphan_timeout_s"),
        );
        assert_error(
            parse(with(
                json!({"discovery_path": "/a/b", "orphan_timeout_s": 1}),
            )),
            FrameErrorCode::FieldInvalid,
            Some("orphan_timeout_s"),
        );
        // 0 或缺省不需要 socket；有 socket 时才可以非 0。
        assert!(parse(with(json!({"orphan_timeout_s": 0}))).is_ok());
        let ok = parse(with(json!({
            "discovery_path": "/a/b",
            "register_socket": "/a/b.sock",
            "orphan_timeout_s": 900
        })))
        .unwrap();
        assert_eq!(ok.orphan_timeout_s, 900);
    }

    #[test]
    fn orphan_timeout_is_an_integer_between_0_and_86400() {
        let base = json!({"discovery_path": "/a/b", "register_socket": "/a/b.sock"});
        for seconds in [json!(0), json!(1), json!(86400)] {
            let mut frame = with(base.clone());
            frame["orphan_timeout_s"] = seconds;
            assert!(parse(frame).is_ok());
        }
        for seconds in [
            json!(86401),
            json!(-1),
            json!(1.5),
            json!(900.0),
            json!("900"),
            json!(null),
            json!(true),
        ] {
            let mut frame = with(base.clone());
            frame["orphan_timeout_s"] = seconds;
            assert_error(
                parse(frame),
                FrameErrorCode::FieldInvalid,
                Some("orphan_timeout_s"),
            );
        }
    }

    #[test]
    fn instance_nonce_must_be_64_lowercase_hex() {
        for nonce in [
            json!(NONCE.to_uppercase()),
            json!(&NONCE[..63]),
            json!(format!("{NONCE}0")),
            json!(1),
            json!(null),
        ] {
            assert_error(
                parse(with(json!({"instance_nonce": nonce}))),
                FrameErrorCode::FieldInvalid,
                Some("instance_nonce"),
            );
        }
    }

    #[test]
    fn engine_fields_are_validated_strictly() {
        assert_error(
            parse(with(json!({"engine": "omlx"}))),
            FrameErrorCode::FieldInvalid,
            Some("engine"),
        );
        for executables in [
            // 只认 omlx：别的引擎种类（即使格式合法）本版本拒绝。
            json!({"lmstudio": "/Applications/LM Studio"}),
            json!({"OMLX": "/a/b"}),
            json!({"omlx": "relative/omlx-cli"}),
            json!({"omlx": 7}),
            json!({"omlx": "/a\u{0}b"}),
            json!("/a/b"),
            json!(["/a/b"]),
        ] {
            assert_error(
                parse(with(json!({"engine": {"executables": executables}}))),
                FrameErrorCode::FieldInvalid,
                Some("engine.executables"),
            );
        }
        for stop in [json!("Owned"), json!("always"), json!(true), json!(null)] {
            assert_error(
                parse(with(json!({"engine": {"stop_on_exit": stop}}))),
                FrameErrorCode::FieldInvalid,
                Some("engine.stop_on_exit"),
            );
        }
    }

    #[test]
    fn errors_render_the_fixed_text_and_never_a_field_value() {
        let secret_values = [TOKEN, NONCE, "/Users/secret-user/.agent24/run/idoris.json"];
        let bad_frames = [
            json!({"v": 1, "admin_token": TOKEN.to_uppercase(), "instance_nonce": NONCE}),
            json!({"v": 2, "admin_token": TOKEN, "instance_nonce": NONCE}),
            json!({"v": 1, "admin_token": TOKEN, "bogus": NONCE}),
            json!({"v": 1, "admin_token": TOKEN, "instance_nonce": NONCE.to_uppercase()}),
            json!({"v": 1, "admin_token": TOKEN, "discovery_path": "/Users/secret-user/.agent24/run/idoris.json", "orphan_timeout_s": 5}),
            json!({"v": 1, "admin_token": TOKEN, "engine": {"stop_on_exit": TOKEN}}),
            json!({"v": 1, "admin_token": TOKEN, "engine": {"executables": {"omlx": NONCE}}}),
        ];
        for frame in bad_frames {
            let rendered = parse(frame).unwrap_err().to_string();
            assert!(rendered.starts_with("launcher 帧无效（"), "{rendered}");
            assert!(rendered.ends_with('）'), "{rendered}");
            for secret in secret_values {
                assert!(!rendered.contains(secret), "{rendered}");
                assert!(!rendered.contains(&secret[..8]), "{rendered}");
            }
        }
        // 语法错误同样不带输入片段，哪怕 JSON 在 token 中间断开。
        let broken = format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",,}}");
        let rendered = parse_frame_line(broken.as_bytes()).unwrap_err().to_string();
        assert_eq!(rendered, "launcher 帧无效（frame_not_json）");
    }

    #[test]
    fn display_matches_the_specified_wording_with_and_without_a_field_name() {
        assert_eq!(
            parse(json!({"v": 1})).unwrap_err().to_string(),
            "launcher 帧无效（field_invalid：admin_token）"
        );
        assert_eq!(
            parse(json!({"v": 3, "admin_token": TOKEN}))
                .unwrap_err()
                .to_string(),
            "launcher 帧无效（unsupported_frame_version）"
        );
    }

    #[test]
    fn frame_line_reader_stops_at_the_first_lf_and_enforces_the_4096_byte_cap_including_lf() {
        // 恰好 4096（含 LF）通过；之后的字节不读。
        let mut exact = vec![b'a'; MAX_FRAME_BYTES - 1];
        exact.push(b'\n');
        exact.extend_from_slice(b"trailing bytes after the first frame");
        let mut reader = std::io::Cursor::new(exact);
        let line = read_frame_line(&mut reader).unwrap();
        assert_eq!(line.len(), MAX_FRAME_BYTES - 1);
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "trailing bytes after the first frame");

        // 4096 字节内没有 LF：too long（哪怕 LF 就在下一个字节）。
        let mut over = vec![b'a'; MAX_FRAME_BYTES];
        over.push(b'\n');
        assert_error(
            read_frame_line(&mut std::io::Cursor::new(over)).map(|_| unreachable_frame()),
            FrameErrorCode::FrameTooLong,
            None,
        );
        // 两行输入只取第一行：读到第一个 LF 就停，第二行根本不读。
        let two = b"{\"v\":1}\n{\"v\":2}\n".to_vec();
        let mut reader = std::io::Cursor::new(two);
        assert_eq!(read_frame_line(&mut reader).unwrap(), b"{\"v\":1}");
    }

    fn unreachable_frame() -> LauncherFrame {
        parse(minimal()).unwrap()
    }

    #[test]
    fn eof_before_a_lf_and_empty_input_are_frame_not_json() {
        for input in [b"".as_slice(), b"{\"v\":1}".as_slice()] {
            assert_error(
                read_frame_line(&mut std::io::Cursor::new(input.to_vec()))
                    .map(|_| unreachable_frame()),
                FrameErrorCode::FrameNotJson,
                None,
            );
        }
        // 只有一个 LF 的空行：读出空内容，解析时是 not_json。
        let line = read_frame_line(&mut std::io::Cursor::new(b"\n".to_vec())).unwrap();
        assert_error(parse_frame_line(&line), FrameErrorCode::FrameNotJson, None);
    }

    #[test]
    fn read_launcher_frame_parses_one_line_end_to_end() {
        let line = format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\"}}\n");
        let frame = read_launcher_frame(&mut std::io::Cursor::new(line.into_bytes())).unwrap();
        assert!(frame.admin_token.matches(TOKEN));
    }

    #[test]
    fn a_reader_that_never_returns_times_out_with_the_timeout_code() {
        let started = std::time::Instant::now();
        let error = read_with_timeout(
            || {
                std::thread::sleep(Duration::from_secs(5));
                Err(FrameError::new(FrameErrorCode::FrameNotJson))
            },
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(error.code(), FrameErrorCode::Timeout);
        assert_eq!(error.to_string(), "launcher 帧无效（timeout）");
        // 在时限（20 毫秒）就返回，没有等读线程睡完 5 秒；阈值留足调度余量。
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    /// H1：读线程卡在一个永远不会来的字节上（launcher 还没写帧），取消标志一置位，等待在一个轮询间隔之内放弃，不等 10 秒的时限。
    #[test]
    fn a_cancelled_wait_gives_up_within_a_poll_interval_not_at_the_timeout() {
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let started = std::time::Instant::now();
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            read_with_timeout_or_cancel(
                || {
                    std::thread::sleep(Duration::from_secs(30));
                    parse(minimal())
                },
                Duration::from_secs(4),
                &|| cancelled.load(std::sync::atomic::Ordering::SeqCst),
            )
        });
        assert!(matches!(result, Ok(None)), "cancelled is not an error");
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(150), "{waited:?}");
        assert!(
            waited < Duration::from_secs(2),
            "gave up promptly, not at the 4 s limit: {waited:?}"
        );
    }

    /// 已经取消了：不起读线程，立刻放弃；没取消：照常返回读到的帧，时限仍然生效（取消与超时是两回事）。
    #[test]
    fn an_already_cancelled_wait_returns_at_once_and_an_uncancelled_one_keeps_its_timeout() {
        let started = std::time::Instant::now();
        let result = read_with_timeout_or_cancel(
            || {
                std::thread::sleep(Duration::from_secs(30));
                parse(minimal())
            },
            Duration::from_secs(20),
            &|| true,
        );
        assert!(matches!(result, Ok(None)));
        assert!(started.elapsed() < Duration::from_secs(2));

        let frame =
            read_with_timeout_or_cancel(|| parse(minimal()), Duration::from_secs(5), &|| false)
                .unwrap()
                .unwrap_or_else(|| panic!("not cancelled: the frame"));
        assert!(frame.admin_token.matches(TOKEN));
        let error = read_with_timeout_or_cancel(
            || {
                std::thread::sleep(Duration::from_secs(5));
                parse(minimal())
            },
            Duration::from_millis(80),
            &|| false,
        )
        .unwrap_err();
        assert_eq!(error.code(), FrameErrorCode::Timeout);
    }

    /// 读线程 panic：没有结果、也不是超时，是 `Disconnected`（帧读不出来）。
    #[test]
    fn a_reader_that_panics_is_reported_as_disconnected_not_as_a_timeout() {
        let waited = wait_on_thread::<u8, _>(
            "test-reader",
            || panic!("the reader died"),
            Duration::from_secs(5),
            &|| false,
        );
        assert_eq!(waited, Waited::Disconnected);
    }

    #[test]
    fn a_reader_that_answers_in_time_returns_its_result_unchanged() {
        let frame = read_with_timeout(|| parse(minimal()), Duration::from_secs(5)).unwrap();
        assert!(frame.admin_token.matches(TOKEN));
        let error = read_with_timeout(
            || Err(FrameError::at(FrameErrorCode::FieldInvalid, "v")),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.field(), Some("v"));
    }

    #[test]
    fn split_hands_the_token_over_and_keeps_every_other_field() {
        let frame = parse(with(json!({
            "discovery_path": "/a/b",
            "register_socket": "/a/b.sock",
            "orphan_timeout_s": 900,
            "instance_nonce": NONCE,
            "engine": {"stop_on_exit": "never"}
        })))
        .unwrap();
        let (token, config) = frame.split();
        assert!(token.matches(TOKEN));
        assert_eq!(
            config.discovery_path.as_deref(),
            Some(std::path::Path::new("/a/b"))
        );
        assert_eq!(
            config.register_socket.as_deref(),
            Some(std::path::Path::new("/a/b.sock"))
        );
        assert_eq!(config.orphan_timeout_s, 900);
        assert_eq!(config.instance_nonce.unwrap().expose_hex(), NONCE);
        assert_eq!(config.engine.stop_on_exit, StopOnExit::Never);
        let default = LauncherConfig::default();
        assert!(default.discovery_path.is_none() && default.register_socket.is_none());
        assert_eq!(default.orphan_timeout_s, 0);
    }

    #[test]
    fn debug_of_a_parsed_frame_redacts_the_token_and_the_nonce() {
        let frame = parse(with(json!({"instance_nonce": NONCE}))).unwrap();
        let rendered = format!("{frame:?}");
        assert!(!rendered.contains(TOKEN), "{rendered}");
        assert!(!rendered.contains(NONCE), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
    }
}

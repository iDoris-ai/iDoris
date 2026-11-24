//! 所有权记录（规范 §2.7.6「所有权记录与孤儿引擎」）：iDoris 拉起的引擎留下的一份「这个进程是我的」的凭据。
//!
//! iDoris 被 SIGKILL 时 `Drop` 不会运行，引擎成为孤儿，继续占着端口和内存。新 iDoris 靠这份记录认回它：
//! 记录里有 `engine_kind`、pid、启动时间（`ps -o lstart=`，防 pid 复用）、命令（`ps -o command=`）、
//! 端口、可执行文件与引擎的私有 API key。认回的判据在 [`super::owned`]，本模块只管这个文件。
//! 身份核对与引擎种类无关；`engine_kind` 决定记录放在哪个目录、由哪个种类的配方认回。
//!
//! 字段与规范 §2.7.6 的示例一致，只多一个 `command`（规范要求「命令」也是身份的一项，要比对就得先记下来；
//! 它在引擎就绪后才写，见下）。没有进程组字段：引擎以 `process_group(0)` 拉起，是自己那个组的组长，
//! 组 id 就是 pid（[`OwnershipRecord::process_group`]）。这样记录里没有任何办法表达「向 pid P 以外的组发信号」。
//!
//! 位置与写法（规范 §2.7.6，内部格式随 v0.6.0）：
//! - `<状态目录>/engines-owned/<engine_kind>/<endpoint 编码>.json`：**按引擎种类分目录**，同一种类的多个实例
//!   各一份（文件名是 endpoint 的编码，布局见 [`super::instance`]）。目录 0700，文件 0600。
//! - 原子写：同目录临时文件（名字带随机后缀，`O_EXCL|O_NOFOLLOW`，0600，`fchmod` 不受 umask 影响）→
//!   `fsync` → `rename` → 目录 `fsync`（尽力而为）。读者要么看到旧文件，要么看到完整的新文件。
//! - 读：`O_NOFOLLOW|O_NONBLOCK` 打开后**用描述符验证**：普通文件、属主是自己、没有 group/other 位、
//!   不超过 8 KiB。符号链接、FIFO、别人也读得了的文件都不用。
//! - 严格解析：未知字段、未知 schema、pid ≤ 1、端口 0、不合格式的 key 或 `engine_kind` 一律拒绝；
//!   文件里的 `engine_kind` 与端口必须等于它所在目录与文件名对应的那个实例（拷来的、放错目录的文件不算数）。
//!   错误只带固定原因码，不回显文件内容（里面有 key）。
//!
//! `command` 缺省（`None`）= 记录尚未「封印」：引擎刚拉起、还没就绪（命令行要等 `exec` 链和进程改名
//! 结束才稳定），见 [`super::owned::OwnedEngine::seal`]。`api_key` 缺省（`None`）= 这个引擎没有私有 key
//! （规范：`api_key` 只有支持私有 key 的引擎种类才有）。两者为 `None` 时写成「没有这个键」，不写 `null`；
//! 读的时候 `null` 也认。

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::fsutil::{DirError, effective_uid, ensure_private_dir, is_own_directory};
use super::instance::{RECORD_EXTENSION, record_dir, record_file_name};
use super::key::{EngineKey, hex, random_bytes};
use super::launch::ENGINE_HOST;
use super::recipe::EngineKind;

/// 记录格式版本。
pub const RECORD_SCHEMA: u64 = 1;
/// 记录文件大小上限（实际几百字节）。
pub const MAX_RECORD_BYTES: u64 = 8 * 1024;
/// `command` 长度上限；更长的命令行不是我们拉起的那种引擎，封印时按失败处理。
pub const MAX_COMMAND_BYTES: usize = 4096;
const MAX_STARTED_AT_BYTES: usize = 128;
const MAX_GENERATION_BYTES: usize = 128;
const MAX_EXECUTABLE_BYTES: usize = 4096;
const MAX_INSTANCE_BYTES: usize = 64;

const FIELDS: [&str; 10] = [
    "schema",
    "engine_kind",
    "pid",
    "port",
    "started_at",
    "generation",
    "command",
    "executable",
    "api_key",
    "launched_by_instance",
];

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// I/O 失败（只保留错误种类，不带路径或内容）。
    #[error("ownership record I/O failed: {0:?}")]
    Io(std::io::ErrorKind),
    /// 记录目录不是自己的真目录（符号链接、别人的目录）。
    #[error("ownership record directory is not a private directory owned by this user")]
    InsecureDir,
    /// 记录文件是符号链接、FIFO、别人的文件，或 group/other 位非零。
    #[error("ownership record file is not a private regular file owned by this user")]
    InsecureFile,
    #[error("ownership record is larger than {MAX_RECORD_BYTES} bytes")]
    TooLarge,
    /// 内容不合格；附固定原因码。
    #[error("ownership record is invalid: {0}")]
    Invalid(&'static str),
}

impl RecordError {
    /// 这条记录本身不可用（内容坏、权限不对、太大），丢掉它重来是安全的；`Io`、`InsecureDir` 不属于。
    pub fn is_unusable_record(&self) -> bool {
        matches!(self, Self::InsecureFile | Self::TooLarge | Self::Invalid(_))
    }
}

/// 一个自己拉起的引擎的身份。
#[derive(Clone, PartialEq, Eq)]
pub struct OwnershipRecord {
    /// 引擎种类（[`super::EngineRecipe::kind`]）。
    pub engine_kind: EngineKind,
    /// 引擎进程的 pid，也是它那个进程组的 id（见 [`Self::process_group`]）。
    pub pid: u32,
    pub port: u16,
    /// `ps -o lstart=` 的原文（`TZ=UTC`、`LC_ALL=C` 下读的，换时区、换语言也读出同一个值）。
    pub started_at: String,
    /// 平台给的进程「代」标识，比 `ps -o lstart=` 的秒更细（Linux：boot_id 加 starttime 节拍数；macOS：内核记的启动时刻，
    /// 到微秒），所以同一秒内的 pid 复用也分得出。`None` = 旧记录，或拉起时读不出来：这条记录只有秒级身份。
    /// 发任何信号之前，记录里有它就要先核对（[`super::owned`] 的身份闸）。
    pub generation: Option<String>,
    /// `ps -o command=` 的原文；`None` = 尚未封印。
    pub command: Option<String>,
    pub executable: PathBuf,
    /// 引擎的私有 API key；`None` = 没有。
    pub api_key: Option<EngineKey>,
    /// 写下记录的 iDoris 实例（`/health.instance_id`），只用于排查。
    pub launched_by_instance: String,
}

impl std::fmt::Debug for OwnershipRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnershipRecord")
            .field("engine_kind", &self.engine_kind)
            .field("pid", &self.pid)
            .field("port", &self.port)
            .field("started_at", &self.started_at)
            .field("generation", &self.generation)
            .field("command", &self.command)
            .field("executable", &self.executable)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("launched_by_instance", &self.launched_by_instance)
            .finish()
    }
}

impl OwnershipRecord {
    /// 引擎所在的进程组 id：就是 pid。引擎以 `process_group(0)` 拉起，是自己那个组的组长；
    /// 整组信号只会发给这个 pid 的组，且发之前要确认这个 pid 此刻仍领着它（见 `owned` 的身份闸）。
    pub fn process_group(&self) -> u32 {
        self.pid
    }

    /// 结构校验。pid 必须 ≥ 2 且放得进 `i32`：`kill(0)`、`kill(1)`、负数都不是引擎。
    pub fn validate(&self) -> Result<(), RecordError> {
        let id_ok = |id: u32| (2..=i32::MAX as u32).contains(&id);
        if self.engine_kind.is_unknown() {
            return Err(RecordError::Invalid("engine_kind"));
        }
        if !id_ok(self.pid) {
            return Err(RecordError::Invalid("pid"));
        }
        if self.port == 0 {
            return Err(RecordError::Invalid("port"));
        }
        if self.started_at.is_empty()
            || self.started_at.len() > MAX_STARTED_AT_BYTES
            || !self
                .started_at
                .bytes()
                .all(|b| b.is_ascii_graphic() || b == b' ')
        {
            return Err(RecordError::Invalid("started_at"));
        }
        if let Some(generation) = &self.generation
            && (generation.is_empty()
                || generation.len() > MAX_GENERATION_BYTES
                || !generation.bytes().all(|b| b.is_ascii_graphic()))
        {
            return Err(RecordError::Invalid("generation"));
        }
        if let Some(command) = &self.command
            && (command.is_empty()
                || command.len() > MAX_COMMAND_BYTES
                || command.chars().any(char::is_control))
        {
            return Err(RecordError::Invalid("command"));
        }
        match self.executable.to_str() {
            Some(path)
                if self.executable.is_absolute()
                    && path.len() <= MAX_EXECUTABLE_BYTES
                    && !path.chars().any(char::is_control) => {}
            _ => return Err(RecordError::Invalid("executable")),
        }
        if self.launched_by_instance.is_empty()
            || self.launched_by_instance.len() > MAX_INSTANCE_BYTES
            || !self
                .launched_by_instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(RecordError::Invalid("launched_by_instance"));
        }
        Ok(())
    }

    /// 紧凑单行 JSON + `\n`。
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, RecordError> {
        self.validate()?;
        let mut value = serde_json::json!({
            "schema": RECORD_SCHEMA,
            "engine_kind": self.engine_kind.as_str(),
            "pid": self.pid,
            "port": self.port,
            "started_at": self.started_at,
            "executable": self.executable.to_str(),
            "launched_by_instance": self.launched_by_instance,
        });
        // Absent, not `null`: no command until sealed, no key for an engine without private keys.
        if let Some(object) = value.as_object_mut() {
            if let Some(generation) = &self.generation {
                object.insert("generation".into(), generation.as_str().into());
            }
            if let Some(command) = &self.command {
                object.insert("command".into(), command.as_str().into());
            }
            if let Some(key) = &self.api_key {
                object.insert("api_key".into(), key.expose().into());
            }
        }
        let mut bytes = serde_json::to_vec(&value).map_err(|_| RecordError::Invalid("encode"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// 严格解析；错误只带固定原因码。
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, RecordError> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| RecordError::Invalid("not_json"))?;
        let object = value
            .as_object()
            .ok_or(RecordError::Invalid("not_object"))?;
        if object.keys().any(|key| !FIELDS.contains(&key.as_str())) {
            return Err(RecordError::Invalid("unknown_field"));
        }
        if object.get("schema").and_then(serde_json::Value::as_u64) != Some(RECORD_SCHEMA) {
            return Err(RecordError::Invalid("unsupported_schema"));
        }
        let uint = |name: &'static str| -> Result<u64, RecordError> {
            object
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .ok_or(RecordError::Invalid(name))
        };
        let text = |name: &'static str| -> Result<String, RecordError> {
            object
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or(RecordError::Invalid(name))
        };
        let command = match object.get("command") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(command)) => Some(command.clone()),
            Some(_) => return Err(RecordError::Invalid("command")),
        };
        let generation = match object.get("generation") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(generation)) => Some(generation.clone()),
            Some(_) => return Err(RecordError::Invalid("generation")),
        };
        let api_key = match object.get("api_key") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(key)) => {
                Some(EngineKey::from_hex(key).map_err(|_| RecordError::Invalid("api_key"))?)
            }
            Some(_) => return Err(RecordError::Invalid("api_key")),
        };
        let record = Self {
            engine_kind: EngineKind::parse(&text("engine_kind")?)
                .map_err(|_| RecordError::Invalid("engine_kind"))?,
            pid: u32::try_from(uint("pid")?).map_err(|_| RecordError::Invalid("pid"))?,
            port: u16::try_from(uint("port")?).map_err(|_| RecordError::Invalid("port"))?,
            started_at: text("started_at")?,
            generation,
            command,
            executable: PathBuf::from(text("executable")?),
            api_key,
            launched_by_instance: text("launched_by_instance")?,
        };
        record.validate()?;
        Ok(record)
    }
}

/// 一个引擎实例（种类加端口）的所有权记录文件：`<状态目录>/engines-owned/<engine_kind>/<endpoint 编码>.json`。
#[derive(Debug, Clone)]
pub struct OwnershipStore {
    kind: EngineKind,
    port: u16,
    dir: PathBuf,
    path: PathBuf,
}

impl OwnershipStore {
    pub fn new(state_dir: &Path, kind: &EngineKind, port: u16) -> Self {
        let dir = record_dir(state_dir, kind);
        let path = dir.join(record_file_name(&format!("http://{ENGINE_HOST}:{port}")));
        Self {
            kind: kind.clone(),
            port,
            dir,
            path,
        }
    }

    /// 这个种类的记录目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 记录文件。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 记录是不是这个实例的：种类与端口都要对得上。
    fn belongs(&self, record: &OwnershipRecord) -> Result<(), RecordError> {
        if record.engine_kind != self.kind {
            return Err(RecordError::Invalid("engine_kind"));
        }
        if record.port != self.port {
            return Err(RecordError::Invalid("port"));
        }
        Ok(())
    }

    /// 原子写（见模块文档）。目录不存在就建（0700）。记录必须是这个实例的（种类与端口）。
    pub fn write(&self, record: &OwnershipRecord) -> Result<(), RecordError> {
        self.belongs(record)?;
        let bytes = record.to_json_bytes()?;
        self.ensure_dir()?;
        let suffix = hex(&random_bytes::<8>().map_err(|e| RecordError::Io(e.kind()))?);
        let temp = self
            .dir
            .join(format!(".owned.{}.{suffix}.tmp", std::process::id()));
        let install = || -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&temp)?;
            // `mode(0o600)` 受 umask 影响；`fchmod` 不受。
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, &self.path)
        };
        if let Err(error) = install() {
            let _ = std::fs::remove_file(&temp);
            return Err(RecordError::Io(error.kind()));
        }
        // 目录项落盘是尽力而为：重启后 pid 都没了，掉电后记录丢了也无妨。
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// 读记录。`Ok(None)` = 没有记录（记录目录也不存在）。
    pub fn read(&self) -> Result<Option<OwnershipRecord>, RecordError> {
        match std::fs::symlink_metadata(&self.dir) {
            Ok(meta) if is_own_directory(&meta) => {}
            Ok(_) => return Err(RecordError::InsecureDir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(RecordError::Io(error.kind())),
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(code) if code == nix::libc::ELOOP || code == nix::libc::EMLINK
                ) =>
            {
                return Err(RecordError::InsecureFile);
            }
            Err(error) => return Err(RecordError::Io(error.kind())),
        };
        let meta = file.metadata().map_err(|e| RecordError::Io(e.kind()))?;
        if !meta.file_type().is_file()
            || meta.permissions().mode() & 0o077 != 0
            || meta.uid() != effective_uid()
        {
            return Err(RecordError::InsecureFile);
        }
        if meta.len() > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge);
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| RecordError::Io(e.kind()))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge);
        }
        let record = OwnershipRecord::from_json_bytes(&bytes)?;
        // 拷来的、放错目录的文件（别的种类或别的端口的记录）不算数。
        self.belongs(&record)?;
        Ok(Some(record))
    }

    /// 无条件删记录。返回有没有删掉东西。
    pub fn remove(&self) -> Result<bool, RecordError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(RecordError::Io(error.kind())),
        }
    }

    /// 读到文件内容仍是 `expected` 才删（别的实例换过记录就不动它）；读与删之间没有锁，不是原子的，同一实例的调用由调用方串行化。
    pub fn remove_if_same(&self, expected: &OwnershipRecord) -> Result<bool, RecordError> {
        match self.read() {
            Ok(Some(current)) if current == *expected => self.remove(),
            Ok(_) => Ok(false),
            // 文件已经不是一条可用的记录：不是我们写的那份，不替它做主。
            Err(error) if error.is_unusable_record() => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn ensure_dir(&self) -> Result<(), RecordError> {
        ensure_private_dir(&self.dir).map_err(|error| match error {
            DirError::Io(kind) => RecordError::Io(kind),
            DirError::Insecure => RecordError::InsecureDir,
        })
    }
}

/// 这个种类的目录里留着记录的端口（按从小到大）。
///
/// 给「配置里已经没有这个端口、但上一个 iDoris 留下的引擎还在」这种情形用：只按配置里的端口去
/// [`super::OwnedEngine::recover`]，看不见那份记录，引擎会一直占着端口与内存。调用方对每个端口建一个
/// 实例（[`super::EngineInstance::new`]）再 `recover`，该停的停掉。文件名是 `http://127.0.0.1:<port>`
/// 的编码才算（引擎只监听 loopback，见 [`super::ENGINE_HOST`]；写法必须与 [`OwnershipStore`] 自己写出的一致）；
/// 临时文件、不是 `.json` 的、名字解不出端口的都忽略。目录不存在是空；目录是符号链接或别人的是
/// [`RecordError::InsecureDir`]。
pub fn recorded_ports(state_dir: &Path, kind: &EngineKind) -> Result<Vec<u16>, RecordError> {
    let dir = record_dir(state_dir, kind);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if is_own_directory(&meta) => {}
        Ok(_) => return Err(RecordError::InsecureDir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(RecordError::Io(error.kind())),
    }
    let mut ports = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| RecordError::Io(e.kind()))? {
        let entry = entry.map_err(|e| RecordError::Io(e.kind()))?;
        let name = entry.file_name();
        if let Some(port) = name.to_str().and_then(port_of_record_file) {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    ports.dedup();
    Ok(ports)
}

/// 记录文件名 → 端口；只认 [`record_file_name`] 能写出来的名字。
fn port_of_record_file(name: &str) -> Option<u16> {
    let stem = name.strip_suffix(RECORD_EXTENSION)?.strip_suffix('.')?;
    let endpoint = percent_encoding::percent_decode_str(stem)
        .decode_utf8()
        .ok()?;
    let port: u16 = endpoint
        .strip_prefix(&format!("http://{ENGINE_HOST}:"))?
        .parse()
        .ok()?;
    // Port 0 is no endpoint (an instance cannot have it); anything but the canonical spelling is
    // not a name this store could have written.
    (port != 0 && record_file_name(&format!("http://{ENGINE_HOST}:{port}")) == name).then_some(port)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::symlink;

    use super::*;

    fn kind(name: &str) -> EngineKind {
        EngineKind::parse(name).unwrap()
    }

    fn store_for(state: &Path, name: &str, port: u16) -> OwnershipStore {
        OwnershipStore::new(state, &kind(name), port)
    }

    fn store(state: &Path) -> OwnershipStore {
        store_for(state, "fakeserve", 19731)
    }

    fn record() -> OwnershipRecord {
        OwnershipRecord {
            engine_kind: kind("fakeserve"),
            pid: 51234,
            port: 19731,
            started_at: "Fri Nov 20 08:14:05 2026".into(),
            command: Some("fake-server".into()),
            executable: "/opt/engines/fake/bin/fake-server".into(),
            api_key: Some(EngineKey::from_hex(&"ab".repeat(32)).unwrap()),
            launched_by_instance: "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11".into(),
            generation: None,
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_then_read_round_trips_sealed_unsealed_and_keyless_records() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        assert_eq!(store.read().unwrap(), None);

        let mut starting = record();
        starting.command = None;
        store.write(&starting).unwrap();
        assert_eq!(store.read().unwrap(), Some(starting));

        store.write(&record()).unwrap();
        assert_eq!(store.read().unwrap(), Some(record()));

        let mut keyless = record();
        keyless.api_key = None;
        store.write(&keyless).unwrap();
        assert_eq!(store.read().unwrap(), Some(keyless));
    }

    #[test]
    fn the_record_names_its_engine_kind_and_sits_where_the_spec_puts_it() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        store.write(&record()).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("\"engine_kind\":\"fakeserve\""), "{text}");
        assert!(
            !text.contains("\"kind\""),
            "the field is engine_kind: {text}"
        );
        assert_eq!(
            store.path(),
            state
                .path()
                .join("engines-owned/fakeserve/http%3A%2F%2F127%2E0%2E0%2E1%3A19731.json")
        );
    }

    #[test]
    fn files_are_private_and_the_write_leaves_no_temp_file() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        store.write(&record()).unwrap();
        assert_eq!(mode(store.path()), 0o600, "the record holds the engine key");
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&state.path().join("engines-owned")), 0o700);
        let names: Vec<_> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "no .tmp leftovers: {names:?}");
        assert!(names[0].ends_with(".json"));
    }

    #[test]
    fn kinds_and_endpoints_keep_separate_records_side_by_side() {
        let state = tempfile::tempdir().unwrap();
        let s = state.path();
        // Same kind, two ports; another kind on the very same port as the first.
        let (a, b, c) = (
            store_for(s, "fakeserve", 19731),
            store_for(s, "fakeserve", 19732),
            store_for(s, "otherkind", 19731),
        );
        let with = |kind_name: &str, port: u16, pid: u32| {
            let mut r = record();
            (r.engine_kind, r.port, r.pid) = (kind(kind_name), port, pid);
            r
        };
        let (ra, rb, rc) = (
            with("fakeserve", 19731, 4001),
            with("fakeserve", 19732, 4002),
            with("otherkind", 19731, 4003),
        );
        a.write(&ra).unwrap();
        b.write(&rb).unwrap();
        c.write(&rc).unwrap();
        assert_eq!(a.read().unwrap(), Some(ra));
        assert_eq!(b.read().unwrap(), Some(rb));
        assert_eq!(c.read().unwrap(), Some(rc));
        assert_eq!(a.dir(), b.dir(), "one directory per kind");
        assert_ne!(a.dir(), c.dir());
        assert!(a.remove().unwrap());
        assert_eq!(a.read().unwrap(), None);
        assert!(
            b.read().unwrap().is_some(),
            "removing one leaves the others"
        );
        assert!(c.read().unwrap().is_some());
    }

    #[test]
    fn a_record_for_another_kind_or_port_is_neither_written_nor_trusted() {
        let state = tempfile::tempdir().unwrap();
        let a = store_for(state.path(), "fakeserve", 19731);
        // A store only writes its own instance's records.
        let mut other_kind = record();
        other_kind.engine_kind = kind("otherkind");
        assert_eq!(
            a.write(&other_kind),
            Err(RecordError::Invalid("engine_kind"))
        );
        let mut other_port = record();
        other_port.port = 19999;
        assert_eq!(a.write(&other_port), Err(RecordError::Invalid("port")));

        // A record file copied into another kind's directory, or onto another endpoint's file
        // name, is refused when read.
        a.write(&record()).unwrap();
        let wrong_kind = store_for(state.path(), "otherkind", 19731);
        std::fs::create_dir_all(wrong_kind.dir()).unwrap();
        std::fs::copy(a.path(), wrong_kind.path()).unwrap();
        std::fs::set_permissions(wrong_kind.path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert_eq!(wrong_kind.read(), Err(RecordError::Invalid("engine_kind")));
        let wrong_port = store_for(state.path(), "fakeserve", 19732);
        std::fs::copy(a.path(), wrong_port.path()).unwrap();
        std::fs::set_permissions(wrong_port.path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert_eq!(wrong_port.read(), Err(RecordError::Invalid("port")));
    }

    #[test]
    fn a_loose_kind_directory_is_tightened_not_trusted_blindly() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::set_permissions(store.dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
        store.write(&record()).unwrap();
        assert_eq!(mode(store.dir()), 0o700);
    }

    #[test]
    fn reading_refuses_loose_permissions_symlinks_and_oversized_files() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        store.write(&record()).unwrap();
        let path = store.path().to_owned();

        for loose in [0o640, 0o604, 0o644, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(loose)).unwrap();
            assert_eq!(store.read(), Err(RecordError::InsecureFile), "{loose:o}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.read().unwrap().is_some());

        // A symlink at the record path is never followed, even to a valid record.
        let target = state.path().join("elsewhere.json");
        std::fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(store.read(), Err(RecordError::InsecureFile));
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, vec![b' '; MAX_RECORD_BYTES as usize + 1]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(store.read(), Err(RecordError::TooLarge));
    }

    #[test]
    fn a_symlinked_kind_directory_is_refused_for_read_and_write() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join("engines-owned")).unwrap();
        symlink(
            elsewhere.path(),
            state.path().join("engines-owned/fakeserve"),
        )
        .unwrap();
        let store = store(state.path());
        assert_eq!(store.read(), Err(RecordError::InsecureDir));
        assert_eq!(store.write(&record()), Err(RecordError::InsecureDir));
        assert_eq!(
            recorded_ports(state.path(), &kind("fakeserve")),
            Err(RecordError::InsecureDir)
        );
        assert_eq!(
            std::fs::read_dir(elsewhere.path()).unwrap().count(),
            0,
            "nothing was written through the link"
        );
    }

    #[test]
    fn leftover_records_can_be_found_by_kind_even_when_no_configuration_names_their_port() {
        let state = tempfile::tempdir().unwrap();
        let s = state.path();
        assert_eq!(
            recorded_ports(s, &kind("fakeserve")).unwrap(),
            Vec::<u16>::new()
        );
        for (name, port) in [
            ("fakeserve", 19732),
            ("fakeserve", 19731),
            ("otherkind", 19733),
        ] {
            let mut r = record();
            (r.engine_kind, r.port) = (kind(name), port);
            store_for(s, name, port).write(&r).unwrap();
        }
        assert_eq!(
            recorded_ports(s, &kind("fakeserve")).unwrap(),
            [19731, 19732],
            "sorted, only this kind's"
        );
        assert_eq!(recorded_ports(s, &kind("otherkind")).unwrap(), [19733]);
        assert_eq!(
            recorded_ports(s, &kind("never_ran")).unwrap(),
            Vec::<u16>::new()
        );

        // Temp files and strangers are ignored, and so are names the store could never have written.
        let dir = store_for(s, "fakeserve", 19731).dir().to_owned();
        for stranger in [
            ".owned.1.deadbeef.tmp",
            "notes.txt",
            ".json",
            "http%3A%2F%2F127%2E0%2E0%2E1%3A0019734.json", // not the canonical spelling of a port
            "http%3A%2F%2F127%2E0%2E0%2E1%3A0.json",       // port 0 is no endpoint
            "http%3A%2F%2F127%2E0%2E0%2E1%3A65536.json",   // not a port
            "http%3A%2F%2Fexample%2Ecom%3A19735.json",     // engines listen on loopback only
            "http%3A%2F%2F127%2E0%2E0%2E1%3A19736",        // no extension
        ] {
            std::fs::write(dir.join(stranger), b"x").unwrap();
        }
        assert_eq!(
            recorded_ports(s, &kind("fakeserve")).unwrap(),
            [19731, 19732]
        );
    }

    #[test]
    fn what_the_store_writes_is_what_recorded_ports_reads_back() {
        let state = tempfile::tempdir().unwrap();
        for port in [1u16, 80, 19731, 65535] {
            let mut r = record();
            r.port = port;
            store_for(state.path(), "fakeserve", port)
                .write(&r)
                .unwrap();
        }
        assert_eq!(
            recorded_ports(state.path(), &kind("fakeserve")).unwrap(),
            [1, 80, 19731, 65535]
        );
    }

    #[test]
    fn the_specs_own_example_record_is_a_valid_unsealed_record() {
        // §2.7.6, with a real-looking 64-hex key in place of the placeholder.
        let example = format!(
            "{{\"schema\":1,\"engine_kind\":\"omlx\",\"pid\":51234,\"port\":19731,\"started_at\":\"Fri Nov 20 15:14:05 2026\",\"executable\":\"/Applications/oMLX.app/Contents/MacOS/omlx-cli\",\"api_key\":\"{}\",\"launched_by_instance\":\"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\"}}",
            "ab".repeat(32)
        );
        let parsed = OwnershipRecord::from_json_bytes(example.as_bytes()).unwrap();
        assert_eq!(parsed.engine_kind, EngineKind::OMLX);
        assert_eq!((parsed.pid, parsed.port), (51234, 19731));
        assert_eq!(parsed.started_at, "Fri Nov 20 15:14:05 2026");
        assert_eq!(parsed.command, None, "no command yet: not sealed");
        assert_eq!(parsed.process_group(), 51234);
        assert!(parsed.api_key.is_some());
    }

    #[test]
    fn the_written_record_has_the_specs_keys_and_optional_ones_only_when_present() {
        let keys = |r: &OwnershipRecord| -> Vec<String> {
            let value: serde_json::Value =
                serde_json::from_slice(&r.to_json_bytes().unwrap()).unwrap();
            let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        let spec = [
            "api_key",
            "engine_kind",
            "executable",
            "launched_by_instance",
            "pid",
            "port",
            "schema",
            "started_at",
        ];
        let mut unsealed = record();
        unsealed.command = None;
        assert_eq!(keys(&unsealed), spec);

        // Sealed: the one key beyond the spec's example.
        let mut sealed_keys = spec.to_vec();
        sealed_keys.push("command");
        sealed_keys.sort_unstable();
        assert_eq!(keys(&record()), sealed_keys);

        // An engine kind without private keys has no `api_key` at all.
        let mut keyless = unsealed.clone();
        keyless.api_key = None;
        let without_key: Vec<&str> = spec.iter().copied().filter(|k| *k != "api_key").collect();
        assert_eq!(keys(&keyless), without_key);
        let text = String::from_utf8(keyless.to_json_bytes().unwrap()).unwrap();
        assert!(!text.contains("null"), "{text}");
    }

    /// M6：可选的「代」标识 `generation`。有就原样往返并出现在文件里；没有（旧记录，或平台读不出来）就不写这个字段，读回来是 `None`；
    /// 空串、过长、含空白或控制字符、不是字符串都是 `generation` 字段无效（不是被悄悄丢掉）。
    #[test]
    fn the_process_generation_round_trips_is_optional_and_is_validated() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        let mut with = record();
        with.generation = Some("darwin:1763622845.123456".into());
        store.write(&with).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(
            text.contains("\"generation\":\"darwin:1763622845.123456\""),
            "{text}"
        );
        assert_eq!(store.read().unwrap(), Some(with.clone()));

        // 没有它：文件里没有这个字段（不写 null），读回来是 None。
        let mut without = record();
        without.generation = None;
        store.write(&without).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("generation"), "{text}");
        assert_eq!(store.read().unwrap().unwrap().generation, None);

        // 无效的值：写不进去，读到也被拒绝。
        let too_long = "g".repeat(MAX_GENERATION_BYTES + 1);
        for bad in ["", "has space", "tab\there", "控制", too_long.as_str()] {
            let mut record = record();
            record.generation = Some(bad.to_string());
            assert_eq!(
                store.write(&record),
                Err(RecordError::Invalid("generation")),
                "{bad:?}"
            );
        }
        let exactly_max = "g".repeat(MAX_GENERATION_BYTES);
        let mut at_the_limit = record();
        at_the_limit.generation = Some(exactly_max);
        store.write(&at_the_limit).unwrap();
        let good = String::from_utf8(with.to_json_bytes().unwrap()).unwrap();
        let wrong_type = good.replace(
            "\"generation\":\"darwin:1763622845.123456\"",
            "\"generation\":7",
        );
        assert_eq!(
            OwnershipRecord::from_json_bytes(wrong_type.as_bytes()),
            Err(RecordError::Invalid("generation"))
        );
    }

    #[test]
    fn parsing_is_strict_and_errors_never_echo_the_content() {
        let good = String::from_utf8(record().to_json_bytes().unwrap()).unwrap();
        assert!(OwnershipRecord::from_json_bytes(good.as_bytes()).is_ok());
        let key = "ab".repeat(32);
        let kind_field = "\"engine_kind\":\"fakeserve\"";
        let bad: Vec<(String, &str)> = vec![
            ("not json".into(), "not_json"),
            ("[1]".into(), "not_object"),
            (
                good.replace("\"schema\":1", "\"schema\":2"),
                "unsupported_schema",
            ),
            (
                good.replace("\"schema\":1", "\"schema\":\"1\""),
                "unsupported_schema",
            ),
            (good.replace("{\"", "{\"extra\":1,\""), "unknown_field"),
            // The old spelling of the field is an unknown field now, and so is a stored group id.
            (good.replace("engine_kind", "kind"), "unknown_field"),
            (
                good.replace("\"pid\":51234", "\"pid\":51234,\"pgid\":51234"),
                "unknown_field",
            ),
            (
                good.replace(kind_field, "\"engine_kind\":\"Fake\""),
                "engine_kind",
            ),
            (
                good.replace(kind_field, "\"engine_kind\":\"\""),
                "engine_kind",
            ),
            (
                good.replace(kind_field, "\"engine_kind\":\"a-b\""),
                "engine_kind",
            ),
            (
                good.replace(kind_field, "\"engine_kind\":\"../x\""),
                "engine_kind",
            ),
            (good.replace(kind_field, "\"engine_kind\":7"), "engine_kind"),
            (
                good.replace(kind_field, "\"engine_kind\":\"unknown\""),
                "engine_kind",
            ),
            (good.replace("\"pid\":51234", "\"pid\":1"), "pid"),
            (good.replace("\"pid\":51234", "\"pid\":0"), "pid"),
            (good.replace("\"pid\":51234", "\"pid\":-5"), "pid"),
            (good.replace("\"pid\":51234", "\"pid\":4294967296"), "pid"),
            (good.replace("\"pid\":51234", "\"pid\":2147483648"), "pid"),
            (good.replace("\"port\":19731", "\"port\":0"), "port"),
            (good.replace("\"port\":19731", "\"port\":65536"), "port"),
            (good.replace(&key, &"AB".repeat(32)), "api_key"),
            (good.replace(&key, "short"), "api_key"),
            (
                good.replace(&format!("\"api_key\":\"{key}\""), "\"api_key\":5"),
                "api_key",
            ),
            (
                good.replace("\"command\":\"fake-server\"", "\"command\":7"),
                "command",
            ),
            (
                good.replace("\"command\":\"fake-server\"", "\"command\":\"\""),
                "command",
            ),
            (
                good.replace("/opt/engines", "relative/engines"),
                "executable",
            ),
            (
                good.replace(
                    "\"started_at\":\"Fri Nov 20 08:14:05 2026\"",
                    "\"started_at\":\"\"",
                ),
                "started_at",
            ),
        ];
        for (text, reason) in bad {
            let error = OwnershipRecord::from_json_bytes(text.as_bytes()).unwrap_err();
            assert_eq!(error, RecordError::Invalid(reason), "{text}");
            assert!(!error.to_string().contains(&key));
        }
    }

    #[test]
    fn a_record_that_cannot_be_validated_is_never_written() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        type Tweak = fn(&mut OwnershipRecord);
        let cases: [(&str, Tweak); 4] = [
            ("pid", |r| r.pid = 1),
            ("command", |r| r.command = Some("a\nb".into())),
            ("executable", |r| r.executable = "fake-server".into()),
            ("started_at", |r| r.started_at = "\u{7}".into()),
        ];
        for (name, mutate) in cases {
            let mut bad = record();
            mutate(&mut bad);
            assert_eq!(store.write(&bad), Err(RecordError::Invalid(name)));
        }
        assert_eq!(store.read().unwrap(), None);
    }

    #[test]
    fn remove_is_conditional_when_asked_to_be() {
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path());
        assert!(!store.remove().unwrap());
        store.write(&record()).unwrap();

        let mut other = record();
        other.pid = 60000;
        assert!(!store.remove_if_same(&other).unwrap());
        assert!(store.read().unwrap().is_some(), "another record stays");
        assert!(store.remove_if_same(&record()).unwrap());
        assert_eq!(store.read().unwrap(), None);
    }

    #[test]
    fn debug_hides_the_key() {
        let shown = format!("{:?}", record());
        assert!(!shown.contains(&"ab".repeat(32)));
        assert!(shown.contains("REDACTED"));
    }
}

//! 本地模型列表（接口规范 §2.9 `GET /v1/models/local` / `idoris models local --json`）。
//!
//! 只读文件系统：扫描 oMLX 模型目录（默认 `~/.omlx/models`，调用方注入），不连接 oMLX、不读任何密钥、
//! 不加载也不下载任何东西。任何单个目录的问题（符号链接、不完整、读不了的 `config.json`）都只影响那一个
//! 目录，永远不会让整个列表失败。
//!
//! 实现口径（规范没写到的地方，全部取保守一侧）：
//! - 符号链接一律不跟随：模型目录本身是符号链接时整项跳过；目录内的符号链接文件既不计入体积、也不算权重。
//! - 「完整」= 目录里有可读的常规文件 `config.json` 且它是合法的 JSON 对象（截断或损坏的 config 说明下载没收完，
//!   首次加载必失败，不算已安装）、至少一个**非空**的权重文件（`.safetensors` / `.gguf` / `.npz`；零字节的
//!   占位文件不算），`model.safetensors.index.json` 列出的分片全部是非空的常规文件，且没有下载残留
//!   （`.incomplete` / `.partial` / `.part` / `.tmp` / `.downloading`）。不完整的目录不出现在列表里，也不算「已安装」。
//! - 符号链接目录（含 HF snapshot 风格的链接）本版**不支持**，一律按未安装：理由与取舍见接口规范 §2.9 与勘误十一。
//! - 遍历上限数的是目录项（文件与子目录都算），超过即当作不完整。
//! - 以 `.` 开头的目录（`.cache`、`.tmp` 之类的暂存）和非 UTF-8 名字跳过。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::{ModelKind, OmlxBinding};

/// `config.json` 的读取上限；超过的当作读不了。
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
/// 单个模型目录最多遍历的目录项数（文件与子目录都算）与深度，防止畸形目录拖住请求。
const MAX_ENTRIES_PER_MODEL: usize = 100_000;
const MAX_DEPTH: usize = 8;
/// 一次扫描（所有模型目录加根目录本身）总共最多看的目录项数。用完之后还没检查的模型进 [`ScanReport::unchecked`]，
/// 按「未安装」处理。一次请求的最坏工作量因此有上界，不随模型目录个数线性增长。
pub const MAX_SCAN_ENTRIES: usize = 300_000;

#[derive(Clone, Copy)]
struct Limits {
    max_entries: usize,
    max_depth: usize,
    max_scan_entries: usize,
}

const LIMITS: Limits = Limits {
    max_entries: MAX_ENTRIES_PER_MODEL,
    max_depth: MAX_DEPTH,
    max_scan_entries: MAX_SCAN_ENTRIES,
};

/// 一个已安装的本地模型。字段名与 §2.9 的响应逐字一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalModel {
    pub omlx_id: String,
    pub kind: ModelKind,
    pub size_bytes: u64,
    /// 不在目录里时为 `null`。
    pub catalog_id: Option<String>,
}

/// `GET /v1/models/local` 与 `idoris models local --json` 共用的响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalModelsResponse {
    pub models: Vec<LocalModel>,
}

/// 默认模型目录：`<home>/.omlx/models`。`home` 由调用方给（CLI 用 `HOME`），这里不读环境变量。
pub fn default_models_dir(home: &Path) -> PathBuf {
    home.join(".omlx").join("models")
}

/// 一次扫描的结果。`unchecked` 是本次因整体工作预算用完而**没有检查**的模型目录名：它们不在 `models` 里，
/// 调用方按「未安装」处理（保守一侧，下载仍须用户确认）；wire 上看不到这个字段（勘误十一）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub models: Vec<LocalModel>,
    pub unchecked: Vec<String>,
    /// 根目录条目多到超过整体预算：读到的那些名字之外还有条目没被看见（它们的名字不在 `unchecked` 里）。
    pub root_truncated: bool,
}

/// 扫描 `dir`。目录不存在、读不了都返回空列表。结果按 `omlx_id` 排序，输出稳定。
pub fn scan_local_models(dir: &Path, bindings: &[OmlxBinding]) -> Vec<LocalModel> {
    scan_local_models_report(dir, bindings).models
}

/// 同 [`scan_local_models`]，另外报告因整体工作预算（[`MAX_SCAN_ENTRIES`]）而没检查的目录。
pub fn scan_local_models_report(dir: &Path, bindings: &[OmlxBinding]) -> ScanReport {
    scan_with(dir, bindings, LIMITS)
}

/// 非 unix 平台没有按句柄逐级打开目录的实现：不扫描，永远是空列表（oMLX 只有 macOS）。
#[cfg(not(unix))]
fn scan_with(_dir: &Path, _bindings: &[OmlxBinding], _limits: Limits) -> ScanReport {
    ScanReport::default()
}

/// 测试用的同步点与故障注入：每次打开目录 / 文件、读目录、`fstatat` 之前调用。生产构建里是空函数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum Stage {
    OpenModel,
    ReadDir,
    Stat,
    OpenSubdir,
    OpenFile,
}

#[cfg(all(test, unix))]
type Hook = Box<dyn Fn(Stage, &Path) -> Option<nix::errno::Errno>>;

#[cfg(all(test, unix))]
thread_local! {
    static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
fn probe(stage: Stage, path: &Path) -> Result<(), nix::errno::Errno> {
    match HOOK.with(|hook| hook.borrow().as_ref().and_then(|f| f(stage, path))) {
        Some(errno) => Err(errno),
        None => Ok(()),
    }
}

#[cfg(all(unix, not(test)))]
#[inline]
fn probe(_stage: Stage, _path: &Path) -> Result<(), nix::errno::Errno> {
    Ok(())
}

#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[cfg(unix)]
use nix::dir::Dir;
#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{AtFlags, OFlag, openat};
#[cfg(unix)]
use nix::sys::stat::{Mode, SFlag, fstatat};

#[cfg(unix)]
fn dir_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    Regular,
    Other,
}

/// 相对已打开的目录句柄 `fstatat`（不跟随符号链接）。`ENOENT`（条目刚好消失）是 `Ok(None)`，别的错误原样返回。
#[cfg(unix)]
fn stat_entry(
    dir: &Dir,
    path: &Path,
    name: &std::ffi::OsStr,
) -> Result<Option<(Kind, u64)>, Errno> {
    if let Err(errno) = probe(Stage::Stat, path) {
        return if errno == Errno::ENOENT {
            Ok(None)
        } else {
            Err(errno)
        };
    }
    match fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let format = stat.st_mode & SFlag::S_IFMT.bits();
            let kind = if format == SFlag::S_IFDIR.bits() {
                Kind::Dir
            } else if format == SFlag::S_IFREG.bits() {
                Kind::Regular
            } else {
                Kind::Other
            };
            Ok(Some((kind, u64::try_from(stat.st_size).unwrap_or(0))))
        }
        Err(Errno::ENOENT) => Ok(None),
        Err(errno) => Err(errno),
    }
}

/// 列出目录里除 `.`、`..` 之外的名字，最多 `room` 个；还有更多条目时第二个返回值为 `true`（超限）。枚举中途的错误
/// （`ENOENT` 除外）整体失败：不能把「读不出来」当成「没有这个条目」。
#[cfg(unix)]
fn list_names(dir: &mut Dir, path: &Path, room: usize) -> Result<(Vec<OsString>, bool), Errno> {
    probe(Stage::ReadDir, path)?;
    let mut names = Vec::new();
    for entry in dir.iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(Errno::ENOENT) => continue,
            Err(errno) => return Err(errno),
        };
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() >= room {
            return Ok((names, true));
        }
        names.push(OsString::from_vec(name.to_vec()));
    }
    Ok((names, false))
}

/// 按句柄读一个常规文件：`O_NOFOLLOW|O_NONBLOCK` 相对 `dir` 打开，对已打开的句柄 `fstat` 确认是常规文件，
/// 最多读 `limit + 1` 字节，超过 `limit` 一律拒绝（恰好 `limit` 字节的文件可以读）。检查与打开之间没有窗口：
/// 类型和大小都是已打开的那个对象的。任何失败（含 `ENOENT`）都是 `None`。
#[cfg(unix)]
fn read_regular_limited(dir: &Dir, path: &Path, name: &str, limit: u64) -> Option<Vec<u8>> {
    probe(Stage::OpenFile, &path.join(name)).ok()?;
    let fd = openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = fs::File::from(fd);
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut raw = Vec::new();
    file.take(limit + 1).read_to_end(&mut raw).ok()?;
    (raw.len() as u64 <= limit).then_some(raw)
}

#[cfg(unix)]
fn scan_with(dir: &Path, bindings: &[OmlxBinding], limits: Limits) -> ScanReport {
    let mut report = ScanReport::default();
    // 模型根目录本身可以是符号链接（`~/.omlx/models` 指向别的盘）；根以下一律不跟随。
    let Ok(mut root) = Dir::open(
        dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) else {
        return report;
    };
    let Ok((mut names, root_overflow)) = list_names(&mut root, dir, limits.max_scan_entries) else {
        return report;
    };
    // 根目录的条目也计入整体预算；超出的部分不再往下看（它们的名字没读出来，只能用 `root_truncated` 报告）。
    report.root_truncated = root_overflow;
    names.truncate(limits.max_scan_entries);
    let mut remaining = limits.max_scan_entries.saturating_sub(names.len());
    names.sort();
    let by_id: BTreeMap<&str, &OmlxBinding> = bindings
        .iter()
        .map(|binding| (binding.omlx_id.as_str(), binding))
        .collect();
    for raw_name in names {
        let Some(name) = raw_name.to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') || !crate::catalog::valid_omlx_id(&name) {
            continue;
        }
        let path = dir.join(&name);
        // 不跟随符号链接：`fstatat` 看到的是链接本身。
        if !matches!(
            stat_entry(&root, &path, &raw_name),
            Ok(Some((Kind::Dir, _)))
        ) {
            continue;
        }
        if remaining == 0 {
            report.unchecked.push(name);
            continue;
        }
        if probe(Stage::OpenModel, &path).is_err() {
            continue;
        }
        let Ok(mut model_dir) =
            Dir::openat(&root, raw_name.as_os_str(), dir_flags(), Mode::empty())
        else {
            continue;
        };
        let (outcome, used) = inspect_model_dir_with(&mut model_dir, &path, limits, remaining);
        remaining = remaining.saturating_sub(used);
        let inspected = match outcome {
            Inspected::Complete(inspected) => inspected,
            Inspected::Incomplete => continue,
            Inspected::OverBudget => {
                remaining = 0;
                report.unchecked.push(name);
                continue;
            }
        };
        let binding = by_id.get(name.as_str());
        let kind = binding
            .and_then(|binding| binding.kind)
            .unwrap_or_else(|| infer_kind(&inspected.config));
        report.models.push(LocalModel {
            omlx_id: name,
            kind,
            size_bytes: inspected.size_bytes,
            catalog_id: binding.map(|binding| binding.catalog_id.clone()),
        });
    }
    report.models.sort_by(|a, b| a.omlx_id.cmp(&b.omlx_id));
    report
}

#[cfg(unix)]
struct InspectedDir {
    config: Value,
    size_bytes: u64,
}

#[cfg(unix)]
enum Inspected {
    Complete(InspectedDir),
    Incomplete,
    /// 整体工作预算用完，这个目录没检查完。
    OverBudget,
}

/// 检查一个已打开的模型目录句柄。返回结果与本目录消耗的目录项数。目录下的一切（`config.json`、索引、子目录、
/// 分片）都相对这个句柄（或它逐级 `openat` 出来的子句柄）访问，路径只用于诊断与测试同步点。
#[cfg(unix)]
fn inspect_model_dir_with(
    dir: &mut Dir,
    path: &Path,
    limits: Limits,
    allowance: usize,
) -> (Inspected, usize) {
    let Some(raw) = read_regular_limited(dir, path, "config.json", MAX_CONFIG_BYTES) else {
        return (Inspected::Incomplete, 0);
    };
    // 解析失败（截断、损坏）或不是对象：下载没收完，首次加载必失败，不算完整。
    let Some(config) = serde_json::from_slice::<Value>(&raw)
        .ok()
        .filter(Value::is_object)
    else {
        return (Inspected::Incomplete, 0);
    };

    let mut walk = Walk::default();
    walk_dir(dir, path, 0, false, limits, allowance, &mut walk);
    let used = walk.entries;
    if walk.budget_hit {
        return (Inspected::OverBudget, used);
    }
    if walk.broken || walk.partial || walk.weights == 0 {
        return (Inspected::Incomplete, used);
    }
    if let Some(is_regular) = walk.index_is_regular
        && (!is_regular || !shards_present(dir, path, &walk.top_weights))
    {
        return (Inspected::Incomplete, used);
    }
    (
        Inspected::Complete(InspectedDir {
            config,
            size_bytes: walk.size_bytes,
        }),
        used,
    )
}

#[cfg(unix)]
#[derive(Default)]
struct Walk {
    size_bytes: u64,
    entries: usize,
    weights: usize,
    partial: bool,
    /// 超出单个模型的上限、读不了的子目录、`NotFound` 以外的枚举或 `fstatat` 错误：这个目录不完整。
    broken: bool,
    /// 整体工作预算用完。
    budget_hit: bool,
    /// 模型目录顶层、可见（非 `.` 开头）、非空的权重文件名：索引里的分片只认这个集合。
    top_weights: BTreeSet<String>,
    /// 顶层有 `model.safetensors.index.json`：它是不是常规文件。
    index_is_regular: Option<bool>,
}

#[cfg(unix)]
const INDEX_NAME: &str = "model.safetensors.index.json";

#[cfg(unix)]
fn walk_dir(
    dir: &mut Dir,
    path: &Path,
    depth: usize,
    hidden: bool,
    limits: Limits,
    allowance: usize,
    walk: &mut Walk,
) {
    if depth > limits.max_depth {
        walk.broken = true;
        return;
    }
    let room_model = limits.max_entries.saturating_sub(walk.entries);
    let room_budget = allowance.saturating_sub(walk.entries);
    let (names, overflow) = match list_names(dir, path, room_model.min(room_budget)) {
        Ok(listed) => listed,
        Err(_) => {
            walk.broken = true;
            return;
        }
    };
    // 列出来的名字立刻计入（在处理它们、在往子目录里走之前）：同级的兄弟越多，留给子目录的空间越少，
    // 溢出的那一次也按已列出的个数计费，下一个模型看到的预算是扣过的。预算数的是目录项个数，不是字节数。
    walk.entries += names.len();
    if overflow {
        if room_model <= room_budget {
            walk.broken = true;
        } else {
            walk.budget_hit = true;
        }
        return;
    }
    for name in names {
        let child = path.join(&name);
        let lower = name.to_string_lossy().to_lowercase();
        let (kind, len) = match stat_entry(dir, &child, &name) {
            Ok(Some(found)) => found,
            // 条目刚好消失：当作没有。别的错误：这个目录不完整。
            Ok(None) => continue,
            Err(_) => {
                walk.broken = true;
                continue;
            }
        };
        if depth == 0 && name.as_bytes() == INDEX_NAME.as_bytes() {
            walk.index_is_regular = Some(kind == Kind::Regular);
        }
        match kind {
            Kind::Dir => {
                match probe(Stage::OpenSubdir, &child) {
                    Ok(()) => {}
                    Err(Errno::ENOENT) => continue,
                    Err(_) => {
                        walk.broken = true;
                        continue;
                    }
                }
                match Dir::openat(&*dir, name.as_os_str(), dir_flags(), Mode::empty()) {
                    Ok(mut sub) => {
                        let sub_hidden = hidden || lower.starts_with('.');
                        walk_dir(
                            &mut sub,
                            &child,
                            depth + 1,
                            sub_hidden,
                            limits,
                            allowance,
                            walk,
                        );
                        if walk.broken || walk.budget_hit {
                            return;
                        }
                    }
                    Err(Errno::ENOENT) => {}
                    // 含被换成符号链接（`ELOOP`/`ENOTDIR`）：不跟随，这个目录不完整。
                    Err(_) => walk.broken = true,
                }
            }
            Kind::Regular => {
                if [".incomplete", ".partial", ".part", ".tmp", ".downloading"]
                    .iter()
                    .any(|suffix| lower.ends_with(suffix))
                {
                    walk.partial = true;
                }
                // 隐藏目录里的文件（暂存）不计体积；残留标记上面已经检查过。
                if !lower.starts_with('.') && !hidden {
                    walk.size_bytes = walk.size_bytes.saturating_add(len);
                    if len > 0
                        && [".safetensors", ".gguf", ".npz"]
                            .iter()
                            .any(|suffix| lower.ends_with(suffix))
                    {
                        walk.weights += 1;
                        if depth == 0
                            && let Some(text) = name.to_str()
                        {
                            walk.top_weights.insert(text.to_owned());
                        }
                    }
                }
            }
            // 符号链接、FIFO、设备：既不计体积，也不跟随。
            Kind::Other => {}
        }
    }
}

/// `model.safetensors.index.json`（按句柄读，最多 [`MAX_CONFIG_BYTES`]）必须有**非空**的 `weight_map`，且它列出的
/// 分片（去重后）全部在遍历时已经认定的「顶层、可见、非空权重文件」集合里：不再重复 `stat`，隐藏文件、
/// 空文件、目录、子目录里的文件都不算分片。
#[cfg(unix)]
fn shards_present(dir: &Dir, path: &Path, top_weights: &BTreeSet<String>) -> bool {
    let Some(raw) = read_regular_limited(dir, path, INDEX_NAME, MAX_CONFIG_BYTES) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let Some(map) = value.get("weight_map").and_then(Value::as_object) else {
        return false;
    };
    if map.is_empty() {
        return false;
    }
    let mut shards = BTreeSet::new();
    for shard in map.values() {
        match shard.as_str() {
            Some(name) if !name.is_empty() => {
                shards.insert(name);
            }
            _ => return false,
        }
    }
    shards.iter().all(|name| top_weights.contains(*name))
}

/// 按 `config.json` 的 `architectures` / `model_type` 推断类别；推断不了为 [`ModelKind::Unknown`]。
///
/// 这是启发式：目录里写了 `kind` 的模型不会走到这里。顺序从最具体到最泛：
/// OCR / TTS / 图像 → 重排 → 向量 → 视觉语言 → 对话 → unknown。
pub fn infer_kind(config: &Value) -> ModelKind {
    let mut names: Vec<String> = config
        .get("architectures")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    let model_type = config
        .get("model_type")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    if let Some(model_type) = &model_type {
        names.push(model_type.clone());
    }
    if names.is_empty() {
        return ModelKind::Unknown;
    }
    let any = |needle: &str| names.iter().any(|name| name.contains(needle));
    let ends = |suffix: &str| names.iter().any(|name| name.ends_with(suffix));

    if any("ocr") {
        return ModelKind::Ocr;
    }
    if any("tts") || any("cosyvoice") || any("text_to_speech") {
        return ModelKind::Tts;
    }
    if any("diffusion") || any("flux") {
        return ModelKind::Image;
    }
    if any("rerank") || ends("forsequenceclassification") {
        return ModelKind::Rerank;
    }
    // 向量模型：名字里带 embedding，或是没有 LM 头的 BERT 系基础模型（`BertModel`、`XLMRobertaModel`…）。
    if any("embedding") || (any("bert") && !any("for")) {
        return ModelKind::Embedding;
    }
    let has_vision = config.get("vision_config").is_some_and(|v| !v.is_null());
    let is_lm = ends("forcausallm") || ends("lmheadmodel");
    // `...ForConditionalGeneration` 同时是 Whisper、T5 等的类名：只有带 vision_config 才认作视觉语言模型，
    // 否则宁可 unknown，也不把语音/翻译模型当成 chat 列给选择器。
    let is_conditional = ends("forconditionalgeneration") || ends("forvision2seq");
    let is_vl_name = any("vlfor") || ends("_vl");
    if (is_lm || is_conditional) && (has_vision || is_vl_name) {
        return ModelKind::Vision;
    }
    if is_lm {
        return ModelKind::Chat;
    }
    ModelKind::Unknown
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const CONFIG: &str = r#"{"architectures":["XForCausalLM"]}"#;

    fn limits(max_entries: usize, max_depth: usize, max_scan_entries: usize) -> Limits {
        Limits {
            max_entries,
            max_depth,
            max_scan_entries,
        }
    }

    fn model(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.json"), CONFIG).unwrap();
        fs::write(dir.join("w.safetensors"), b"weights").unwrap();
        dir
    }

    fn ids(report: &ScanReport) -> Vec<&str> {
        report.models.iter().map(|m| m.omlx_id.as_str()).collect()
    }

    fn scan(root: &Path) -> ScanReport {
        scan_with(root, &[], LIMITS)
    }

    /// 测试同步点：只在本线程、本测试里生效，离开作用域自动摘掉。
    struct HookGuard;

    impl HookGuard {
        fn set(hook: impl Fn(Stage, &Path) -> Option<Errno> + 'static) -> Self {
            HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
            Self
        }
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }

    fn outside_with_big_weight() -> tempfile::TempDir {
        let outside = tempfile::tempdir().unwrap();
        model(outside.path(), "x");
        fs::write(
            outside.path().join("x").join("big.safetensors"),
            vec![0u8; 5000],
        )
        .unwrap();
        outside
    }

    #[test]
    fn the_entry_limit_counts_subdirectories_not_just_files() {
        let tight = limits(10, 8, 1_000);
        for (count, listed) in [(7, true), (12, false)] {
            let root = tempfile::tempdir().unwrap();
            let dir = model(root.path(), "m");
            for index in 0..count {
                fs::create_dir(dir.join(format!("d{index}"))).unwrap();
            }
            assert_eq!(
                !scan_with(root.path(), &[], tight).models.is_empty(),
                listed,
                "count={count}"
            );
        }
    }

    #[test]
    fn the_depth_limit_still_applies() {
        let root = tempfile::tempdir().unwrap();
        let dir = model(root.path(), "m");
        fs::create_dir_all(dir.join("a/b/c/d")).unwrap();
        assert!(
            scan_with(root.path(), &[], limits(100, 2, 1_000))
                .models
                .is_empty()
        );
        assert_eq!(scan(root.path()).models.len(), 1);
    }

    /// #609（Codex）：子目录是从已打开的父句柄逐级 `openat(O_NOFOLLOW|O_DIRECTORY)` 打开的。`fstatat` 与打开之间
    /// 把子目录换成指向根外的符号链接：不会跟出去，这个模型不完整。
    #[test]
    fn a_subdirectory_swapped_for_a_symlink_mid_scan_does_not_lead_outside_the_root() {
        let outside = outside_with_big_weight();
        let target = outside.path().join("x");
        let root = tempfile::tempdir().unwrap();
        let dir = model(root.path(), "m");
        fs::create_dir(dir.join("sub")).unwrap();
        assert_eq!(scan(root.path()).models.len(), 1, "对照：没有换");
        let swapped = dir.join("sub");
        let _hook = HookGuard::set(move |stage, path| {
            if stage == Stage::OpenSubdir && path.ends_with("m/sub") {
                fs::remove_dir(&swapped).unwrap();
                symlink(&target, &swapped).unwrap();
            }
            None
        });
        let report = scan(root.path());
        assert!(report.models.is_empty(), "{report:?}");
    }

    #[test]
    fn a_model_directory_swapped_for_a_symlink_between_stat_and_open_is_not_listed() {
        let outside = outside_with_big_weight();
        let root = tempfile::tempdir().unwrap();
        let dir = model(root.path(), "m");
        model(root.path(), "other");
        let target = outside.path().join("x");
        let _hook = HookGuard::set(move |stage, path| {
            if stage == Stage::OpenModel && path.ends_with("m") {
                fs::remove_dir_all(&dir).unwrap();
                symlink(&target, &dir).unwrap();
            }
            None
        });
        assert_eq!(ids(&scan(root.path())), ["other"]);
    }

    #[test]
    fn config_and_index_swapped_for_symlinks_before_the_open_are_not_read() {
        let outside = outside_with_big_weight();
        let outside_file = outside.path().join("x").join("config.json");
        let root = tempfile::tempdir().unwrap();
        let dir = model(root.path(), "m");
        let config = dir.join("config.json");
        let hook = HookGuard::set(move |stage, path| {
            if stage == Stage::OpenFile && path.ends_with("m/config.json") {
                fs::remove_file(&config).unwrap();
                symlink(&outside_file, &config).unwrap();
            }
            None
        });
        assert!(scan(root.path()).models.is_empty());
        drop(hook);

        let root = tempfile::tempdir().unwrap();
        let dir = model(root.path(), "m");
        let index = dir.join(INDEX_NAME);
        fs::write(&index, r#"{"weight_map":{"a":"w.safetensors"}}"#).unwrap();
        assert_eq!(scan(root.path()).models.len(), 1, "对照：没有换");
        let outside_index = outside.path().join("index.json");
        fs::write(&outside_index, r#"{"weight_map":{"a":"w.safetensors"}}"#).unwrap();
        let _hook = HookGuard::set(move |stage, path| {
            if stage == Stage::OpenFile && path.ends_with(INDEX_NAME) {
                fs::remove_file(&index).unwrap();
                symlink(&outside_index, &index).unwrap();
            }
            None
        });
        assert!(scan(root.path()).models.is_empty());
    }

    /// #609（Codex）：只有 `NotFound` 当作「条目不存在」；别的枚举或 `fstatat` 错误让**这个模型**不完整，
    /// 其他模型照常扫描。
    #[test]
    fn only_not_found_means_the_entry_is_gone_other_errors_make_that_model_incomplete() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            let dir = model(root.path(), name);
            fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
            fs::create_dir(dir.join("sub")).unwrap();
        }
        assert_eq!(ids(&scan(root.path())), ["a", "b"]);

        let inject = |stage_wanted: Stage, suffix: &'static str, errno: Errno| {
            HookGuard::set(move |stage, path| {
                (stage == stage_wanted && path.ends_with(suffix)).then_some(errno)
            })
        };
        // 文件 fstatat 出 EIO：a 不完整，b 照常。
        {
            let _hook = inject(Stage::Stat, "a/tokenizer.json", Errno::EIO);
            assert_eq!(ids(&scan(root.path())), ["b"]);
        }
        // 同一个位置出 ENOENT（条目刚好消失）：当作没有，模型仍完整。
        {
            let _hook = inject(Stage::Stat, "a/tokenizer.json", Errno::ENOENT);
            assert_eq!(ids(&scan(root.path())), ["a", "b"]);
        }
        // 读子目录出 EACCES / EIO：不完整。
        for errno in [Errno::EACCES, Errno::EIO] {
            let _hook = inject(Stage::ReadDir, "a/sub", errno);
            assert_eq!(ids(&scan(root.path())), ["b"], "{errno}");
        }
        // 读模型目录本身出 EIO：不完整。
        {
            let _hook = inject(Stage::ReadDir, "b", Errno::EIO);
            assert_eq!(ids(&scan(root.path())), ["a"]);
        }
        // 打开子目录出 EMFILE：不完整；打开时 ENOENT（刚好消失）：当作没有。
        {
            let _hook = inject(Stage::OpenSubdir, "a/sub", Errno::EMFILE);
            assert_eq!(ids(&scan(root.path())), ["b"]);
        }
        {
            let _hook = inject(Stage::OpenSubdir, "a/sub", Errno::ENOENT);
            assert_eq!(ids(&scan(root.path())), ["a", "b"]);
        }
        // 没有注入：回到全部完整。
        assert_eq!(ids(&scan(root.path())), ["a", "b"]);
    }

    /// #609（Codex）：整个扫描有总工作预算；用完之后还没检查的模型进 `unchecked`，不在列表里。
    #[test]
    fn the_whole_scan_has_a_work_budget_and_unchecked_models_are_reported() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            model(root.path(), name);
        }
        // 根 3 项 + 每个模型 2 项：预算 9 刚好够。
        let all = scan_with(root.path(), &[], limits(100, 8, 9));
        assert_eq!(ids(&all), ["a", "b", "c"]);
        assert!(all.unchecked.is_empty());
        // 预算 7：a、b 检查完，预算耗尽，c 没检查。
        let some = scan_with(root.path(), &[], limits(100, 8, 7));
        assert_eq!(ids(&some), ["a", "b"]);
        assert_eq!(some.unchecked, ["c"]);
        // 预算 6：b 检查到一半就用完（不当成完整，也不当成不完整），b、c 都没检查。
        let fewer = scan_with(root.path(), &[], limits(100, 8, 6));
        assert_eq!(ids(&fewer), ["a"]);
        assert_eq!(fewer.unchecked, ["b", "c"]);
        // 预算比根目录项还少：一个模型也没检查，读到的名字进 unchecked，并报告根目录被截断。
        let none = scan_with(root.path(), &[], limits(100, 8, 2));
        assert!(none.models.is_empty());
        assert!(none.root_truncated);
        assert_eq!(none.unchecked.len(), 2);
        assert!(!all.root_truncated && !some.root_truncated);
    }

    /// #609（PR-Daemon B1）：单模型上限数的是已列出的全部名字，不是已处理的。嵌套的兄弟越多，子目录越没空间；
    /// 与 readdir 的顺序无关（上限只看个数）。
    #[test]
    fn the_per_model_cap_is_enforced_for_siblings_listed_before_descending() {
        // Codex 的最小例子：d/、config、权重、d/x，上限 3：共 4 项，不完整。
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("m");
        fs::create_dir_all(dir.join("d")).unwrap();
        fs::write(dir.join("config.json"), "{}").unwrap();
        fs::write(dir.join("w.safetensors"), b"w").unwrap();
        fs::write(dir.join("d").join("x"), b"x").unwrap();
        assert!(
            scan_with(root.path(), &[], limits(3, 8, 1_000))
                .models
                .is_empty()
        );
        assert_eq!(
            scan_with(root.path(), &[], limits(4, 8, 1_000))
                .models
                .len(),
            1
        );

        // 评审的例子：config、权重、f0..f9 加 sub/（里面 s 个文件）：顶层 13 项，总数 13 + s。
        for s in 0..=25usize {
            let root = tempfile::tempdir().unwrap();
            let dir = model(root.path(), "m");
            for index in 0..10 {
                fs::write(dir.join(format!("f{index}")), b"f").unwrap();
            }
            fs::create_dir(dir.join("sub")).unwrap();
            for index in 0..s {
                fs::write(dir.join("sub").join(format!("s{index}")), b"s").unwrap();
            }
            let listed = !scan_with(root.path(), &[], limits(30, 8, 1_000))
                .models
                .is_empty();
            assert_eq!(listed, 13 + s <= 30, "s={s}");
        }
    }

    /// #609（PR-Daemon B2）：溢出的那个模型已列出的名字也计入整体预算，下一个模型看到的是扣过的预算。
    #[test]
    fn a_model_that_overflows_still_charges_the_global_budget() {
        let root = tempfile::tempdir().unwrap();
        let a = model(root.path(), "a");
        fs::write(a.join("x"), b"x").unwrap();
        fs::write(a.join("y"), b"y").unwrap();
        model(root.path(), "b");
        // 根 2 项，预算 5 剩 3；a 有 4 项溢出（计 3），b 没有预算了：unchecked。
        let report = scan_with(root.path(), &[], limits(3, 8, 5));
        assert!(report.models.is_empty(), "{report:?}");
        assert_eq!(report.unchecked, ["b"]);
    }
}

//! 本地模型列表（接口规范 §2.9 `GET /v1/models/local` / `idoris models local --json`）。
//!
//! 只读文件系统：扫描 oMLX 模型目录（默认 `~/.omlx/models`，调用方注入），不连接 oMLX、不读任何密钥、
//! 不加载也不下载任何东西。任何单个目录的问题（符号链接、不完整、读不了的 `config.json`）都只影响那一个
//! 目录，永远不会让整个列表失败。
//!
//! 实现口径（规范没写到的地方，全部取保守一侧）：
//! - 符号链接一律不跟随：模型目录本身是符号链接时整项跳过；目录内的符号链接文件既不计入体积、也不算权重。
//! - 「完整」= 目录里有可读的常规文件 `config.json`、至少一个权重文件（`.safetensors` / `.gguf` / `.npz`），
//!   `model.safetensors.index.json` 列出的分片全部存在，且没有下载残留（`.incomplete` / `.partial` /
//!   `.part` / `.tmp` / `.downloading`）。不完整的目录不出现在列表里，也不算「已安装」。
//! - `config.json` 读得到但不是合法 JSON：目录仍算完整，`kind` 取目录里写的值，没有就是 `unknown`。
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
/// 单个模型目录最多遍历的文件数与深度，防止畸形目录拖住请求。
const MAX_FILES_PER_MODEL: usize = 100_000;
const MAX_DEPTH: usize = 8;

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

/// 扫描 `dir`。目录不存在、读不了都返回空列表。结果按 `omlx_id` 排序，输出稳定。
pub fn scan_local_models(dir: &Path, bindings: &[OmlxBinding]) -> Vec<LocalModel> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let by_id: BTreeMap<&str, &OmlxBinding> = bindings
        .iter()
        .map(|binding| (binding.omlx_id.as_str(), binding))
        .collect();
    let mut models = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') || !crate::catalog::valid_omlx_id(&name) {
            continue;
        }
        let path = entry.path();
        // 不跟随符号链接：`symlink_metadata` 看到的是链接本身。
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(inspected) = inspect_model_dir(&path) else {
            continue;
        };
        let binding = by_id.get(name.as_str());
        let kind = binding
            .and_then(|binding| binding.kind)
            .or_else(|| inspected.config.as_ref().map(infer_kind))
            .unwrap_or(ModelKind::Unknown);
        models.push(LocalModel {
            omlx_id: name,
            kind,
            size_bytes: inspected.size_bytes,
            catalog_id: binding.map(|binding| binding.catalog_id.clone()),
        });
    }
    models.sort_by(|a, b| a.omlx_id.cmp(&b.omlx_id));
    models
}

struct InspectedDir {
    /// `None` 表示 `config.json` 读得到但不是合法 JSON。
    config: Option<Value>,
    size_bytes: u64,
}

/// 目录完整时返回其体积与解析后的 `config.json`；不完整返回 `None`。
fn inspect_model_dir(dir: &Path) -> Option<InspectedDir> {
    let config_path = dir.join("config.json");
    let meta = fs::symlink_metadata(&config_path).ok()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let mut raw = Vec::new();
    fs::File::open(&config_path)
        .ok()?
        .take(MAX_CONFIG_BYTES)
        .read_to_end(&mut raw)
        .ok()?;
    let config = serde_json::from_slice::<Value>(&raw).ok();

    let mut walk = Walk::default();
    walk_dir(dir, 0, &mut walk);
    if walk.truncated || walk.partial || walk.weights == 0 {
        return None;
    }
    if !shards_present(dir) {
        return None;
    }
    Some(InspectedDir {
        config,
        size_bytes: walk.size_bytes,
    })
}

#[derive(Default)]
struct Walk {
    size_bytes: u64,
    files: usize,
    weights: usize,
    partial: bool,
    truncated: bool,
}

fn walk_dir(dir: &Path, depth: usize, walk: &mut Walk) {
    if depth > MAX_DEPTH {
        walk.truncated = true;
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        // 读不了的子目录：保守地当作不完整。
        walk.truncated = true;
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if meta.is_dir() {
            walk_dir(&entry.path(), depth + 1, walk);
        } else if meta.is_file() {
            walk.files += 1;
            if walk.files > MAX_FILES_PER_MODEL {
                walk.truncated = true;
                return;
            }
            if [".incomplete", ".partial", ".part", ".tmp", ".downloading"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            {
                walk.partial = true;
            }
            // 隐藏目录里的文件（暂存）不计体积；残留标记上面已经检查过。
            if !name.starts_with('.') && !is_in_hidden_component(dir, depth) {
                walk.size_bytes = walk.size_bytes.saturating_add(meta.len());
                if [".safetensors", ".gguf", ".npz"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
                {
                    walk.weights += 1;
                }
            }
        }
        // 符号链接：既不计体积，也不跟随。
    }
}

fn is_in_hidden_component(dir: &Path, depth: usize) -> bool {
    dir.components()
        .rev()
        .take(depth + 1)
        .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
}

/// `model.safetensors.index.json` 存在时，它列出的分片必须全部是目录里的常规文件。
fn shards_present(dir: &Path) -> bool {
    let index = dir.join("model.safetensors.index.json");
    let Ok(meta) = fs::symlink_metadata(&index) else {
        return true;
    };
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return false;
    }
    let Ok(raw) = fs::read(&index) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let Some(map) = value.get("weight_map").and_then(Value::as_object) else {
        return false;
    };
    map.values().all(|shard| {
        shard.as_str().is_some_and(|name| {
            !name.is_empty()
                && !name.contains(['/', '\\'])
                && name != ".."
                && fs::symlink_metadata(dir.join(name)).is_ok_and(|meta| meta.is_file())
        })
    })
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

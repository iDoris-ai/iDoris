//! 目录条目与 oMLX 模型目录的对应关系（接口规范 §2.9，wire 1.2.0）。
//!
//! `config/catalog.yaml` 的条目可以带两个可选的新字段：`omlx_id`（oMLX 模型目录名，即
//! `~/.omlx/models/<omlx_id>`）与 `kind`（模型类别）。它们不进 [`super::CatalogModel`]：现有的
//! 推荐、角色路由与 `/capabilities` 完全不读这两个字段，单独解析可以保证旧路径一字不变。

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use super::CatalogError;

/// `GET /v1/models/local` 的 `kind` 取值（§2.9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Chat,
    Vision,
    Embedding,
    Rerank,
    Image,
    Ocr,
    Tts,
    Unknown,
}

impl ModelKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Vision => "vision",
            Self::Embedding => "embedding",
            Self::Rerank => "rerank",
            Self::Image => "image",
            Self::Ocr => "ocr",
            Self::Tts => "tts",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "chat" => Self::Chat,
            "vision" => Self::Vision,
            "embedding" => Self::Embedding,
            "rerank" => Self::Rerank,
            "image" => Self::Image,
            "ocr" => Self::Ocr,
            "tts" => Self::Tts,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// 一个带 `omlx_id` 的目录条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmlxBinding {
    pub catalog_id: String,
    pub omlx_id: String,
    /// 目录里没写 `kind` 时为 `None`，由调用方退到 `config.json` 推断。
    pub kind: Option<ModelKind>,
}

/// `omlx_id` 是目录名，不是路径：不能为空、不能带分隔符、不能以 `.` 开头（隐藏目录与 `.`/`..`）。
pub fn valid_omlx_id(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.contains(['/', '\\', '\0'])
        && !value.chars().any(char::is_control)
}

/// 从已经解析的 YAML 里取出每个条目的 `omlx_id` / `kind`。`parse_catalog` 也会调用它做校验，所以
/// 非法的 `kind`、重复或不合法的 `omlx_id` 在加载目录时就失败，不会拖到请求期。
pub fn parse_omlx_bindings(raw: &Value) -> Result<Vec<OmlxBinding>, CatalogError> {
    let Some(models) = raw
        .as_mapping()
        .and_then(|root| field(root, "catalog"))
        .and_then(Value::as_sequence)
    else {
        return Ok(Vec::new());
    };
    let mut bindings = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, item) in models.iter().enumerate() {
        let path = format!("$.catalog[{index}]");
        let Some(model) = item.as_mapping() else {
            continue;
        };
        let omlx_id = match field(model, "omlx_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if valid_omlx_id(value) => Some(value.clone()),
            Some(_) => {
                return Err(CatalogError::new(
                    "must be a directory name (non-empty, no path separators, not starting with '.')",
                    format!("{path}.omlx_id"),
                ));
            }
        };
        let kind = match field(model, "kind") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(ModelKind::parse(value).ok_or_else(|| {
                CatalogError::new(format!("unknown kind {value:?}"), format!("{path}.kind"))
            })?),
            Some(_) => {
                return Err(CatalogError::new(
                    "must be a string",
                    format!("{path}.kind"),
                ));
            }
        };
        let Some(omlx_id) = omlx_id else {
            if kind.is_some() {
                return Err(CatalogError::new(
                    "kind requires omlx_id",
                    format!("{path}.kind"),
                ));
            }
            continue;
        };
        if !seen.insert(omlx_id.clone()) {
            return Err(CatalogError::new(
                format!("duplicate omlx_id: {omlx_id}"),
                "$.catalog",
            ));
        }
        let catalog_id = field(model, "id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        bindings.push(OmlxBinding {
            catalog_id,
            omlx_id,
            kind,
        });
    }
    Ok(bindings)
}

/// 读 `path` 指向的目录文件，取出 `omlx_id` / `kind`；与 [`super::load_catalog`] 一样不搜索默认位置。
pub fn load_omlx_bindings(path: impl AsRef<Path>) -> Result<Vec<OmlxBinding>, CatalogError> {
    let path = path.as_ref();
    let display = path.display().to_string();
    let text = fs::read_to_string(path).map_err(|error| {
        CatalogError::new(format!("failed to read catalog: {error}"), display.clone())
    })?;
    let value: Value = serde_yaml::from_str(&text).map_err(|error| {
        CatalogError::new(format!("catalog YAML is invalid: {error}"), display.clone())
    })?;
    parse_omlx_bindings(&value)
}

fn field<'a>(map: &'a Mapping, name: &str) -> Option<&'a Value> {
    map.get(Value::String(name.to_owned()))
}

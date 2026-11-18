//! 本机 chat 模型推荐与本地模型列表（接口规范 §2.9，wire 1.2.0）。
//!
//! `GET /v1/recommendation?role=chat`、`GET /v1/models/local` 与 CLI `idoris recommend --role chat --json`、
//! `idoris models local --json` 共用 [`LocalChatSource`]：同一个序列化结构、同一份逻辑，输出形状天然一致。
//!
//! 只读：不加载、不下载、不改任何状态，不连接 oMLX，**不读任何密钥**（包括 oMLX 的 `api_key`：本模块
//! 既不读 `IDORIS_OMLX_API_KEY`，也不读 `~/.omlx/settings.json`）。CLI 路径不开数据库、不取实例锁。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use idoris_recommender::catalog::{Catalog, OmlxBinding, load_catalog, load_omlx_bindings};
use idoris_recommender::chat_recommendation::{
    RecommendInput, probed_gpu_available_bytes, recommend_chat,
};
use idoris_recommender::local_models::{
    LocalModelsResponse, default_models_dir, scan_local_models,
};
use serde_json::Value;

use crate::{AppState, error_envelope_with_reason};

/// 一次推荐 / 列表所需的全部输入。创建后不可变；每次调用重新扫描模型目录。
#[derive(Debug, Clone)]
pub struct LocalChatSource {
    catalog: Catalog,
    bindings: Vec<OmlxBinding>,
    models_dir: PathBuf,
    ram_bytes: u64,
    allocated_budget_bytes: Option<u64>,
}

impl LocalChatSource {
    pub fn new(
        catalog: Catalog,
        bindings: Vec<OmlxBinding>,
        models_dir: PathBuf,
        ram_bytes: u64,
        allocated_budget_bytes: Option<u64>,
    ) -> Self {
        Self {
            catalog,
            bindings,
            models_dir,
            ram_bytes,
            allocated_budget_bytes,
        }
    }

    /// 真实环境：从 `catalog_path` 读目录，探测物理内存，模型目录为 `<home>/.omlx/models`。
    pub fn probe(
        catalog_path: &Path,
        home: &Path,
        allocated_budget_bytes: Option<u64>,
    ) -> Result<Self, String> {
        let catalog = load_catalog(catalog_path).map_err(|error| {
            format!(
                "无法加载模型目录 \"{}\"（IDORIS_CATALOG）：{error}",
                catalog_path.display()
            )
        })?;
        let bindings = load_omlx_bindings(catalog_path).map_err(|error| error.to_string())?;
        let ram_bytes = crate::host_facts::current_ram_bytes()?;
        Ok(Self::new(
            catalog,
            bindings,
            default_models_dir(home),
            ram_bytes,
            allocated_budget_bytes,
        ))
    }

    /// `GET /v1/recommendation?role=chat` 的响应体。
    pub fn recommendation_json(&self) -> Result<Value, String> {
        let installed = scan_local_models(&self.models_dir, &self.bindings);
        let recommendation = recommend_chat(&RecommendInput {
            ram_bytes: self.ram_bytes,
            allocated_budget_bytes: self.allocated_budget_bytes,
            gpu_available_bytes: probed_gpu_available_bytes(self.ram_bytes),
            installed: &installed,
            catalog: &self.catalog,
            bindings: &self.bindings,
        })
        .map_err(|error| error.to_string())?;
        serde_json::to_value(recommendation).map_err(|error| error.to_string())
    }

    /// `GET /v1/models/local` 的响应体。
    pub fn local_models_json(&self) -> Result<Value, String> {
        let models = scan_local_models(&self.models_dir, &self.bindings);
        serde_json::to_value(LocalModelsResponse { models }).map_err(|error| error.to_string())
    }
}

fn unavailable(what: &str) -> Response {
    error_envelope_with_reason(
        StatusCode::SERVICE_UNAVAILABLE,
        "recommendation_unavailable",
        "recommendation_unavailable",
        format!("{what}暂时不可用：这台机器的内存或模型目录没有探测到"),
    )
}

/// 查询串里恰好有一个 `role`，且值恰好是 `chat`。缺省、重复、其他值都不算。
fn role_is_chat(query: Option<&str>) -> bool {
    let mut roles = query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .filter(|(key, _)| *key == "role")
        .map(|(_, value)| value);
    matches!((roles.next(), roles.next()), (Some("chat"), None))
}

async fn run_blocking(
    source: Arc<LocalChatSource>,
    read: fn(&LocalChatSource) -> Result<Value, String>,
    what: &'static str,
) -> Response {
    match tokio::task::spawn_blocking(move || read(&source)).await {
        Ok(Ok(body)) => axum::Json(body).into_response(),
        Ok(Err(_)) | Err(_) => unavailable(what),
    }
}

/// `GET /v1/recommendation?role=chat`（只读；与 `/v1/models` 同级，由全局的数据 key 中间件鉴权）。
pub(crate) async fn get_recommendation(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    if !role_is_chat(query.as_deref()) {
        return error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "unsupported_role",
            "unsupported_role",
            "本版只支持 role=chat",
        );
    }
    match state.local_chat.clone() {
        Some(source) => {
            run_blocking(source, LocalChatSource::recommendation_json, "模型推荐").await
        }
        None => unavailable("模型推荐"),
    }
}

/// `GET /v1/models/local`（只读；同上鉴权）。
pub(crate) async fn get_local_models(State(state): State<Arc<AppState>>) -> Response {
    match state.local_chat.clone() {
        Some(source) => {
            run_blocking(source, LocalChatSource::local_models_json, "本地模型列表").await
        }
        None => unavailable("本地模型列表"),
    }
}

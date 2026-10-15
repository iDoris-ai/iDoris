//! `GET /api/tags` and `GET /api/ps`. Both parsers fail closed. A missing
//! list must not look like "nothing is loaded".

use idoris_backend::{BackendError, BackendStatus, ModelInfo, Pressure};

use super::upstream_error;

const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunningModel {
    pub ids: Vec<String>,
    pub size_bytes: u64,
}

pub(super) fn parse_tags(raw: &serde_json::Value) -> Result<Vec<ModelInfo>, BackendError> {
    let models = models_array(raw, "GET /api/tags")?;
    let mut seen = std::collections::HashSet::with_capacity(models.len());
    let mut parsed = Vec::with_capacity(models.len());
    for (index, model) in models.iter().enumerate() {
        let id = required_name(model, "GET /api/tags", index)?;
        if !seen.insert(id.clone()) {
            return Err(upstream_error(format!(
                "Ollama GET /api/tags has duplicate model id {id}"
            )));
        }
        let size_bytes = required_u64(model, "size", "GET /api/tags", index)?;
        parsed.push(ModelInfo {
            id,
            memory_gb: bytes_to_gib(size_bytes),
        });
    }
    Ok(parsed)
}

pub(super) fn parse_ps(raw: &serde_json::Value) -> Result<Vec<RunningModel>, BackendError> {
    let models = models_array(raw, "GET /api/ps")?;
    let mut seen = std::collections::HashSet::with_capacity(models.len());
    let mut parsed = Vec::with_capacity(models.len());
    for (index, model) in models.iter().enumerate() {
        let name = required_name(model, "GET /api/ps", index)?;
        let mut ids = vec![name.clone()];
        if let Some(alias) = model.get("model").and_then(serde_json::Value::as_str) {
            if alias.is_empty() {
                return Err(upstream_error(format!(
                    "Ollama GET /api/ps models[{index}].model must be a non-empty string when present"
                )));
            }
            if alias != name {
                ids.push(alias.to_string());
            }
        }
        for id in &ids {
            if !seen.insert(id.clone()) {
                return Err(upstream_error(format!(
                    "Ollama GET /api/ps has duplicate model id {id}"
                )));
            }
        }
        let size_bytes = required_u64(model, "size", "GET /api/ps", index)?;
        let _vram_bytes = required_u64(model, "size_vram", "GET /api/ps", index)?;
        parsed.push(RunningModel { ids, size_bytes });
    }
    Ok(parsed)
}

pub(super) fn status_from_ps(running: &[RunningModel]) -> Result<BackendStatus, BackendError> {
    let mut total_bytes: u64 = 0;
    let mut loaded = Vec::new();
    for model in running {
        total_bytes = total_bytes
            .checked_add(model.size_bytes)
            .ok_or_else(|| upstream_error("Ollama GET /api/ps model sizes overflow u64"))?;
        loaded.extend(model.ids.iter().cloned());
    }
    Ok(BackendStatus {
        // `/api/ps` has no pressure tier. Unknown is not Ok.
        pressure: Pressure::Unknown,
        used_gb: bytes_to_gib(total_bytes),
        // `/api/ps` has no engine memory cap. 0 means "not reported".
        model_memory_max_gb: 0.0,
        loaded,
    })
}

pub(super) fn contains_id(running: &[RunningModel], id: &str) -> bool {
    running
        .iter()
        .any(|model| model.ids.iter().any(|got| got == id))
}

fn models_array<'a>(
    raw: &'a serde_json::Value,
    endpoint: &str,
) -> Result<&'a Vec<serde_json::Value>, BackendError> {
    raw.get("models")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            upstream_error(format!(
                "Ollama {endpoint} response is missing a models array"
            ))
        })
}

fn required_name(
    model: &serde_json::Value,
    endpoint: &str,
    index: usize,
) -> Result<String, BackendError> {
    model
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            upstream_error(format!(
                "Ollama {endpoint} models[{index}].name must be a non-empty string"
            ))
        })
}

fn required_u64(
    model: &serde_json::Value,
    field: &str,
    endpoint: &str,
    index: usize,
) -> Result<u64, BackendError> {
    model
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            upstream_error(format!(
                "Ollama {endpoint} models[{index}].{field} must be a non-negative integer"
            ))
        })
}

fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / BYTES_PER_GIB
}

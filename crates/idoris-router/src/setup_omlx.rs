//! `idoris setup omlx`: probe an oMLX endpoint and write a components
//! directory that the server's own loader ([`crate::components::load_components`])
//! accepts. The API key is used for probing only and is never persisted or
//! echoed; at serve time it still comes from `IDORIS_OMLX_API_KEY`.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use idoris_contracts::{ComponentCard, load_policy::LoadMode};
use serde_json::{Value, json};

use crate::cli::SetupOmlxOptions;

const OMLX_CARD: &str = include_str!("../../../config/components/omlx.yaml");
const SUBSCRIPTION_CARD: &str = include_str!("../../../config/components/subscription.yaml");
const MANAGED: [&str; 2] = ["omlx.yaml", "subscription.yaml"];
const MAX_KEY_BYTES: u64 = 4096;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "[::1]", "localhost"];

#[derive(Debug)]
pub struct SetupError {
    pub code: &'static str,
    pub message: String,
}

impl SetupError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"status": "error", "code": self.code, "message": self.message})
    }
}

/// Reads one bounded line (the API key). The key is never part of an error.
pub fn read_api_key(reader: &mut impl Read) -> Result<String, SetupError> {
    let bad = || {
        SetupError::new(
            "invalid_api_key_input",
            "stdin 必须提供单行、非空、无空白的 API key",
        )
    };
    let mut line = String::new();
    std::io::BufReader::new(reader.take(MAX_KEY_BYTES + 1))
        .read_line(&mut line)
        .map_err(|_| bad())?;
    let key = line.trim_end_matches(['\n', '\r']);
    if key.is_empty()
        || line.len() as u64 > MAX_KEY_BYTES
        || key.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(bad());
    }
    Ok(key.to_string())
}

fn normalize_endpoint(raw: &str) -> Result<String, SetupError> {
    let bad = |why: &str| SetupError::new("invalid_endpoint", format!("endpoint 无效：{why}"));
    let url = reqwest::Url::parse(raw).map_err(|_| bad("无法解析"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("仅支持 http/https"));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(bad("不得包含凭据、路径、查询或片段"));
    }
    if !url.host_str().is_some_and(|h| LOOPBACK_HOSTS.contains(&h)) {
        return Err(bad("必须是 loopback（127.0.0.1 / [::1] / localhost）"));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

enum Reply {
    Ok(Value),
    Denied,
}

async fn get(client: &reqwest::Client, url: &str, key: Option<&str>) -> Result<Reply, SetupError> {
    let unreachable = || SetupError::new("unreachable", "无法连接 oMLX endpoint");
    let not_omlx = || SetupError::new("not_omlx", "endpoint 的响应不像 oMLX");
    let mut request = client.get(url);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let mut response = request.send().await.map_err(|_| unreachable())?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Ok(Reply::Denied);
    }
    if !status.is_success() {
        return Err(not_omlx());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unreachable())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY_BYTES {
            return Err(not_omlx());
        }
    }
    serde_json::from_slice(&body)
        .map(Reply::Ok)
        .map_err(|_| not_omlx())
}

/// Returns `(model ids, auth required)`.
async fn probe(
    base: &str,
    key: Option<&str>,
    mode: LoadMode,
) -> Result<(Vec<String>, bool), SetupError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|_| SetupError::new("unreachable", "无法创建 HTTP 客户端"))?;
    let models_url = format!("{base}/v1/models");
    let denied = |has_key| {
        if has_key {
            SetupError::new("auth_failed", "oMLX 拒绝了提供的 API key")
        } else {
            SetupError::new("auth_required", "oMLX 需要 API key（使用 --api-key-stdin）")
        }
    };
    // Unauthenticated first so `auth` reflects the server, not our key.
    let anon = get(&client, &models_url, None).await?;
    let (first, auth_required) = match (anon, key) {
        (Reply::Denied, None) => return Err(denied(false)),
        (Reply::Denied, Some(key)) => match get(&client, &models_url, Some(key)).await? {
            Reply::Denied => return Err(denied(true)),
            ok => (ok, true),
        },
        (ok, _) => (ok, false),
    };
    let key = key.filter(|_| auth_required);
    let Reply::Ok(listing) = first else {
        unreachable!()
    };
    let ids = listing
        .get("data")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| SetupError::new("not_omlx", "endpoint 的响应不像 oMLX"))?;
    if mode == LoadMode::OnDemand {
        // The lifecycle adapter needs these two; their absence means this is
        // not an oMLX the on_demand mode can drive.
        for (path, shape) in [("/v1/models/status", "models"), ("/api/status", "")] {
            let ok = match get(&client, &format!("{base}{path}"), key).await? {
                Reply::Denied => return Err(denied(key.is_some())),
                Reply::Ok(v) if shape.is_empty() => v.is_object(),
                Reply::Ok(v) => v.get(shape).is_some_and(Value::is_array),
            };
            if !ok {
                return Err(SetupError::new(
                    "not_omlx",
                    "endpoint 缺少 oMLX 生命周期状态接口",
                ));
            }
        }
    }
    Ok((ids, auth_required))
}

fn build_card(
    endpoint: &str,
    mode: LoadMode,
    bindings: &[(String, String)],
) -> Result<String, SetupError> {
    let internal = |e: &dyn std::fmt::Display| {
        SetupError::new("validation_failed", format!("生成组件卡失败：{e}"))
    };
    let mut card: ComponentCard = serde_yaml::from_str(OMLX_CARD).map_err(|e| internal(&e))?;
    card.endpoint = endpoint.to_string();
    if let Some(policy) = card.load_policy.as_mut() {
        policy.mode = mode;
        if mode == LoadMode::Resident {
            // The card contract ties resident to pinned keepalive.
            policy.keepalive = idoris_contracts::load_policy::Keepalive::Pinned { pinned: true };
            policy.admission = idoris_contracts::load_policy::Admission::Coexist;
        }
    }
    if !bindings.is_empty() {
        let models: serde_json::Map<String, Value> = bindings
            .iter()
            .map(|(r, m)| (r.clone(), json!(m)))
            .collect();
        card.extensions = Some(
            [
                (
                    "_degradation".to_string(),
                    json!("Removing these bindings disables role aliases on this backend."),
                ),
                ("idoris_role_models".to_string(), Value::Object(models)),
            ]
            .into_iter()
            .collect(),
        );
    }
    serde_yaml::to_string(&card).map_err(|e| internal(&e))
}

/// Entry point. `api_key` is used only for probing.
pub async fn run(options: &SetupOmlxOptions, api_key: Option<&str>) -> Result<Value, SetupError> {
    let endpoint = normalize_endpoint(&options.endpoint)?;
    for (role, _) in &options.bindings {
        if !idoris_policy::ROLES.iter().any(|r| r.as_str() == role) {
            return Err(SetupError::new(
                "invalid_role",
                format!("未知角色 {role:?}"),
            ));
        }
    }
    check_out_dir(&options.out_dir, options.force)?;
    let (models, auth_required) = probe(&endpoint, api_key, options.load_mode).await?;
    for (role, model) in &options.bindings {
        if !models.contains(model) {
            return Err(SetupError::new(
                "model_not_found",
                format!("角色 {role} 绑定的模型不在 oMLX 的模型列表中"),
            ));
        }
    }
    let card = build_card(&endpoint, options.load_mode, &options.bindings)?;
    let dir = write_validated(&options.out_dir, options.force, &card)?;
    let bindings: serde_json::Map<String, Value> = options
        .bindings
        .iter()
        .map(|(r, m)| (r.clone(), json!(m)))
        .collect();
    let mut next = json!({"env": {"IDORIS_COMPONENTS_DIR": dir.display().to_string()}});
    if auth_required {
        next["needs_env"] = json!([crate::setup_omlx::KEY_ENV]);
    }
    Ok(json!({
        "status": "ok", "endpoint": endpoint,
        "auth": if auth_required { "required" } else { "none" },
        "models": models, "bindings": bindings,
        "components_dir": dir.display().to_string(), "next": next,
    }))
}

pub const KEY_ENV: &str = idoris_upstream::omlx::OMLX_API_KEY_ENV;

fn check_out_dir(dir: &Path, force: bool) -> Result<(), SetupError> {
    let not_empty = |m: &str| SetupError::new("out_dir_not_empty", m.to_string());
    if !dir.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|_| not_empty("out-dir 不是可读目录"))?;
    let names: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    if force && names.iter().all(|n| MANAGED.iter().any(|m| n == m)) {
        return Ok(());
    }
    Err(not_empty(
        "out-dir 非空；--force 只允许覆盖 omlx.yaml / subscription.yaml",
    ))
}

/// Stage in a sibling dir, run the server's loader on it, then move into
/// place. Nothing in `out_dir` changes unless validation passed, and replacing
/// an existing dir is all-or-nothing: the old managed files are renamed aside
/// first and restored if any later step fails. `out_dir` is re-checked right
/// before replacing (the probe may have taken seconds), so a dir that gained
/// foreign files meanwhile is not clobbered. Write failures are `write_failed`.
fn write_validated(out_dir: &Path, force: bool, card: &str) -> Result<PathBuf, SetupError> {
    let io = |e: std::io::Error| SetupError::new("write_failed", format!("写入失败：{e}"));
    let parent = match out_dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent).map_err(io)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let stage = parent.join(format!(".idoris-setup-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&stage).map_err(io)?;
    let result = (|| {
        std::fs::write(stage.join("omlx.yaml"), card).map_err(io)?;
        std::fs::write(stage.join("subscription.yaml"), SUBSCRIPTION_CARD).map_err(io)?;
        crate::components::load_components(&stage, false).map_err(|e| {
            SetupError::new(
                "validation_failed",
                e.to_string()
                    .replace(&stage.display().to_string(), "<staging>"),
            )
        })?;
        check_out_dir(out_dir, force)?;
        if out_dir.exists() {
            replace_managed(&stage, out_dir).map_err(io)?;
        } else {
            std::fs::rename(&stage, out_dir).map_err(io)?;
        }
        out_dir.canonicalize().map_err(io)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Moves the staged managed files into an existing `out_dir`, all or nothing.
fn replace_managed(stage: &Path, out_dir: &Path) -> std::io::Result<()> {
    let tag = format!(".idoris-bak-{}", std::process::id());
    let mut backed_up: Vec<&str> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();
    let attempt = (|| {
        for name in MANAGED {
            let target = out_dir.join(name);
            if target.exists() {
                std::fs::rename(&target, out_dir.join(format!("{name}{tag}")))?;
                backed_up.push(name);
            }
        }
        for name in MANAGED {
            std::fs::rename(stage.join(name), out_dir.join(name))?;
            placed.push(name);
        }
        Ok(())
    })();
    if attempt.is_err() {
        for name in &placed {
            let _ = std::fs::rename(out_dir.join(name), stage.join(name));
        }
        for name in &backed_up {
            let _ = std::fs::rename(out_dir.join(format!("{name}{tag}")), out_dir.join(name));
        }
    } else {
        for name in &backed_up {
            let _ = std::fs::remove_file(out_dir.join(format!("{name}{tag}")));
        }
    }
    attempt
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn card() -> String {
        build_card("http://127.0.0.1:8000", LoadMode::OnDemand, &[]).unwrap()
    }

    #[test]
    fn foreign_file_appearing_after_the_first_check_is_not_clobbered() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("comp");
        std::fs::create_dir(&out).unwrap();
        // Simulates a dir populated while the probe was in flight.
        std::fs::write(out.join("mine.txt"), "keep").unwrap();
        std::fs::write(out.join("omlx.yaml"), "old").unwrap();
        let err = write_validated(&out, true, &card()).unwrap_err();
        assert_eq!(err.code, "out_dir_not_empty");
        assert_eq!(
            std::fs::read_to_string(out.join("mine.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("omlx.yaml")).unwrap(),
            "old"
        );
    }

    #[test]
    fn mid_replace_failure_restores_the_previous_files() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("comp");
        let stage = tmp.path().join("stage");
        std::fs::create_dir(&out).unwrap();
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(out.join("omlx.yaml"), "old-omlx").unwrap();
        std::fs::write(out.join("subscription.yaml"), "old-sub").unwrap();
        std::fs::write(stage.join("omlx.yaml"), "new-omlx").unwrap();
        // subscription.yaml missing from the stage: the second move fails
        // after omlx.yaml was already replaced.
        replace_managed(&stage, &out).unwrap_err();
        assert_eq!(
            std::fs::read_to_string(out.join("omlx.yaml")).unwrap(),
            "old-omlx"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("subscription.yaml")).unwrap(),
            "old-sub"
        );
        let mut names: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["omlx.yaml", "subscription.yaml"]);
    }

    #[test]
    fn io_failure_is_write_failed_not_validation_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let err = write_validated(&blocker.join("comp"), false, &card()).unwrap_err();
        assert_eq!(err.code, "write_failed");
    }
}

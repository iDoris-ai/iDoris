//! `idoris setup omlx`: oMLX 作为 [`crate::setup_engine::SetupEngine`] 的第一个实现。通用流程（采纳、自动探测、托管、
//! 生成并校验组件目录）在 `setup_engine`；这里只有 oMLX 才有的东西：候选端口、怎么认出 oMLX（`/v1/models` 的列表加两个
//! 生命周期状态接口）、卡的模板、`~/.omlx/models`、`oMLX.app` 的位置（经 [`OmlxRecipe`]）。
//! The API key is used for probing only and is never persisted or
//! echoed; at serve time it still comes from `IDORIS_OMLX_API_KEY`.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_recommender::local_models::{default_models_dir, scan_local_models};
#[cfg(unix)]
use idoris_upstream::omlx::engine::OmlxRecipe;
use serde_json::Value;

use crate::cli::SetupOptions;
#[cfg(unix)]
use crate::setup_engine::recipe_backed;
use crate::setup_engine::{
    FindFailure, FoundExecutable, MANAGED_PORT_RANGE, Probed, SetupEngine, run_setup,
};
pub use crate::setup_engine::{SetupError, read_api_key};

const OMLX_CARD: &str = include_str!("../../../config/components/omlx.yaml");
const SUBSCRIPTION_CARD: &str = include_str!("../../../config/components/subscription.yaml");
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// `--auto` 依次探测的 loopback 端口（接口规范 §2.7.8；Agent24 原来在自己代码里探测的同一组）。
pub const AUTO_PORTS: [u16; 3] = [8088, 8000, 8001];

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

/// `lifecycle` 为真时还要求 `/v1/models/status` 与 `/api/status`（oMLX 生命周期适配器用的两个接口）。
async fn probe(base: &str, key: Option<&str>, lifecycle: bool) -> Result<Probed, SetupError> {
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
    if lifecycle {
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
    Ok(Probed {
        models: ids,
        auth_required,
    })
}

/// oMLX 的 `setup`：宿主相关的位置（家目录）与测试要换掉的参数都在实例里。
#[derive(Debug, Clone)]
pub struct OmlxSetup {
    #[cfg(unix)]
    recipe: OmlxRecipe,
    models_dir: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    explicit_executable: Option<PathBuf>,
    auto_ports: Vec<u16>,
    managed_ports: RangeInclusive<u16>,
}

impl OmlxSetup {
    /// 取 `HOME`：`~/Applications/oMLX.app` 这个发现位置与 `~/.omlx/models`。
    pub fn from_env() -> Self {
        Self::new(std::env::var_os("HOME").map(PathBuf::from))
    }

    /// `home` 为 `None`、为空或不是绝对路径时，没有家目录下的发现位置，也没有模型目录。
    pub fn new(home: Option<PathBuf>) -> Self {
        let home = home.filter(|home| home.is_absolute());
        #[cfg(unix)]
        let recipe = match &home {
            Some(home) => OmlxRecipe::new().with_home(home),
            None => OmlxRecipe::new(),
        };
        Self {
            #[cfg(unix)]
            recipe,
            models_dir: home.as_deref().map(default_models_dir),
            explicit_executable: None,
            auto_ports: AUTO_PORTS.to_vec(),
            managed_ports: MANAGED_PORT_RANGE,
        }
    }

    /// 显式给定可执行文件（只看它，不回退；规范 §2.7.8 没有对应的命令行旗标）。
    pub fn with_explicit_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.explicit_executable = Some(path.into());
        self
    }

    /// 换掉 `--auto` 的候选端口。
    pub fn with_auto_ports(mut self, ports: impl IntoIterator<Item = u16>) -> Self {
        self.auto_ports = ports.into_iter().collect();
        self
    }

    /// 换掉托管缺省端口的候选范围。
    pub fn with_managed_port_range(mut self, range: RangeInclusive<u16>) -> Self {
        self.managed_ports = range;
        self
    }

    /// 换掉离线核对 `--bind` 用的模型目录。
    pub fn with_models_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.models_dir = Some(dir.into());
        self
    }
}

impl SetupEngine for OmlxSetup {
    fn kind(&self) -> &str {
        "omlx"
    }

    fn display_name(&self) -> &'static str {
        "oMLX"
    }

    fn install_hint(&self) -> &'static str {
        "请先安装 oMLX（把 oMLX.app 放进 /Applications 或 ~/Applications），再重新运行"
    }

    fn key_env(&self) -> &'static str {
        KEY_ENV
    }

    async fn locate_executable(&self) -> Option<PathBuf> {
        #[cfg(unix)]
        {
            recipe_backed::locate(&self.recipe, self.explicit_executable.as_deref()).await
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    async fn discover_executable(&self) -> Result<FoundExecutable, FindFailure> {
        #[cfg(unix)]
        {
            recipe_backed::discover(&self.recipe, self.explicit_executable.as_deref()).await
        }
        #[cfg(not(unix))]
        {
            Err(FindFailure::Unsupported)
        }
    }

    fn auto_ports(&self) -> &[u16] {
        &self.auto_ports
    }

    fn managed_port_range(&self) -> RangeInclusive<u16> {
        self.managed_ports.clone()
    }

    fn installed_models(&self) -> Option<Vec<String>> {
        let dir = self.models_dir.as_deref()?;
        Some(
            scan_local_models(dir, &[])
                .into_iter()
                .map(|model| model.omlx_id)
                .collect(),
        )
    }

    async fn probe(
        &self,
        endpoint: &str,
        key: Option<&str>,
        lifecycle: bool,
    ) -> Result<Probed, SetupError> {
        probe(endpoint, key, lifecycle).await
    }

    fn card_file(&self) -> &'static str {
        "omlx.yaml"
    }

    fn card_template(&self) -> Result<ComponentCard, SetupError> {
        serde_yaml::from_str(OMLX_CARD)
            .map_err(|e| SetupError::new("validation_failed", format!("生成组件卡失败：{e}")))
    }

    fn static_files(&self) -> &'static [(&'static str, &'static str)] {
        &[("subscription.yaml", SUBSCRIPTION_CARD)]
    }
}

/// Entry point. `api_key` is used only for probing.
pub async fn run(options: &SetupOptions, api_key: Option<&str>) -> Result<Value, SetupError> {
    run_setup(&OmlxSetup::from_env(), options, api_key).await
}

pub const KEY_ENV: &str = idoris_upstream::omlx::OMLX_API_KEY_ENV;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use idoris_upstream::engine::EngineRecipe;

    const USER_APP: &str = "Applications/oMLX.app/Contents/MacOS/omlx-cli";

    /// 家目录传到了配方上：`~/Applications/oMLX.app` 是发现位置之一（规范 §2.7.6 的发现顺序（3））。
    /// 系统级的 `/Applications/oMLX.app` 在这台机器上有没有不影响这里：断言的是配方的计划，不是磁盘上的结果。
    #[test]
    fn the_home_directory_reaches_the_recipe_and_the_models_dir() {
        let home = PathBuf::from("/Users/someone");
        let setup = OmlxSetup::new(Some(home.clone()));
        let apps = setup.recipe.discovery_plan().app_executables;
        assert!(apps.contains(&home.join(USER_APP)), "{apps:?}");
        assert_eq!(setup.models_dir, Some(home.join(".omlx/models")));
    }

    /// 没有家目录（缺失或不是绝对路径）就没有家目录下的发现位置，也没有模型目录：不回退到相对路径。
    #[test]
    fn a_missing_or_relative_home_means_no_user_level_locations() {
        for home in [
            None,
            Some(PathBuf::from("relative/home")),
            Some(PathBuf::new()),
        ] {
            let setup = OmlxSetup::new(home.clone());
            let apps = setup.recipe.discovery_plan().app_executables;
            assert_eq!(apps.len(), 1, "{home:?}: {apps:?}");
            assert!(setup.models_dir.is_none(), "{home:?}");
        }
    }

    #[test]
    fn the_setup_engine_is_the_reserved_omlx_kind_and_the_specs_candidate_order() {
        let setup = OmlxSetup::new(None);
        assert_eq!(
            setup.kind(),
            idoris_upstream::engine::EngineKind::OMLX.as_str()
        );
        assert_eq!(setup.auto_ports(), [8088, 8000, 8001]);
        assert_eq!(setup.managed_port_range(), 19000..=19999);
    }
}

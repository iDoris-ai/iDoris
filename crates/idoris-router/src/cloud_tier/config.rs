//! 云端档配置（YAML）。
//!
//! 由环境变量 [`CONFIG_ENV`]（`IDORIS_CLOUD_TIER_CONFIG`）指向的文件加载；**不设就完全不启用**：
//! 本机的一切行为一字不变，也不会为此读任何文件、打开任何数据库表。
//!
//! 云端地址在这里；**密钥永远不写在配置里**，只写来源（和云上游配置 `api_key_env` / `api_key_file` 同一个做法）：
//!
//! ```yaml
//! schema_version: 1
//! base_url: https://idoris.ai/api        # 数据面：POST {base_url}/v1/chat/completions。https；只有回环地址允许 http（测试用）
//! # control_url: https://idoris.ai/control/v1   # 控制面（配对用）。缺省 = base_url 的源（scheme + host + 端口）+ /control/v1，且必须同源
//!
//! credential:                             # 云凭证从哪来
//!   source: paired_device                 # 个人：`idoris cloud pair` 兑换出来的设备凭证（缺省）
//!   # source: node_file                   # 组织：运维用云控制面签发的 Node 凭证，放在一个 0600 的文件里
//!   # file: /etc/idoris/cloud-node-credential
//!   # source: node_env                    # 组织：同上，放在环境变量里（不推荐：环境变量会被同用户的进程读到）
//!   # env: IDORIS_CLOUD_NODE_CREDENTIAL
//!
//! model: idoris/agent                     # 没有声明角色的请求（具体模型名、idoris/auto）发给云端时用的云端模型名
//! role_models:                            # 声明了角色的请求：角色 -> 云端模型名。只有这里列出的角色会被云端档接手
//!   fast: idoris/agent                    #   （只允许 fast / daily / deep：decide 是本机判定器，永远不出网；
//!   daily: idoris/agent                   #    embed / rerank / vision 不是这个端点的能力）
//!   deep: idoris/agent
//!
//! state_dir: ./cloud-tier                 # 可选：设备凭证放哪。缺省 = 数据库文件所在目录下的 cloud-tier/
//!
//! limits:                                 # 全部可选，括号里是默认值
//!   connect_timeout_ms: 5000
//!   total_timeout_ms: 150000              # 一次调用从开始到读完响应体（云端自己的总期限是 120 s，这里留出余量）
//!   max_response_bytes: 4194304
//!   max_concurrent: 8                     # 同时在途的云端档调用数；满了不排队，当作「云端档繁忙」
//! ```
//!
//! 所有对象拒绝未知字段。相对路径相对于配置文件所在的目录。
//!
//! # 为什么云端模型名要显式配置
//!
//! 本机请求里的 `model` 是本机的东西（`idoris/daily` 这样的角色别名，或者本机某个模型的 id），云端的路由簿里没有它们；
//! 云端认的是它自己的路由名（`GET /api/v1/models` 列出来的那些）。所以发往云端之前 `model` **一律被替换**成这里配置的名字，
//! 本机的模型名和角色别名不出网。没有映射的角色就没有云端档：宁可不回落，也不猜一个模型名。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;

/// 指向配置文件的环境变量。不设 = 云端档关闭。
pub const CONFIG_ENV: &str = "IDORIS_CLOUD_TIER_CONFIG";

/// 当前支持的配置 schema 版本。
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// 云端档能接手的角色。`decide` 是本机判定器（永远本机），`embed` / `rerank` 不是 chat 端点的能力，
/// `vision` 云端档没有声明这个能力；`auto` 不是模型角色。
pub const CLOUD_ROLES: [&str; 3] = ["fast", "daily", "deep"];

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_TOTAL_TIMEOUT_MS: u64 = 150_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENT: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    base_url: String,
    #[serde(default)]
    control_url: Option<String>,
    #[serde(default)]
    credential: RawCredential,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    role_models: BTreeMap<String, String>,
    #[serde(default)]
    state_dir: Option<PathBuf>,
    #[serde(default)]
    limits: RawLimits,
}

/// 注意 `PairedDevice` 必须是**结构体变体**（`{}`），不能是单元变体：内部带标签的枚举里，单元变体会悄悄忽略多余的字段，
/// 那样 `credential: {source: paired_device, secret: idk_…}` 就会被收下并丢弃，密钥被写进了一个以为安全的配置文件。
#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
enum RawCredential {
    PairedDevice {},
    NodeFile { file: PathBuf },
    NodeEnv { env: String },
}

impl Default for RawCredential {
    fn default() -> Self {
        RawCredential::PairedDevice {}
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    #[serde(default)]
    connect_timeout_ms: Option<u64>,
    #[serde(default)]
    total_timeout_ms: Option<u64>,
    #[serde(default)]
    max_response_bytes: Option<usize>,
    #[serde(default)]
    max_concurrent: Option<usize>,
}

/// 云凭证的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSourceConfig {
    /// 个人：`idoris cloud pair` 写下的设备凭证。
    PairedDevice,
    /// 组织：运维签发的 Node 凭证，从一个文件读（每次调用重新读，轮换不用重启）。
    NodeFile(PathBuf),
    /// 组织：同上，从环境变量读（启动时读一次）。
    NodeEnv(String),
}

/// 校验过的云端档配置。
#[derive(Debug, Clone)]
pub struct CloudTierConfig {
    /// 数据面基址（不含结尾 `/`）。
    pub base_url: Url,
    /// `POST` 的完整地址：`{base_url}/v1/chat/completions`。
    pub chat_url: Url,
    /// 控制面基址（不含结尾 `/`）：`…/control/v1`。
    pub control_url: Url,
    pub credential: CredentialSourceConfig,
    /// 没有声明角色的请求用的云端模型名。
    pub default_model: Option<String>,
    /// 角色 -> 云端模型名，键是 [`CLOUD_ROLES`] 里的角色。
    pub role_models: BTreeMap<String, String>,
    pub state_dir: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub total_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_concurrent: usize,
}

impl CloudTierConfig {
    /// 读文件并校验。相对路径相对于配置文件所在的目录。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("无法读取云端档配置 \"{}\"：{error}", path.display()))?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&text, dir)
            .map_err(|error| format!("云端档配置 \"{}\" 无效：{error}", path.display()))
    }

    /// 解析并校验。`base_dir` 用来解析相对路径。
    pub fn parse(text: &str, base_dir: &Path) -> Result<Self, String> {
        let raw: RawConfig = serde_yaml::from_str(text).map_err(|error| error.to_string())?;
        if raw.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(format!(
                "schema_version 必须是 {CONFIG_SCHEMA_VERSION}（读到 {}）",
                raw.schema_version
            ));
        }
        let base = idoris_upstream::cloud::validate_base_url(&raw.base_url)
            .map_err(|error| format!("base_url：{error}"))?;
        let base_url = with_path(&base, base.path().trim_end_matches('/'))?;
        let chat_url = with_path(
            &base,
            &format!("{}/v1/chat/completions", base.path().trim_end_matches('/')),
        )?;
        let control_url = match raw.control_url.as_deref() {
            Some(explicit) => {
                let url = idoris_upstream::cloud::validate_base_url(explicit)
                    .map_err(|error| format!("control_url：{error}"))?;
                if url.origin() != base.origin() {
                    return Err(
                        "control_url 必须和 base_url 同源（scheme、主机、端口都相同）：配对兑换出来的凭证只会发往它被签发的那个源"
                            .to_string(),
                    );
                }
                with_path(&url, url.path().trim_end_matches('/'))?
            }
            None => with_path(&base, "/control/v1")?,
        };
        let credential = match raw.credential {
            RawCredential::PairedDevice {} => CredentialSourceConfig::PairedDevice,
            RawCredential::NodeFile { file } => {
                CredentialSourceConfig::NodeFile(resolve(base_dir, file))
            }
            RawCredential::NodeEnv { env } => {
                if !valid_env_name(&env) {
                    return Err(format!("credential.env 不是合法的环境变量名：\"{env}\""));
                }
                CredentialSourceConfig::NodeEnv(env)
            }
        };
        let default_model = match raw.model {
            Some(model) => Some(validate_model("model", model)?),
            None => None,
        };
        let mut role_models = BTreeMap::new();
        for (role, model) in raw.role_models {
            if !CLOUD_ROLES.contains(&role.as_str()) {
                return Err(format!(
                    "role_models 里的角色 \"{role}\" 云端档不接手：只允许 {}",
                    CLOUD_ROLES.join(" / ")
                ));
            }
            role_models.insert(
                role.clone(),
                validate_model(&format!("role_models.{role}"), model)?,
            );
        }
        if default_model.is_none() && role_models.is_empty() {
            return Err(
                "至少要配置 model（没有声明角色的请求）或 role_models（声明了角色的请求）之一：没有云端模型名就没有云端档"
                    .to_string(),
            );
        }
        let limits = raw.limits;
        let positive = |name: &str, value: Option<u64>, default: u64| -> Result<u64, String> {
            match value {
                None => Ok(default),
                Some(0) => Err(format!("limits.{name} 必须大于 0")),
                Some(value) => Ok(value),
            }
        };
        let connect = positive(
            "connect_timeout_ms",
            limits.connect_timeout_ms,
            DEFAULT_CONNECT_TIMEOUT_MS,
        )?;
        let total = positive(
            "total_timeout_ms",
            limits.total_timeout_ms,
            DEFAULT_TOTAL_TIMEOUT_MS,
        )?;
        if total < connect {
            return Err("limits.total_timeout_ms 不能小于 connect_timeout_ms".to_string());
        }
        let max_response_bytes = match limits.max_response_bytes {
            None => DEFAULT_MAX_RESPONSE_BYTES,
            Some(0) => return Err("limits.max_response_bytes 必须大于 0".to_string()),
            Some(value) => value,
        };
        let max_concurrent = match limits.max_concurrent {
            None => DEFAULT_MAX_CONCURRENT,
            Some(0) => return Err("limits.max_concurrent 必须大于 0".to_string()),
            Some(value) => value,
        };
        Ok(Self {
            base_url,
            chat_url,
            control_url,
            credential,
            default_model,
            role_models,
            state_dir: raw.state_dir.map(|dir| resolve(base_dir, dir)),
            connect_timeout: Duration::from_millis(connect),
            total_timeout: Duration::from_millis(total),
            max_response_bytes,
            max_concurrent,
        })
    }

    /// 数据面所在的源：`scheme://host[:port]`，设备凭证绑定到它。
    pub fn origin(&self) -> String {
        self.base_url.origin().ascii_serialization()
    }

    /// 设备凭证放哪：配置里的 `state_dir`，否则数据库文件所在目录下的 `cloud-tier/`。
    pub fn state_dir_for(&self, db_path: &Path) -> PathBuf {
        match &self.state_dir {
            Some(dir) => dir.clone(),
            None => db_path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .join("cloud-tier"),
        }
    }

    /// 这个请求要发给云端的模型名：声明了（目录里的）角色就按角色映射，没有映射就是 `None`；
    /// 没有声明角色（具体模型名、`idoris/auto`）用默认模型名。
    pub fn cloud_model_for(&self, role: Option<&str>) -> Option<&str> {
        match role {
            Some(role) => self.role_models.get(role).map(String::as_str),
            None => self.default_model.as_deref(),
        }
    }
}

fn resolve(base_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

fn with_path(url: &Url, path: &str) -> Result<Url, String> {
    let mut out = url.clone();
    out.set_path(if path.is_empty() { "/" } else { path });
    // 没有 path 的基址在 `Url` 里总是 `/`；统一去掉结尾的 `/` 再拼，保证 `{base}/v1/...` 里没有双斜杠。
    Ok(out)
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}

fn validate_model(field: &str, model: String) -> Result<String, String> {
    if model.is_empty()
        || model.len() > 256
        || model != model.trim()
        || !model.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(format!(
            "{field} 必须是 1 到 256 个可见 ASCII 字符的云端模型名（没有空白、控制字符）"
        ));
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn parse(extra: &str) -> Result<CloudTierConfig, String> {
        CloudTierConfig::parse(
            &format!(
                "schema_version: 1\nbase_url: https://idoris.ai/api\nmodel: idoris/agent\n{extra}"
            ),
            Path::new("/etc/idoris"),
        )
    }

    #[test]
    fn a_minimal_config_derives_the_data_and_control_urls_from_the_base_url() {
        let config = parse("").unwrap();
        assert_eq!(
            config.chat_url.as_str(),
            "https://idoris.ai/api/v1/chat/completions"
        );
        assert_eq!(config.control_url.as_str(), "https://idoris.ai/control/v1");
        assert_eq!(config.origin(), "https://idoris.ai");
        assert_eq!(config.credential, CredentialSourceConfig::PairedDevice);
        assert_eq!(config.cloud_model_for(None), Some("idoris/agent"));
        assert_eq!(config.cloud_model_for(Some("daily")), None);
        assert_eq!(config.max_concurrent, 8);
        assert_eq!(config.total_timeout, Duration::from_millis(150_000));
    }

    #[test]
    fn a_trailing_slash_or_a_port_does_not_double_the_slashes() {
        for (base, chat) in [
            (
                "https://idoris.ai/api/",
                "https://idoris.ai/api/v1/chat/completions",
            ),
            (
                "https://idoris.ai/api",
                "https://idoris.ai/api/v1/chat/completions",
            ),
            ("https://idoris.ai", "https://idoris.ai/v1/chat/completions"),
            (
                "https://idoris.ai:8443/x/y/",
                "https://idoris.ai:8443/x/y/v1/chat/completions",
            ),
            (
                "http://127.0.0.1:9/api",
                "http://127.0.0.1:9/api/v1/chat/completions",
            ),
        ] {
            let config = CloudTierConfig::parse(
                &format!("schema_version: 1\nbase_url: {base}\nmodel: m\n"),
                Path::new("."),
            )
            .unwrap();
            assert_eq!(config.chat_url.as_str(), chat, "{base}");
        }
    }

    #[test]
    fn plain_http_is_only_for_loopback_and_credentials_in_the_url_are_refused() {
        for bad in [
            "http://idoris.ai/api",
            "http://10.0.0.5/api",
            "ftp://idoris.ai/api",
            "https://user:pass@idoris.ai/api",
            "https://idoris.ai/api?x=1",
            "https://idoris.ai/api#frag",
            "idoris.ai/api",
            "",
        ] {
            let result = CloudTierConfig::parse(
                &format!("schema_version: 1\nbase_url: \"{bad}\"\nmodel: m\n"),
                Path::new("."),
            );
            assert!(result.is_err(), "{bad:?} must be refused");
        }
        for ok in [
            "http://127.0.0.1:8080/api",
            "http://localhost:8080",
            "http://[::1]:8080/api",
        ] {
            assert!(
                CloudTierConfig::parse(
                    &format!("schema_version: 1\nbase_url: \"{ok}\"\nmodel: m\n"),
                    Path::new("."),
                )
                .is_ok(),
                "{ok}"
            );
        }
    }

    #[test]
    fn the_control_url_must_be_the_same_origin_as_the_data_plane() {
        let ok = parse("control_url: https://idoris.ai/other/control/v1\n").unwrap();
        assert_eq!(
            ok.control_url.as_str(),
            "https://idoris.ai/other/control/v1"
        );
        for other in [
            "https://evil.example/control/v1",
            "https://idoris.ai:8443/control/v1",
            "http://127.0.0.1:9/control/v1",
        ] {
            let error = parse(&format!("control_url: {other}\n")).unwrap_err();
            assert!(error.contains("同源"), "{other}: {error}");
        }
    }

    #[test]
    fn credential_sources_parse_and_resolve_relative_paths_against_the_config_directory() {
        let node =
            parse("credential: {source: node_file, file: secrets/node.key}\nstate_dir: tier\n")
                .unwrap();
        assert_eq!(
            node.credential,
            CredentialSourceConfig::NodeFile(PathBuf::from("/etc/idoris/secrets/node.key"))
        );
        assert_eq!(node.state_dir, Some(PathBuf::from("/etc/idoris/tier")));
        let absolute = parse("credential: {source: node_file, file: /run/secrets/node}\n").unwrap();
        assert_eq!(
            absolute.credential,
            CredentialSourceConfig::NodeFile(PathBuf::from("/run/secrets/node"))
        );
        let env =
            parse("credential: {source: node_env, env: IDORIS_CLOUD_NODE_CREDENTIAL}\n").unwrap();
        assert_eq!(
            env.credential,
            CredentialSourceConfig::NodeEnv("IDORIS_CLOUD_NODE_CREDENTIAL".into())
        );
        assert_eq!(
            parse("credential: {source: paired_device}\n")
                .unwrap()
                .credential,
            CredentialSourceConfig::PairedDevice
        );
        for bad in [
            "credential: {source: node_env, env: lower}\n",
            "credential: {source: node_env, env: 1ABC}\n",
            "credential: {source: node_env}\n",
            "credential: {source: node_file}\n",
            "credential: {source: secret_in_config, secret: idk_abc}\n",
            "credential: {source: paired_device, secret: idk_abc}\n",
            "credential: {source: unknown}\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_state_dir_defaults_next_to_the_database() {
        let config = parse("").unwrap();
        assert_eq!(
            config.state_dir_for(Path::new("/data/iDoris/idoris.sqlite3")),
            PathBuf::from("/data/iDoris/cloud-tier")
        );
        assert_eq!(
            config.state_dir_for(Path::new("idoris.sqlite3")),
            PathBuf::from("./cloud-tier")
        );
        let explicit = parse("state_dir: /var/tier\n").unwrap();
        assert_eq!(
            explicit.state_dir_for(Path::new("/data/x.sqlite3")),
            PathBuf::from("/var/tier")
        );
    }

    #[test]
    fn only_chat_roles_the_cloud_can_serve_are_accepted_and_decide_never_leaves_the_machine() {
        let config = parse("role_models: {fast: a, daily: b, deep: c}\n").unwrap();
        assert_eq!(config.cloud_model_for(Some("fast")), Some("a"));
        assert_eq!(config.cloud_model_for(Some("daily")), Some("b"));
        assert_eq!(config.cloud_model_for(Some("deep")), Some("c"));
        assert_eq!(config.cloud_model_for(Some("vision")), None);
        for role in ["decide", "embed", "rerank", "vision", "auto", "Daily", ""] {
            let error = parse(&format!("role_models: {{\"{role}\": x}}\n")).unwrap_err();
            assert!(error.contains("不接手"), "{role}: {error}");
        }
    }

    #[test]
    fn a_cloud_model_name_is_required_somewhere_and_must_be_a_plain_name() {
        // 没有任何模型名 = 没有云端档
        let error = CloudTierConfig::parse(
            "schema_version: 1\nbase_url: https://idoris.ai/api\n",
            Path::new("."),
        )
        .unwrap_err();
        assert!(error.contains("没有云端模型名"), "{error}");
        // 只有角色映射也可以（没有角色的请求就不回落）
        let only_roles = CloudTierConfig::parse(
            "schema_version: 1\nbase_url: https://idoris.ai/api\nrole_models: {deep: d}\n",
            Path::new("."),
        )
        .unwrap();
        assert_eq!(only_roles.cloud_model_for(None), None);
        for bad in [
            "\"\"",
            "\" padded \"",
            "\"has space\"",
            "\"tab\\there\"",
            "\"é\"",
        ] {
            assert!(
                parse(&format!("model: {bad}\n")).is_err() || bad == "\"\"",
                "{bad}"
            );
        }
        assert!(
            CloudTierConfig::parse(
                "schema_version: 1\nbase_url: https://idoris.ai/api\nmodel: \"\"\n",
                Path::new(".")
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_fields_a_wrong_version_and_nonsense_limits_are_refused() {
        for extra in [
            "surprise: 1\n",
            "limits: {surprise: 1}\n",
            "limits: {connect_timeout_ms: 0}\n",
            "limits: {total_timeout_ms: 0}\n",
            "limits: {max_response_bytes: 0}\n",
            "limits: {max_concurrent: 0}\n",
            "limits: {connect_timeout_ms: 9000, total_timeout_ms: 8000}\n",
            "api_key: idk_0000\n",
        ] {
            assert!(parse(extra).is_err(), "{extra}");
        }
        assert!(
            CloudTierConfig::parse(
                "schema_version: 2\nbase_url: https://idoris.ai/api\nmodel: m\n",
                Path::new(".")
            )
            .is_err()
        );
        assert!(CloudTierConfig::parse("not: [valid", Path::new(".")).is_err());
        assert!(CloudTierConfig::parse("", Path::new(".")).is_err());
    }

    #[test]
    fn the_secret_is_never_a_config_field() {
        // 密钥只写来源：配置里写 secret / api_key / token 一律被「未知字段」拒绝，不会被悄悄收下。
        for field in ["secret", "api_key", "token", "credential_value", "bearer"] {
            let text = format!(
                "schema_version: 1\nbase_url: https://idoris.ai/api\nmodel: m\n{field}: idk_{}\n",
                "0".repeat(64)
            );
            let error = CloudTierConfig::parse(&text, Path::new(".")).unwrap_err();
            assert!(
                !error.contains(&"0".repeat(64)),
                "the secret must not be echoed: {error}"
            );
        }
    }
}

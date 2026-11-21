//! `idoris setup upstream`（最小版，接口规范 §2.7.8）：登记 loopback 上的 OpenAI 兼容上游（LM Studio 等），生成一张
//! resident 的 HTTP 组件卡，加进一个组件目录。它是第二个 [`SetupEngine`]：探测、核对 `--bind`、生成并校验卡、落盘、
//! 输出这条流程是 `setup_engine::run_setup`，这里放这种上游专属的内容。
//!
//! # 范围（v0.6.0 的承诺）
//!
//! - **loopback**：`--endpoint` 的主机取 `127.0.0.1`、`[::1]`、`localhost`（`setup_engine::normalize_endpoint`；与卡契约的
//!   回环主机白名单相同，`127.0.0.2` 这样的写法在加载卡时会被拒，所以在这里就拒），不含路径、凭据、查询与片段。
//! - **无 key**：没有密钥参数，探测不带凭据；上游要求鉴权（401 / 403）是 `auth_required`，不写卡。
//! - **只 chat**：`--bind` 只接受 chat 类角色（[`is_chat_role`]），卡的能力只有 `chat`；embedding 由 `setup omlx` 的
//!   伴生卡承载（规范 §2.3）。
//! - **探测**：写卡之前 `GET <endpoint>/v1/models`，要恰好 200 加带 `data` 数组的 JSON（每一项有字符串 `id`）；每个 `--bind`
//!   的模型要在这个列表里。
//!
//! # 生成的卡
//!
//! resident 的 `http_service` 卡：`privacy_class: local_only`、`allowed_egress: [loopback]`、`fail_closed`、免费，
//! `load_policy` 是 resident 加 pinned 加 coexist。`version_pin` 是 `openai_compat@unknown`，所以 `engine_kind` 是
//! `openai_compat`（规范 §2.3 的来源规则）。`extensions` 带 `idoris_upstream: {kind: openai_compat, flavor: openai_compat}`、
//! `idoris_role_models`、`--default` 时的 `idoris_default: true`，以及说明这些键降级方式的 `_degradation`。除这些标注与
//! `version_pin` 外，它与 Agent24 在 v0.5.0 上手写的 LM Studio 卡逐字段相同。
//!
//! # 命令行没规定、这里定下的几处
//!
//! - **`--out-dir` 是已有的组件目录**（[`OutDirUse::AddCard`]）：写 `<id>.yaml` 这一个文件，别的文件不动；目录不存在就创建。
//! - **`--id`**：卡的 `provider.id` 与文件名 `<id>.yaml`。取值 `[a-z0-9][a-z0-9_-]{0,47}`，不得是保留的 id
//!   （[`RESERVED_IDS`]）。缺省 `<种类>-<端口>`，如 `openai_compat-1234`：同一个端点每次得到同一个 id。
//! - **`--force`**：同名卡（`<id>.yaml`）已经在时才需要，替换的只有这一个文件；别的文件里已经有同一个 `provider.id`
//!   不能靠它覆盖（`id_conflict`）。
//! - **`--default`**：卡带 `extensions.idoris_default: true`。选择语义仍是预留（规范 §2.12.3），v0.6.0 的路由不读它；
//!   目录里已经有别的默认卡时拒绝（至多一张）。输出 JSON 的 `default` 说明这次写出的卡有没有带它。
//! - **输出**：`setup omlx` 的全部字段（`mode` 恒为 `"adopt"`，`auth` 恒为 `"none"`，`engine.installed` 恒为 `false`、
//!   `engine.executable` 恒为 `null`：这种上游没有可执行文件的概念），再加 `id`、`card_file`、`default`。

use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::LoadMode;
use idoris_policy::{ROLES, Role};
use serde_json::{Map, Value, json};
use std::path::PathBuf;

use crate::cli::{SetupOptions, SetupTarget, UpstreamKind, UpstreamOptions};
use crate::engine_kind::{DEFAULT_EXTENSION, UPSTREAM_EXTENSION};
use crate::setup_engine::{
    Accept, FindFailure, FoundExecutable, OutDirUse, Probed, Reply, SetupEngine, SetupError,
    get_json, normalize_endpoint, probe_client, run_setup,
};

/// 给人看的名字，错误说明里用。
const DISPLAY_NAME: &str = "OpenAI 兼容上游";
/// `version_pin` 里 `@` 之后的部分：上游的版本不知道，规范 §2.3 的写法是 `unknown`（如 `lmstudio@unknown`）。
const VERSION_PIN_VERSION: &str = "unknown";
const MAX_ID_LEN: usize = 48;
/// 不能当 `--id` 的值：`omlx`、`omlx-embed` 是 `setup omlx` 的卡的 id（`proxy.rs` 现在按 `provider.id == "omlx"` 注入
/// oMLX 的 key，自定义上游叫这个名字会拿到它）；`mock` 在加载时被跳过，`subscription` 有固定的卡边界，`idoris-cloud`
/// 是云端档合成卡的 id。
pub const RESERVED_IDS: [&str; 5] = ["omlx", "omlx-embed", "mock", "subscription", "idoris-cloud"];

/// 卡的模板：端点、`provider.id`、`version_pin`、`extensions` 由 [`UpstreamSetup`] 与通用流程填。
const CARD_TEMPLATE: &str = r#"
provider:
  id: upstream
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:0"
version_pin: "openai_compat@unknown"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
"#;

/// `_degradation` 里说明 `idoris_upstream`、`idoris_default` 的那一句；通用流程再接上 `idoris_role_models` 的说明。
const DEGRADATION: &str = "去掉 idoris_upstream 或 idoris_default 只丢掉种类标注与默认偏好，这张卡仍是普通的 loopback resident http_service 卡。";

/// chat 类角色：`--bind` 只接受它们（规范 §2.7.8）。用穷举的 `match`：以后加新角色，这里编译不过，逼着作一次决定。
pub fn is_chat_role(role: Role) -> bool {
    match role {
        Role::Fast | Role::Daily | Role::Deep | Role::Decide | Role::Auto => true,
        Role::Vision | Role::Embed | Role::Rerank => false,
    }
}

/// `--id` 的格式与保留名。说明里回显取值（是调用方自己给的，不是秘密），按 Debug 转义。
fn check_id(id: &str) -> Result<(), SetupError> {
    let bad = |why: &str| SetupError::new("invalid_id", format!("--id {id:?} 不合规：{why}"));
    let mut chars = id.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if !first_ok || !rest_ok || id.len() > MAX_ID_LEN {
        return Err(bad(&format!(
            "只能由小写字母、数字、下划线与连字符组成，以字母或数字开头，长度 1 到 {MAX_ID_LEN}"
        )));
    }
    if RESERVED_IDS.contains(&id) {
        return Err(bad(&format!(
            "这是保留的 id（{}）",
            RESERVED_IDS.join("、")
        )));
    }
    Ok(())
}

/// 没给 `--id` 时的缺省：`<种类>-<端点的端口>`。`endpoint` 是规范化之后的写法。
fn default_id(kind: UpstreamKind, endpoint: &str) -> Result<String, SetupError> {
    let port = reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.port_or_known_default())
        .ok_or_else(|| SetupError::new("invalid_endpoint", "endpoint 无效：取不到端口"))?;
    Ok(format!("{}-{port}", kind.as_str()))
}

/// 这种上游在 `setup` 里的专属知识。
struct UpstreamSetup {
    kind: UpstreamKind,
    /// 卡的 `provider.id`。
    id: String,
    /// `<id>.yaml`。
    file: String,
    default: bool,
}

impl UpstreamSetup {
    fn new(options: &UpstreamOptions, endpoint: &str) -> Result<Self, SetupError> {
        let id = match &options.id {
            Some(id) => id.clone(),
            None => default_id(options.kind, endpoint)?,
        };
        check_id(&id)?;
        Ok(Self {
            kind: options.kind,
            file: format!("{id}.yaml"),
            id,
            default: options.default,
        })
    }
}

impl SetupEngine for UpstreamSetup {
    fn kind(&self) -> &str {
        self.kind.as_str()
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn install_hint(&self) -> &'static str {
        "这种上游没有要安装的引擎：先在本机启动上游服务，再重新运行"
    }

    /// 这个命令不带密钥：探测遇到要鉴权的上游直接报错，输出里没有 `needs_env`，所以没有环境变量名。
    fn key_env(&self) -> &'static str {
        ""
    }

    /// 没有可执行文件的概念：`engine.installed` 是 `false`，`engine.executable` 是 `null`。
    async fn locate_executable(&self) -> Option<PathBuf> {
        None
    }

    /// 不托管（目标恒为采纳一个端点），不会走到这里。
    async fn discover_executable(&self) -> Result<FoundExecutable, FindFailure> {
        Err(FindFailure::Unsupported)
    }

    fn auto_ports(&self) -> &[u16] {
        &[]
    }

    fn installed_models(&self) -> Option<Vec<String>> {
        None
    }

    fn managed_models_dir(&self) -> Result<PathBuf, SetupError> {
        Err(SetupError::new(
            "validation_failed",
            "setup upstream 不托管引擎，没有模型目录",
        ))
    }

    /// `GET <endpoint>/v1/models`，不带凭据，要求恰好 200；`lifecycle` 对 OpenAI 兼容上游没有意义（没有生命周期状态接口）。
    async fn probe(
        &self,
        endpoint: &str,
        key: Option<&str>,
        _lifecycle: bool,
    ) -> Result<Probed, SetupError> {
        if key.is_some() {
            return Err(SetupError::new(
                "invalid_api_key_input",
                "setup upstream 不接受 API key",
            ));
        }
        let not_openai = || {
            SetupError::new(
                "not_openai_compatible",
                "endpoint 的 GET /v1/models 响应不像 OpenAI 兼容服务（要 200 加带 data 数组的 JSON）",
            )
        };
        let client = probe_client()?;
        let listing = match get_json(
            &client,
            &format!("{endpoint}/v1/models"),
            None,
            DISPLAY_NAME,
            Accept::Ok200,
            not_openai,
        )
        .await?
        {
            Reply::Ok(listing) => listing,
            Reply::Denied => {
                return Err(SetupError::new(
                    "auth_required",
                    "上游要求 API key：setup upstream 的最小版（v0.6.0）不带密钥，要密钥的上游等 vault 接线之后另行支持",
                ));
            }
        };
        let models = listing
            .get("data")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(not_openai)?;
        Ok(Probed {
            models,
            auth_required: false,
        })
    }

    fn card_file(&self) -> &str {
        &self.file
    }

    fn card_template(&self) -> Result<ComponentCard, SetupError> {
        let mut card: ComponentCard = serde_yaml::from_str(CARD_TEMPLATE)
            .map_err(|e| SetupError::new("validation_failed", format!("生成组件卡失败：{e}")))?;
        let kind = self.kind.as_str();
        card.provider.id = self.id.clone();
        card.version_pin = format!("{kind}@{VERSION_PIN_VERSION}");
        let mut extensions = Map::new();
        extensions.insert("_degradation".to_string(), json!(DEGRADATION));
        extensions.insert(
            UPSTREAM_EXTENSION.to_string(),
            json!({"kind": kind, "flavor": kind}),
        );
        if self.default {
            extensions.insert(DEFAULT_EXTENSION.to_string(), json!(true));
        }
        card.extensions = Some(extensions);
        Ok(card)
    }

    fn out_dir_use(&self) -> OutDirUse {
        OutDirUse::AddCard
    }

    /// 至少一个绑定，而且都是 chat 类角色。通用流程已经确认过角色名是 `idoris/<role>` 里的合法角色。
    fn check_requested_bindings(&self, bindings: &[(String, String)]) -> Result<(), SetupError> {
        let chat_roles = || {
            ROLES
                .iter()
                .copied()
                .filter(|role| is_chat_role(*role))
                .map(|role| role.as_str())
                .collect::<Vec<_>>()
                .join("、")
        };
        if bindings.is_empty() {
            return Err(SetupError::new(
                "invalid_role",
                format!("至少要用 --bind 绑定一个 chat 角色（{}）", chat_roles()),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for (role, _) in bindings {
            // 命令行在解析期就拒绝同一个角色绑两次；库调用方不经过解析，这里再核对一次（否则后一个悄悄盖掉前一个）。
            if !seen.insert(role.as_str()) {
                return Err(SetupError::new(
                    "invalid_role",
                    format!("角色 {role} 绑定了不止一次：每个角色只能绑一个模型"),
                ));
            }
            let chat = ROLES
                .iter()
                .copied()
                .find(|candidate| candidate.as_str() == role)
                .is_some_and(is_chat_role);
            if !chat {
                return Err(SetupError::new(
                    "invalid_role",
                    format!(
                        "角色 {role} 不是 chat 类角色：setup upstream 只接受 {}（embedding 由 setup omlx 的伴生卡承载）",
                        chat_roles()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// 入口。没有 key 参数：这个命令不带密钥。
pub async fn run(options: &UpstreamOptions) -> Result<Value, SetupError> {
    let endpoint = normalize_endpoint(&options.endpoint)?;
    let engine = UpstreamSetup::new(options, &endpoint)?;
    let setup_options = SetupOptions {
        target: SetupTarget::Endpoint {
            url: endpoint,
            probe: true,
        },
        out_dir: options.out_dir.clone(),
        bindings: options.bindings.clone(),
        load_mode: LoadMode::Resident,
        api_key_stdin: false,
        force: options.force,
    };
    let mut report = run_setup(&engine, &setup_options, None).await?;
    if let Some(fields) = report.as_object_mut() {
        fields.insert("id".to_string(), json!(engine.id));
        fields.insert("card_file".to_string(), json!(engine.file));
        fields.insert("default".to_string(), json!(engine.default));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use idoris_upstream::engine::EngineKind;

    fn setup(id: Option<&str>, default: bool) -> UpstreamSetup {
        let options = UpstreamOptions {
            kind: UpstreamKind::OpenAiCompat,
            endpoint: "http://127.0.0.1:1234".into(),
            out_dir: "o".into(),
            bindings: vec![("daily".into(), "m".into())],
            id: id.map(str::to_string),
            default,
            force: false,
        };
        UpstreamSetup::new(&options, "http://127.0.0.1:1234").unwrap()
    }

    /// `--kind` 的写法就是规范保留的引擎种类名（§2.12.1）。
    #[test]
    fn the_kind_is_the_reserved_openai_compat_name() {
        assert_eq!(
            UpstreamKind::OpenAiCompat.as_str(),
            EngineKind::OPENAI_COMPAT.as_str()
        );
        assert!(
            EngineKind::parse(UpstreamKind::OpenAiCompat.as_str())
                .unwrap()
                .is_reserved()
        );
    }

    #[test]
    fn ids_follow_the_documented_format_and_the_reserved_names_are_refused() {
        for good in [
            "lmstudio",
            "a",
            "0",
            "openai_compat-1234",
            "my-llama_cpp-2",
            &"a".repeat(MAX_ID_LEN),
        ] {
            check_id(good).unwrap_or_else(|e| panic!("{good}: {}", e.message));
        }
        for bad in [
            "",
            "LMStudio",
            "-lm",
            "_lm",
            ".lm",
            "lm studio",
            "lm.studio",
            "lm/studio",
            "../x",
            "lm\u{4e2d}",
            "lm\n",
            &"a".repeat(MAX_ID_LEN + 1),
        ] {
            assert_eq!(check_id(bad).unwrap_err().code, "invalid_id", "{bad:?}");
        }
        for reserved in RESERVED_IDS {
            let error = check_id(reserved).unwrap_err();
            assert_eq!(error.code, "invalid_id", "{reserved}");
            assert!(error.message.contains("保留"), "{}", error.message);
        }
    }

    #[test]
    fn the_default_id_is_the_kind_and_the_port_and_stays_the_same_for_the_same_endpoint() {
        let kind = UpstreamKind::OpenAiCompat;
        assert_eq!(
            default_id(kind, "http://127.0.0.1:1234").unwrap(),
            "openai_compat-1234"
        );
        assert_eq!(
            default_id(kind, "http://localhost:1234").unwrap(),
            "openai_compat-1234"
        );
        assert_eq!(
            default_id(kind, "http://[::1]:8080").unwrap(),
            "openai_compat-8080"
        );
        // 没写端口：取协议的缺省端口。
        assert_eq!(
            default_id(kind, "http://127.0.0.1").unwrap(),
            "openai_compat-80"
        );
        assert_eq!(
            default_id(kind, "https://localhost").unwrap(),
            "openai_compat-443"
        );
        // 缺省值本身合规。
        check_id(&default_id(kind, "http://127.0.0.1:65535").unwrap()).unwrap();
    }

    /// 穷举：chat 类角色恰好是 fast、daily、deep、decide、auto；vision、embed、rerank 不是。
    #[test]
    fn only_the_text_generation_roles_are_chat_roles() {
        let chat: Vec<&str> = ROLES
            .iter()
            .copied()
            .filter(|role| is_chat_role(*role))
            .map(Role::as_str)
            .collect();
        assert_eq!(chat, ["fast", "daily", "deep", "decide", "auto"]);
    }

    #[test]
    fn requested_bindings_must_be_present_and_chat_only() {
        let engine = setup(None, false);
        let bind = |role: &str| vec![(role.to_string(), "m".to_string())];
        assert!(engine.check_requested_bindings(&bind("daily")).is_ok());
        for role in ["embed", "rerank", "vision", "nope"] {
            let error = engine.check_requested_bindings(&bind(role)).unwrap_err();
            assert_eq!(error.code, "invalid_role", "{role}");
            assert!(error.message.contains("chat"), "{}", error.message);
        }
        // 一个合格、一个不合格：整体拒绝。
        let mixed = vec![
            ("daily".to_string(), "m".to_string()),
            ("embed".to_string(), "m".to_string()),
        ];
        assert_eq!(
            engine.check_requested_bindings(&mixed).unwrap_err().code,
            "invalid_role"
        );
        assert_eq!(
            engine.check_requested_bindings(&[]).unwrap_err().code,
            "invalid_role"
        );
        // 同一个角色绑两次（库调用方不经过命令行的解析）：拒绝，不是后一个盖掉前一个。
        let twice = vec![
            ("daily".to_string(), "a".to_string()),
            ("daily".to_string(), "b".to_string()),
        ];
        let error = engine.check_requested_bindings(&twice).unwrap_err();
        assert_eq!(error.code, "invalid_role");
        assert!(error.message.contains("不止一次"), "{}", error.message);
    }

    /// 卡的标注：`idoris_upstream` 的 `kind`、`flavor` 与 `version_pin` 的产品名是同一个种类名（规范 §2.3）。
    #[test]
    fn the_card_annotates_the_kind_and_flavor_and_the_version_pin_product_agree() {
        let card = setup(Some("lm"), false).card_template().unwrap();
        let extensions = card.extensions.as_ref().unwrap();
        assert_eq!(
            extensions[UPSTREAM_EXTENSION],
            json!({"kind": "openai_compat", "flavor": "openai_compat"})
        );
        assert_eq!(card.version_pin, "openai_compat@unknown");
        let identity = crate::engine_kind::engine_identity(&card).unwrap();
        assert_eq!(identity.engine_kind, "openai_compat");
        assert!(!identity.managed);
        assert!(!extensions.contains_key(DEFAULT_EXTENSION));
        // `--default` 才有 `idoris_default`。
        let default_card = setup(Some("lm"), true).card_template().unwrap();
        assert_eq!(
            default_card.extensions.as_ref().unwrap()[DEFAULT_EXTENSION],
            json!(true)
        );
    }

    /// 不带密钥：引擎边界上就拒绝 key，不是靠调用方不给。
    #[tokio::test(flavor = "multi_thread")]
    async fn probe_refuses_a_key_without_connecting() {
        let engine = setup(None, false);
        // 端口 9 上没有人听：如果探测去连了，错误码会是 `unreachable`。
        let error = engine
            .probe("http://127.0.0.1:9", Some("secret"), false)
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_api_key_input");
        assert!(!error.message.contains("secret"));
    }
}

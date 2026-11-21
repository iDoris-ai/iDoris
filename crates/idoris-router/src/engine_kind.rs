//! 引擎种类 `engine_kind`（接口规范 §2.3、§2.12.1，wire 契约 1.3.0 的内容；字段从 S1 起出现在 `/health`，
//! `contract_version` 到 v0.6.0 发版才升 1.3.0）。
//!
//! 开放集合的字符串，`^[a-z][a-z0-9_]{0,31}$`。每张引擎型卡恰好有一个来源：
//!
//! 1. 卡带 `extensions.idoris_managed` → 取它的 `engine`（该引擎由 iDoris 托管，`managed = true`）；
//! 2. 否则取 `version_pin` 里 `@` 之前的产品名（`omlx@0.6.4` 得 `omlx`，`lmstudio@unknown` 得 `lmstudio`）；
//!    产品名不符合上面的格式、或没有 `@`，得 `unknown`；
//! 3. 两者同时存在而不一致：卡校验失败（启动失败，不静默忽略）。
//!
//! 本模块只回答「这张卡是哪种引擎、有没有声明托管」，S1 不拉起任何进程（引擎管理是 S3/S5）。
//! `managed` 因此只是**声明事实**：`/health.upstreams[].owned` 在 S1 恒为 false。

use idoris_contracts::ComponentCard;

/// 无法识别种类时的取值。
pub const UNKNOWN_ENGINE_KIND: &str = "unknown";
/// 卡的 `extensions` 里声明「该引擎由 iDoris 托管」的键（规范 §2.3）。
pub const MANAGED_EXTENSION: &str = "idoris_managed";
/// 卡的 `extensions` 里标注「自定义 OpenAI 兼容上游」的键（规范 §2.3）：`{kind: openai_compat, flavor: <引擎种类>}`。
/// 上游的引擎种类仍来自 `version_pin`（本模块的来源规则）；serve 只核对 `flavor` 与它一致（[`check_upstream_declaration`]，#626）。
pub const UPSTREAM_EXTENSION: &str = "idoris_upstream";
/// 卡的 `extensions` 里「默认上游」的静态指定（规范 §2.12.3）：值为 `true` 的卡是默认。选择语义仍是预留，
/// v0.6.0 的选择不读它；`setup upstream --default` 写它，并在落盘前核对：新卡要当默认时，目录里没有别的默认卡。
pub const DEFAULT_EXTENSION: &str = "idoris_default";
/// 本版本认识、能够托管的引擎种类。别的取值写进 `idoris_managed.engine` 就是卡校验失败。
pub const SUPPORTED_MANAGED_ENGINES: &[&str] = &["omlx"];

/// 卡带着 `extensions.idoris_default: true`（布尔真；字符串 `"true"`、数字 1 都不算）。
pub fn declares_default(card: &ComponentCard) -> bool {
    card.extensions
        .as_ref()
        .and_then(|extensions| extensions.get(DEFAULT_EXTENSION))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// `^[a-z][a-z0-9_]{0,31}$`，不引入正则依赖。
pub fn is_valid_engine_kind(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && value.len() <= 32
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// `extensions.idoris_upstream` 声明不合规。`code()` 是稳定的原因码，写进卡校验失败的说明里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamDeclarationError {
    /// `idoris_upstream` 不是对象，或没有字符串 `flavor`，或 `flavor` 不是合法的引擎种类格式。
    FlavorInvalid,
    /// `flavor` 与 `version_pin` 的产品名不一致（`version_pin` 没有合法产品名也算不一致）。
    FlavorMismatch {
        flavor: String,
        version_pin_product: Option<String>,
    },
}

impl UpstreamDeclarationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::FlavorInvalid => "upstream_flavor_invalid",
            Self::FlavorMismatch { .. } => "upstream_flavor_mismatch",
        }
    }
}

impl std::fmt::Display for UpstreamDeclarationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FlavorInvalid => write!(
                f,
                "extensions.{UPSTREAM_EXTENSION} 必须是对象，且带合法格式（小写字母开头，只含小写字母、数字、下划线，至多 32 个字符）的字符串 flavor"
            ),
            Self::FlavorMismatch {
                flavor,
                version_pin_product,
            } => match version_pin_product {
                Some(product) => write!(
                    f,
                    "extensions.{UPSTREAM_EXTENSION}.flavor \"{flavor}\" 与 version_pin 的产品名 \"{product}\" 不一致"
                ),
                None => write!(
                    f,
                    "extensions.{UPSTREAM_EXTENSION}.flavor \"{flavor}\" 在 version_pin 里找不到对应的产品名（要写成 <产品名>@<版本>）"
                ),
            },
        }
        .and_then(|()| write!(f, "（原因码 {}）", self.code()))
    }
}

/// 卡带 `extensions.idoris_upstream` 时，`flavor` 要存在、格式合法，并与 `version_pin` 的产品名一致（规范 §2.3）。
/// 不带这个键的卡不受影响，直接通过。
pub fn check_upstream_declaration(card: &ComponentCard) -> Result<(), UpstreamDeclarationError> {
    let Some(declaration) = card
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get(UPSTREAM_EXTENSION))
    else {
        return Ok(());
    };
    let flavor = declaration
        .as_object()
        .and_then(|object| object.get("flavor"))
        .and_then(serde_json::Value::as_str)
        .filter(|flavor| is_valid_engine_kind(flavor))
        .ok_or(UpstreamDeclarationError::FlavorInvalid)?;
    let product = version_pin_product(&card.version_pin);
    if product == Some(flavor) {
        return Ok(());
    }
    Err(UpstreamDeclarationError::FlavorMismatch {
        flavor: flavor.to_string(),
        version_pin_product: product.map(str::to_string),
    })
}

/// 一张引擎型卡的种类与托管声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineIdentity {
    pub engine_kind: String,
    /// 卡声明了 `extensions.idoris_managed`。
    pub managed: bool,
}

/// 卡校验失败：iDoris 拒绝启动。文案只含卡 id 与引擎种类名，不含任何秘密。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineKindError {
    /// `idoris_managed` 不是带字符串 `engine` 的对象。
    ManagedDeclarationInvalid { provider_id: String },
    /// `idoris_managed.engine` 不是合法格式，或本版本不认识。
    ManagedEngineUnsupported { provider_id: String, engine: String },
    /// `idoris_managed.engine` 与 `version_pin` 的产品名同时存在而不一致。
    KindMismatch {
        provider_id: String,
        managed_engine: String,
        version_pin_product: String,
    },
    /// `idoris_upstream` 声明不合规（flavor 缺失、非法或与 `version_pin` 不一致）。
    UpstreamDeclaration {
        provider_id: String,
        error: UpstreamDeclarationError,
    },
}

impl std::fmt::Display for EngineKindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManagedDeclarationInvalid { provider_id } => write!(
                f,
                "组件卡 \"{provider_id}\" 的 extensions.{MANAGED_EXTENSION} 必须是带字符串 engine 的对象"
            ),
            Self::ManagedEngineUnsupported {
                provider_id,
                engine,
            } => write!(
                f,
                "组件卡 \"{provider_id}\" 的 extensions.{MANAGED_EXTENSION}.engine \"{engine}\" 不是本版本认识的引擎种类（本版本只认 {}）",
                SUPPORTED_MANAGED_ENGINES.join("、")
            ),
            Self::KindMismatch {
                provider_id,
                managed_engine,
                version_pin_product,
            } => write!(
                f,
                "组件卡 \"{provider_id}\" 的 extensions.{MANAGED_EXTENSION}.engine \"{managed_engine}\" 与 version_pin 的产品名 \"{version_pin_product}\" 不一致"
            ),
            Self::UpstreamDeclaration { provider_id, error } => {
                write!(f, "组件卡 \"{provider_id}\" 的 {error}")
            }
        }
    }
}

impl std::error::Error for EngineKindError {}

/// `version_pin` 里 `@` 之前的产品名；没有 `@` 或格式不符返回 `None`。
fn version_pin_product(version_pin: &str) -> Option<&str> {
    let (product, _) = version_pin.split_once('@')?;
    is_valid_engine_kind(product).then_some(product)
}

/// 按上面的来源规则解析一张卡。
pub fn engine_identity(card: &ComponentCard) -> Result<EngineIdentity, EngineKindError> {
    let provider_id = || card.provider.id.clone();
    check_upstream_declaration(card).map_err(|error| EngineKindError::UpstreamDeclaration {
        provider_id: provider_id(),
        error,
    })?;
    let product = version_pin_product(&card.version_pin);
    let Some(declaration) = card
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get(MANAGED_EXTENSION))
    else {
        return Ok(EngineIdentity {
            engine_kind: product.unwrap_or(UNKNOWN_ENGINE_KIND).to_string(),
            managed: false,
        });
    };
    let engine = declaration
        .as_object()
        .and_then(|object| object.get("engine"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| EngineKindError::ManagedDeclarationInvalid {
            provider_id: provider_id(),
        })?;
    if !is_valid_engine_kind(engine) || !SUPPORTED_MANAGED_ENGINES.contains(&engine) {
        return Err(EngineKindError::ManagedEngineUnsupported {
            provider_id: provider_id(),
            engine: engine.to_string(),
        });
    }
    if let Some(product) = product
        && product != engine
    {
        return Err(EngineKindError::KindMismatch {
            provider_id: provider_id(),
            managed_engine: engine.to_string(),
            version_pin_product: product.to_string(),
        });
    }
    Ok(EngineIdentity {
        engine_kind: engine.to_string(),
        managed: true,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    fn card(version_pin: &str, extensions: Option<serde_json::Value>) -> ComponentCard {
        let mut card: ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        card.version_pin = version_pin.to_string();
        card.extensions = extensions.map(|value| value.as_object().unwrap().clone());
        card
    }

    fn managed(engine: serde_json::Value) -> Option<serde_json::Value> {
        Some(json!({
            "_degradation": "未托管时退回外部引擎：iDoris 只探测，不拉起",
            "idoris_managed": {"engine": engine, "mode": "auto"}
        }))
    }

    #[test]
    fn kind_format_is_lowercase_letter_first_then_lowercase_digits_underscore_up_to_32() {
        for valid in ["omlx", "llama_cpp", "mlx_lm", "a", "a1_b2", &"a".repeat(32)] {
            assert!(is_valid_engine_kind(valid), "{valid}");
        }
        for invalid in [
            "",
            "1omlx",
            "_omlx",
            "OMLX",
            "oMLX",
            "llama-cpp",
            "llama.cpp",
            "omlx ",
            "omlx\n",
            "引擎",
            &"a".repeat(33),
        ] {
            assert!(!is_valid_engine_kind(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn without_a_managed_declaration_the_kind_is_the_version_pin_product() {
        for (pin, expected) in [
            ("omlx@0.6.4", "omlx"),
            ("lmstudio@unknown", "lmstudio"),
            ("llama_cpp@b4000", "llama_cpp"),
            ("fixture@1", "fixture"),
            ("omlx@", "omlx"),
        ] {
            let identity = engine_identity(&card(pin, None)).unwrap();
            assert_eq!(
                identity,
                EngineIdentity {
                    engine_kind: expected.to_string(),
                    managed: false
                },
                "{pin}"
            );
        }
    }

    #[test]
    fn unparsable_version_pin_product_is_unknown_not_an_error() {
        for pin in ["omlx", "OMLX@1", "1omlx@1", "@1", "llama-cpp@1", "引擎@1"] {
            let identity = engine_identity(&card(pin, None)).unwrap();
            assert_eq!(identity.engine_kind, UNKNOWN_ENGINE_KIND, "{pin}");
            assert!(!identity.managed, "{pin}");
        }
    }

    #[test]
    fn a_managed_declaration_wins_and_marks_the_card_managed() {
        let identity = engine_identity(&card("omlx@0.6.4", managed(json!("omlx")))).unwrap();
        assert_eq!(
            identity,
            EngineIdentity {
                engine_kind: "omlx".to_string(),
                managed: true
            }
        );
        // version_pin 的产品名不可解析时没有「两个来源」，不构成冲突。
        let identity = engine_identity(&card("OMLX@1", managed(json!("omlx")))).unwrap();
        assert_eq!(identity.engine_kind, "omlx");
        assert!(identity.managed);
    }

    #[test]
    fn a_managed_engine_that_disagrees_with_the_version_pin_fails_card_validation() {
        let error = engine_identity(&card("lmstudio@1", managed(json!("omlx")))).unwrap_err();
        assert_eq!(
            error,
            EngineKindError::KindMismatch {
                provider_id: "omlx".to_string(),
                managed_engine: "omlx".to_string(),
                version_pin_product: "lmstudio".to_string()
            }
        );
        assert!(error.to_string().contains("不一致"));
    }

    #[test]
    fn a_managed_engine_this_version_does_not_know_fails_card_validation() {
        for engine in [
            json!("lmstudio"),
            json!("Omlx"),
            json!("not an engine"),
            json!(""),
        ] {
            let error = engine_identity(&card("omlx@0.6.4", managed(engine))).unwrap_err();
            assert!(
                matches!(error, EngineKindError::ManagedEngineUnsupported { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_malformed_managed_declaration_fails_card_validation() {
        for declaration in [json!("omlx"), json!(true), json!({}), json!({"engine": 7})] {
            let extensions = Some(json!({
                "_degradation": "降级说明",
                "idoris_managed": declaration
            }));
            let error = engine_identity(&card("omlx@0.6.4", extensions)).unwrap_err();
            assert!(
                matches!(error, EngineKindError::ManagedDeclarationInvalid { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn unrelated_extensions_do_not_make_a_card_managed() {
        let extensions = Some(json!({
            "_degradation": "降级说明",
            "idoris_role_models": {"daily": "m"}
        }));
        let identity = engine_identity(&card("omlx@0.6.4", extensions)).unwrap();
        assert!(!identity.managed);
        assert_eq!(identity.engine_kind, "omlx");
    }

    fn upstream(pin: &str, declaration: serde_json::Value) -> ComponentCard {
        card(
            pin,
            Some(json!({"_degradation": "d", "idoris_upstream": declaration})),
        )
    }

    #[test]
    fn an_upstream_flavor_must_match_the_version_pin_product() {
        for (pin, flavor) in [
            ("lmstudio@0", "lmstudio"),
            ("openai_compat@unknown", "openai_compat"),
            ("omlx@0.6.4", "omlx"),
        ] {
            let c = upstream(pin, json!({"kind": "openai_compat", "flavor": flavor}));
            check_upstream_declaration(&c).unwrap();
            assert_eq!(engine_identity(&c).unwrap().engine_kind, flavor);
        }
        let mismatch = upstream(
            "lmstudio@0",
            json!({"kind": "openai_compat", "flavor": "vllm"}),
        );
        let error = check_upstream_declaration(&mismatch).unwrap_err();
        assert_eq!(error.code(), "upstream_flavor_mismatch");
        assert!(error.to_string().contains("upstream_flavor_mismatch"));
        assert!(matches!(
            engine_identity(&mismatch).unwrap_err(),
            EngineKindError::UpstreamDeclaration { .. }
        ));
        // version_pin 没有合法产品名：也是不一致，不是放行。
        for pin in ["lmstudio", "@1", "LM@1"] {
            let c = upstream(pin, json!({"kind": "openai_compat", "flavor": "lmstudio"}));
            assert_eq!(
                check_upstream_declaration(&c).unwrap_err().code(),
                "upstream_flavor_mismatch",
                "{pin}"
            );
        }
    }

    #[test]
    fn a_missing_or_malformed_flavor_is_invalid() {
        for declaration in [
            json!({"kind": "openai_compat"}),
            json!({"flavor": 7}),
            json!({"flavor": null}),
            json!({"flavor": "LM-Studio"}),
            json!({"flavor": ""}),
            json!("lmstudio"),
            json!(true),
        ] {
            let c = upstream("lmstudio@0", declaration.clone());
            assert_eq!(
                check_upstream_declaration(&c).unwrap_err().code(),
                "upstream_flavor_invalid",
                "{declaration}"
            );
        }
    }

    #[test]
    fn a_card_without_the_upstream_declaration_is_never_checked() {
        for pin in ["lmstudio@0", "nonsense", "", "omlx@0.6.4"] {
            check_upstream_declaration(&card(pin, None)).unwrap();
        }
    }
}

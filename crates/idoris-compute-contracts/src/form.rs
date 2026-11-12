//! 三种部署形态：个人 / 组织 / 云（设计稿 §9），以及它们与既有 `deploy_mode` 的共存。
//!
//! # 共存方式：加法，不改既有契约
//!
//! 既有契约 `deploy_mode: personal | tenant`（[`idoris_contracts::DeployMode`]，
//! `packages/contracts/schema/deploy-mode.schema.json`）**一个字符都不改**：它还是只有两个值，
//! 反序列化结果、序列化结果、schema 文件都不变（见本模块的测试，其中一条直接读 schema 文件核对）。
//!
//! 新形态用一个**独立的、可选的**键 `form` 表达：
//!
//! | `deploy_mode` | `form`（缺省 = 不写） | 解析结果 |
//! |---|---|---|
//! | `personal` | 缺省 / `personal` | 个人 |
//! | `personal` | `organization` / `cloud` | **错误**（个人模式不可能是组织或云） |
//! | `tenant` | `organization` / `cloud` | 照所写 |
//! | `tenant` | **缺省** | **组织**（兼容：存量的租户部署没有这个键，按最保守的租户形态处理，不会因此打开云形态独有的能力） |
//! | `tenant` | `personal` | **错误** |
//!
//! 所以 `Organization` 和 `Cloud` 都映射到既有的 `DeployMode::Tenant`（[`DeploymentForm::deploy_mode`]），
//! 既有代码里所有 `match deploy_mode { Personal => .., Tenant => .. }` 对新形态依然成立；
//! 存量配置（只有 `deploy_mode`）反序列化出来的 `form` 是 `None`，结果与改动前完全一致。
//!
//! 新键的取值**严格**：未知字符串是错误，不会像 `IDORIS_DEPLOY_MODE` 那样把一切未知值收成 `tenant`
//! （[`DeploymentForm::from_env_value`]）；手滑把 `cloud` 写成 `clound` 不应该悄悄变成组织形态。
//!
//! 本模块**不接线**：不读环境变量、不改启动流程，只给出类型、解析函数和「哪种形态哪些能力开或关」的判定。
//! `FORM_ENV_VAR` 只是建议的名字，由 S1 决定是否采用。

use idoris_contracts::DeployMode;
use serde::{Deserialize, Serialize};

/// 建议用来承载 `form` 的环境变量名（与 `IDORIS_DEPLOY_MODE` 并列）。
pub const FORM_ENV_VAR: &str = "IDORIS_FORM";

/// 部署形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentForm {
    /// 个人：用户的 Mac，由 Agent24 托管，只听 loopback。
    Personal,
    /// 组织：组织的 Mac Studio，局域网 + TLS + 成员凭证。
    Organization,
    /// 云：idoris.ai/api，我们运营，公网 + TLS + 开通流程。
    Cloud,
}

/// 入口监听范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenScope {
    Loopback,
    Lan,
    Public,
}

/// 计量方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingModel {
    /// 不计费（个人形态）。
    None,
    /// 配额 `QUOTA:<节点>`（组织形态）。
    Quota,
    /// 货币 `THB` / `USD`，外加补贴额度（云形态）。
    Currency,
}

/// 云端「认谁」（设计稿 §9 表中「云端认谁」一行，对**调用云端档的一方**而言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudIdentity {
    /// 个人云账号。
    PersonalCloudAccount,
    /// 只认组织付款账户；组织按节点或用途分别发受限凭证。
    OrgPaymentAccountOnly,
    /// 本身就是云，不适用。
    NotApplicable,
}

/// 账本权威。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerAuthority {
    /// 本机。
    Local,
    /// 组织内部的归因。
    OrgInternal,
    /// 云端成本以云账本为准。
    CloudLedger,
}

/// 租户从哪里来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantSource {
    /// 个人形态：固定的个人租户。
    FixedPersonal,
    /// 请求头 `X-iDoris-Tenant`（既有的本地租户模式契约，不变）。
    TenantHeader,
    /// 从凭证推导（云入口，设计稿 §9 数据面的租户）。
    FromCredential,
}

/// 一种形态下各项能力的开关，对应设计稿 §9 的表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormProfile {
    pub listen: ListenScope,
    pub billing: BillingModel,
    /// 有本机模型（个人、组织）。
    pub local_models: bool,
    /// 会调用云端档（个人、组织：节点主动调用云，云从不反向访问）。
    pub uses_cloud_tier: bool,
    /// 上游是端点许可表里的第三方端点（云）。
    pub uses_endpoint_permit_table: bool,
    pub cloud_identity: CloudIdentity,
    pub ledger_authority: LedgerAuthority,
    pub tenant_source: TenantSource,
    /// 允许 subscription 路径（它走 CLI）。**只有个人形态**；云端必须排除（设计稿 §8），
    /// 既有的 `registration.ts` 也只在 `deploy_mode=personal` 下注册它，所以组织（= 既有 tenant）同样关。
    pub allows_subscription_path: bool,
    /// 启动时是否用配置文件里的租户列表删掉库里多余的租户（`retain_tenants`，设计稿 §9 第 2 项）。
    /// 个人保持现状（是）；组织和云以数据库为准（否）。
    pub prunes_unlisted_tenants_on_startup: bool,
    /// 是否需要写操作的控制面（设计稿 §9 第 1 项，独立于只读的 Admin v0）。
    pub control_plane: bool,
    /// 公网防滥用（设计稿 §9 第 3 项）：只在云形态加载。
    pub public_abuse_protection: bool,
    /// 平台补贴池与免费额度（只在云形态）。
    pub subsidy_pool: bool,
}

/// 形态与 `deploy_mode` 的冲突。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormError {
    #[error("form {form:?} conflicts with deploy_mode {deploy_mode:?}")]
    ConflictsWithDeployMode {
        deploy_mode: DeployMode,
        form: DeploymentForm,
    },
    #[error("unknown form {0:?}: expected personal, organization or cloud")]
    UnknownForm(String),
}

impl DeploymentForm {
    pub const ALL: [DeploymentForm; 3] = [
        DeploymentForm::Personal,
        DeploymentForm::Organization,
        DeploymentForm::Cloud,
    ];

    /// 线上字符串（与 `serde` 形式一致），固定不变。
    pub fn as_str(self) -> &'static str {
        match self {
            DeploymentForm::Personal => "personal",
            DeploymentForm::Organization => "organization",
            DeploymentForm::Cloud => "cloud",
        }
    }

    /// 对应的既有 `deploy_mode`：个人 → `Personal`；组织、云 → `Tenant`。
    pub fn deploy_mode(self) -> DeployMode {
        match self {
            DeploymentForm::Personal => DeployMode::Personal,
            DeploymentForm::Organization | DeploymentForm::Cloud => DeployMode::Tenant,
        }
    }

    /// 由既有的 `deploy_mode` 和可选的 `form` 得到形态。规则见模块文档的表。
    pub fn resolve(
        deploy_mode: DeployMode,
        declared: Option<DeploymentForm>,
    ) -> Result<DeploymentForm, FormError> {
        match (deploy_mode, declared) {
            (DeployMode::Personal, None) => Ok(DeploymentForm::Personal),
            (DeployMode::Tenant, None) => Ok(DeploymentForm::Organization),
            (mode, Some(form)) if form.deploy_mode() == mode => Ok(form),
            (mode, Some(form)) => Err(FormError::ConflictsWithDeployMode {
                deploy_mode: mode,
                form,
            }),
        }
    }

    /// 解析 `IDORIS_FORM` 这类环境变量的值：首尾空白忽略、大小写不敏感；空 / 未设 = `None`；
    /// **未知值是错误**，不回退（见模块文档）。
    pub fn from_env_value(raw: Option<&str>) -> Result<Option<DeploymentForm>, FormError> {
        let normalized = raw.unwrap_or("").trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" => Ok(None),
            "personal" => Ok(Some(DeploymentForm::Personal)),
            "organization" => Ok(Some(DeploymentForm::Organization)),
            "cloud" => Ok(Some(DeploymentForm::Cloud)),
            _ => Err(FormError::UnknownForm(raw.unwrap_or("").to_string())),
        }
    }

    /// 这个形态下各项能力的开关（设计稿 §9 的表）。
    pub fn profile(self) -> FormProfile {
        match self {
            DeploymentForm::Personal => FormProfile {
                listen: ListenScope::Loopback,
                billing: BillingModel::None,
                local_models: true,
                uses_cloud_tier: true,
                uses_endpoint_permit_table: false,
                cloud_identity: CloudIdentity::PersonalCloudAccount,
                ledger_authority: LedgerAuthority::Local,
                tenant_source: TenantSource::FixedPersonal,
                allows_subscription_path: true,
                prunes_unlisted_tenants_on_startup: true,
                control_plane: false,
                public_abuse_protection: false,
                subsidy_pool: false,
            },
            DeploymentForm::Organization => FormProfile {
                listen: ListenScope::Lan,
                billing: BillingModel::Quota,
                local_models: true,
                uses_cloud_tier: true,
                uses_endpoint_permit_table: false,
                cloud_identity: CloudIdentity::OrgPaymentAccountOnly,
                ledger_authority: LedgerAuthority::OrgInternal,
                tenant_source: TenantSource::TenantHeader,
                allows_subscription_path: false,
                prunes_unlisted_tenants_on_startup: false,
                control_plane: true,
                public_abuse_protection: false,
                subsidy_pool: false,
            },
            DeploymentForm::Cloud => FormProfile {
                listen: ListenScope::Public,
                billing: BillingModel::Currency,
                local_models: false,
                uses_cloud_tier: false,
                uses_endpoint_permit_table: true,
                cloud_identity: CloudIdentity::NotApplicable,
                ledger_authority: LedgerAuthority::CloudLedger,
                tenant_source: TenantSource::FromCredential,
                allows_subscription_path: false,
                prunes_unlisted_tenants_on_startup: false,
                control_plane: true,
                public_abuse_protection: true,
                subsidy_pool: true,
            },
        }
    }

    /// 是否计费。
    pub fn bills(self) -> bool {
        self.profile().billing != BillingModel::None
    }

    /// 是否允许 subscription 路径。云端必须排除。
    pub fn allows_subscription_path(self) -> bool {
        self.profile().allows_subscription_path
    }

    /// 启动时是否按配置文件裁剪库里的租户（`retain_tenants`）。
    pub fn prunes_unlisted_tenants_on_startup(self) -> bool {
        self.profile().prunes_unlisted_tenants_on_startup
    }

    /// 是否要用端点许可表约束上游。
    pub fn uses_endpoint_permit_table(self) -> bool {
        self.profile().uses_endpoint_permit_table
    }
}

/// 配置里 `deploy_mode` 与 `form` 并列出现的样子。
///
/// 这是**示意性的消费者类型**：证明既有的、只有 `deploy_mode` 的配置原样反序列化（`form` 为 `None`），
/// 并且序列化回来**字节不变**（`form` 为 `None` 时不输出）。S1 接线时可以直接用它，也可以把 `form`
/// 并进自己的配置结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    pub deploy_mode: DeployMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<DeploymentForm>,
}

impl DeployConfig {
    pub fn resolve(&self) -> Result<DeploymentForm, FormError> {
        DeploymentForm::resolve(self.deploy_mode, self.form)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use idoris_contracts::deploy_mode::DEPLOY_MODE_VALUES;
    use std::path::Path;

    // ---------- 与既有 deploy_mode 共存：既有结果一个字节都不变 ----------

    #[test]
    fn existing_deploy_mode_still_has_exactly_two_values_and_the_same_wire_strings() {
        assert_eq!(DEPLOY_MODE_VALUES, &["personal", "tenant"]);
        assert_eq!(
            serde_json::to_string(&DeployMode::Personal).unwrap(),
            "\"personal\""
        );
        assert_eq!(
            serde_json::to_string(&DeployMode::Tenant).unwrap(),
            "\"tenant\""
        );
        assert_eq!(
            serde_json::from_str::<DeployMode>("\"personal\"").unwrap(),
            DeployMode::Personal
        );
        assert_eq!(
            serde_json::from_str::<DeployMode>("\"tenant\"").unwrap(),
            DeployMode::Tenant
        );
        // 新形态的名字没有被塞进旧枚举
        for new_value in ["organization", "cloud", "community", "city"] {
            assert!(
                serde_json::from_str::<DeployMode>(&format!("\"{new_value}\"")).is_err(),
                "{new_value}"
            );
        }
    }

    #[test]
    fn the_deploy_mode_schema_file_is_untouched() {
        // 直接读 schema 真源：enum 必须仍然只有这两个值。谁往里加了值，这条测试就会失败。
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/contracts/schema/deploy-mode.schema.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let schema: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(schema["enum"], serde_json::json!(["personal", "tenant"]));
        assert_eq!(schema["title"], "DeployMode");
    }

    #[test]
    fn existing_config_shapes_deserialize_and_serialize_byte_identically() {
        for (json, mode) in [
            (r#"{"deploy_mode":"personal"}"#, DeployMode::Personal),
            (r#"{"deploy_mode":"tenant"}"#, DeployMode::Tenant),
        ] {
            let parsed: DeployConfig = serde_json::from_str(json).unwrap();
            assert_eq!(parsed.deploy_mode, mode);
            assert_eq!(parsed.form, None, "存量配置没有 form 键");
            // 序列化回来与原文逐字节相同：没有多出任何键
            assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
        }
        // YAML 同理
        let yaml: DeployConfig = serde_yaml::from_str("deploy_mode: tenant\n").unwrap();
        assert_eq!((yaml.deploy_mode, yaml.form), (DeployMode::Tenant, None));
        assert_eq!(
            serde_yaml::to_string(&yaml).unwrap(),
            "deploy_mode: tenant\n"
        );
        // 既有配置里 deploy_mode 的取值范围没变：新形态的名字写在 deploy_mode 里仍然是错误
        assert!(serde_json::from_str::<DeployConfig>(r#"{"deploy_mode":"cloud"}"#).is_err());
        // 未知键照旧被拒绝（与仓库其它契约一致）
        assert!(serde_json::from_str::<DeployConfig>(r#"{"deploy_mode":"tenant","x":1}"#).is_err());
    }

    #[test]
    fn new_form_key_is_additive_and_round_trips() {
        let json = r#"{"deploy_mode":"tenant","form":"cloud"}"#;
        let parsed: DeployConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.resolve(), Ok(DeploymentForm::Cloud));
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
        assert!(
            serde_json::from_str::<DeployConfig>(r#"{"deploy_mode":"tenant","form":"clound"}"#)
                .is_err()
        );
    }

    // ---------- 解析表 ----------

    #[test]
    fn resolution_table_is_exhaustive() {
        use DeployMode::{Personal as DP, Tenant as DT};
        use DeploymentForm::{Cloud, Organization, Personal};
        let cases: [(
            DeployMode,
            Option<DeploymentForm>,
            Result<DeploymentForm, ()>,
        ); 8] = [
            (DP, None, Ok(Personal)),
            (DP, Some(Personal), Ok(Personal)),
            (DP, Some(Organization), Err(())),
            (DP, Some(Cloud), Err(())),
            (DT, None, Ok(Organization)), // 兼容：存量租户部署 = 组织
            (DT, Some(Organization), Ok(Organization)),
            (DT, Some(Cloud), Ok(Cloud)),
            (DT, Some(Personal), Err(())),
        ];
        for (mode, declared, expected) in cases {
            let got = DeploymentForm::resolve(mode, declared);
            match expected {
                Ok(form) => assert_eq!(got, Ok(form), "{mode:?} {declared:?}"),
                Err(()) => assert_eq!(
                    got,
                    Err(FormError::ConflictsWithDeployMode {
                        deploy_mode: mode,
                        form: declared.unwrap()
                    }),
                    "{mode:?} {declared:?}"
                ),
            }
        }
    }

    #[test]
    fn every_form_maps_back_to_a_deploy_mode_that_resolves_to_itself() {
        for form in DeploymentForm::ALL {
            assert_eq!(
                DeploymentForm::resolve(form.deploy_mode(), Some(form)),
                Ok(form)
            );
        }
        assert_eq!(DeploymentForm::Personal.deploy_mode(), DeployMode::Personal);
        assert_eq!(
            DeploymentForm::Organization.deploy_mode(),
            DeployMode::Tenant
        );
        assert_eq!(DeploymentForm::Cloud.deploy_mode(), DeployMode::Tenant);
    }

    #[test]
    fn env_value_parsing_is_strict_unlike_deploy_mode() {
        for (raw, expected) in [
            (None, None),
            (Some(""), None),
            (Some("   "), None),
            (Some("personal"), Some(DeploymentForm::Personal)),
            (Some(" Cloud "), Some(DeploymentForm::Cloud)),
            (Some("ORGANIZATION"), Some(DeploymentForm::Organization)),
        ] {
            assert_eq!(DeploymentForm::from_env_value(raw), Ok(expected), "{raw:?}");
        }
        for bad in ["clound", "tenant", "community", "city", "org", "1"] {
            assert_eq!(
                DeploymentForm::from_env_value(Some(bad)),
                Err(FormError::UnknownForm(bad.to_string())),
                "{bad}"
            );
        }
        assert_eq!(FORM_ENV_VAR, "IDORIS_FORM");
    }

    #[test]
    fn form_wire_strings_are_pinned() {
        for (form, text) in [
            (DeploymentForm::Personal, "personal"),
            (DeploymentForm::Organization, "organization"),
            (DeploymentForm::Cloud, "cloud"),
        ] {
            assert_eq!(form.as_str(), text);
            assert_eq!(serde_json::to_string(&form).unwrap(), format!("\"{text}\""));
            assert_eq!(
                serde_json::from_str::<DeploymentForm>(&format!("\"{text}\"")).unwrap(),
                form
            );
        }
        assert!(serde_json::from_str::<DeploymentForm>("\"tenant\"").is_err());
    }

    // ---------- 能力开关：设计稿 §9 的表 ----------

    #[test]
    fn capability_predicates_match_the_design_table() {
        use DeploymentForm::*;
        // 单位：个人不计费 / 组织 QUOTA / 云 THB·USD
        assert_eq!(Personal.profile().billing, BillingModel::None);
        assert_eq!(Organization.profile().billing, BillingModel::Quota);
        assert_eq!(Cloud.profile().billing, BillingModel::Currency);
        assert_eq!(
            [Personal.bills(), Organization.bills(), Cloud.bills()],
            [false, true, true]
        );
        // 入口
        assert_eq!(Personal.profile().listen, ListenScope::Loopback);
        assert_eq!(Organization.profile().listen, ListenScope::Lan);
        assert_eq!(Cloud.profile().listen, ListenScope::Public);
        // 云端必须排除 subscription；组织与既有 tenant 一致，也关
        assert!(Personal.allows_subscription_path());
        assert!(!Organization.allows_subscription_path());
        assert!(!Cloud.allows_subscription_path());
        // retain_tenants：个人保持现状，组织和云不再裁剪
        assert!(Personal.prunes_unlisted_tenants_on_startup());
        assert!(!Organization.prunes_unlisted_tenants_on_startup());
        assert!(!Cloud.prunes_unlisted_tenants_on_startup());
        // 上游
        assert!(
            Personal.profile().local_models
                && Organization.profile().local_models
                && !Cloud.profile().local_models
        );
        assert!(
            Personal.profile().uses_cloud_tier
                && Organization.profile().uses_cloud_tier
                && !Cloud.profile().uses_cloud_tier
        );
        assert_eq!(
            [
                Personal.uses_endpoint_permit_table(),
                Organization.uses_endpoint_permit_table(),
                Cloud.uses_endpoint_permit_table()
            ],
            [false, false, true]
        );
        // 云端认谁
        assert_eq!(
            Personal.profile().cloud_identity,
            CloudIdentity::PersonalCloudAccount
        );
        assert_eq!(
            Organization.profile().cloud_identity,
            CloudIdentity::OrgPaymentAccountOnly
        );
        assert_eq!(Cloud.profile().cloud_identity, CloudIdentity::NotApplicable);
        // 账本权威
        assert_eq!(Personal.profile().ledger_authority, LedgerAuthority::Local);
        assert_eq!(
            Organization.profile().ledger_authority,
            LedgerAuthority::OrgInternal
        );
        assert_eq!(
            Cloud.profile().ledger_authority,
            LedgerAuthority::CloudLedger
        );
        // 租户来源：本地租户模式的 X-iDoris-Tenant 契约不变；云从凭证推导
        assert_eq!(
            Personal.profile().tenant_source,
            TenantSource::FixedPersonal
        );
        assert_eq!(
            Organization.profile().tenant_source,
            TenantSource::TenantHeader
        );
        assert_eq!(Cloud.profile().tenant_source, TenantSource::FromCredential);
        // 云形态独有
        assert!(
            Cloud.profile().public_abuse_protection
                && !Organization.profile().public_abuse_protection
                && !Personal.profile().public_abuse_protection
        );
        assert!(
            Cloud.profile().subsidy_pool
                && !Organization.profile().subsidy_pool
                && !Personal.profile().subsidy_pool
        );
        // 写操作控制面：个人没有（Admin v0 只读），组织和云有
        assert!(
            !Personal.profile().control_plane
                && Organization.profile().control_plane
                && Cloud.profile().control_plane
        );
    }

    #[test]
    fn the_full_profile_of_each_form_is_pinned() {
        // 任何一格能力的改动都会让这条失败：改开关必须是有意的。
        let expected = [
            (
                DeploymentForm::Personal,
                r#"{"listen":"loopback","billing":"none","local_models":true,"uses_cloud_tier":true,"uses_endpoint_permit_table":false,"cloud_identity":"personal_cloud_account","ledger_authority":"local","tenant_source":"fixed_personal","allows_subscription_path":true,"prunes_unlisted_tenants_on_startup":true,"control_plane":false,"public_abuse_protection":false,"subsidy_pool":false}"#,
            ),
            (
                DeploymentForm::Organization,
                r#"{"listen":"lan","billing":"quota","local_models":true,"uses_cloud_tier":true,"uses_endpoint_permit_table":false,"cloud_identity":"org_payment_account_only","ledger_authority":"org_internal","tenant_source":"tenant_header","allows_subscription_path":false,"prunes_unlisted_tenants_on_startup":false,"control_plane":true,"public_abuse_protection":false,"subsidy_pool":false}"#,
            ),
            (
                DeploymentForm::Cloud,
                r#"{"listen":"public","billing":"currency","local_models":false,"uses_cloud_tier":false,"uses_endpoint_permit_table":true,"cloud_identity":"not_applicable","ledger_authority":"cloud_ledger","tenant_source":"from_credential","allows_subscription_path":false,"prunes_unlisted_tenants_on_startup":false,"control_plane":true,"public_abuse_protection":true,"subsidy_pool":true}"#,
            ),
        ];
        for (form, json) in expected {
            assert_eq!(
                serde_json::to_string(&form.profile()).unwrap(),
                json,
                "{form:?}"
            );
            assert_eq!(
                serde_json::from_str::<FormProfile>(json).unwrap(),
                form.profile()
            );
        }
    }

    #[test]
    fn cloud_never_enables_subscription_regardless_of_other_flags() {
        // 云形态排除 subscription：这是设计稿 §8 的硬规则，不依赖其它任何开关。
        assert!(!DeploymentForm::Cloud.allows_subscription_path());
        assert!(!DeploymentForm::Cloud.profile().allows_subscription_path);
    }
}

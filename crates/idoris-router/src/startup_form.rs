//! 部署形态接入启动（计算服务 S1，设计稿 §9）。
//!
//! # 两个开关，各管各的
//!
//! - `IDORIS_DEPLOY_MODE`（既有，`personal | tenant`，未知值收成 `tenant`）：**一个字符都没动**。
//! - `IDORIS_FORM`（S0 建议的名字，`personal | organization | cloud`）：新增、可选、严格。
//!   没设（或只有空白）= 完全按改动前的行为启动；设了未知值是启动错误，不会像 `IDORIS_DEPLOY_MODE`
//!   那样悄悄收成别的形态；与 `IDORIS_DEPLOY_MODE` 冲突（比如个人模式 + `organization`）也是启动错误。
//!
//! # 为什么不直接用 S0 的 `DeploymentForm::resolve(deploy_mode, None)`
//!
//! S0 把「`deploy_mode=tenant` 又没有 `form`」解析成 `Organization`，而 `Organization` 的能力表里
//! `prunes_unlisted_tenants_on_startup = false`。如果启动直接照这个结果办，**存量的 tenant 部署
//! 一升级就不再在启动时按配置清单删租户**，这和「不设新开关时行为不变」矛盾。
//! 所以这里多记一位 `declared`：只有**显式写了 `IDORIS_FORM`**，才按 S0 能力表里该形态的值办；
//! 没写就是存量行为（个人和 tenant 都照旧在启动时 `retain_tenants`）。S0 契约本身没有改。
//!
//! | `IDORIS_DEPLOY_MODE` | `IDORIS_FORM` | 启动时删配置清单之外的租户 | 租户来源 | 连接设置 | 账户存储 |
//! |---|---|---|---|---|---|
//! | personal | 未设 | 是（存量） | 配置清单是权威 | 存量 | 不打开 |
//! | tenant | 未设 | 是（存量） | 配置清单是权威 | 存量 | 不打开 |
//! | personal | `personal` | 是 | 配置清单是权威 | 存量 | 不打开 |
//! | tenant | `organization` / `cloud` | **否** | **数据库是权威**，配置清单只做一次性初始化 | Standard | 打开 |
//! | personal | `organization` / `cloud` | 启动失败：形态与 deploy_mode 冲突 | | | |
//! | 任意 | 未知值 | 启动失败 | | | |

use idoris_compute_contracts::form::{DeploymentForm, FORM_ENV_VAR, FormError};
use idoris_contracts::DeployMode;
use idoris_tenancy::sqlite_conn::ConnectionProfile;

/// 启动时确定下来的形态，以及它是不是被显式声明的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupForm {
    form: DeploymentForm,
    declared: bool,
}

impl StartupForm {
    /// 承载形态的环境变量名，`IDORIS_FORM`。
    pub const ENV_VAR: &'static str = FORM_ENV_VAR;

    /// 没有显式声明形态：存量行为。`deploy_mode` 是 `IDORIS_DEPLOY_MODE` 解析出的既有取值。
    pub fn legacy(deploy_mode: DeployMode) -> Self {
        let form = match deploy_mode {
            DeployMode::Personal => DeploymentForm::Personal,
            DeployMode::Tenant => DeploymentForm::Organization,
        };
        Self {
            form,
            declared: false,
        }
    }

    /// 显式声明了形态；与 `deploy_mode` 冲突返回错误。
    pub fn declared(deploy_mode: DeployMode, form: DeploymentForm) -> Result<Self, FormError> {
        let form = DeploymentForm::resolve(deploy_mode, Some(form))?;
        Ok(Self {
            form,
            declared: true,
        })
    }

    /// 由 `IDORIS_FORM` 的值（未设为 `None`）和既有的 `deploy_mode` 得到启动形态。
    /// 空白等同未设；大小写不敏感；未知值、与 `deploy_mode` 冲突都是错误。
    pub fn from_env_value(deploy_mode: DeployMode, raw: Option<&str>) -> Result<Self, FormError> {
        match DeploymentForm::from_env_value(raw)? {
            None => Ok(Self::legacy(deploy_mode)),
            Some(form) => Self::declared(deploy_mode, form),
        }
    }

    pub fn form(&self) -> DeploymentForm {
        self.form
    }

    /// 是否显式写了 `IDORIS_FORM`。`false` = 存量行为。
    pub fn is_declared(&self) -> bool {
        self.declared
    }

    /// 对应的既有 `deploy_mode`（与 `IDORIS_DEPLOY_MODE` 解析出来的一致）。
    pub fn deploy_mode(&self) -> DeployMode {
        self.form.deploy_mode()
    }

    /// 启动时是否按配置清单删掉库里多余的租户（`retain_tenants`，设计稿 §9 第 2 项）。
    /// 存量（没设 `IDORIS_FORM`）一律是；显式声明时照 S0 能力表：个人是，组织、云不是。
    pub fn prunes_unlisted_tenants_on_startup(&self) -> bool {
        if self.declared {
            self.form.prunes_unlisted_tenants_on_startup()
        } else {
            true
        }
    }

    /// 租户来源是否以数据库为准：配置清单只做一次性初始化，之后不覆盖、不删除。
    /// 与 [`prunes_unlisted_tenants_on_startup`](Self::prunes_unlisted_tenants_on_startup) 恰好相反，
    /// 写成两个函数是为了让调用处读起来是它要表达的意思。
    pub fn tenants_are_database_authoritative(&self) -> bool {
        !self.prunes_unlisted_tenants_on_startup()
    }

    /// 打开存储时用的连接设置：只有显式声明的组织、云形态用 `Standard`（WAL 已验证、显式 busy_timeout、
    /// `foreign_keys=ON` 已回读）；个人和存量 tenant 部署的连接一个字节都不变。
    pub fn connection_profile(&self) -> ConnectionProfile {
        if self.declared && self.form != DeploymentForm::Personal {
            ConnectionProfile::Standard
        } else {
            ConnectionProfile::Legacy
        }
    }

    /// 是否在启动时打开账户存储：显式声明的、需要控制面的形态（组织、云）。
    pub fn opens_account_store(&self) -> bool {
        self.declared && self.form.profile().control_plane
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn parse(mode: DeployMode, raw: Option<&str>) -> Result<StartupForm, FormError> {
        StartupForm::from_env_value(mode, raw)
    }

    #[test]
    fn the_variable_name_is_the_one_s0_proposed() {
        assert_eq!(StartupForm::ENV_VAR, "IDORIS_FORM");
    }

    #[test]
    fn unset_or_blank_is_legacy_behaviour_for_both_existing_deploy_modes() {
        for mode in [DeployMode::Personal, DeployMode::Tenant] {
            for raw in [None, Some(""), Some("   "), Some("\t\n")] {
                let startup = parse(mode, raw).unwrap();
                assert!(!startup.is_declared(), "{mode:?} {raw:?}");
                assert_eq!(startup.deploy_mode(), mode);
                // 存量：个人和 tenant 都照旧在启动时按清单删租户
                assert!(
                    startup.prunes_unlisted_tenants_on_startup(),
                    "{mode:?} {raw:?}"
                );
                assert!(!startup.tenants_are_database_authoritative());
                assert_eq!(startup.connection_profile(), ConnectionProfile::Legacy);
                assert!(!startup.opens_account_store());
                assert_eq!(startup, StartupForm::legacy(mode));
            }
        }
    }

    #[test]
    fn a_legacy_tenant_deployment_is_not_silently_turned_into_a_database_authoritative_one() {
        // 对照：S0 的 resolve 会把它解析成 Organization（其能力表不删租户）。存量部署必须不吃到这个差别。
        let resolved = DeploymentForm::resolve(DeployMode::Tenant, None).unwrap();
        assert_eq!(resolved, DeploymentForm::Organization);
        assert!(!resolved.prunes_unlisted_tenants_on_startup());
        let startup = StartupForm::legacy(DeployMode::Tenant);
        assert!(startup.prunes_unlisted_tenants_on_startup());
    }

    #[test]
    fn organization_and_cloud_are_database_authoritative_with_standard_connections() {
        for text in ["organization", "cloud", "  Cloud ", "ORGANIZATION"] {
            let startup = parse(DeployMode::Tenant, Some(text)).unwrap();
            assert!(startup.is_declared());
            assert_eq!(startup.deploy_mode(), DeployMode::Tenant);
            assert!(!startup.prunes_unlisted_tenants_on_startup(), "{text}");
            assert!(startup.tenants_are_database_authoritative());
            assert_eq!(startup.connection_profile(), ConnectionProfile::Standard);
            assert!(startup.opens_account_store());
        }
        assert_eq!(
            parse(DeployMode::Tenant, Some("organization"))
                .unwrap()
                .form(),
            DeploymentForm::Organization
        );
        assert_eq!(
            parse(DeployMode::Tenant, Some("cloud")).unwrap().form(),
            DeploymentForm::Cloud
        );
    }

    #[test]
    fn an_explicit_personal_form_behaves_exactly_like_the_default_personal_start() {
        let startup = parse(DeployMode::Personal, Some("personal")).unwrap();
        assert!(startup.is_declared());
        assert!(startup.prunes_unlisted_tenants_on_startup());
        assert_eq!(startup.connection_profile(), ConnectionProfile::Legacy);
        assert!(!startup.opens_account_store());
    }

    #[test]
    fn a_form_that_conflicts_with_the_deploy_mode_is_an_error_not_a_guess() {
        for (mode, text) in [
            (DeployMode::Personal, "organization"),
            (DeployMode::Personal, "cloud"),
            (DeployMode::Tenant, "personal"),
        ] {
            match parse(mode, Some(text)) {
                Err(FormError::ConflictsWithDeployMode { .. }) => {}
                other => panic!("{mode:?} + {text}: {other:?}"),
            }
        }
    }

    #[test]
    fn an_unknown_form_is_an_error_and_never_falls_back_to_tenant() {
        for text in [
            "clound",
            "tenant",
            "community",
            "city",
            "org",
            "0",
            "personal-ish",
        ] {
            match parse(DeployMode::Tenant, Some(text)) {
                Err(FormError::UnknownForm(raw)) => assert_eq!(raw, text),
                other => panic!("{text}: {other:?}"),
            }
        }
    }
}

//! 云入口配置（YAML，计算服务 S5）。
//!
//! 由环境变量 [`CONFIG_ENV`]（`IDORIS_CLOUD_INGRESS_CONFIG`）指向的文件加载。**云形态（`IDORIS_FORM=cloud`）必须有这份
//! 文件**：它是「公网配置」——受信反向代理、各级上限、防滥用、CORS、控制面是否对公网开放，以及（最重要的）
//! 每个租户的数据处理要求。其它形态不读它。所有对象拒绝未知字段，写错一个键名就启动失败，不会悄悄用默认值。
//!
//! ```yaml
//! schema_version: 1
//!
//! public:                                   # 公网入口（TLS 由反向代理终止，iDoris 自己不做 TLS）
//!   trusted_proxies: ["10.0.0.0/8"]         # 只有这些对端写的 X-Forwarded-For 才被相信；监听非 loopback 时不能为空
//!
//! control_plane:
//!   public: false                           # true = /control/v1 也挂在公网监听上；默认不挂（本机运维命令仍然可用）
//!
//! cors:                                     # 默认不开放（P0：eduai 走后端代理，浏览器不直连）
//!   allowed_origins: []                     # 精确匹配的来源；不支持通配符
//!   max_age_secs: 600
//!
//! abuse:                                    # 公网防滥用（括号里是默认值）
//!   body_max_bytes: 1048576                 # 请求体上限
//!   body_read_timeout_ms: 10000             # 请求体必须在这么久之内读完（慢请求）
//!   anonymous:            {requests_per_minute: 60, burst: 20}   # 所有「没有凭证」的请求合起来的总配额
//!   per_source_anonymous: {requests_per_minute: 20, burst: 10}   # 单个来源的软配额（辅助信号，只作用于没有凭证的请求）
//!   auth_failures: {max_failures: 5, window_secs: 60}            # 同一个凭证前缀的认证失败上限
//!
//! limits:                                   # 三级上限：请求速率（令牌桶）和并发（在途个数）
//!   platform: {requests_per_minute: 3000, burst: 100, max_concurrent: 128}
//!   tenant:   {requests_per_minute: 600,  burst: 40,  max_concurrent: 32}    # 每个租户的默认值
//!   member:   {requests_per_minute: 20,   burst: 5,   max_concurrent: 2}     # 每个成员的默认值
//!
//! tenants:                                  # 租户 = 付款账户树里平台根之下的顶层客户（见 caller.rs）
//!   - account: eduai                        # 租户的账户 id
//!     data_classes: [child_edu]             # child_edu | adult | public
//!     max_retention: zdr                    # zdr | with_exceptions | retained（含 child_edu 时固定 zdr）
//!     require_dpa: true                     # 含 child_edu 时固定为要求已签 DPA
//!     allowed_regions: [sg-1]               # 可选；不写 = 不限区域
//!     billing: subsidized                   # paid | subsidized | unbilled
//!     limits:                               # 可选：这个租户自己的上限（写就写完整的一块）
//!       tenant: {requests_per_minute: 1200, burst: 60, max_concurrent: 64}
//!       member: {requests_per_minute: 30, burst: 10, max_concurrent: 3}
//! ```
//!
//! # 租户的数据处理要求为什么在这里
//!
//! 数据类别（是不是 `child_edu`）是**平台**对一个客户的判定：这个客户能把什么数据送进来。它不能由请求自己说，
//! 也不能由租户自己的管理员通过控制面改（否则一所学校的管理员可以把自己从 `child_edu` 改成 `adult` 来绕过
//! 儿童保护）。所以它在运维控制的这份配置里，由租户账户 id 索引；改它要改文件、重启，没有任何网络入口能动它。
//! 一个租户没有在这里声明，它的请求一律被拒（403 `tenant_not_provisioned`）：不知道用什么标准处理数据，就不处理。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use idoris_compute_contracts::ids::AccountId;
use idoris_compute_contracts::permit::{Billing, DataClass, Region, Retention, TenantDataPolicy};
use serde::Deserialize;

use super::bucket::RateLimit;
use super::cors::CorsPolicy;
use super::limits::{CallerLimits, LevelLimits};
use super::net::TrustedProxies;

/// 指向配置文件的环境变量。
pub const CONFIG_ENV: &str = "IDORIS_CLOUD_INGRESS_CONFIG";

/// 当前支持的配置 schema 版本。
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

const MIN_BODY_BYTES: usize = 1_024;
const MAX_BODY_BYTES: usize = 16 * 1_024 * 1_024;
const MIN_READ_TIMEOUT_MS: u64 = 100;
const MAX_READ_TIMEOUT_MS: u64 = 120_000;
const MAX_FAILURE_WINDOW_SECS: u64 = 3_600;

// ---------------------------------------------------------------------------
// 文件形状（serde）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RateFile {
    requests_per_minute: u32,
    burst: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LevelFile {
    requests_per_minute: u32,
    burst: u32,
    max_concurrent: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailuresFile {
    max_failures: u32,
    window_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct PublicFile {
    trusted_proxies: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ControlPlaneFile {
    public: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct CorsFile {
    allowed_origins: Vec<String>,
    max_age_secs: u64,
}

impl Default for CorsFile {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            max_age_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct AbuseFile {
    body_max_bytes: usize,
    body_read_timeout_ms: u64,
    anonymous: RateFile,
    per_source_anonymous: RateFile,
    auth_failures: FailuresFile,
}

impl Default for AbuseFile {
    fn default() -> Self {
        Self {
            body_max_bytes: 1_048_576,
            body_read_timeout_ms: 10_000,
            anonymous: RateFile {
                requests_per_minute: 60,
                burst: 20,
            },
            per_source_anonymous: RateFile {
                requests_per_minute: 20,
                burst: 10,
            },
            auth_failures: FailuresFile {
                max_failures: 5,
                window_secs: 60,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LimitsFile {
    platform: LevelFile,
    tenant: LevelFile,
    member: LevelFile,
}

impl Default for LimitsFile {
    fn default() -> Self {
        Self {
            platform: LevelFile {
                requests_per_minute: 3_000,
                burst: 100,
                max_concurrent: 128,
            },
            tenant: LevelFile {
                requests_per_minute: 600,
                burst: 40,
                max_concurrent: 32,
            },
            member: LevelFile {
                requests_per_minute: 20,
                burst: 5,
                max_concurrent: 2,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantLimitsFile {
    #[serde(default)]
    tenant: Option<LevelFile>,
    #[serde(default)]
    member: Option<LevelFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantFile {
    account: String,
    data_classes: Vec<DataClass>,
    #[serde(default)]
    allowed_regions: Option<Vec<String>>,
    max_retention: Retention,
    #[serde(default)]
    require_dpa: bool,
    billing: Billing,
    #[serde(default)]
    limits: Option<TenantLimitsFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngressFile {
    schema_version: u32,
    #[serde(default)]
    public: PublicFile,
    #[serde(default)]
    control_plane: ControlPlaneFile,
    #[serde(default)]
    cors: CorsFile,
    #[serde(default)]
    abuse: AbuseFile,
    #[serde(default)]
    limits: LimitsFile,
    #[serde(default)]
    tenants: Vec<TenantFile>,
}

// ---------------------------------------------------------------------------
// 运行时结构
// ---------------------------------------------------------------------------

/// 一个租户：它的数据处理要求、计费性质和上限。
#[derive(Debug, Clone)]
pub struct TenantProfile {
    pub account: AccountId,
    pub data: TenantDataPolicy,
    pub billing: Billing,
    pub limits: CallerLimits,
}

/// 校验过的云入口配置。
#[derive(Debug, Clone)]
pub struct IngressConfig {
    pub trusted_proxies: TrustedProxies,
    pub control_plane_public: bool,
    pub cors: CorsPolicy,
    pub body_max_bytes: usize,
    pub body_read_timeout: Duration,
    /// 所有没有凭证的请求合起来的总配额。
    pub anonymous_global: RateLimit,
    /// 单个来源的软配额。
    pub anonymous_per_source: RateLimit,
    pub auth_failure_max: u32,
    pub auth_failure_window_secs: u64,
    pub platform: LevelLimits,
    /// 没有自己声明上限的租户用这一组。
    pub default_limits: CallerLimits,
    tenants: BTreeMap<AccountId, TenantProfile>,
}

fn bad(message: impl Into<String>) -> String {
    format!("云入口配置：{}", message.into())
}

fn level(what: &str, file: &LevelFile) -> Result<LevelLimits, String> {
    LevelLimits::new(file.requests_per_minute, file.burst, file.max_concurrent)
        .map_err(|e| bad(format!("{what}: {e}")))
}

fn rate(what: &str, file: &RateFile) -> Result<RateLimit, String> {
    RateLimit::new(file.requests_per_minute, file.burst).map_err(|e| bad(format!("{what}: {e}")))
}

impl IngressConfig {
    /// 租户的档案。没有声明的租户返回 `None`。
    pub fn tenant(&self, account: &AccountId) -> Option<&TenantProfile> {
        self.tenants.get(account)
    }

    pub fn tenants(&self) -> impl Iterator<Item = &TenantProfile> {
        self.tenants.values()
    }

    /// 从 YAML 文本构造并校验。
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: IngressFile =
            serde_yaml::from_str(text).map_err(|e| bad(format!("无法解析 YAML：{e}")))?;
        if file.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(bad(format!(
                "不支持的 schema_version {}（支持 {CONFIG_SCHEMA_VERSION}）",
                file.schema_version
            )));
        }

        let trusted_proxies =
            TrustedProxies::parse(file.public.trusted_proxies.iter().map(String::as_str))
                .map_err(|e| bad(format!("public.trusted_proxies: {e}")))?;

        let cors =
            CorsPolicy::new(&file.cors.allowed_origins, file.cors.max_age_secs).map_err(bad)?;

        let abuse = &file.abuse;
        if !(MIN_BODY_BYTES..=MAX_BODY_BYTES).contains(&abuse.body_max_bytes) {
            return Err(bad(format!(
                "abuse.body_max_bytes 必须在 {MIN_BODY_BYTES}..={MAX_BODY_BYTES} 之间"
            )));
        }
        if !(MIN_READ_TIMEOUT_MS..=MAX_READ_TIMEOUT_MS).contains(&abuse.body_read_timeout_ms) {
            return Err(bad(format!(
                "abuse.body_read_timeout_ms 必须在 {MIN_READ_TIMEOUT_MS}..={MAX_READ_TIMEOUT_MS} 之间"
            )));
        }
        let anonymous_global = rate("abuse.anonymous", &abuse.anonymous)?;
        let anonymous_per_source = rate("abuse.per_source_anonymous", &abuse.per_source_anonymous)?;
        if abuse.auth_failures.max_failures == 0 {
            return Err(bad("abuse.auth_failures.max_failures 必须至少是 1"));
        }
        if !(1..=MAX_FAILURE_WINDOW_SECS).contains(&abuse.auth_failures.window_secs) {
            return Err(bad(format!(
                "abuse.auth_failures.window_secs 必须在 1..={MAX_FAILURE_WINDOW_SECS} 之间"
            )));
        }

        let platform = level("limits.platform", &file.limits.platform)?;
        let default_limits = CallerLimits {
            tenant: level("limits.tenant", &file.limits.tenant)?,
            member: level("limits.member", &file.limits.member)?,
        };

        if file.tenants.is_empty() {
            return Err(bad(
                "tenants 不能为空：没有声明任何租户，每个请求都会被拒绝（tenant_not_provisioned）",
            ));
        }
        let mut tenants = BTreeMap::new();
        for tenant in &file.tenants {
            let account = AccountId::new(&tenant.account)
                .map_err(|e| bad(format!("tenants[].account {:?}: {e}", tenant.account)))?;
            let what = |field: &str| format!("租户 {account} 的 {field}");
            let regions = tenant
                .allowed_regions
                .as_ref()
                .map(|list| {
                    list.iter()
                        .map(|r| {
                            Region::new(r.clone())
                                .map_err(|e| bad(format!("{}: {e}", what("allowed_regions"))))
                        })
                        .collect::<Result<BTreeSet<_>, _>>()
                })
                .transpose()?;
            let data = TenantDataPolicy::new(
                tenant.data_classes.iter().copied().collect(),
                regions,
                tenant.max_retention,
                tenant.require_dpa,
            )
            .map_err(|e| bad(format!("{}: {e}", what("数据处理要求"))))?;
            let limits = match &tenant.limits {
                None => default_limits,
                Some(own) => CallerLimits {
                    tenant: match &own.tenant {
                        Some(file) => level(&what("limits.tenant"), file)?,
                        None => default_limits.tenant,
                    },
                    member: match &own.member {
                        Some(file) => level(&what("limits.member"), file)?,
                        None => default_limits.member,
                    },
                },
            };
            let profile = TenantProfile {
                account: account.clone(),
                data,
                billing: tenant.billing,
                limits,
            };
            if tenants.insert(account.clone(), profile).is_some() {
                return Err(bad(format!("租户 {account} 声明了两次")));
            }
        }

        Ok(Self {
            trusted_proxies,
            control_plane_public: file.control_plane.public,
            cors,
            body_max_bytes: abuse.body_max_bytes,
            body_read_timeout: Duration::from_millis(abuse.body_read_timeout_ms),
            anonymous_global,
            anonymous_per_source,
            auth_failure_max: abuse.auth_failures.max_failures,
            auth_failure_window_secs: abuse.auth_failures.window_secs,
            platform,
            default_limits,
            tenants,
        })
    }

    /// 从文件加载。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| bad(format!("无法读取 {}：{e}", path.display())))?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const MINIMAL: &str = "schema_version: 1
tenants:
  - account: agent24
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
";

    const FULL: &str = "schema_version: 1
public:
  trusted_proxies: [\"10.0.0.0/8\", \"127.0.0.1\"]
control_plane:
  public: true
cors:
  allowed_origins: [\"https://app.example.com\"]
  max_age_secs: 300
abuse:
  body_max_bytes: 65536
  body_read_timeout_ms: 2000
  anonymous: {requests_per_minute: 30, burst: 10}
  per_source_anonymous: {requests_per_minute: 12, burst: 4}
  auth_failures: {max_failures: 3, window_secs: 120}
limits:
  platform: {requests_per_minute: 1000, burst: 50, max_concurrent: 64}
  tenant: {requests_per_minute: 300, burst: 20, max_concurrent: 16}
  member: {requests_per_minute: 10, burst: 3, max_concurrent: 1}
tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    allowed_regions: [sg-1]
    billing: subsidized
    limits:
      tenant: {requests_per_minute: 900, burst: 90, max_concurrent: 48}
  - account: agent24
    data_classes: [adult, public]
    max_retention: with_exceptions
    billing: paid
";

    fn expect_error(text: &str, needle: &str) {
        match IngressConfig::parse(text) {
            Ok(_) => panic!("expected an error containing {needle:?}"),
            Err(error) => assert!(error.contains(needle), "{needle:?} not in {error}"),
        }
    }

    fn id(text: &str) -> AccountId {
        AccountId::new(text).unwrap()
    }

    #[test]
    fn the_minimal_file_loads_with_every_default() {
        let config = IngressConfig::parse(MINIMAL).unwrap();
        assert!(config.trusted_proxies.is_empty());
        assert!(
            !config.control_plane_public,
            "the control plane is off by default"
        );
        assert!(!config.cors.is_enabled(), "CORS is off by default");
        assert_eq!(config.body_max_bytes, 1_048_576);
        assert_eq!(config.body_read_timeout, Duration::from_secs(10));
        assert_eq!(config.anonymous_global.requests_per_minute(), 60);
        assert_eq!(config.anonymous_per_source.burst(), 10);
        assert_eq!(
            (config.auth_failure_max, config.auth_failure_window_secs),
            (5, 60)
        );
        assert_eq!(config.platform.max_concurrent, 128);
        assert_eq!(config.default_limits.tenant.rate.requests_per_minute(), 600);
        assert_eq!(config.default_limits.member.max_concurrent, 2);
        let tenant = config.tenant(&id("agent24")).unwrap();
        assert_eq!(tenant.billing, Billing::Paid);
        assert!(!tenant.data.handles_child_data());
        assert_eq!(tenant.limits, config.default_limits);
        assert!(config.tenant(&id("eduai")).is_none());
    }

    #[test]
    fn the_full_file_loads_every_part() {
        let config = IngressConfig::parse(FULL).unwrap();
        assert_eq!(config.trusted_proxies.len(), 2);
        assert!(config.control_plane_public);
        assert!(config.cors.allows("https://app.example.com"));
        assert_eq!(config.cors.max_age_secs(), 300);
        assert_eq!(config.body_max_bytes, 65_536);
        assert_eq!(config.body_read_timeout, Duration::from_millis(2_000));
        assert_eq!(config.platform.rate.requests_per_minute(), 1_000);
        // eduai 是 child_edu：固定 ZDR + 已签 DPA；自己的租户级上限，成员级沿用默认
        let eduai = config.tenant(&id("eduai")).unwrap();
        assert!(eduai.data.handles_child_data());
        assert_eq!(eduai.billing, Billing::Subsidized);
        assert_eq!(eduai.limits.tenant.rate.requests_per_minute(), 900);
        assert_eq!(eduai.limits.tenant.max_concurrent, 48);
        assert_eq!(eduai.limits.member, config.default_limits.member);
        assert_eq!(config.default_limits.member.rate.requests_per_minute(), 10);
        // agent24 没写 limits：用默认
        assert_eq!(
            config.tenant(&id("agent24")).unwrap().limits,
            config.default_limits
        );
        assert_eq!(config.tenants().count(), 2);
    }

    #[test]
    fn broken_files_are_refused_with_a_reason() {
        expect_error(
            &MINIMAL.replace("schema_version: 1", "schema_version: 2"),
            "schema_version",
        );
        expect_error(&format!("{MINIMAL}surprise: true\n"), "unknown field");
        expect_error(
            &MINIMAL.replace("billing: paid", "billing: paid\n    surprise: 1"),
            "unknown field",
        );
        // 没有租户
        expect_error("schema_version: 1\n", "tenants 不能为空");
        expect_error("schema_version: 1\ntenants: []\n", "tenants 不能为空");
        // 租户：重复、坏 id、空数据类别、坏区域、未知计费
        expect_error(
            &format!(
                "{MINIMAL}  - account: agent24\n    data_classes: [adult]\n    max_retention: zdr\n    billing: paid\n"
            ),
            "声明了两次",
        );
        expect_error(&MINIMAL.replace("agent24", "bad id!"), "tenants[].account");
        expect_error(&MINIMAL.replace("[adult]", "[]"), "数据处理要求");
        expect_error(
            &MINIMAL.replace("billing: paid", "billing: paid\n    allowed_regions: []"),
            "数据处理要求",
        );
        expect_error(
            &MINIMAL.replace("billing: paid", "billing: free"),
            "unknown variant",
        );
        // 受信代理：坏 CIDR、带主机位
        expect_error(
            &format!("{MINIMAL}public:\n  trusted_proxies: [\"10.0.0.5/8\"]\n"),
            "host bits",
        );
        expect_error(
            &format!("{MINIMAL}public:\n  trusted_proxies: [\"proxy.example.com\"]\n"),
            "trusted_proxies",
        );
        // CORS：通配符
        expect_error(
            &format!("{MINIMAL}cors:\n  allowed_origins: [\"*\"]\n"),
            "cors.allowed_origins",
        );
        // 防滥用的范围
        expect_error(
            &format!("{MINIMAL}abuse:\n  body_max_bytes: 10\n"),
            "body_max_bytes",
        );
        expect_error(
            &format!("{MINIMAL}abuse:\n  body_read_timeout_ms: 0\n"),
            "body_read_timeout_ms",
        );
        expect_error(
            &format!("{MINIMAL}abuse:\n  auth_failures: {{max_failures: 0, window_secs: 60}}\n"),
            "max_failures",
        );
        expect_error(
            &format!("{MINIMAL}abuse:\n  auth_failures: {{max_failures: 3, window_secs: 99999}}\n"),
            "window_secs",
        );
        // 上限：零
        expect_error(
            &format!(
                "{MINIMAL}limits:\n  member: {{requests_per_minute: 0, burst: 1, max_concurrent: 1}}\n"
            ),
            "requests_per_minute",
        );
        expect_error(
            &format!(
                "{MINIMAL}limits:\n  platform: {{requests_per_minute: 1, burst: 1, max_concurrent: 0}}\n"
            ),
            "max_concurrent",
        );
        // 部分写的上限块（少一个字段）是错误，不是悄悄补默认值
        expect_error(
            &format!("{MINIMAL}limits:\n  tenant: {{requests_per_minute: 5}}\n"),
            "missing field",
        );
    }

    #[test]
    fn a_missing_file_is_a_startup_error_that_names_the_path() {
        let error = IngressConfig::load(Path::new("/nonexistent/ingress.yaml")).unwrap_err();
        assert!(error.contains("/nonexistent/ingress.yaml"), "{error}");
        assert!(error.starts_with("云入口配置"), "{error}");
    }

    #[test]
    fn the_documented_example_in_the_module_header_is_a_valid_file() {
        // 模块文档里的示例必须真的能加载：文档和代码不能各说各话
        let source = include_str!("config.rs");
        let start = source.find("//! ```yaml\n").unwrap() + "//! ```yaml\n".len();
        let end = start + source[start..].find("//! ```\n").unwrap();
        let yaml: String = source[start..end]
            .lines()
            .map(|line| {
                line.strip_prefix("//! ")
                    .unwrap_or(line.strip_prefix("//!").unwrap_or(line))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config = IngressConfig::parse(&yaml).unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        assert!(config.tenant(&id("eduai")).is_some());
    }
}

//! 云上游配置（YAML）。
//!
//! 由环境变量 [`CONFIG_ENV`]（`IDORIS_CLOUD_UPSTREAM_CONFIG`）指向的文件加载；**不设就完全不启用**
//! （默认关闭，现有的本地 oMLX 路径、订阅中转与全部既有 HTTP 行为一字不变）。
//!
//! 上游的**地址和密钥只在这里**：契约（S0）里没有 URL，也没有密钥。密钥永远不写在配置里，只写来源：
//! `api_key_env`（环境变量名）或 `api_key_file`（文件路径）二选一。
//!
//! ```yaml
//! schema_version: 1
//! permit_table: ./endpoint-permit-table.yaml     # S0 schema 的端点许可表；相对路径相对于本文件
//!
//! limits:                                        # 全部可选，括号里是默认值
//!   total_deadline_ms: 120000                    # 一个请求（含所有切换）的总期限
//!   connect_timeout_ms: 5000
//!   response_head_timeout_ms: 30000              # 单次尝试：到收到响应头
//!   body_timeout_ms: 120000                      # 非流式：读完响应体
//!   stream_idle_timeout_ms: 30000                # 流式：两块之间最长静默
//!   stream_total_timeout_ms: 600000              # 流式：一条流最长持续多久（首字节之后）
//!   max_response_bytes: 4194304
//!   max_event_bytes: 1048576
//!   max_attempts: 3                              # 1..=4（契约上限 MAX_ATTEMPTS_PER_REQUEST）
//!
//! reserve:                                       # 预留额的硬上限（设计稿 §5）
//!   max_tokens_hard_cap: 4096
//!   min_useful_max_tokens: 16
//!
//! endpoints:                                     # 每个端点的地址、密钥来源、价目
//!   - provider: example-provider                 # 以下三项 = 许可表里的端点键 (provider, model, endpoint)
//!     model: example-org/chat-70b
//!     endpoint: openai-compat
//!     base_url: https://api.example.invalid/v1   # https；只有回环地址允许 http
//!     api_key_env: EXAMPLE_PROVIDER_API_KEY      # 或 api_key_file: /run/secrets/example-provider
//!     supplier_request_id_header: x-request-id   # 可选：供应商请求号也从这个头里找（正文里的 id 优先）
//!     extra_body: {provider: {zdr: true}}        # 可选：固定附加字段，盖在客户端字段之上
//!     price_version: pv-2026-11                  # 价格版本标签，随每次尝试记录，对账用
//!     cost:   {unit: USD, input_per_mtok: "0.26", output_per_mtok: "0.38", cached_input_per_mtok: "0.13"}
//!     prices:                                    # 对用户的价：按付款账户的单位各给一份
//!       - {unit: THB, input_per_mtok: "9.5", output_per_mtok: "14"}
//!
//! routes:                                        # 客户端用的模型名 -> 按偏好排好序的候选
//!   - model: idoris/agent
//!     candidates:
//!       - {provider: example-provider, model: example-org/chat-70b, endpoint: openai-compat}
//!
//! callers:                                       # 谁付款、数据处理要求（S1/S2/S5 之前的静态替身）
//!   - tenant_id: agent24                         # 虚拟 key 上的 tenant_id；不写 = 默认调用方
//!     credential_id: cred-agent24
//!     payer_account: acct-agent24
//!     unit: USD
//!     billing: paid                              # paid | subsidized | unbilled
//!     data_classes: [adult]                      # child_edu | adult | public
//!     allowed_regions: [sg-1]                    # 可选；不写 = 不限区域
//!     max_retention: with_exceptions             # zdr | with_exceptions | retained（含 child_edu 时固定 zdr）
//!     require_dpa: false
//! ```
//!
//! 价格是**十进制字符串**（每百万 token 多少单位，最多 6 位小数，精确换成微单位整数），不接受 YAML 浮点数：
//! 钱不能经过浮点。所有对象拒绝未知字段。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, AccountTree, Directory,
};
use idoris_compute_contracts::credential::{
    Credential, CredentialBinding, CredentialStatus, PaymentContext, resolve_payment_context,
};
use idoris_compute_contracts::ids::{
    AccountId, CredentialId, EndpointKey, PriceVersion, TimestampMs,
};
use idoris_compute_contracts::money::Unit;
use idoris_compute_contracts::permit::{
    Billing, DataClass, PermitTable, Region, Retention, TenantDataPolicy,
};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard};
use idoris_compute_contracts::request::MAX_ATTEMPTS_PER_REQUEST;
use idoris_compute_contracts::reserve::ReservePolicy;
use idoris_upstream::cloud::{
    CloudEndpointClient, CloudEndpointSettings, FileCredentialSource, build_http_client,
};
use idoris_upstream::remote::{CredentialSource, EnvCredentialSource};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use super::safety::config::{BuildInputs, SafetyConfig, SafetySetup};

/// 指向配置文件的环境变量。不设 = 云上游关闭。
pub const CONFIG_ENV: &str = "IDORIS_CLOUD_UPSTREAM_CONFIG";

/// 当前支持的配置 schema 版本。
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

fn default_total_deadline_ms() -> u64 {
    120_000
}
fn default_connect_timeout_ms() -> u64 {
    5_000
}
fn default_response_head_timeout_ms() -> u64 {
    30_000
}
fn default_body_timeout_ms() -> u64 {
    120_000
}
fn default_stream_idle_timeout_ms() -> u64 {
    30_000
}
fn default_stream_total_timeout_ms() -> u64 {
    600_000
}
fn default_max_response_bytes() -> usize {
    idoris_upstream::cloud::client::DEFAULT_MAX_RESPONSE_BYTES
}
fn default_max_event_bytes() -> usize {
    idoris_upstream::cloud::client::DEFAULT_MAX_EVENT_BYTES
}
fn default_max_attempts() -> usize {
    3
}
fn default_hard_cap() -> u64 {
    4_096
}
fn default_min_useful() -> u64 {
    16
}

/// 十进制字符串（价格）。只接受 YAML 字符串：浮点数会在这里被拒绝，不让钱经过浮点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decimal(pub String);

impl<'de> Deserialize<'de> for Decimal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DecimalVisitor;
        impl Visitor<'_> for DecimalVisitor {
            type Value = Decimal;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a decimal price written as a quoted string, e.g. \"0.26\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Decimal, E> {
                Ok(Decimal(v.to_string()))
            }
        }
        deserializer.deserialize_any(DecimalVisitor)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default = "default_total_deadline_ms")]
    pub total_deadline_ms: u64,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_response_head_timeout_ms")]
    pub response_head_timeout_ms: u64,
    #[serde(default = "default_body_timeout_ms")]
    pub body_timeout_ms: u64,
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    #[serde(default = "default_stream_total_timeout_ms")]
    pub stream_total_timeout_ms: u64,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default = "default_max_event_bytes")]
    pub max_event_bytes: usize,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            total_deadline_ms: default_total_deadline_ms(),
            connect_timeout_ms: default_connect_timeout_ms(),
            response_head_timeout_ms: default_response_head_timeout_ms(),
            body_timeout_ms: default_body_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
            stream_total_timeout_ms: default_stream_total_timeout_ms(),
            max_response_bytes: default_max_response_bytes(),
            max_event_bytes: default_max_event_bytes(),
            max_attempts: default_max_attempts(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveConfig {
    #[serde(default = "default_hard_cap")]
    pub max_tokens_hard_cap: u64,
    #[serde(default = "default_min_useful")]
    pub min_useful_max_tokens: u64,
}

impl Default for ReserveConfig {
    fn default() -> Self {
        Self {
            max_tokens_hard_cap: default_hard_cap(),
            min_useful_max_tokens: default_min_useful(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateConfig {
    pub unit: String,
    pub input_per_mtok: Decimal,
    pub output_per_mtok: Decimal,
    #[serde(default)]
    pub cached_input_per_mtok: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub supplier_request_id_header: Option<String>,
    #[serde(default)]
    pub extra_body: Map<String, Value>,
    pub price_version: String,
    pub cost: RateConfig,
    pub prices: Vec<RateConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointRef {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub model: String,
    pub candidates: Vec<EndpointRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallerConfig {
    #[serde(default)]
    pub tenant_id: Option<String>,
    pub credential_id: String,
    pub payer_account: String,
    pub unit: String,
    pub billing: Billing,
    pub data_classes: Vec<DataClass>,
    #[serde(default)]
    pub allowed_regions: Option<Vec<String>>,
    pub max_retention: Retention,
    #[serde(default)]
    pub require_dpa: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudConfigFile {
    pub schema_version: u32,
    pub permit_table: PathBuf,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub reserve: ReserveConfig,
    pub endpoints: Vec<EndpointConfig>,
    pub routes: Vec<RouteConfig>,
    pub callers: Vec<CallerConfig>,
    /// S6 儿童安全：有处理儿童数据的调用方（`data_classes` 含 `child_edu`）时必填，见 [`super::safety::config`]。
    #[serde(default)]
    pub safety: Option<SafetyConfig>,
}

// ---------------------------------------------------------------------------
// 运行时结构
// ---------------------------------------------------------------------------

/// 运行时限制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudLimits {
    pub total_deadline: Duration,
    pub response_head: Duration,
    pub body_total: Duration,
    pub stream_idle: Duration,
    pub stream_total: Duration,
    pub max_attempts: usize,
    pub max_event_bytes: usize,
}

impl Default for CloudLimits {
    fn default() -> Self {
        Self {
            total_deadline: Duration::from_millis(default_total_deadline_ms()),
            response_head: Duration::from_millis(default_response_head_timeout_ms()),
            body_total: Duration::from_millis(default_body_timeout_ms()),
            stream_idle: Duration::from_millis(default_stream_idle_timeout_ms()),
            stream_total: Duration::from_millis(default_stream_total_timeout_ms()),
            max_attempts: default_max_attempts(),
            max_event_bytes: default_max_event_bytes(),
        }
    }
}

/// 一个端点的运行时：传输客户端、价格版本、价目。
pub struct EndpointRuntime {
    pub key: EndpointKey,
    pub client: CloudEndpointClient,
    pub price_version: PriceVersion,
    pub cost_rates: RateCard<Cost>,
    /// 对用户的价，按付款账户的单位。
    pub price_rates: BTreeMap<Unit, RateCard<Price>>,
}

impl std::fmt::Debug for EndpointRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointRuntime")
            .field("key", &self.key.to_string())
            .field("client", &self.client)
            .field("price_version", &self.price_version)
            .finish_non_exhaustive()
    }
}

/// 全部端点。
#[derive(Debug, Default)]
pub struct EndpointRegistry(BTreeMap<EndpointKey, EndpointRuntime>);

impl EndpointRegistry {
    pub fn new(endpoints: impl IntoIterator<Item = EndpointRuntime>) -> Self {
        Self(endpoints.into_iter().map(|e| (e.key.clone(), e)).collect())
    }

    pub fn get(&self, key: &EndpointKey) -> Option<&EndpointRuntime> {
        self.0.get(key)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 客户端模型名 -> 候选（按偏好排好序）。
#[derive(Debug, Default, Clone)]
pub struct RouteBook(BTreeMap<String, Vec<EndpointKey>>);

impl RouteBook {
    pub fn new(routes: impl IntoIterator<Item = (String, Vec<EndpointKey>)>) -> Self {
        Self(routes.into_iter().collect())
    }

    pub fn candidates(&self, model: &str) -> Option<&[EndpointKey]> {
        self.0.get(model).map(Vec::as_slice)
    }
}

/// 一个调用方的付款上下文与数据处理要求。
#[derive(Debug, Clone)]
pub struct CallerPolicy {
    pub payment: PaymentContext,
    pub tenant: TenantDataPolicy,
    pub billing: Billing,
}

/// 调用方簿：按虚拟 key 上的 `tenant_id` 找。**有 tenant_id 但簿里没有的，拒绝**（不回落到默认调用方）；
/// 没有 tenant_id 的才用默认调用方（如果配置了）。
#[derive(Debug, Default, Clone)]
pub struct CallerBook {
    by_tenant: BTreeMap<String, CallerPolicy>,
    default: Option<CallerPolicy>,
}

impl CallerBook {
    pub fn new(by_tenant: BTreeMap<String, CallerPolicy>, default: Option<CallerPolicy>) -> Self {
        Self { by_tenant, default }
    }

    pub fn resolve(&self, tenant_id: Option<&str>) -> Option<&CallerPolicy> {
        match tenant_id {
            Some(tenant) => self.by_tenant.get(tenant),
            None => self.default.as_ref(),
        }
    }

    /// 是否有任何调用方处理儿童数据（`data_classes` 含 `child_edu`）。
    pub fn has_child_data_callers(&self) -> bool {
        self.by_tenant
            .values()
            .chain(self.default.iter())
            .any(|caller| caller.tenant.handles_child_data())
    }

    /// 处理儿童数据的调用方的租户标识，格式与 `ChatProxy` 的缓存键里的租户范围一致：
    /// 有租户的就是租户 id；默认调用方（没有租户）是 `"\u{0}personal"`。
    /// 用来关掉这些租户在旧代理路径上的响应缓存（S6：edu 关缓存）。
    pub fn child_data_tenant_scopes(&self) -> std::collections::BTreeSet<String> {
        let mut scopes: std::collections::BTreeSet<String> = self
            .by_tenant
            .iter()
            .filter(|(_, caller)| caller.tenant.handles_child_data())
            .map(|(tenant, _)| tenant.clone())
            .collect();
        if self
            .default
            .as_ref()
            .is_some_and(|caller| caller.tenant.handles_child_data())
        {
            scopes.insert("\u{0}personal".to_string());
        }
        scopes
    }
}

/// 加载好的全部部件。
pub struct CloudParts {
    pub permits: PermitTable,
    pub endpoints: EndpointRegistry,
    pub routes: RouteBook,
    pub callers: CallerBook,
    pub limits: CloudLimits,
    pub reserve_policy: ReservePolicy,
    /// S6：安全检查（检查器、上限、启动警告）。没有配置 `safety:` 段为 `None`（只有在没有儿童调用方时才可能）。
    pub safety: Option<SafetySetup>,
}

// ---------------------------------------------------------------------------
// 加载与校验
// ---------------------------------------------------------------------------

fn bad(message: impl Into<String>) -> String {
    format!("云上游配置：{}", message.into())
}

/// 十进制字符串 -> 微单位整数。至多 6 位小数，只有数字和一个小数点，不接受符号、指数、空串。
pub fn parse_micros(field: &str, text: &str) -> Result<i64, String> {
    let (int_part, frac_part) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if int_part.is_empty()
        || !digits(int_part)
        || !digits(frac_part)
        || frac_part.len() > 6
        || (text.contains('.') && frac_part.is_empty())
    {
        return Err(bad(format!(
            "{field} 必须是十进制字符串（最多 6 位小数），收到 {text:?}"
        )));
    }
    let overflow = || bad(format!("{field} 太大：{text:?}"));
    let whole: i64 = int_part.parse().map_err(|_| overflow())?;
    let frac: i64 = format!("{frac_part:0<6}").parse().map_err(|_| overflow())?;
    whole
        .checked_mul(1_000_000)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(overflow)
}

fn parse_unit(field: &str, text: &str) -> Result<Unit, String> {
    let unit: Unit = text.parse().map_err(|_| {
        bad(format!(
            "{field} 的单位 {text:?} 不认识（THB / USD / QUOTA:<节点>）"
        ))
    })?;
    Ok(unit)
}

pub(crate) fn rate_card_cost(
    field: &str,
    rate: &RateConfig,
) -> Result<(Unit, RateCard<Cost>), String> {
    let unit = parse_unit(field, &rate.unit)?;
    let make = |name: &str, text: &str| -> Result<Cost, String> {
        Cost::from_micros(
            unit.clone(),
            parse_micros(&format!("{field}.{name}"), text)?,
        )
        .map_err(|e| bad(format!("{field}.{name}: {e}")))
    };
    let card = RateCard::new(
        make("input_per_mtok", &rate.input_per_mtok.0)?,
        make("output_per_mtok", &rate.output_per_mtok.0)?,
        rate.cached_input_per_mtok
            .as_ref()
            .map(|c| make("cached_input_per_mtok", &c.0))
            .transpose()?,
    )
    .map_err(|e| bad(format!("{field}: {e}")))?;
    Ok((unit, card))
}

fn rate_card_price(field: &str, rate: &RateConfig) -> Result<(Unit, RateCard<Price>), String> {
    let unit = parse_unit(field, &rate.unit)?;
    let make = |name: &str, text: &str| -> Result<Price, String> {
        Price::from_micros(
            unit.clone(),
            parse_micros(&format!("{field}.{name}"), text)?,
        )
        .map_err(|e| bad(format!("{field}.{name}: {e}")))
    };
    let card = RateCard::new(
        make("input_per_mtok", &rate.input_per_mtok.0)?,
        make("output_per_mtok", &rate.output_per_mtok.0)?,
        rate.cached_input_per_mtok
            .as_ref()
            .map(|c| make("cached_input_per_mtok", &c.0))
            .transpose()?,
    )
    .map_err(|e| bad(format!("{field}: {e}")))?;
    Ok((unit, card))
}

pub(crate) fn key_of(provider: &str, model: &str, endpoint: &str) -> Result<EndpointKey, String> {
    EndpointKey::new(provider, model, endpoint)
        .map_err(|e| bad(format!("端点 {provider}/{model}@{endpoint}: {e}")))
}

pub(crate) fn build_credentials(
    config: &EndpointConfig,
) -> Result<Arc<dyn CredentialSource>, String> {
    match (&config.api_key_env, &config.api_key_file) {
        (Some(env), None) if !env.trim().is_empty() => Ok(Arc::new(EnvCredentialSource::new(env))),
        (None, Some(path)) => Ok(Arc::new(FileCredentialSource::new(path))),
        _ => Err(bad(format!(
            "端点 {}/{}@{} 必须且只能给 api_key_env 或 api_key_file 之一",
            config.provider, config.model, config.endpoint
        ))),
    }
}

/// 用静态配置里的调用方，经 S0 的 `resolve_payment_context` 得到付款上下文：账户树只有「平台 -> 该组织账户」，
/// 凭证是绑定到该组织账户的节点凭证（`CredentialBinding::Node`）。付款上下文只能由契约的这个入口产生，
/// 这里不绕过它。S1/S2/S5 落地后换成账户库里真实的账户、凭证与成员关系。
fn build_caller(config: &CallerConfig, now: TimestampMs) -> Result<CallerPolicy, String> {
    let unit = parse_unit("callers.unit", &config.unit)?;
    if !unit.is_currency() {
        return Err(bad(
            "云形态的付款单位必须是货币（THB / USD），QUOTA 是组织形态的配额",
        ));
    }
    let wrap = |what: &str, e: &dyn std::fmt::Display| bad(format!("调用方 {what}: {e}"));
    let payer = AccountId::new(&config.payer_account).map_err(|e| wrap("payer_account", &e))?;
    let platform = AccountId::new("platform").map_err(|e| wrap("platform", &e))?;
    if payer == platform {
        return Err(bad("payer_account 不能叫 platform（保留给平台根账户）"));
    }
    let tree = AccountTree::build(vec![
        Account {
            id: platform.clone(),
            kind: AccountKind::Platform,
            parent: None,
            unit: unit.clone(),
            status: AccountStatus::Active,
        },
        Account {
            id: payer.clone(),
            kind: AccountKind::Org,
            parent: Some(platform),
            unit,
            status: AccountStatus::Active,
        },
    ])
    .map_err(|e| wrap("账户树", &e))?;
    let directory =
        Directory::build(tree, Vec::new(), Vec::new()).map_err(|e| wrap("账户目录", &e))?;
    let credential = Credential::new(
        CredentialId::new(&config.credential_id).map_err(|e| wrap("credential_id", &e))?,
        CredentialBinding::Node { account: payer },
        CredentialStatus::Active,
        None,
        Vec::new(),
    )
    .map_err(|e| wrap("凭证", &e))?;
    let payment = resolve_payment_context(&directory, &credential, None, now)
        .map_err(|e| wrap("付款上下文", &e))?;

    let regions = config
        .allowed_regions
        .as_ref()
        .map(|list| {
            list.iter()
                .map(|r| Region::new(r.clone()).map_err(|e| wrap("allowed_regions", &e)))
                .collect::<Result<BTreeSet<_>, _>>()
        })
        .transpose()?;
    let tenant = TenantDataPolicy::new(
        config.data_classes.iter().copied().collect(),
        regions,
        config.max_retention,
        config.require_dpa,
    )
    .map_err(|e| wrap("数据处理要求", &e))?;
    Ok(CallerPolicy {
        payment,
        tenant,
        billing: config.billing,
    })
}

/// 从 YAML 文本构造全部部件。`base_dir` 用来解析相对路径（许可表）；`now` 用来解析静态付款上下文。
pub fn parse(text: &str, base_dir: &Path, now: TimestampMs) -> Result<CloudParts, String> {
    let file: CloudConfigFile =
        serde_yaml::from_str(text).map_err(|e| bad(format!("无法解析 YAML：{e}")))?;
    if file.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(bad(format!(
            "不支持的 schema_version {}（支持 {CONFIG_SCHEMA_VERSION}）",
            file.schema_version
        )));
    }

    // 许可表
    let table_path = if file.permit_table.is_absolute() {
        file.permit_table.clone()
    } else {
        base_dir.join(&file.permit_table)
    };
    let table_text = std::fs::read_to_string(&table_path)
        .map_err(|e| bad(format!("无法读取端点许可表 {}：{e}", table_path.display())))?;
    let permits = PermitTable::from_yaml_str(&table_text)
        .map_err(|e| bad(format!("端点许可表 {} 无效：{e}", table_path.display())))?;

    // 限制
    let limits = &file.limits;
    if limits.max_attempts == 0 || limits.max_attempts > MAX_ATTEMPTS_PER_REQUEST {
        return Err(bad(format!(
            "limits.max_attempts 必须在 1..={MAX_ATTEMPTS_PER_REQUEST}"
        )));
    }
    for (name, value) in [
        ("total_deadline_ms", limits.total_deadline_ms),
        ("connect_timeout_ms", limits.connect_timeout_ms),
        ("response_head_timeout_ms", limits.response_head_timeout_ms),
        ("body_timeout_ms", limits.body_timeout_ms),
        ("stream_idle_timeout_ms", limits.stream_idle_timeout_ms),
        ("stream_total_timeout_ms", limits.stream_total_timeout_ms),
    ] {
        if value == 0 {
            return Err(bad(format!("limits.{name} 必须大于 0")));
        }
    }
    if limits.connect_timeout_ms > limits.response_head_timeout_ms {
        return Err(bad(
            "limits.connect_timeout_ms 不能大于 response_head_timeout_ms（连接时间算在响应头期限里）",
        ));
    }
    if limits.max_response_bytes == 0 || limits.max_event_bytes == 0 {
        return Err(bad(
            "limits.max_response_bytes / max_event_bytes 必须大于 0",
        ));
    }
    let reserve_policy = ReservePolicy::new(
        file.reserve.max_tokens_hard_cap,
        file.reserve.min_useful_max_tokens,
    )
    .map_err(|e| bad(format!("reserve: {e}")))?;
    let http = build_http_client(Duration::from_millis(limits.connect_timeout_ms))
        .map_err(|e| bad(e.to_string()))?;

    // 端点
    let mut runtimes = Vec::new();
    let mut seen = BTreeSet::new();
    for endpoint in &file.endpoints {
        let key = key_of(&endpoint.provider, &endpoint.model, &endpoint.endpoint)?;
        if !seen.insert(key.clone()) {
            return Err(bad(format!("端点 {key} 配置了两次")));
        }
        if permits.lookup(&key).is_none() {
            return Err(bad(format!(
                "端点 {key} 不在端点许可表里：表外的端点永远不会被选中，这几乎一定是拼写错误"
            )));
        }
        let mut settings =
            CloudEndpointSettings::new(&endpoint.base_url, endpoint.provider.clone());
        if let Some(header) = &endpoint.supplier_request_id_header {
            settings.supplier_request_id_header = header.to_ascii_lowercase();
        }
        settings.extra_body = endpoint.extra_body.clone();
        settings.max_response_bytes = limits.max_response_bytes;
        let client = CloudEndpointClient::new(settings, build_credentials(endpoint)?, http.clone())
            .map_err(|e| bad(format!("端点 {key}: {e}")))?;
        let (_, cost_rates) = rate_card_cost(&format!("端点 {key} 的 cost"), &endpoint.cost)?;
        let mut price_rates = BTreeMap::new();
        for rate in &endpoint.prices {
            let (unit, card) = rate_card_price(&format!("端点 {key} 的 prices"), rate)?;
            if price_rates.insert(unit.clone(), card).is_some() {
                return Err(bad(format!(
                    "端点 {key} 的 prices 里单位 {unit} 出现了两次"
                )));
            }
        }
        runtimes.push(EndpointRuntime {
            key,
            client,
            price_version: PriceVersion::new(&endpoint.price_version)
                .map_err(|e| bad(format!("price_version: {e}")))?,
            cost_rates,
            price_rates,
        });
    }

    // 路由
    let mut routes = Vec::new();
    let mut seen_models = BTreeSet::new();
    for route in &file.routes {
        if route.model.trim().is_empty() || !seen_models.insert(route.model.clone()) {
            return Err(bad(format!("路由模型名 {:?} 为空或重复", route.model)));
        }
        if route.candidates.is_empty() {
            return Err(bad(format!("路由 {} 没有候选", route.model)));
        }
        let mut keys = Vec::new();
        for candidate in &route.candidates {
            let key = key_of(&candidate.provider, &candidate.model, &candidate.endpoint)?;
            if !seen.contains(&key) {
                return Err(bad(format!(
                    "路由 {} 引用了没有配置的端点 {key}",
                    route.model
                )));
            }
            keys.push(key);
        }
        routes.push((route.model.clone(), keys));
    }

    // 调用方
    let mut by_tenant = BTreeMap::new();
    let mut default = None;
    for caller in &file.callers {
        let policy = build_caller(caller, now)?;
        match &caller.tenant_id {
            Some(tenant) => {
                if by_tenant.insert(tenant.clone(), policy).is_some() {
                    return Err(bad(format!("调用方 tenant_id {tenant:?} 出现了两次")));
                }
            }
            None => {
                if default.replace(policy).is_some() {
                    return Err(bad("最多只能有一个默认调用方（不写 tenant_id 的）"));
                }
            }
        }
    }

    // 安全检查（S6）：有儿童调用方就必须配置；LLM 检查端点要用许可表校验，不合规启动失败
    let child_policies: Vec<&TenantDataPolicy> = by_tenant
        .values()
        .chain(default.iter())
        .map(|caller: &CallerPolicy| &caller.tenant)
        .filter(|tenant| tenant.handles_child_data())
        .collect();
    let safety = match &file.safety {
        Some(config) => Some(super::safety::config::build(
            config,
            &BuildInputs {
                permits: &Arc::new(permits.clone()),
                endpoints: &file.endpoints,
                http: &http,
                child_policies: child_policies.clone(),
                now,
            },
        )?),
        None if !child_policies.is_empty() => {
            return Err(bad(
                "有调用方处理儿童数据（data_classes 含 child_edu），必须配置 safety: 段（安全检查是儿童业务上线的前提，设计稿 §7）",
            ));
        }
        None => None,
    };

    Ok(CloudParts {
        permits,
        endpoints: EndpointRegistry::new(runtimes),
        routes: RouteBook::new(routes),
        callers: CallerBook::new(by_tenant, default),
        safety,
        limits: CloudLimits {
            total_deadline: Duration::from_millis(limits.total_deadline_ms),
            response_head: Duration::from_millis(limits.response_head_timeout_ms),
            body_total: Duration::from_millis(limits.body_timeout_ms),
            stream_idle: Duration::from_millis(limits.stream_idle_timeout_ms),
            stream_total: Duration::from_millis(limits.stream_total_timeout_ms),
            max_attempts: limits.max_attempts,
            max_event_bytes: limits.max_event_bytes,
        },
        reserve_policy,
    })
}

/// 从文件加载。
pub fn load(path: &Path, now: TimestampMs) -> Result<CloudParts, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| bad(format!("无法读取 {}：{e}", path.display())))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    parse(&text, base, now)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn decimal_prices_become_exact_micros() {
        for (text, micros) in [
            ("0", 0),
            ("0.26", 260_000),
            ("12", 12_000_000),
            ("0.000001", 1),
            ("1.5", 1_500_000),
            ("0.380000", 380_000),
            ("9223372036854.775807", 9_223_372_036_854_775_807),
        ] {
            assert_eq!(parse_micros("f", text).unwrap(), micros, "{text}");
        }
        for text in [
            "",
            ".5",
            "1.",
            "-1",
            "+1",
            "1e3",
            "0.0000001",
            "1,5",
            " 1",
            "1 ",
            "0x10",
            "1.2.3",
            "９",
            "9223372036855",
            "99999999999999999999",
        ] {
            assert!(
                parse_micros("f", text).is_err(),
                "{text:?} must be rejected"
            );
        }
    }

    const PERMITS: &str = "schema_version: 1
verification_max_age_days: 90
endpoints:
  - provider: example-alpha
    model: example-org/chat-70b
    endpoint: openai-compat
    access: api
    processing_regions: [sg-1]
    retention: with_exceptions
    dpa: unsigned
    allowed_data_classes: [adult]
    capabilities: {tool_calling: true, stream_usage: true, cache_pricing: false, structured_output: true, cancellation: true}
    verified_at: \"2026-11-12\"
";

    const VALID: &str = "schema_version: 1
permit_table: permits.yaml
endpoints:
  - provider: example-alpha
    model: example-org/chat-70b
    endpoint: openai-compat
    base_url: https://api.example.invalid/v1
    api_key_env: EXAMPLE_ALPHA_API_KEY
    price_version: pv-1
    cost: {unit: USD, input_per_mtok: \"0.26\", output_per_mtok: \"0.38\"}
    prices:
      - {unit: THB, input_per_mtok: \"10\", output_per_mtok: \"40\"}
routes:
  - model: idoris/agent
    candidates:
      - {provider: example-alpha, model: example-org/chat-70b, endpoint: openai-compat}
callers:
  - tenant_id: agent24
    credential_id: cred-agent24
    payer_account: acct-agent24
    unit: THB
    billing: paid
    data_classes: [adult]
    max_retention: with_exceptions
  - credential_id: cred-default
    payer_account: acct-default
    unit: THB
    billing: paid
    data_classes: [adult]
    max_retention: with_exceptions
";

    fn load(yaml: &str) -> Result<CloudParts, String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("permits.yaml"), PERMITS).unwrap();
        parse(yaml, dir.path(), TimestampMs(1_794_528_000_000))
    }

    fn expect_error(yaml: &str, needle: &str) {
        let error = match load(yaml) {
            Ok(_) => panic!("expected an error containing {needle:?}"),
            Err(error) => error,
        };
        assert!(error.contains(needle), "{needle:?} not in {error}");
        // 配置里没有密钥，错误里也不会有
        assert!(!error.contains("sk-"));
    }

    #[test]
    fn a_valid_config_loads_every_part() {
        let parts = load(VALID).unwrap();
        assert_eq!(parts.endpoints.len(), 1);
        let key =
            EndpointKey::new("example-alpha", "example-org/chat-70b", "openai-compat").unwrap();
        let endpoint = parts.endpoints.get(&key).unwrap();
        assert_eq!(endpoint.price_version.as_str(), "pv-1");
        assert_eq!(endpoint.cost_rates.input_per_mtok().micros(), 260_000);
        assert_eq!(
            endpoint.price_rates[&Unit::Thb].output_per_mtok().micros(),
            40_000_000
        );
        assert_eq!(parts.routes.candidates("idoris/agent").unwrap(), &[key]);
        assert!(parts.routes.candidates("idoris/other").is_none());
        // 有 tenant_id 的找得到；没有 tenant_id 的用默认；认识的 tenant_id 之外的不回落到默认
        let tenant = parts.callers.resolve(Some("agent24")).unwrap();
        assert_eq!(tenant.payment.payer().as_str(), "acct-agent24");
        assert_eq!(
            parts
                .callers
                .resolve(None)
                .unwrap()
                .payment
                .payer()
                .as_str(),
            "acct-default"
        );
        assert!(parts.callers.resolve(Some("stranger")).is_none());
        assert_eq!(parts.limits.max_attempts, 3);
        assert_eq!(parts.reserve_policy.max_tokens_hard_cap, 4_096);
    }

    #[test]
    fn broken_configs_are_refused_with_a_reason() {
        expect_error(
            &VALID.replace("schema_version: 1", "schema_version: 2"),
            "schema_version",
        );
        // 未知字段
        expect_error(&format!("{VALID}surprise: true\n"), "unknown field");
        expect_error(
            &VALID.replace(
                "price_version: pv-1",
                "price_version: pv-1\n    api_key: sk-inline-secret",
            ),
            "unknown field",
        );
        // 端点不在许可表里（几乎一定是拼写错误）
        expect_error(&VALID.replace("provider: example-alpha\n    model: example-org/chat-70b\n    endpoint: openai-compat\n    base_url", "provider: example-typo\n    model: example-org/chat-70b\n    endpoint: openai-compat\n    base_url"), "不在端点许可表里");
        // 路由引用了没有配置的端点
        expect_error(
            &VALID.replace(
                "candidates:\n      - {provider: example-alpha",
                "candidates:\n      - {provider: example-nope",
            ),
            "没有配置的端点",
        );
        // 密钥来源：必须且只能有一个
        expect_error(
            &VALID.replace("    api_key_env: EXAMPLE_ALPHA_API_KEY\n", ""),
            "api_key_env 或 api_key_file",
        );
        expect_error(
            &VALID.replace(
                "api_key_env: EXAMPLE_ALPHA_API_KEY",
                "api_key_env: A\n    api_key_file: /tmp/k",
            ),
            "api_key_env 或 api_key_file",
        );
        // 明文 http、带凭证的 URL
        expect_error(
            &VALID.replace(
                "https://api.example.invalid/v1",
                "http://api.example.invalid/v1",
            ),
            "https",
        );
        expect_error(
            &VALID.replace(
                "https://api.example.invalid/v1",
                "https://user:pw@api.example.invalid/v1",
            ),
            "credentials",
        );
        // 价格：浮点、负数、多于 6 位小数
        expect_error(
            &VALID.replace("input_per_mtok: \"0.26\"", "input_per_mtok: 0.26"),
            "quoted string",
        );
        expect_error(
            &VALID.replace("input_per_mtok: \"0.26\"", "input_per_mtok: \"-1\""),
            "十进制",
        );
        expect_error(
            &VALID.replace("input_per_mtok: \"0.26\"", "input_per_mtok: \"0.1234567\""),
            "6 位小数",
        );
        // 重复的调用方、两个默认调用方、QUOTA 单位
        expect_error(
            &VALID.replace(
                "  - credential_id: cred-default",
                "  - tenant_id: agent24\n    credential_id: cred-default",
            ),
            "出现了两次",
        );
        expect_error(
            &VALID.replace(
                "  - tenant_id: agent24\n    credential_id: cred-agent24",
                "  - credential_id: cred-agent24",
            ),
            "最多只能有一个默认调用方",
        );
        expect_error(&VALID.replace("unit: THB\n    billing: paid\n    data_classes: [adult]\n    max_retention: with_exceptions\n  - credential_id", "unit: QUOTA:node-1\n    billing: paid\n    data_classes: [adult]\n    max_retention: with_exceptions\n  - credential_id"), "QUOTA");
        // 限制
        expect_error(
            &format!("{VALID}limits:\n  max_attempts: 0\n"),
            "max_attempts",
        );
        expect_error(
            &format!("{VALID}limits:\n  max_attempts: 5\n"),
            "max_attempts",
        );
        expect_error(
            &format!(
                "{VALID}limits:\n  connect_timeout_ms: 9000\n  response_head_timeout_ms: 1000\n"
            ),
            "connect_timeout_ms",
        );
        expect_error(
            &format!("{VALID}limits:\n  stream_idle_timeout_ms: 0\n"),
            "必须大于 0",
        );
    }

    #[test]
    fn a_missing_or_broken_permit_table_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = match parse(VALID, dir.path(), TimestampMs(0)) {
            Ok(_) => panic!("no permit table"),
            Err(error) => error,
        };
        assert!(error.contains("无法读取端点许可表"), "{error}");
        std::fs::write(
            dir.path().join("permits.yaml"),
            "schema_version: 1\nendpoints: []\n",
        )
        .unwrap();
        let error = match parse(VALID, dir.path(), TimestampMs(0)) {
            Ok(_) => panic!("broken permit table"),
            Err(error) => error,
        };
        assert!(error.contains("端点许可表"), "{error}");
    }

    #[test]
    fn a_yaml_float_is_not_a_price() {
        let yaml = "unit: USD\ninput_per_mtok: 0.26\noutput_per_mtok: \"0.38\"\n";
        let err = serde_yaml::from_str::<RateConfig>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("quoted string"), "{err}");
        let ok = serde_yaml::from_str::<RateConfig>(
            "unit: USD\ninput_per_mtok: \"0.26\"\noutput_per_mtok: \"0.38\"\n",
        )
        .unwrap();
        assert_eq!(ok.input_per_mtok.0, "0.26");
    }
}

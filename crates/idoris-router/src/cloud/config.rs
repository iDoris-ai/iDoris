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
//!     extra_body: {provider: {zdr: true}}        # 可选：固定附加字段，盖在客户端字段之上；不能含 model / max_tokens / stream / n / messages / tools / user / store 等协议、计费、身份、留存字段（加载时拒绝）
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
//!     billing: paid                              # paid | subsidized（unbilled 没有实现，加载时拒绝）
//!     data_classes: [adult]                      # child_edu | adult | public
//!     allowed_regions: [sg-1]                    # 可选；不写 = 不限区域
//!     max_retention: with_exceptions             # zdr | with_exceptions | retained（含 child_edu 时固定 zdr）
//!     require_dpa: false
//! ```
//!
//! 价格是**十进制字符串**（每百万 token 多少单位，最多 6 位小数，精确换成微单位整数），不接受 YAML 浮点数：
//! 钱不能经过浮点。所有对象拒绝未知字段。
//!
//! `safety:` 段的字段和示例在 [`super::safety::config`]。有儿童租户时生产启动默认要求 LLM 检查器；
//! `rules_only` 必须另写 `rules_only_acknowledgement`（原因、确认人、UTC 到期日，且不超过今天起 90 天），否则启动失败。
//! 没有儿童租户时，`rules_only` 不需要这份确认。

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
    reserved_extra_body_field,
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
    /// 静态调用方簿：S4 的测试夹具。**生产（云入口生效时）必须是空的**，付款上下文由凭证解析（S5，见
    /// `ingress::caller`）；所以这个键可以不写。
    #[serde(default)]
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

    /// 全部客户端模型名（按名字排序）：`GET /api/v1/models` 列的就是它们。
    pub fn models(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
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
///
/// **S5 之后这只是测试夹具**：云入口（`ingress`）生效时，付款上下文、租户、数据类别全部从凭证推导，
/// 处理函数读请求扩展里的 [`CallerPolicy`]，不看这本簿；`CloudIngress::new` 拒绝启动一个簿不为空的云上游，
/// 所以生产里它永远是空的。S4 / S3 的端到端测试和 S6 的夹具继续用它来隔离「付款上下文」以外的行为。
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

    /// 簿里一个调用方都没有（生产的云入口要求这样）。
    pub fn is_empty(&self) -> bool {
        self.by_tenant.is_empty() && self.default.is_none()
    }

    /// 是否有任何调用方处理儿童数据（`data_classes` 含 `child_edu`）。
    pub fn has_child_data_callers(&self) -> bool {
        self.by_tenant
            .values()
            .chain(self.default.iter())
            .any(|caller| caller.tenant.handles_child_data())
    }

    /// 启动日志用：(处理儿童数据的调用方, 其它调用方)。只有租户标识，没有任何内容；默认调用方写成 `(default)`。
    pub fn caller_summary(&self) -> (Vec<String>, Vec<String>) {
        let (mut child, mut other) = (Vec::new(), Vec::new());
        let named = self
            .by_tenant
            .iter()
            .map(|(tenant, caller)| (tenant.as_str(), caller));
        let default = self.default.iter().map(|caller| ("(default)", caller));
        for (name, caller) in named.chain(default) {
            if caller.tenant.handles_child_data() {
                child.push(name.to_string());
            } else {
                other.push(name.to_string());
            }
        }
        (child, other)
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

/// 端点许可表里**已有行的供应商**，和它们的 API 主机（`config/compute/endpoint-urls.md`，查询日期 2026-11-12；`*.` 开头的
/// 条目匹配任何子域名）。
///
/// 为什么：许可表（区域、留存、DPA、允许的数据类别）核验的是**某个供应商的某个端点**，而云上游配置里的 `base_url` 是运维
/// 手写的，两边之间原来没有任何绑定：配置里把 `deepinfra` 这一行的 `base_url` 写成别的主机，孩子的数据就会带着
/// 「已核验」的标签发往一个没人核验过的主机。现在这些供应商的 `base_url` 主机必须在表里。
///
/// 范围（有意收窄，需要 owner 确认更彻底的做法）：
/// - 回环地址永远允许（本机测试用的假上游）；
/// - 表里**没有**的供应商标签（测试里的 `example-*`、以后新增的行）不受限：这里只绑定今天已经核验过的行；
/// - 完整 URL / 主机**没核实**的供应商（`together`、`google-vertex`、`siliconflow`、`moonshot`）：主机列表是空的，
///   这些行不能配置到任何非回环地址（草案没给出可核对的主机，不补一个猜出来的）。
const VERIFIED_PROVIDER_HOSTS: &[(&str, &[&str])] = &[
    ("deepinfra", &["api.deepinfra.com"]),
    ("openrouter", &["openrouter.ai"]),
    ("deepseek-official", &["api.deepseek.com"]),
    ("groq", &["api.groq.com"]),
    ("opentyphoon", &["api.opentyphoon.ai"]),
    ("fireworks", &["api.fireworks.ai", "us.api.fireworks.ai"]),
    (
        "alibaba-model-studio-intl",
        &["*.ap-southeast-1.maas.aliyuncs.com"],
    ),
    ("together", &[]),
    ("google-vertex", &[]),
    ("siliconflow", &[]),
    ("moonshot", &[]),
];

fn host_is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn host_matches(host: &str, pattern: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => {
            host.len() > suffix.len() + 1
                && host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
        }
        None => host.eq_ignore_ascii_case(pattern),
    }
}

/// 许可表里已核验的供应商，`base_url` 的主机必须是它核验过的 API 主机（见 [`VERIFIED_PROVIDER_HOSTS`]）。
pub(crate) fn check_provider_host(key: &EndpointKey, base_url: &str) -> Result<(), String> {
    // URL 本身不合法由客户端的构造函数报（那边的错误更具体）
    let Ok(url) = idoris_upstream::cloud::validate_base_url(base_url) else {
        return Ok(());
    };
    let Some(host) = url.host_str() else {
        return Ok(());
    };
    if host_is_loopback(host) {
        return Ok(());
    }
    let Some((_, hosts)) = VERIFIED_PROVIDER_HOSTS
        .iter()
        .find(|(provider, _)| *provider == key.provider.as_str())
    else {
        return Ok(());
    };
    if hosts.iter().any(|pattern| host_matches(host, pattern)) {
        return Ok(());
    }
    Err(bad(format!(
        "端点 {key} 的 base_url 主机 {host} 不是许可表为供应商 {} 核验过的 API 主机（{}）：\
         许可表核验的是这个供应商的端点，数据不能带着「已核验」的标签发往别的主机",
        key.provider,
        if hosts.is_empty() {
            "这个供应商的主机没有核实，不能配置到非回环地址".to_string()
        } else {
            hosts.join(", ")
        }
    )))
}

/// `billing: unbilled`（不计费）在云形态**没有实现**，加载配置时直接拒绝（失败即关闭）。
///
/// 准入账本、预留、结算都不认这个计费性质：只有许可表筛选和请求记录里会看到它。配置成 `unbilled` 的租户，请求照样
/// 预留、照样向用户记 `user_price`、照样扣余额；流式请求还会因此免掉「必须有流式 usage」的端点约束，
/// 结算时按整笔预留收费。所以与其让一个名字叫「不计费」的配置悄悄收钱，不如启动失败。
/// 要免费请用 `subsidized`（补贴池出钱，设计稿 §4）。
pub(crate) fn reject_unbilled(what: &str, billing: Billing) -> Result<(), String> {
    if billing == Billing::Unbilled {
        return Err(bad(format!(
            "{what}: unbilled 在云形态没有实现（准入、预留和结算都不认它：用户额度照扣）；要免费请用 subsidized"
        )));
    }
    Ok(())
}

/// 用静态配置里的调用方，经 S0 的 `resolve_payment_context` 得到付款上下文：账户树只有「平台 -> 该组织账户」，
/// 凭证是绑定到该组织账户的节点凭证（`CredentialBinding::Node`）。付款上下文只能由契约的这个入口产生，
/// 这里不绕过它。S1/S2/S5 落地后换成账户库里真实的账户、凭证与成员关系。
fn build_caller(config: &CallerConfig, now: TimestampMs) -> Result<CallerPolicy, String> {
    reject_unbilled("callers.billing", config.billing)?;
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

/// 云入口（S5）配置里声明的租户和它的数据处理要求。它们不在静态调用方簿里（生产里簿是空的），但调用方的数据处理要求
/// 同样决定走不走儿童安全管线（`handles_child_data()`），所以加载云上游配置时要把它们和簿里的调用方**一起**算进
/// S6 的启动校验：其中有儿童租户就必须有 `safety:` 段，LLM 检查端点要满足它们每一个的区域 / 留存 / DPA 要求，
/// 只用规则检查器要留警告；启动日志也按「走 / 不走儿童管线」把它们列出来，运营方一眼能核对。
#[derive(Debug, Clone)]
pub struct IngressTenant {
    /// 租户的账户 id（只用来写进错误和日志，没有任何内容）。
    pub name: String,
    pub policy: TenantDataPolicy,
}

/// 从 YAML 文本构造全部部件。`base_dir` 用来解析相对路径（许可表）；`now` 用来解析静态付款上下文。
pub fn parse(text: &str, base_dir: &Path, now: TimestampMs) -> Result<CloudParts, String> {
    parse_with_ingress_tenants(text, base_dir, now, &[])
}

/// 同 [`parse`]，另外把云入口配置里声明的租户算进 S6 的启动校验（见 [`IngressTenant`]）。
pub fn parse_with_ingress_tenants(
    text: &str,
    base_dir: &Path,
    now: TimestampMs,
    ingress_tenants: &[IngressTenant],
) -> Result<CloudParts, String> {
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
        // 许可表核验的是这个供应商的端点：`base_url` 的主机必须是它核验过的 API 主机（见 `VERIFIED_PROVIDER_HOSTS`）
        check_provider_host(&key, &endpoint.base_url)?;
        // `extra_body` 盖在客户端字段之上，但不能碰协议 / 计费 / 身份 / 留存字段：准入与许可检查是按「这个模型、
        // 这个 `max_tokens`」算好的，被它改掉就绕过了许可表和预留（见 `idoris_upstream::cloud::wire`）。
        if let Some(field) = reserved_extra_body_field(&endpoint.extra_body) {
            return Err(bad(format!(
                "端点 {key} 的 extra_body 不能包含 {field}：model、max_tokens、stream、n、messages、tools、user、store 一类\
                 协议 / 计费 / 身份 / 留存字段由请求自己决定（准入时算好的值），extra_body 只用来放供应商自己的路由或策略参数\
                 （比如 provider: {{zdr: true}}）"
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
        .chain(
            ingress_tenants
                .iter()
                .map(|tenant| &tenant.policy)
                .filter(|policy| policy.handles_child_data()),
        )
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
                "有调用方（含云入口配置里的租户）处理儿童数据（data_classes 含 child_edu），必须配置 safety: 段（安全检查是儿童业务上线的前提，设计稿 §7）",
            ));
        }
        None => None,
    };

    // 安全检查（LLM）端点要对每个儿童调用方的付款单位有价目：检查的成本要折算成付款单位去占额度
    // （`safety::LedgerExtraCost`：检查端点的 `prices` × 检查器给出的 token 上界），没有价目就没法计价，
    // 运行时只能失败即关闭——不如启动时就拒绝这种配置，和「LLM 检查端点必须在许可表里」是同一类校验。
    if let Some(judge) = safety.as_ref().and_then(|setup| setup.judge.as_ref()) {
        let judge_runtime = runtimes
            .iter()
            .find(|runtime| runtime.key == *judge.endpoint());
        let named = by_tenant
            .iter()
            .map(|(tenant, caller)| (tenant.as_str(), caller));
        let unnamed = default.iter().map(|caller| ("(default)", caller));
        for (name, caller) in named.chain(unnamed) {
            if !caller.tenant.handles_child_data() {
                continue;
            }
            let unit = caller.payment.unit();
            if !judge_runtime.is_some_and(|runtime| runtime.price_rates.contains_key(unit)) {
                return Err(bad(format!(
                    "安全检查端点 {} 没有配置付款单位 {unit} 的价目（prices）：调用方 {name} 的检查成本无法折算成付款单位占额度",
                    judge.endpoint()
                )));
            }
        }
    }

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

/// **生产启动的额外要求**（owner 2026-11-13 方案 B）：有处理儿童数据的调用方 / 租户（静态调用方簿里的，或云入口
/// 配置里声明的）就必须配 LLM 安全检查器（`safety.mode: rules_and_llm`）。只写 `rules_only` 启动失败，除非同一段里有一份
/// **此刻仍然有效**的 `rules_only_acknowledgement`（原因、确认人、UTC 到期日，且到期日不晚于 `now` 起 90 天）。
///
/// 为什么：规则检查器是关键词 / 正则，换个字体（数学粗体字母）、西里尔字母的 o、拉长拼写（`boooomb`）、base64、
/// 藏在工具描述里的有害内容都能绕过输入规则；儿童数据不该把这道防线只押在它上面。显式放宽是有期限的例外，过期就回到默认。
///
/// `now` 和加载配置时用的是同一个时钟读数（生产装配先取一次 [`crate::cloud::Clock::now`]）。只看「写了这个块」不够：
/// 到期日相对 `now` 已经无效，仍然拒绝。
///
/// 不放进 [`parse_with_ingress_tenants`]：它是各种测试夹具共用的解析入口（很多测试用 `rules_only` 单独验证规则检查器的
/// 行为，不需要 LLM 检查器）；生产装配 [`crate::cloud::CloudUpstream::from_file_with_ingress_tenants`] 加载之后调用它。
/// 没有儿童租户时，`rules_only` 不需要这份确认。
pub fn require_llm_judge_for_child_tenants(
    parts: &CloudParts,
    ingress_tenants: &[IngressTenant],
    now: TimestampMs,
) -> Result<(), String> {
    let has_child = parts.callers.has_child_data_callers()
        || ingress_tenants
            .iter()
            .any(|tenant| tenant.policy.handles_child_data());
    if !has_child {
        return Ok(());
    }
    let setup = parts.safety.as_ref();
    if setup.is_some_and(|safety| safety.judge.is_some()) {
        return Ok(());
    }
    if let Some(ack) = setup.and_then(|safety| safety.acknowledgement.as_ref()) {
        return ack.ensure_current(now).map_err(|error| {
            bad(format!(
                "儿童租户的 rules_only 显式放宽已失效（{error}）。要继续只用规则检查器，请更新 reason、acknowledged_by 和 expires_on；否则改为 mode: rules_and_llm 并配置 llm_judge"
            ))
        });
    }
    Err(bad(
        "有调用方（含云入口配置里的租户）处理儿童数据（data_classes 含 child_edu），safety: 段必须配置 LLM 检查器\
         （mode: rules_and_llm + llm_judge），不能只用规则检查器（rules_only）：规则检查器靠关键词和正则，\
         换字体、拼写变形、base64、藏在工具描述里的内容都能绕过。若暂时只能用规则检查器，必须在 \
         safety.rules_only_acknowledgement 里显式放宽，三项都要写：reason（去掉首尾空白后至少 10 个字符）、\
         acknowledged_by（非空）、expires_on（YYYY-MM-DD，UTC，不早于今天且不超过今天起 90 天）",
    ))
}

/// 从文件加载。
pub fn load(path: &Path, now: TimestampMs) -> Result<CloudParts, String> {
    load_with_ingress_tenants(path, now, &[])
}

/// 同 [`load`]，另外把云入口配置里声明的租户算进 S6 的启动校验（见 [`IngressTenant`]）。
pub fn load_with_ingress_tenants(
    path: &Path,
    now: TimestampMs,
    ingress_tenants: &[IngressTenant],
) -> Result<CloudParts, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| bad(format!("无法读取 {}：{e}", path.display())))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    parse_with_ingress_tenants(&text, base, now, ingress_tenants)
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

    /// 2026-11-13T00:00:00Z。和 safety 确认窗口的单测用同一个「今天」。
    const NOW: TimestampMs = TimestampMs(1_794_528_000_000);

    fn load(yaml: &str) -> Result<CloudParts, String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("permits.yaml"), PERMITS).unwrap();
        parse(yaml, dir.path(), NOW)
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

    /// R2 High#2：`extra_body` 不能带协议 / 计费 / 身份 / 留存字段（它盖在客户端字段之上，能写 `model` 就能把请求发给没被许可的
    /// 模型，能写 `max_tokens` 就能把余额不足时被压低的上限放大回去）。供应商自己的路由参数照常可以。
    #[test]
    fn extra_body_cannot_carry_protocol_billing_or_identity_fields() {
        let with_extra = |extra: &str| {
            VALID.replace(
                "price_version: pv-1",
                &format!("price_version: pv-1\n    extra_body: {extra}"),
            )
        };
        for field in [
            "model",
            "max_tokens",
            "max_completion_tokens",
            "stream",
            "stream_options",
            "n",
            "messages",
            "tools",
            "service_tier",
            "user",
            "store",
        ] {
            expect_error(
                &with_extra(&format!("{{provider: {{zdr: true}}, {field}: 1}}")),
                &format!("extra_body 不能包含 {field}"),
            );
        }
        // 正当用途：供应商自己的路由 / 策略参数
        let parts = load(&with_extra(
            "{provider: {zdr: true, allow_fallbacks: false}}",
        ))
        .unwrap();
        assert_eq!(parts.endpoints.len(), 1);
    }

    /// PR-Daemon S4 Low：许可表核验的是某个供应商的端点，`base_url` 的主机必须是它核验过的 API 主机，
    /// 不能带着「已核验」的标签把数据发往别的主机。
    #[test]
    fn a_verified_providers_base_url_must_be_one_of_its_verified_hosts() {
        let permits = PERMITS.replace("example-alpha", "deepinfra");
        let yaml = VALID
            .replace("example-alpha", "deepinfra")
            .replace("EXAMPLE_ALPHA_API_KEY", "DEEPINFRA_API_KEY");
        let load_with = |provider: &str, base_url: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("permits.yaml"),
                permits.replace("deepinfra", provider),
            )
            .unwrap();
            parse(
                &yaml
                    .replace("deepinfra", provider)
                    .replace("https://api.example.invalid/v1", base_url),
                dir.path(),
                TimestampMs(1_794_528_000_000),
            )
            .map(|_| ())
        };
        for ok in [
            "https://api.deepinfra.com/v1/openai",
            "https://API.DeepInfra.com/v1/openai",
            // 回环地址永远允许（本机测试用的假上游）
            "http://127.0.0.1:9/v1",
            "http://localhost:9/v1",
        ] {
            load_with("deepinfra", ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "https://api.deepseek.com/v1",
            "https://evil.example.com/v1",
            "https://api.deepinfra.com.evil.example/v1",
            "https://deepinfra.com/v1",
        ] {
            let error = load_with("deepinfra", bad).unwrap_err();
            assert!(error.contains("核验过的 API 主机"), "{bad}: {error}");
        }
        // 主机没核实的供应商：不能配置到任何非回环地址
        let error = load_with("together", "https://api.together.example/v1").unwrap_err();
        assert!(error.contains("没有核实"), "{error}");
        load_with("together", "http://127.0.0.1:9/v1").unwrap();
        // 表里没有的供应商标签（测试里的 example-*）不受限
        load_with("example-alpha", "https://api.example.invalid/v1").unwrap();
    }

    #[test]
    fn a_host_pattern_with_a_leading_wildcard_needs_a_real_subdomain() {
        let pattern = "*.ap-southeast-1.maas.aliyuncs.com";
        assert!(host_matches(
            "ws-1.ap-southeast-1.maas.aliyuncs.com",
            pattern
        ));
        assert!(host_matches(
            "WS-1.AP-southeast-1.maas.aliyuncs.com",
            pattern
        ));
        assert!(!host_matches("ap-southeast-1.maas.aliyuncs.com", pattern));
        assert!(!host_matches(
            "evilap-southeast-1.maas.aliyuncs.com",
            pattern
        ));
        assert!(!host_matches(
            "ws-1.ap-southeast-1.maas.aliyuncs.com.evil.example",
            pattern
        ));
        assert!(host_matches("API.deepinfra.com", "api.deepinfra.com"));
    }

    /// R2 High#3：`billing: unbilled` 没有实现（准入、预留、结算都不认它：用户额度照扣），加载配置时直接拒绝。
    #[test]
    fn billing_unbilled_is_refused_not_silently_charged() {
        expect_error(
            &VALID.replace("billing: paid", "billing: unbilled"),
            "unbilled 在云形态没有实现",
        );
        // 补贴、付费照常
        assert!(load(&VALID.replace("billing: paid", "billing: subsidized")).is_ok());
    }

    /// PR-Daemon B3（需 owner 确认）：生产启动时，儿童调用方 / 云入口声明的儿童租户必须配 LLM 检查器，只用规则检查器失败。
    #[test]
    fn production_start_up_needs_the_llm_judge_for_child_tenants() {
        use std::collections::BTreeSet;

        use idoris_compute_contracts::permit::DataClass;

        let child = || IngressTenant {
            name: "eduai".to_string(),
            policy: TenantDataPolicy::new(
                BTreeSet::from([DataClass::ChildEdu]),
                None,
                Retention::Zdr,
                true,
            )
            .unwrap(),
        };
        // 只有成人调用方、也没有儿童租户：不要求
        let adult_only = load(VALID).unwrap();
        require_llm_judge_for_child_tenants(&adult_only, &[], NOW).unwrap();
        // 云入口声明了儿童租户，云上游配置只有规则检查器：失败
        let rules_only = load(&format!("{VALID}safety:\n  mode: rules_only\n")).unwrap();
        let error = require_llm_judge_for_child_tenants(&rules_only, &[child()], NOW).unwrap_err();
        assert!(
            error.contains("LLM 检查器") && error.contains("rules_only"),
            "{error}"
        );
        assert!(
            error.contains("rules_only_acknowledgement")
                && error.contains("reason")
                && error.contains("acknowledged_by")
                && error.contains("expires_on"),
            "{error}"
        );
        // 静态调用方簿里的儿童调用方同样
        let head = VALID.split("callers:\n").next().unwrap();
        let child_caller = load(&format!(
            "{head}callers:
  - credential_id: cred-edu
    payer_account: acct-edu
    unit: THB
    billing: paid
    data_classes: [child_edu]
    max_retention: zdr
safety:
  mode: rules_only
"
        ))
        .unwrap();
        assert!(require_llm_judge_for_child_tenants(&child_caller, &[], NOW).is_err());
        // 对照：没有儿童租户时，规则检查器不被要求加 LLM，也不需要 acknowledgement
        require_llm_judge_for_child_tenants(&rules_only, &[], NOW).unwrap();
    }

    fn rules_only_ack(expires_on: &str) -> String {
        format!(
            "{VALID}safety:\n  mode: rules_only\n  rules_only_acknowledgement:\n    reason: \"内测：尚无签了 DPA 的检查器模型\"\n    acknowledged_by: \"jason\"\n    expires_on: \"{expires_on}\"\n"
        )
    }

    /// 显式放宽只在窗口内放行儿童租户的 `rules_only`。过期的确认块即使还挂在已加载的配置上，生产启动也拒绝。
    #[test]
    fn an_explicit_acknowledgement_lets_child_tenants_use_rules_only_until_it_expires() {
        use std::collections::BTreeSet;

        use idoris_compute_contracts::permit::DataClass;

        let child = || IngressTenant {
            name: "eduai".to_string(),
            policy: TenantDataPolicy::new(
                BTreeSet::from([DataClass::ChildEdu]),
                None,
                Retention::Zdr,
                true,
            )
            .unwrap(),
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("permits.yaml"), PERMITS).unwrap();
        let parts =
            parse_with_ingress_tenants(&rules_only_ack("2026-11-13"), dir.path(), NOW, &[child()])
                .unwrap();
        require_llm_judge_for_child_tenants(&parts, &[child()], NOW).unwrap();
        let safety = parts.safety.as_ref().unwrap();
        assert_eq!(
            safety.startup_label(),
            "rules_only (acknowledged until 2026-11-13)"
        );
        assert_eq!(
            safety.warnings,
            [
                "WARNING child-data tenants run on the RULE CHECKER ONLY by explicit acknowledgement: reason=\"内测：尚无签了 DPA 的检查器模型\" acknowledged_by=\"jason\" expires_on=2026-11-13"
            ]
        );
        // 没有儿童租户时，同一份确认不打这行儿童警告
        let adult_with_ack = load(&rules_only_ack("2026-11-13")).unwrap();
        assert!(adult_with_ack.safety.as_ref().unwrap().warnings.is_empty());
        // 今天 + 90 天仍然有效（边界）
        let until = load(&rules_only_ack("2027-02-11")).unwrap();
        require_llm_judge_for_child_tenants(&until, &[child()], NOW).unwrap();
        // 加载时就已经超出窗口：根本进不了 require
        let too_far = match load(&rules_only_ack("2027-02-12")) {
            Ok(_) => panic!("today+91 must not load"),
            Err(error) => error,
        };
        assert!(
            too_far.contains("90") && too_far.contains("2027-02-12"),
            "{too_far}"
        );
        let already_expired = match load(&rules_only_ack("2026-11-12")) {
            Ok(_) => panic!("an expired acknowledgement must not load"),
            Err(error) => error,
        };
        assert!(already_expired.contains("已经过期"), "{already_expired}");
        // 和 rules_and_llm 写在一起：加载失败（这条要是被接受，下面的变异就看不出来）
        let both = match load(
            &rules_only_ack("2026-11-13").replace("mode: rules_only", "mode: rules_and_llm"),
        ) {
            Ok(_) => panic!("rules_and_llm must reject the acknowledgement block"),
            Err(error) => error,
        };
        assert!(
            both.contains("rules_and_llm") && both.contains("rules_only_acknowledgement"),
            "{both}"
        );
        // 加载时有效、require 用的 now 已经是第二天：只看「块在不在」会放行，必须看有效性
        let next_day = TimestampMs(NOW.0 + 86_400_000);
        let error = require_llm_judge_for_child_tenants(&parts, &[child()], next_day).unwrap_err();
        assert!(
            error.contains("已经过期") && error.contains("2026-11-13"),
            "{error}"
        );
        // 没有儿童租户：不需要这份确认，有一份有效的也不挡
        require_llm_judge_for_child_tenants(&parts, &[], next_day).unwrap();
        let adult_rules = load(&format!("{VALID}safety:\n  mode: rules_only\n")).unwrap();
        require_llm_judge_for_child_tenants(&adult_rules, &[], NOW).unwrap();
        assert_eq!(
            adult_rules.safety.as_ref().unwrap().startup_label(),
            "rules_only"
        );
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

    /// S6：哪些调用方算「处理儿童数据」只由 `data_classes` 含 `child_edu` 决定（默认调用方也算）；
    /// 旧代理缓存的关闭范围与缓存键里的租户范围写法一致，启动日志的摘要里只有租户标识。
    #[test]
    fn child_data_callers_are_identified_by_their_data_classes_alone() {
        // 没有儿童调用方：没有关闭范围，摘要里全是「其它」
        let parts = load(VALID).unwrap();
        assert!(!parts.callers.has_child_data_callers());
        assert!(parts.callers.child_data_tenant_scopes().is_empty());
        let (child, other) = parts.callers.caller_summary();
        assert!(child.is_empty());
        assert_eq!(other, ["agent24", "(default)"]);

        // 一个具名儿童租户、一个普通租户、一个同时含 child_edu 的默认调用方（`data_classes` 里只要有 child_edu 就算）
        let head = VALID.split("callers:\n").next().unwrap();
        let yaml = format!(
            "{head}callers:
  - tenant_id: edu-th
    credential_id: cred-edu
    payer_account: acct-edu
    unit: THB
    billing: paid
    data_classes: [child_edu]
    max_retention: zdr
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
    data_classes: [child_edu, adult]
    max_retention: zdr
safety:
  mode: rules_only
"
        );
        let parts = load(&yaml).unwrap();
        assert!(parts.callers.has_child_data_callers());
        assert_eq!(
            parts
                .callers
                .child_data_tenant_scopes()
                .into_iter()
                .collect::<Vec<_>>(),
            ["\u{0}personal", "edu-th"],
            "没有租户的默认调用方用缓存键里的 \\0personal 写法"
        );
        let (child, other) = parts.callers.caller_summary();
        assert_eq!(child, ["edu-th", "(default)"]);
        assert_eq!(other, ["agent24"]);
        // 摘要里只有租户标识：没有凭证号、付款账户这些别的东西
        for name in child.iter().chain(other.iter()) {
            assert!(!name.contains("cred-") && !name.contains("acct-"), "{name}");
        }

        // 儿童调用方存在而没有 safety 段：加载失败（安全检查是儿童业务上线的前提）
        let without_safety = yaml.replace("safety:\n  mode: rules_only\n", "");
        let error = match load(&without_safety) {
            Ok(_) => panic!("a child caller without a safety section must not load"),
            Err(error) => error,
        };
        assert!(error.contains("必须配置 safety"), "{error}");
    }
}

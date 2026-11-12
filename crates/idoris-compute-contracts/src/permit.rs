//! 端点级许可表（设计稿 §8，评审 H7 / M2 / L1）。
//!
//! 这张表是**唯一的权威路由约束**：ZDR 不能是一个布尔标签，所以每个端点单独列出处理区域、留存、
//! DPA、允许的数据类别、能力和核验时间；路由先过这些硬约束，兜底同样要过，**没有合规候选就拒绝**。
//!
//! # YAML schema（`schema_version: 1`）
//!
//! ```yaml
//! schema_version: 1              # 必填，目前只有 1
//! verification_max_age_days: 90  # 必填，1..=365。核验时间超过这个天数就自动停用
//! endpoints:                     # 必填，非空
//!   - provider: example-host                 # 供应商 id：[A-Za-z0-9._-]，<=128
//!     model: example-org/chat-70b            # 模型 id：[A-Za-z0-9._:~/-]，<=128
//!     endpoint: openai-compat                # 具体端点名：[A-Za-z0-9._-]，<=128。
//!                                            #   同一供应商同一模型下不同的 API 面/路由方式各一条
//!                                            #   （如 openrouter 的 `zdr-route` 与 `default-route`）
//!     access: api                            # api | subscription。subscription 走 CLI，云形态排除
//!     processing_regions: [us]               # 推理实际执行的区域，非空、不重复，小写两位字母（ISO 3166-1
//!                                            #   alpha-2，如 us sg th eu）。路由时必须**全部**被租户允许
//!     retention: zdr                         # zdr | with_exceptions | retained
//!                                            #   zdr = 零数据留存；with_exceptions = 有例外（如 Safety
//!                                            #   Retention）；retained = 会留存
//!     dpa: signed                            # signed | unsigned
//!     allowed_data_classes: [child_edu, adult, public]   # 非空、不重复
//!     capabilities:                          # 五项都必须写明 true/false，没有缺省
//!       tool_calling: true                   #   工具调用
//!       stream_usage: true                   #   流式响应里带 usage（stream_options.include_usage）
//!       cache_pricing: false                 #   缓存计价
//!       structured_output: true              #   结构化输出
//!       cancellation: true                   #   取消
//!     verified_at: "2026-11-05"              # 核验时间：RFC 3339（如 2026-11-05T08:00:00Z，可带偏移）
//!                                            #   或纯日期 YYYY-MM-DD（按当天 UTC 00:00 算）
//!     notes: "free text"                     # 可选，<=1000 字符，仅供人看，不参与任何判断
//!     evidence: ["https://example.invalid/dpa"]   # 可选，每条 <=500 字符的证据引用，仅供人看
//! ```
//!
//! 所有对象拒绝未知字段，所有枚举拒绝未知取值（与仓库其它契约一致）。键 `(provider, model, endpoint)`
//! 在表内唯一。
//!
//! 加载时的自洽校验（[`PermitTable::from_yaml_str`]）：声明了 `child_edu` 的条目**自己**必须是
//! `retention: zdr` 且 `dpa: signed`，坏表不能给自己发通行证；路由时（[`select_compliant`]）会**再查一遍**，
//! 纵深防御。
//!
//! # 硬约束过滤（[`select_compliant`]）
//!
//! 对每个候选，按下面这些依次检查，任何一条不过就排除并记下原因（[`ExcludeReason`]）：
//!
//! 1. 必须在表里（表外的端点一律不合规，失败即关闭）；
//! 2. 云形态排除 `access: subscription`；
//! 3. **核验没过期**：`verified_at <= now < verified_at + verification_max_age_days`。过期的端点自动视为停用；
//!    核验时间在未来也不认；
//! 4. **租户的数据类别 ⊆ 端点允许的类别**；
//! 5. 留存：端点的留存不能比租户允许的最差留存更差；**含 `child_edu` 的租户固定要求 `zdr`**；
//! 6. DPA：租户要求或含 `child_edu` 时必须 `signed`；
//! 7. 区域：租户限定了区域时，端点的**每一个**处理区域都必须在其中；
//! 8. **付费或补贴的流式请求**，端点必须支持流式 usage；
//! 9. 请求需要的其它能力端点都得有。
//!
//! 结果是 `Ok(NonEmpty)` 或 `Err(NoCompliantEndpoint)`：**没有合规候选返回明确的拒绝，而不是一个空列表**，
//! 空列表没有出路让调用方「误用」。

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::form::DeploymentForm;
use crate::ids::{EndpointKey, EndpointName, ModelId, ProviderId, TimestampMs};
use crate::reason::ReasonCode;

/// 当前支持的 schema 版本。
pub const PERMIT_SCHEMA_VERSION: u32 = 1;
/// `verification_max_age_days` 的上限：核验超过一年没更新的表，不该靠改大这个数字续命。
pub const MAX_VERIFICATION_AGE_DAYS_LIMIT: u32 = 365;
/// `notes` 最大长度（字符）。
pub const MAX_NOTES_CHARS: usize = 1_000;
/// 每条 `evidence` 最大长度（字符）。
pub const MAX_EVIDENCE_CHARS: usize = 500;

const MS_PER_DAY: i64 = 86_400_000;

// ---------------------------------------------------------------------------
// 取值类型
// ---------------------------------------------------------------------------

/// 数据类别（设计稿 §8「允许的数据类别」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    /// 儿童教育数据。最严：只能落到 ZDR + 已签 DPA 的端点。
    ChildEdu,
    /// 成人数据（如 Agent24 办公）。
    Adult,
    /// 公开数据。
    Public,
}

/// 留存。按「对数据的保护」从严到宽排序：`Zdr` < `WithExceptions` < `Retained`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// 零数据留存。
    Zdr,
    /// 有例外（比如 Safety Retention）。
    WithExceptions,
    /// 会留存。
    Retained,
}

impl Retention {
    fn rank(self) -> u8 {
        match self {
            Retention::Zdr => 0,
            Retention::WithExceptions => 1,
            Retention::Retained => 2,
        }
    }

    /// 这种留存是否满足「最差也不能比 `worst_acceptable` 更差」。
    pub fn satisfies(self, worst_acceptable: Retention) -> bool {
        self.rank() <= worst_acceptable.rank()
    }
}

/// 数据处理协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dpa {
    Signed,
    Unsigned,
}

/// 访问路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessPath {
    /// 供应商的 HTTP API。
    Api,
    /// 订阅账号走 CLI。云形态排除。
    Subscription,
}

/// 处理区域：小写两位字母（ISO 3166-1 alpha-2，如 `us`、`sg`、`th`、`eu`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Region(String);

impl Region {
    pub fn new(code: impl Into<String>) -> Result<Self, PermitProblem> {
        let code = code.into();
        if code.len() == 2 && code.bytes().all(|b| b.is_ascii_lowercase()) {
            Ok(Self(code))
        } else {
            Err(PermitProblem::InvalidRegion(code))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Region {
    type Error = PermitProblem;
    fn try_from(value: String) -> Result<Self, PermitProblem> {
        Self::new(value)
    }
}

impl From<Region> for String {
    fn from(value: Region) -> String {
        value.0
    }
}

impl FromStr for Region {
    type Err = PermitProblem;
    fn from_str(s: &str) -> Result<Self, PermitProblem> {
        Self::new(s)
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 能力（设计稿 §8「能力」列的五项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    ToolCalling,
    StreamUsage,
    CachePricing,
    StructuredOutput,
    Cancellation,
}

/// 端点具备的能力。五项都必须明确写 `true`/`false`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub tool_calling: bool,
    pub stream_usage: bool,
    pub cache_pricing: bool,
    pub structured_output: bool,
    pub cancellation: bool,
}

impl Capabilities {
    pub fn has(&self, kind: CapabilityKind) -> bool {
        match kind {
            CapabilityKind::ToolCalling => self.tool_calling,
            CapabilityKind::StreamUsage => self.stream_usage,
            CapabilityKind::CachePricing => self.cache_pricing,
            CapabilityKind::StructuredOutput => self.structured_output,
            CapabilityKind::Cancellation => self.cancellation,
        }
    }
}

// ---------------------------------------------------------------------------
// 时间：RFC 3339 或纯日期 <-> TimestampMs
// ---------------------------------------------------------------------------

/// 把 RFC 3339（可带偏移）或纯日期 `YYYY-MM-DD`（当天 UTC 00:00）解析成 UTC 毫秒。
/// 纯日期必须是严格的 10 个字符、日历上存在（2026-02-30 这种拒绝）。
pub fn parse_verified_at(text: &str) -> Result<TimestampMs, PermitProblem> {
    let bad = || PermitProblem::InvalidTimestamp(text.to_string());
    let bytes = text.as_bytes();
    if bytes.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        let digits_ok = bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        if !digits_ok {
            return Err(bad());
        }
        let year: i32 = text[0..4].parse().map_err(|_| bad())?;
        let month: u32 = text[5..7].parse().map_err(|_| bad())?;
        let day: u32 = text[8..10].parse().map_err(|_| bad())?;
        let date = NaiveDate::from_ymd_opt(year, month, day).ok_or_else(bad)?;
        let midnight = date.and_hms_opt(0, 0, 0).ok_or_else(bad)?;
        return Ok(TimestampMs(midnight.and_utc().timestamp_millis()));
    }
    let parsed = DateTime::parse_from_rfc3339(text).map_err(|_| bad())?;
    Ok(TimestampMs(parsed.with_timezone(&Utc).timestamp_millis()))
}

/// 规范输出：UTC、`Z` 结尾；毫秒为 0 时只到秒。
pub fn format_verified_at(ts: TimestampMs) -> Option<String> {
    let dt = DateTime::<Utc>::from_timestamp_millis(ts.millis())?;
    let precision = if ts.millis().rem_euclid(1_000) == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Millis
    };
    Some(dt.to_rfc3339_opts(precision, true))
}

mod verified_at_format {
    use super::*;

    pub fn serialize<S: Serializer>(ts: &TimestampMs, serializer: S) -> Result<S::Ok, S::Error> {
        match format_verified_at(*ts) {
            Some(text) => serializer.serialize_str(&text),
            None => Err(serde::ser::Error::custom("verified_at is out of range")),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<TimestampMs, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_verified_at(&text).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// 条目与表
// ---------------------------------------------------------------------------

/// 许可表里的一个端点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointPermit {
    pub provider: ProviderId,
    pub model: ModelId,
    pub endpoint: EndpointName,
    pub access: AccessPath,
    pub processing_regions: Vec<Region>,
    pub retention: Retention,
    pub dpa: Dpa,
    pub allowed_data_classes: Vec<DataClass>,
    pub capabilities: Capabilities,
    #[serde(with = "verified_at_format")]
    pub verified_at: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

impl EndpointPermit {
    pub fn key(&self) -> EndpointKey {
        EndpointKey {
            provider: self.provider.clone(),
            model: self.model.clone(),
            endpoint: self.endpoint.clone(),
        }
    }

    /// 条目自身的自洽校验（不涉及表级规则）。
    fn check_self_consistency(&self) -> Result<(), PermitProblem> {
        if self.processing_regions.is_empty() {
            return Err(PermitProblem::NoRegions);
        }
        let mut seen = BTreeSet::new();
        for region in &self.processing_regions {
            if !seen.insert(region) {
                return Err(PermitProblem::DuplicateRegion(region.clone()));
            }
        }
        if self.allowed_data_classes.is_empty() {
            return Err(PermitProblem::NoDataClasses);
        }
        let mut seen = BTreeSet::new();
        for class in &self.allowed_data_classes {
            if !seen.insert(*class) {
                return Err(PermitProblem::DuplicateDataClass(*class));
            }
        }
        if self.allowed_data_classes.contains(&DataClass::ChildEdu)
            && !(self.retention == Retention::Zdr && self.dpa == Dpa::Signed)
        {
            return Err(PermitProblem::ChildEduRequiresZdrAndSignedDpa {
                retention: self.retention,
                dpa: self.dpa,
            });
        }
        if let Some(notes) = &self.notes
            && notes.chars().count() > MAX_NOTES_CHARS
        {
            return Err(PermitProblem::NotesTooLong);
        }
        if self
            .evidence
            .iter()
            .any(|e| e.is_empty() || e.chars().count() > MAX_EVIDENCE_CHARS)
        {
            return Err(PermitProblem::BadEvidence);
        }
        Ok(())
    }
}

/// 许可表加载 / 校验的问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PermitProblem {
    #[error("invalid region {0:?}: expected two lowercase letters (ISO 3166-1 alpha-2)")]
    InvalidRegion(String),
    #[error("invalid timestamp {0:?}: expected RFC 3339 or YYYY-MM-DD")]
    InvalidTimestamp(String),
    #[error("processing_regions must not be empty")]
    NoRegions,
    #[error("duplicate processing region {0}")]
    DuplicateRegion(Region),
    #[error("allowed_data_classes must not be empty")]
    NoDataClasses,
    #[error("duplicate data class {0:?}")]
    DuplicateDataClass(DataClass),
    #[error(
        "an endpoint that allows child_edu must be retention=zdr and dpa=signed (got {retention:?} / {dpa:?})"
    )]
    ChildEduRequiresZdrAndSignedDpa { retention: Retention, dpa: Dpa },
    #[error("notes longer than {MAX_NOTES_CHARS} characters")]
    NotesTooLong,
    #[error("evidence entries must be non-empty and at most {MAX_EVIDENCE_CHARS} characters")]
    BadEvidence,
    #[error("verified_at plus the maximum age overflows the timestamp range")]
    VerificationOverflow,
}

/// 许可表错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PermitError {
    #[error("cannot parse permit table YAML: {0}")]
    Yaml(String),
    #[error("unsupported schema_version {found}; this build supports {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error(
        "verification_max_age_days {days} out of range (1..={MAX_VERIFICATION_AGE_DAYS_LIMIT})"
    )]
    InvalidMaxAge { days: u32 },
    #[error("the permit table has no endpoints")]
    EmptyTable,
    #[error("duplicate endpoint {0}")]
    DuplicateEndpoint(EndpointKey),
    #[error("endpoint {endpoint}: {problem}")]
    Entry {
        endpoint: EndpointKey,
        problem: PermitProblem,
    },
}

/// 磁盘上的整张表（原样解析，未校验）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitTableFile {
    pub schema_version: u32,
    pub verification_max_age_days: u32,
    pub endpoints: Vec<EndpointPermit>,
}

/// 经过校验的许可表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PermitTableFile", into = "PermitTableFile")]
pub struct PermitTable {
    verification_max_age_days: u32,
    endpoints: Vec<EndpointPermit>,
}

impl TryFrom<PermitTableFile> for PermitTable {
    type Error = PermitError;
    fn try_from(file: PermitTableFile) -> Result<Self, PermitError> {
        PermitTable::from_file(file)
    }
}

impl From<PermitTable> for PermitTableFile {
    fn from(table: PermitTable) -> PermitTableFile {
        PermitTableFile {
            schema_version: PERMIT_SCHEMA_VERSION,
            verification_max_age_days: table.verification_max_age_days,
            endpoints: table.endpoints,
        }
    }
}

impl PermitTable {
    /// 从 YAML 文本加载并校验。
    pub fn from_yaml_str(text: &str) -> Result<PermitTable, PermitError> {
        let file: PermitTableFile =
            serde_yaml::from_str(text).map_err(|e| PermitError::Yaml(e.to_string()))?;
        PermitTable::from_file(file)
    }

    /// 校验：schema 版本、最大年龄范围、表非空、键唯一、每个条目自洽、核验时间加上最大年龄不溢出。
    pub fn from_file(file: PermitTableFile) -> Result<PermitTable, PermitError> {
        if file.schema_version != PERMIT_SCHEMA_VERSION {
            return Err(PermitError::UnsupportedSchemaVersion {
                found: file.schema_version,
                supported: PERMIT_SCHEMA_VERSION,
            });
        }
        let days = file.verification_max_age_days;
        if days == 0 || days > MAX_VERIFICATION_AGE_DAYS_LIMIT {
            return Err(PermitError::InvalidMaxAge { days });
        }
        if file.endpoints.is_empty() {
            return Err(PermitError::EmptyTable);
        }
        let mut seen = BTreeSet::new();
        for entry in &file.endpoints {
            let key = entry.key();
            if !seen.insert(key.clone()) {
                return Err(PermitError::DuplicateEndpoint(key));
            }
            entry
                .check_self_consistency()
                .map_err(|problem| PermitError::Entry {
                    endpoint: key.clone(),
                    problem,
                })?;
            if expiry_of(entry.verified_at, days).is_none() {
                return Err(PermitError::Entry {
                    endpoint: key,
                    problem: PermitProblem::VerificationOverflow,
                });
            }
        }
        Ok(PermitTable {
            verification_max_age_days: days,
            endpoints: file.endpoints,
        })
    }

    pub fn verification_max_age_days(&self) -> u32 {
        self.verification_max_age_days
    }

    pub fn endpoints(&self) -> &[EndpointPermit] {
        &self.endpoints
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    pub fn lookup(&self, key: &EndpointKey) -> Option<&EndpointPermit> {
        self.endpoints.iter().find(|e| {
            e.provider == key.provider && e.model == key.model && e.endpoint == key.endpoint
        })
    }

    /// 这个条目的核验在什么时刻过期（不含）：`verified_at + verification_max_age_days`。
    pub fn expires_at(&self, entry: &EndpointPermit) -> Option<TimestampMs> {
        expiry_of(entry.verified_at, self.verification_max_age_days)
    }

    /// 核验此刻是否有效：`verified_at <= now < expires_at`。过期（含刚好等于 `expires_at`）或还没到
    /// （核验时间在未来）都算停用。
    pub fn is_verified_at(&self, entry: &EndpointPermit, now: TimestampMs) -> bool {
        self.verification_state(entry, now).is_none()
    }

    fn verification_state(
        &self,
        entry: &EndpointPermit,
        now: TimestampMs,
    ) -> Option<ExcludeReason> {
        if now < entry.verified_at {
            return Some(ExcludeReason::VerifiedInTheFuture {
                verified_at: entry.verified_at,
            });
        }
        match self.expires_at(entry) {
            Some(expires_at) if now < expires_at => None,
            Some(expires_at) => Some(ExcludeReason::VerificationExpired {
                verified_at: entry.verified_at,
                expired_at: expires_at,
            }),
            // 加载时已保证不溢出；这里当作已过期，失败即关闭
            None => Some(ExcludeReason::VerificationExpired {
                verified_at: entry.verified_at,
                expired_at: entry.verified_at,
            }),
        }
    }
}

fn expiry_of(verified_at: TimestampMs, days: u32) -> Option<TimestampMs> {
    let delta = i64::from(days).checked_mul(MS_PER_DAY)?;
    verified_at.checked_add_ms(delta)
}

// ---------------------------------------------------------------------------
// 路由硬约束过滤
// ---------------------------------------------------------------------------

/// 一次请求的计费性质。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// 向用户收费。
    Paid,
    /// 由补贴池出钱（免费额度）。
    Subsidized,
    /// 不计费（个人形态等）。
    Unbilled,
}

/// 租户的数据处理要求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "TenantDataPolicyWire")]
pub struct TenantDataPolicy {
    data_classes: BTreeSet<DataClass>,
    allowed_regions: Option<BTreeSet<Region>>,
    max_retention: Retention,
    require_dpa: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantDataPolicyWire {
    data_classes: BTreeSet<DataClass>,
    allowed_regions: Option<BTreeSet<Region>>,
    max_retention: Retention,
    require_dpa: bool,
}

impl TryFrom<TenantDataPolicyWire> for TenantDataPolicy {
    type Error = PermitProblem;
    fn try_from(wire: TenantDataPolicyWire) -> Result<Self, PermitProblem> {
        TenantDataPolicy::new(
            wire.data_classes,
            wire.allowed_regions,
            wire.max_retention,
            wire.require_dpa,
        )
    }
}

impl TenantDataPolicy {
    /// `data_classes` 非空；`allowed_regions` 为 `None` 表示不限区域，`Some` 时不能为空集
    /// （空集 = 没有任何区域可用，几乎一定是配置错误）。
    pub fn new(
        data_classes: BTreeSet<DataClass>,
        allowed_regions: Option<BTreeSet<Region>>,
        max_retention: Retention,
        require_dpa: bool,
    ) -> Result<Self, PermitProblem> {
        if data_classes.is_empty() {
            return Err(PermitProblem::NoDataClasses);
        }
        if allowed_regions.as_ref().is_some_and(BTreeSet::is_empty) {
            return Err(PermitProblem::NoRegions);
        }
        Ok(Self {
            data_classes,
            allowed_regions,
            max_retention,
            require_dpa,
        })
    }

    pub fn data_classes(&self) -> &BTreeSet<DataClass> {
        &self.data_classes
    }

    pub fn allowed_regions(&self) -> Option<&BTreeSet<Region>> {
        self.allowed_regions.as_ref()
    }

    /// 含 `child_edu` 的租户固定要求 ZDR + 已签 DPA，不管租户自己怎么配。
    pub fn handles_child_data(&self) -> bool {
        self.data_classes.contains(&DataClass::ChildEdu)
    }

    /// 实际生效的「最差可接受留存」：含 `child_edu` 时恒为 `Zdr`。
    pub fn effective_max_retention(&self) -> Retention {
        if self.handles_child_data() {
            Retention::Zdr
        } else {
            self.max_retention
        }
    }

    /// 实际生效的 DPA 要求：含 `child_edu` 时恒为必须已签。
    pub fn effective_require_dpa(&self) -> bool {
        self.handles_child_data() || self.require_dpa
    }
}

/// 请求需要的能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityNeeds {
    #[serde(default)]
    pub tool_calling: bool,
    #[serde(default)]
    pub stream_usage: bool,
    #[serde(default)]
    pub cache_pricing: bool,
    #[serde(default)]
    pub structured_output: bool,
    #[serde(default)]
    pub cancellation: bool,
}

/// 一次路由请求的硬约束输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRequirements {
    pub form: DeploymentForm,
    pub tenant: TenantDataPolicy,
    /// 是否流式。
    pub stream: bool,
    pub billing: Billing,
    pub needs: CapabilityNeeds,
}

impl RouteRequirements {
    /// 这次请求实际需要的能力：请求自己要的，再加上「付费或补贴的流式请求必须有流式 usage」。
    pub fn required_capabilities(&self) -> BTreeSet<CapabilityKind> {
        let mut required = BTreeSet::new();
        let n = &self.needs;
        for (needed, kind) in [
            (n.tool_calling, CapabilityKind::ToolCalling),
            (n.stream_usage, CapabilityKind::StreamUsage),
            (n.cache_pricing, CapabilityKind::CachePricing),
            (n.structured_output, CapabilityKind::StructuredOutput),
            (n.cancellation, CapabilityKind::Cancellation),
        ] {
            if needed {
                required.insert(kind);
            }
        }
        if self.stream && self.billing != Billing::Unbilled {
            required.insert(CapabilityKind::StreamUsage);
        }
        required
    }
}

/// 一个候选被排除的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ExcludeReason {
    /// 不在许可表里：失败即关闭。
    NotInTable,
    /// 云形态排除 subscription 路径。
    SubscriptionNotAllowedInCloud,
    /// 核验已过期，自动停用。
    VerificationExpired {
        verified_at: TimestampMs,
        expired_at: TimestampMs,
    },
    /// 核验时间在未来，不认。
    VerifiedInTheFuture { verified_at: TimestampMs },
    /// 租户有端点不允许的数据类别。
    DataClassNotAllowed { missing: Vec<DataClass> },
    /// 留存比租户允许的更差（含 `child_edu` 时「允许的」固定是 `zdr`）。
    RetentionTooWeak { have: Retention, need: Retention },
    /// 需要已签 DPA，但没签。
    DpaRequired,
    /// 端点的处理区域不在租户允许的区域里。
    RegionNotAllowed { regions: Vec<Region> },
    /// 付费或补贴的流式请求，端点不支持流式 usage。
    StreamUsageRequired,
    /// 缺少请求需要的能力。
    MissingCapability { capability: CapabilityKind },
}

/// 一个被排除的候选和它的全部原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Exclusion {
    pub endpoint: EndpointKey,
    pub reasons: Vec<ExcludeReason>,
}

/// 没有合规候选。对应原因码 `no_compliant_endpoint`（[`NoCompliantEndpoint::reason_code`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("no compliant endpoint among {considered} candidate(s)")]
pub struct NoCompliantEndpoint {
    /// 参与过滤的候选个数（去重后）。0 表示调用方给了空候选。
    pub considered: usize,
    pub excluded: Vec<Exclusion>,
}

impl NoCompliantEndpoint {
    pub fn reason_code(&self) -> ReasonCode {
        ReasonCode::NoCompliantEndpoint
    }
}

/// 保证非空的列表：[`select_compliant`] 成功时返回它，调用方拿不到「空列表」。
///
/// 结构上就是「一个头 + 其余」，所以 `first()` 不可能失败，不需要任何 unwrap。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    head: T,
    tail: Vec<T>,
}

impl<T> NonEmpty<T> {
    pub fn from_vec(items: Vec<T>) -> Option<Self> {
        let mut iter = items.into_iter();
        let head = iter.next()?;
        Some(Self {
            head,
            tail: iter.collect(),
        })
    }

    /// 偏好顺序里的第一个。
    pub fn first(&self) -> &T {
        &self.head
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.head).chain(self.tail.iter())
    }

    pub fn len(&self) -> usize {
        1 + self.tail.len()
    }

    /// 永远为 `false`（类型保证非空）。
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn into_vec(self) -> Vec<T> {
        let mut all = Vec::with_capacity(1 + self.tail.len());
        all.push(self.head);
        all.extend(self.tail);
        all
    }
}

/// 评估一个已经在表里的条目对这次请求的全部排除原因（顺序固定；空 = 合规）。
pub fn evaluate_entry(
    table: &PermitTable,
    entry: &EndpointPermit,
    req: &RouteRequirements,
    now: TimestampMs,
) -> Vec<ExcludeReason> {
    let mut reasons = Vec::new();

    // 2. 云形态排除 subscription
    if req.form == DeploymentForm::Cloud && entry.access == AccessPath::Subscription {
        reasons.push(ExcludeReason::SubscriptionNotAllowedInCloud);
    }
    // 3. 核验
    if let Some(reason) = table.verification_state(entry, now) {
        reasons.push(reason);
    }
    // 4. 数据类别 ⊆
    let missing: Vec<DataClass> = req
        .tenant
        .data_classes()
        .iter()
        .filter(|class| !entry.allowed_data_classes.contains(class))
        .copied()
        .collect();
    if !missing.is_empty() {
        reasons.push(ExcludeReason::DataClassNotAllowed { missing });
    }
    // 5. 留存（含 child_edu 的租户固定 zdr）
    let need = req.tenant.effective_max_retention();
    if !entry.retention.satisfies(need) {
        reasons.push(ExcludeReason::RetentionTooWeak {
            have: entry.retention,
            need,
        });
    }
    // 6. DPA（含 child_edu 的租户固定要求已签）
    if req.tenant.effective_require_dpa() && entry.dpa != Dpa::Signed {
        reasons.push(ExcludeReason::DpaRequired);
    }
    // 7. 区域：端点的每一个处理区域都必须被允许
    if let Some(allowed) = req.tenant.allowed_regions() {
        let offending: Vec<Region> = entry
            .processing_regions
            .iter()
            .filter(|r| !allowed.contains(r))
            .cloned()
            .collect();
        if !offending.is_empty() {
            reasons.push(ExcludeReason::RegionNotAllowed { regions: offending });
        }
    }
    // 8/9. 能力（付费或补贴的流式请求必须有流式 usage）
    let stream_usage_is_policy = req.stream && req.billing != Billing::Unbilled;
    for capability in req.required_capabilities() {
        if !entry.capabilities.has(capability) {
            if capability == CapabilityKind::StreamUsage && stream_usage_is_policy {
                reasons.push(ExcludeReason::StreamUsageRequired);
            } else {
                reasons.push(ExcludeReason::MissingCapability { capability });
            }
        }
    }
    reasons
}

/// 硬约束过滤。
///
/// - 保持 `candidates` 的顺序（路由偏好由调用方排好），重复的候选只保留第一个；
/// - 表外的候选排除并记 [`ExcludeReason::NotInTable`]；
/// - 一个都不合规（包括候选为空）返回 [`NoCompliantEndpoint`]，附上每个候选被排除的全部原因；
/// - **兜底候选走同一个函数**：兜底不豁免任何约束。
pub fn select_compliant<'a>(
    table: &'a PermitTable,
    candidates: &[EndpointKey],
    req: &RouteRequirements,
    now: TimestampMs,
) -> Result<NonEmpty<&'a EndpointPermit>, NoCompliantEndpoint> {
    let mut seen: BTreeSet<&EndpointKey> = BTreeSet::new();
    let mut accepted: Vec<&'a EndpointPermit> = Vec::new();
    let mut excluded: Vec<Exclusion> = Vec::new();
    for key in candidates {
        if !seen.insert(key) {
            continue;
        }
        match table.lookup(key) {
            None => excluded.push(Exclusion {
                endpoint: key.clone(),
                reasons: vec![ExcludeReason::NotInTable],
            }),
            Some(entry) => {
                let reasons = evaluate_entry(table, entry, req, now);
                if reasons.is_empty() {
                    accepted.push(entry);
                } else {
                    excluded.push(Exclusion {
                        endpoint: key.clone(),
                        reasons,
                    });
                }
            }
        }
    }
    let considered = seen.len();
    NonEmpty::from_vec(accepted).ok_or(NoCompliantEndpoint {
        considered,
        excluded,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const EXAMPLE: &str = include_str!("../testdata/endpoint-permit-table.example.yaml");

    /// 示例表里条目的核验时间都是 2026-11-05，最大年龄 90 天；以下是几个固定的「现在」。
    const VERIFIED: i64 = 1_793_836_800_000; // 2026-11-05T00:00:00Z
    const DAY: i64 = 86_400_000;

    fn table() -> PermitTable {
        PermitTable::from_yaml_str(EXAMPLE).unwrap()
    }

    fn key(provider: &str, model: &str, endpoint: &str) -> EndpointKey {
        EndpointKey::new(provider, model, endpoint).unwrap()
    }

    fn classes(items: &[DataClass]) -> BTreeSet<DataClass> {
        items.iter().copied().collect()
    }

    fn regions(items: &[&str]) -> BTreeSet<Region> {
        items.iter().map(|r| Region::new(*r).unwrap()).collect()
    }

    fn tenant(items: &[DataClass]) -> TenantDataPolicy {
        TenantDataPolicy::new(classes(items), None, Retention::Retained, false).unwrap()
    }

    fn req(form: DeploymentForm, tenant: TenantDataPolicy) -> RouteRequirements {
        RouteRequirements {
            form,
            tenant,
            stream: false,
            billing: Billing::Paid,
            needs: CapabilityNeeds::default(),
        }
    }

    fn now() -> TimestampMs {
        TimestampMs(VERIFIED + DAY) // 核验后一天
    }

    // ---------- 示例文件与 schema ----------

    #[test]
    fn the_example_file_loads_and_covers_every_rule() {
        let t = table();
        assert_eq!(t.verification_max_age_days(), 90);
        assert_eq!(t.len(), 7);
        assert!(!t.is_empty());
        // 每种留存、两种 DPA、三种数据类别、两种访问路径、多区域，示例里都出现过
        let all = t.endpoints();
        for retention in [
            Retention::Zdr,
            Retention::WithExceptions,
            Retention::Retained,
        ] {
            assert!(
                all.iter().any(|e| e.retention == retention),
                "{retention:?}"
            );
        }
        assert!(all.iter().any(|e| e.dpa == Dpa::Unsigned));
        assert!(all.iter().any(|e| e.access == AccessPath::Subscription));
        assert!(all.iter().any(|e| e.processing_regions.len() > 1));
        assert!(all.iter().any(|e| !e.capabilities.stream_usage));
        for class in [DataClass::ChildEdu, DataClass::Adult, DataClass::Public] {
            assert!(
                all.iter().any(|e| e.allowed_data_classes.contains(&class)),
                "{class:?}"
            );
        }
        // 示例用的全是明显的假数据
        assert!(
            all.iter()
                .all(|e| e.provider.as_str().starts_with("example-"))
        );
    }

    #[test]
    fn yaml_round_trip_is_stable() {
        let t = table();
        let yaml = serde_yaml::to_string(&t).unwrap();
        let back = PermitTable::from_yaml_str(&yaml).unwrap();
        assert_eq!(back, t);
        // 再序列化一次，文本不变（规范形式）
        assert_eq!(serde_yaml::to_string(&back).unwrap(), yaml);
        // JSON 同样可以往返
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<PermitTable>(&json).unwrap(), t);
    }

    #[test]
    fn timestamps_accept_rfc3339_and_dates_and_print_canonically() {
        assert_eq!(
            parse_verified_at("2026-11-05").unwrap(),
            TimestampMs(VERIFIED)
        );
        assert_eq!(
            parse_verified_at("2026-11-05T00:00:00Z").unwrap(),
            TimestampMs(VERIFIED)
        );
        assert_eq!(
            parse_verified_at("2026-11-05T00:00:00+00:00").unwrap(),
            TimestampMs(VERIFIED)
        );
        // 带偏移：东七区 07:00 = UTC 00:00
        assert_eq!(
            parse_verified_at("2026-11-05T07:00:00+07:00").unwrap(),
            TimestampMs(VERIFIED)
        );
        assert_eq!(
            parse_verified_at("2026-11-05T00:00:00.250Z").unwrap(),
            TimestampMs(VERIFIED + 250)
        );
        assert_eq!(
            format_verified_at(TimestampMs(VERIFIED)).unwrap(),
            "2026-11-05T00:00:00Z"
        );
        assert_eq!(
            format_verified_at(TimestampMs(VERIFIED + 250)).unwrap(),
            "2026-11-05T00:00:00.250Z"
        );
        for bad in [
            "",
            "2026-11-5",
            "2026-1-05",
            "26-11-05",
            "2026/11/05",
            "2026-02-30",
            "2026-13-01",
            "2026-11-05 00:00:00",
            "2026-11-05T00:00:00",
            "yesterday",
            "２０２６-11-05",
            "2026-11-0a",
        ] {
            assert!(parse_verified_at(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn region_is_two_lowercase_letters() {
        for ok in ["us", "sg", "th", "eu", "cn"] {
            assert_eq!(Region::new(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "u", "usa", "US", "u1", "u-", "ü", "u s"] {
            assert!(Region::new(bad).is_err(), "{bad:?}");
        }
        assert!(serde_json::from_str::<Region>("\"US\"").is_err());
    }

    // ---------- 加载与校验 ----------

    fn entry_yaml(provider: &str, extra: &str) -> String {
        format!(
            "  - provider: {provider}\n    model: m\n    endpoint: e\n    access: api\n    processing_regions: [us]\n    retention: zdr\n    dpa: signed\n    allowed_data_classes: [adult]\n    capabilities: {{tool_calling: true, stream_usage: true, cache_pricing: false, structured_output: true, cancellation: true}}\n    verified_at: \"2026-11-05\"\n{extra}"
        )
    }

    fn one_entry_table(version: u32, days: u32, entries: &str) -> String {
        format!(
            "schema_version: {version}\nverification_max_age_days: {days}\nendpoints:\n{entries}"
        )
    }

    #[test]
    fn a_minimal_table_loads() {
        let t = PermitTable::from_yaml_str(&one_entry_table(1, 90, &entry_yaml("p", ""))).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.endpoints()[0].notes, None);
        assert!(t.endpoints()[0].evidence.is_empty());
    }

    #[test]
    fn table_level_validation_errors() {
        let ok_entry = entry_yaml("p", "");
        assert_eq!(
            PermitTable::from_yaml_str(&one_entry_table(2, 90, &ok_entry)).unwrap_err(),
            PermitError::UnsupportedSchemaVersion {
                found: 2,
                supported: 1
            }
        );
        assert_eq!(
            PermitTable::from_yaml_str(&one_entry_table(0, 90, &ok_entry)).unwrap_err(),
            PermitError::UnsupportedSchemaVersion {
                found: 0,
                supported: 1
            }
        );
        for days in [0, 366, 10_000] {
            assert_eq!(
                PermitTable::from_yaml_str(&one_entry_table(1, days, &ok_entry)).unwrap_err(),
                PermitError::InvalidMaxAge { days }
            );
        }
        for days in [1, 90, 365] {
            assert!(PermitTable::from_yaml_str(&one_entry_table(1, days, &ok_entry)).is_ok());
        }
        assert_eq!(
            PermitTable::from_yaml_str(
                "schema_version: 1\nverification_max_age_days: 90\nendpoints: []\n"
            )
            .unwrap_err(),
            PermitError::EmptyTable
        );
        let dup = format!("{}{}", entry_yaml("p", ""), entry_yaml("p", ""));
        assert_eq!(
            PermitTable::from_yaml_str(&one_entry_table(1, 90, &dup)).unwrap_err(),
            PermitError::DuplicateEndpoint(key("p", "m", "e"))
        );
        // 同一个供应商和模型，不同端点：不是重复
        let other = entry_yaml("p", "").replace("endpoint: e", "endpoint: e2");
        let two = format!("{}{}", entry_yaml("p", ""), other);
        assert_eq!(
            PermitTable::from_yaml_str(&one_entry_table(1, 90, &two))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn yaml_unknown_fields_values_and_missing_fields_are_rejected() {
        let base = one_entry_table(1, 90, &entry_yaml("p", ""));
        let mut cases: Vec<(&str, String)> = vec![
            ("unknown top-level key", format!("{base}extra: 1\n")),
            (
                "unknown entry key",
                one_entry_table(1, 90, &entry_yaml("p", "    enabled: true\n")),
            ),
            (
                "unknown retention",
                base.replace("retention: zdr", "retention: forever"),
            ),
            ("unknown dpa", base.replace("dpa: signed", "dpa: pending")),
            ("unknown access", base.replace("access: api", "access: cli")),
            (
                "unknown data class",
                base.replace("[adult]", "[adult, teen]"),
            ),
            (
                "unknown capability key",
                base.replace("cancellation: true", "cancellation: true, vision: true"),
            ),
            (
                "missing capability",
                base.replace(", cancellation: true", ""),
            ),
            (
                "missing verified_at",
                base.replace("    verified_at: \"2026-11-05\"\n", ""),
            ),
            ("bad verified_at", base.replace("2026-11-05", "05/11/2026")),
            ("bad region", base.replace("[us]", "[usa]")),
            (
                "capability not a bool",
                base.replace("tool_calling: true", "tool_calling: yes please"),
            ),
            ("not yaml", "{{{".to_string()),
        ];
        cases.push((
            "missing schema_version",
            base.replace("schema_version: 1\n", ""),
        ));
        for (name, yaml) in cases {
            assert!(
                PermitTable::from_yaml_str(&yaml).is_err(),
                "{name} should be rejected"
            );
        }
        assert!(matches!(
            PermitTable::from_yaml_str("{{{").unwrap_err(),
            PermitError::Yaml(_)
        ));
        // YAML 里重复的键：serde_yaml 直接报错
        let dup_key = base.replace("retention: zdr", "retention: zdr\n    retention: retained");
        assert!(PermitTable::from_yaml_str(&dup_key).is_err());
    }

    #[test]
    fn entries_must_be_self_consistent() {
        let bad = |extra_from: &str, extra_to: &str| {
            PermitTable::from_yaml_str(&one_entry_table(
                1,
                90,
                &entry_yaml("p", "").replace(extra_from, extra_to),
            ))
        };
        assert_eq!(
            bad("processing_regions: [us]", "processing_regions: []").unwrap_err(),
            PermitError::Entry {
                endpoint: key("p", "m", "e"),
                problem: PermitProblem::NoRegions
            }
        );
        assert!(matches!(
            bad("processing_regions: [us]", "processing_regions: [us, us]").unwrap_err(),
            PermitError::Entry {
                problem: PermitProblem::DuplicateRegion(_),
                ..
            }
        ));
        assert!(matches!(
            bad("[adult]", "[]").unwrap_err(),
            PermitError::Entry {
                problem: PermitProblem::NoDataClasses,
                ..
            }
        ));
        assert!(matches!(
            bad("[adult]", "[adult, adult]").unwrap_err(),
            PermitError::Entry {
                problem: PermitProblem::DuplicateDataClass(DataClass::Adult),
                ..
            }
        ));
        // 声明了 child_edu 的条目自己必须是 zdr + signed
        assert!(bad("[adult]", "[child_edu]").is_ok());
        assert_eq!(bad("[adult]", "[child_edu]").map(|t| t.len()).unwrap(), 1);
        for (retention, dpa) in [
            ("with_exceptions", "signed"),
            ("retained", "signed"),
            ("zdr", "unsigned"),
            ("retained", "unsigned"),
        ] {
            let yaml = entry_yaml("p", "")
                .replace("[adult]", "[child_edu]")
                .replace("retention: zdr", &format!("retention: {retention}"))
                .replace("dpa: signed", &format!("dpa: {dpa}"));
            assert!(
                matches!(
                    PermitTable::from_yaml_str(&one_entry_table(1, 90, &yaml)).unwrap_err(),
                    PermitError::Entry {
                        problem: PermitProblem::ChildEduRequiresZdrAndSignedDpa { .. },
                        ..
                    }
                ),
                "{retention}/{dpa}"
            );
        }
        // 备注与证据长度
        let long_notes = format!("    notes: \"{}\"\n", "x".repeat(MAX_NOTES_CHARS + 1));
        assert!(matches!(
            PermitTable::from_yaml_str(&one_entry_table(1, 90, &entry_yaml("p", &long_notes)))
                .unwrap_err(),
            PermitError::Entry {
                problem: PermitProblem::NotesTooLong,
                ..
            }
        ));
        let ok_notes = format!("    notes: \"{}\"\n", "x".repeat(MAX_NOTES_CHARS));
        assert!(
            PermitTable::from_yaml_str(&one_entry_table(1, 90, &entry_yaml("p", &ok_notes)))
                .is_ok()
        );
        let empty_evidence = "    evidence: [\"\"]\n";
        assert!(matches!(
            PermitTable::from_yaml_str(&one_entry_table(1, 90, &entry_yaml("p", empty_evidence)))
                .unwrap_err(),
            PermitError::Entry {
                problem: PermitProblem::BadEvidence,
                ..
            }
        ));
    }

    // ---------- 核验过期自动停用 ----------

    #[test]
    fn verification_expiry_boundaries_are_exact() {
        let t = table();
        let e = &t.endpoints()[0];
        assert_eq!(e.verified_at, TimestampMs(VERIFIED));
        let expires = t.expires_at(e).unwrap();
        assert_eq!(expires, TimestampMs(VERIFIED + 90 * DAY));
        // (now, 是否有效)
        for (now, valid) in [
            (VERIFIED - 1, false), // 核验时间还没到
            (VERIFIED, true),      // 刚好等于核验时间
            (VERIFIED + 1, true),
            (VERIFIED + 90 * DAY - 1, true), // 到期前 1 毫秒
            (VERIFIED + 90 * DAY, false),    // 刚好到期：右端开，已停用
            (VERIFIED + 90 * DAY + 1, false),
            (VERIFIED + 10_000 * DAY, false),
        ] {
            assert_eq!(t.is_verified_at(e, TimestampMs(now)), valid, "now={now}");
        }
        assert_eq!(
            t.verification_state(e, TimestampMs(VERIFIED + 90 * DAY)),
            Some(ExcludeReason::VerificationExpired {
                verified_at: TimestampMs(VERIFIED),
                expired_at: TimestampMs(VERIFIED + 90 * DAY)
            })
        );
        assert_eq!(
            t.verification_state(e, TimestampMs(VERIFIED - 1)),
            Some(ExcludeReason::VerifiedInTheFuture {
                verified_at: TimestampMs(VERIFIED)
            })
        );
    }

    #[test]
    fn an_expired_endpoint_is_never_selected_even_if_everything_else_is_perfect() {
        let t = table();
        let candidates = [key(
            "example-zdr-host",
            "example-org/chat-70b",
            "openai-compat",
        )];
        let r = req(DeploymentForm::Cloud, tenant(&[DataClass::ChildEdu]));
        assert!(select_compliant(&t, &candidates, &r, now()).is_ok());
        let late = TimestampMs(VERIFIED + 90 * DAY);
        let err = select_compliant(&t, &candidates, &r, late).unwrap_err();
        assert_eq!(err.excluded.len(), 1);
        assert!(matches!(
            err.excluded[0].reasons.as_slice(),
            [ExcludeReason::VerificationExpired { .. }]
        ));
        assert_eq!(err.reason_code(), ReasonCode::NoCompliantEndpoint);
    }

    #[test]
    fn a_shorter_max_age_expires_sooner() {
        let one_day = one_entry_table(1, 1, &entry_yaml("p", ""));
        let t = PermitTable::from_yaml_str(&one_day).unwrap();
        let e = &t.endpoints()[0];
        assert!(t.is_verified_at(e, TimestampMs(VERIFIED + DAY - 1)));
        assert!(!t.is_verified_at(e, TimestampMs(VERIFIED + DAY)));
    }

    #[test]
    fn verification_overflow_is_rejected_at_load_time() {
        let far = entry_yaml("p", "").replace("\"2026-11-05\"", "\"9999-12-31\"");
        let loaded = PermitTable::from_yaml_str(&one_entry_table(1, 365, &far));
        assert!(loaded.is_ok(), "9999 年 + 365 天仍在 i64 毫秒范围内");
        // 直接构造一个会溢出的条目
        let mut entry = table().endpoints()[0].clone();
        entry.verified_at = TimestampMs(i64::MAX - 1);
        let file = PermitTableFile {
            schema_version: 1,
            verification_max_age_days: 1,
            endpoints: vec![entry],
        };
        assert!(matches!(
            PermitTable::from_file(file).unwrap_err(),
            PermitError::Entry {
                problem: PermitProblem::VerificationOverflow,
                ..
            }
        ));
    }

    // ---------- child_edu 只能落到 ZDR + 已签 DPA ----------

    #[test]
    fn child_edu_only_lands_on_zdr_plus_signed_dpa() {
        let t = table();
        let all_keys: Vec<EndpointKey> = t.endpoints().iter().map(EndpointPermit::key).collect();
        let r = req(DeploymentForm::Cloud, tenant(&[DataClass::ChildEdu]));
        let chosen = select_compliant(&t, &all_keys, &r, now()).unwrap();
        assert!(!chosen.is_empty());
        for e in chosen.iter() {
            assert_eq!(e.retention, Retention::Zdr, "{}", e.key());
            assert_eq!(e.dpa, Dpa::Signed, "{}", e.key());
            assert!(e.allowed_data_classes.contains(&DataClass::ChildEdu));
        }
        // 反过来：所有「核验没过期、zdr、已签 DPA、列了 child_edu」的条目都在结果里，别的都不在
        let expected: Vec<EndpointKey> = t
            .endpoints()
            .iter()
            .filter(|e| t.is_verified_at(e, now()))
            .filter(|e| {
                e.retention == Retention::Zdr
                    && e.dpa == Dpa::Signed
                    && e.allowed_data_classes.contains(&DataClass::ChildEdu)
            })
            .map(EndpointPermit::key)
            .collect();
        assert_eq!(
            expected.len(),
            2,
            "示例里有 2 个合格且未过期的（第 7 个已过期）"
        );
        let got: Vec<EndpointKey> = chosen.iter().map(|e| e.key()).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn child_edu_is_enforced_at_route_time_even_if_a_table_entry_somehow_lies() {
        // 纵深防御：绕过加载校验，手工造一张「撒谎」的表（条目声称允许 child_edu 但留存是 retained）。
        let mut t = table();
        let lie = t
            .endpoints
            .iter_mut()
            .find(|e| e.retention == Retention::Retained)
            .unwrap();
        lie.allowed_data_classes.push(DataClass::ChildEdu);
        let lie_key = lie.key();
        let r = req(DeploymentForm::Cloud, tenant(&[DataClass::ChildEdu]));
        let err = select_compliant(&t, std::slice::from_ref(&lie_key), &r, now()).unwrap_err();
        let reasons = &err.excluded[0].reasons;
        assert!(reasons.contains(&ExcludeReason::RetentionTooWeak {
            have: Retention::Retained,
            need: Retention::Zdr
        }));
        assert!(reasons.contains(&ExcludeReason::DpaRequired));
        // 同样地：with_exceptions（Safety Retention 之类）也不行
        let mut t = table();
        let e = t
            .endpoints
            .iter_mut()
            .find(|e| e.retention == Retention::WithExceptions)
            .unwrap();
        e.allowed_data_classes.push(DataClass::ChildEdu);
        e.dpa = Dpa::Signed;
        let k = e.key();
        let err = select_compliant(&t, std::slice::from_ref(&k), &r, now()).unwrap_err();
        assert!(
            err.excluded[0]
                .reasons
                .contains(&ExcludeReason::RetentionTooWeak {
                    have: Retention::WithExceptions,
                    need: Retention::Zdr
                })
        );
        // zdr 但 DPA 未签：也不行
        let mut t = table();
        let e = t
            .endpoints
            .iter_mut()
            .find(|e| e.retention == Retention::Zdr)
            .unwrap();
        e.dpa = Dpa::Unsigned;
        let k = e.key();
        let err = select_compliant(&t, std::slice::from_ref(&k), &r, now()).unwrap_err();
        assert!(
            err.excluded[0]
                .reasons
                .contains(&ExcludeReason::DpaRequired)
        );
    }

    #[test]
    fn a_tenants_own_laxer_settings_cannot_loosen_child_edu() {
        // 租户自己写 max_retention=retained、require_dpa=false，但含 child_edu：仍然固定 zdr + 已签
        let lax = TenantDataPolicy::new(
            classes(&[DataClass::ChildEdu]),
            None,
            Retention::Retained,
            false,
        )
        .unwrap();
        assert!(lax.handles_child_data());
        assert_eq!(lax.effective_max_retention(), Retention::Zdr);
        assert!(lax.effective_require_dpa());
        // 不含 child_edu 才听租户自己的
        let adult = TenantDataPolicy::new(
            classes(&[DataClass::Adult]),
            None,
            Retention::WithExceptions,
            false,
        )
        .unwrap();
        assert!(!adult.handles_child_data());
        assert_eq!(adult.effective_max_retention(), Retention::WithExceptions);
        assert!(!adult.effective_require_dpa());
        // 多类别租户，只要含 child_edu 就按最严的来
        let mixed = TenantDataPolicy::new(
            classes(&[DataClass::ChildEdu, DataClass::Adult]),
            None,
            Retention::Retained,
            false,
        )
        .unwrap();
        assert_eq!(mixed.effective_max_retention(), Retention::Zdr);
    }

    // ---------- 数据类别 ⊆ ----------

    #[test]
    fn tenant_data_classes_must_be_a_subset_of_the_endpoints() {
        let t = table();
        let official = key("example-official", "example-org/chat-70b", "official-api"); // adult, public
        for (tenant_classes, ok) in [
            (vec![DataClass::Adult], true),
            (vec![DataClass::Public], true),
            (vec![DataClass::Adult, DataClass::Public], true),
            (vec![DataClass::ChildEdu], false),
            (vec![DataClass::ChildEdu, DataClass::Adult], false), // 只要有一个不在，整体不行
        ] {
            let r = req(
                DeploymentForm::Cloud,
                TenantDataPolicy::new(classes(&tenant_classes), None, Retention::Retained, false)
                    .unwrap(),
            );
            let got = select_compliant(&t, std::slice::from_ref(&official), &r, now());
            assert_eq!(got.is_ok(), ok, "{tenant_classes:?}");
            if !ok {
                let reasons = &got.unwrap_err().excluded[0].reasons;
                assert!(reasons.iter().any(|x| matches!(x, ExcludeReason::DataClassNotAllowed { missing } if missing.contains(&DataClass::ChildEdu))));
            }
        }
    }

    // ---------- 区域 / 留存 ----------

    #[test]
    fn regions_must_all_be_allowed() {
        let t = table();
        let multi = key("example-router", "example-org/chat-70b", "zdr-route"); // regions [us, eu]
        for (allowed, ok) in [
            (None, true),
            (Some(regions(&["us", "eu"])), true),
            (Some(regions(&["us", "eu", "sg"])), true),
            (Some(regions(&["us"])), false), // eu 不被允许：只要有一个不允许就不行
            (Some(regions(&["eu"])), false),
            (Some(regions(&["sg"])), false),
        ] {
            let tenant = TenantDataPolicy::new(
                classes(&[DataClass::Adult]),
                allowed.clone(),
                Retention::Retained,
                false,
            )
            .unwrap();
            let got = select_compliant(
                &t,
                std::slice::from_ref(&multi),
                &req(DeploymentForm::Cloud, tenant),
                now(),
            );
            assert_eq!(got.is_ok(), ok, "{allowed:?}");
        }
        let tenant = TenantDataPolicy::new(
            classes(&[DataClass::Adult]),
            Some(regions(&["us"])),
            Retention::Retained,
            false,
        )
        .unwrap();
        let err = select_compliant(
            &t,
            std::slice::from_ref(&multi),
            &req(DeploymentForm::Cloud, tenant),
            now(),
        )
        .unwrap_err();
        assert_eq!(
            err.excluded[0].reasons,
            vec![ExcludeReason::RegionNotAllowed {
                regions: vec![Region::new("eu").unwrap()]
            }]
        );
    }

    #[test]
    fn retention_must_not_be_worse_than_the_tenant_allows() {
        let t = table();
        let exceptions = key("example-router", "example-org/chat-70b", "default-route"); // with_exceptions
        for (max, ok) in [
            (Retention::Zdr, false),
            (Retention::WithExceptions, true),
            (Retention::Retained, true),
        ] {
            let tenant =
                TenantDataPolicy::new(classes(&[DataClass::Adult]), None, max, false).unwrap();
            let got = select_compliant(
                &t,
                std::slice::from_ref(&exceptions),
                &req(DeploymentForm::Cloud, tenant),
                now(),
            );
            assert_eq!(got.is_ok(), ok, "{max:?}");
        }
        // 满足顺序：zdr < with_exceptions < retained
        assert!(Retention::Zdr.satisfies(Retention::Zdr));
        assert!(Retention::Zdr.satisfies(Retention::Retained));
        assert!(!Retention::Retained.satisfies(Retention::WithExceptions));
        assert!(!Retention::WithExceptions.satisfies(Retention::Zdr));
    }

    #[test]
    fn require_dpa_applies_to_non_child_tenants_when_asked() {
        let t = table();
        let official = key("example-official", "example-org/chat-70b", "official-api"); // unsigned
        let with = |require| {
            TenantDataPolicy::new(
                classes(&[DataClass::Adult]),
                None,
                Retention::Retained,
                require,
            )
            .unwrap()
        };
        assert!(
            select_compliant(
                &t,
                std::slice::from_ref(&official),
                &req(DeploymentForm::Cloud, with(false)),
                now()
            )
            .is_ok()
        );
        let err = select_compliant(
            &t,
            std::slice::from_ref(&official),
            &req(DeploymentForm::Cloud, with(true)),
            now(),
        )
        .unwrap_err();
        assert_eq!(err.excluded[0].reasons, vec![ExcludeReason::DpaRequired]);
    }

    // ---------- 流式 usage 与能力 ----------

    #[test]
    fn paid_or_subsidized_streaming_needs_stream_usage() {
        let t = table();
        let no_usage = key(
            "example-cheap-host",
            "example-org/small-8b",
            "openai-compat",
        ); // stream_usage false, public
        let public = tenant(&[DataClass::Public]);
        // (stream, billing, 期望通过)
        for (stream, billing, ok) in [
            (false, Billing::Paid, true),
            (false, Billing::Subsidized, true),
            (false, Billing::Unbilled, true),
            (true, Billing::Paid, false),       // 付费流式：不行
            (true, Billing::Subsidized, false), // 补贴流式：不行
            (true, Billing::Unbilled, true),    // 不计费的流式：可以
        ] {
            let mut r = req(DeploymentForm::Cloud, public.clone());
            r.stream = stream;
            r.billing = billing;
            let got = select_compliant(&t, std::slice::from_ref(&no_usage), &r, now());
            assert_eq!(got.is_ok(), ok, "stream={stream} {billing:?}");
            if !ok {
                assert_eq!(
                    got.unwrap_err().excluded[0].reasons,
                    vec![ExcludeReason::StreamUsageRequired]
                );
            }
        }
        // 支持流式 usage 的端点，付费流式没问题
        let with_usage = key("example-zdr-host", "example-org/chat-70b", "openai-compat");
        let mut r = req(DeploymentForm::Cloud, public);
        r.stream = true;
        assert!(select_compliant(&t, std::slice::from_ref(&with_usage), &r, now()).is_ok());
    }

    #[test]
    fn explicit_capability_needs_are_hard_requirements() {
        let t = table();
        let k = key("example-zdr-host", "example-org/chat-70b", "openai-compat"); // cache_pricing false
        let mut r = req(DeploymentForm::Cloud, tenant(&[DataClass::Adult]));
        r.needs.tool_calling = true;
        assert!(select_compliant(&t, std::slice::from_ref(&k), &r, now()).is_ok());
        r.needs.cache_pricing = true;
        let err = select_compliant(&t, std::slice::from_ref(&k), &r, now()).unwrap_err();
        assert_eq!(
            err.excluded[0].reasons,
            vec![ExcludeReason::MissingCapability {
                capability: CapabilityKind::CachePricing
            }]
        );
        // 显式要求流式 usage、但请求不是付费流式：缺了就是 MissingCapability（不是策略原因）
        let no_usage = key(
            "example-cheap-host",
            "example-org/small-8b",
            "openai-compat",
        );
        let mut r = req(DeploymentForm::Cloud, tenant(&[DataClass::Public]));
        r.needs.stream_usage = true;
        let err = select_compliant(&t, std::slice::from_ref(&no_usage), &r, now()).unwrap_err();
        assert_eq!(
            err.excluded[0].reasons,
            vec![ExcludeReason::MissingCapability {
                capability: CapabilityKind::StreamUsage
            }]
        );
    }

    #[test]
    fn required_capabilities_union_of_needs_and_the_streaming_policy() {
        let mut r = req(DeploymentForm::Cloud, tenant(&[DataClass::Public]));
        assert!(r.required_capabilities().is_empty());
        r.stream = true;
        assert_eq!(
            r.required_capabilities(),
            [CapabilityKind::StreamUsage].into_iter().collect()
        );
        r.billing = Billing::Unbilled;
        assert!(r.required_capabilities().is_empty());
        r.needs.structured_output = true;
        r.needs.cancellation = true;
        assert_eq!(
            r.required_capabilities(),
            [
                CapabilityKind::StructuredOutput,
                CapabilityKind::Cancellation
            ]
            .into_iter()
            .collect()
        );
    }

    // ---------- 云形态排除 subscription ----------

    #[test]
    fn cloud_excludes_the_subscription_path_other_forms_do_not() {
        let t = table();
        let cli = key("example-cli", "example-org/chat-70b", "subscription-cli");
        let adult = tenant(&[DataClass::Adult]);
        for (form, ok) in [
            (DeploymentForm::Cloud, false),
            (DeploymentForm::Organization, true),
            (DeploymentForm::Personal, true),
        ] {
            let got = select_compliant(
                &t,
                std::slice::from_ref(&cli),
                &req(form, adult.clone()),
                now(),
            );
            assert_eq!(got.is_ok(), ok, "{form:?}");
            if !ok {
                assert!(
                    got.unwrap_err().excluded[0]
                        .reasons
                        .contains(&ExcludeReason::SubscriptionNotAllowedInCloud)
                );
            }
        }
    }

    // ---------- 没有合规候选就拒绝；兜底同样受约束 ----------

    #[test]
    fn no_compliant_candidate_is_an_explicit_refusal_not_an_empty_list() {
        let t = table();
        let r = req(DeploymentForm::Cloud, tenant(&[DataClass::ChildEdu]));
        // 空候选
        let err = select_compliant(&t, &[], &r, now()).unwrap_err();
        assert_eq!(err.considered, 0);
        assert!(err.excluded.is_empty());
        assert_eq!(err.reason_code(), ReasonCode::NoCompliantEndpoint);
        // 全部表外：失败即关闭
        let ghost = key("ghost", "m", "e");
        let err = select_compliant(&t, std::slice::from_ref(&ghost), &r, now()).unwrap_err();
        assert_eq!(
            err.excluded,
            vec![Exclusion {
                endpoint: ghost,
                reasons: vec![ExcludeReason::NotInTable]
            }]
        );
        // 全部不合规：每个候选的原因都在
        let bad = [
            key("example-official", "example-org/chat-70b", "official-api"),
            key("example-router", "example-org/chat-70b", "default-route"),
        ];
        let err = select_compliant(&t, &bad, &r, now()).unwrap_err();
        assert_eq!(err.considered, 2);
        assert_eq!(err.excluded.len(), 2);
        assert!(err.excluded.iter().all(|x| !x.reasons.is_empty()));
    }

    #[test]
    fn fallbacks_go_through_the_same_filter_and_order_is_preserved() {
        let t = table();
        let r = req(DeploymentForm::Cloud, tenant(&[DataClass::ChildEdu]));
        // 偏好顺序：[不合规的首选, 合规的兜底 1, 表外的兜底, 合规的兜底 2]
        let candidates = [
            key("example-official", "example-org/chat-70b", "official-api"),
            key("example-router", "example-org/chat-70b", "zdr-route"),
            key("ghost", "m", "e"),
            key("example-zdr-host", "example-org/chat-70b", "openai-compat"),
        ];
        let chosen = select_compliant(&t, &candidates, &r, now()).unwrap();
        let got: Vec<String> = chosen
            .iter()
            .map(|e| e.endpoint.as_str().to_string())
            .collect();
        assert_eq!(got, ["zdr-route", "openai-compat"]);
        assert_eq!(chosen.first().endpoint.as_str(), "zdr-route");
        assert_eq!(chosen.len(), 2);
        // 重复候选只保留第一个
        let dup = [candidates[1].clone(), candidates[1].clone()];
        assert_eq!(select_compliant(&t, &dup, &r, now()).unwrap().len(), 1);
        assert_eq!(
            select_compliant(&t, &dup, &r, now())
                .unwrap()
                .into_vec()
                .len(),
            1
        );
    }

    #[test]
    fn every_reason_for_one_endpoint_is_reported_together() {
        let t = table();
        let official = key("example-official", "example-org/chat-70b", "official-api"); // retained, unsigned, [cn], adult+public, stream_usage?
        let tenant = TenantDataPolicy::new(
            classes(&[DataClass::ChildEdu]),
            Some(regions(&["th"])),
            Retention::Zdr,
            true,
        )
        .unwrap();
        let mut r = req(DeploymentForm::Cloud, tenant);
        r.stream = true;
        r.needs.cache_pricing = true;
        let late = TimestampMs(VERIFIED + 365 * DAY);
        let err = select_compliant(&t, std::slice::from_ref(&official), &r, late).unwrap_err();
        let kinds: Vec<&str> = err.excluded[0]
            .reasons
            .iter()
            .map(|reason| match reason {
                ExcludeReason::VerificationExpired { .. } => "expired",
                ExcludeReason::DataClassNotAllowed { .. } => "classes",
                ExcludeReason::RetentionTooWeak { .. } => "retention",
                ExcludeReason::DpaRequired => "dpa",
                ExcludeReason::RegionNotAllowed { .. } => "region",
                ExcludeReason::StreamUsageRequired => "stream",
                ExcludeReason::MissingCapability { .. } => "capability",
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "expired",
                "classes",
                "retention",
                "dpa",
                "region",
                "stream",
                "capability"
            ]
        );
    }

    // ---------- 租户策略与其它类型的校验 ----------

    #[test]
    fn tenant_policy_validation_and_serde() {
        assert_eq!(
            TenantDataPolicy::new(BTreeSet::new(), None, Retention::Zdr, false).unwrap_err(),
            PermitProblem::NoDataClasses
        );
        assert_eq!(
            TenantDataPolicy::new(
                classes(&[DataClass::Adult]),
                Some(BTreeSet::new()),
                Retention::Zdr,
                false
            )
            .unwrap_err(),
            PermitProblem::NoRegions
        );
        let p = TenantDataPolicy::new(
            classes(&[DataClass::Adult, DataClass::Public]),
            Some(regions(&["sg", "us"])),
            Retention::WithExceptions,
            true,
        )
        .unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(
            json,
            r#"{"data_classes":["adult","public"],"allowed_regions":["sg","us"],"max_retention":"with_exceptions","require_dpa":true}"#
        );
        assert_eq!(serde_json::from_str::<TenantDataPolicy>(&json).unwrap(), p);
        assert!(serde_json::from_str::<TenantDataPolicy>(r#"{"data_classes":[],"allowed_regions":null,"max_retention":"zdr","require_dpa":false}"#).is_err());
        assert!(serde_json::from_str::<TenantDataPolicy>(r#"{"data_classes":["adult"],"allowed_regions":null,"max_retention":"zdr","require_dpa":false,"x":1}"#).is_err());
    }

    #[test]
    fn wire_value_strings_are_pinned() {
        for (value, text) in [
            (
                serde_json::to_value(DataClass::ChildEdu).unwrap(),
                "child_edu",
            ),
            (serde_json::to_value(DataClass::Adult).unwrap(), "adult"),
            (serde_json::to_value(DataClass::Public).unwrap(), "public"),
            (serde_json::to_value(Retention::Zdr).unwrap(), "zdr"),
            (
                serde_json::to_value(Retention::WithExceptions).unwrap(),
                "with_exceptions",
            ),
            (
                serde_json::to_value(Retention::Retained).unwrap(),
                "retained",
            ),
            (serde_json::to_value(Dpa::Signed).unwrap(), "signed"),
            (serde_json::to_value(Dpa::Unsigned).unwrap(), "unsigned"),
            (serde_json::to_value(AccessPath::Api).unwrap(), "api"),
            (
                serde_json::to_value(AccessPath::Subscription).unwrap(),
                "subscription",
            ),
            (serde_json::to_value(Billing::Paid).unwrap(), "paid"),
            (
                serde_json::to_value(Billing::Subsidized).unwrap(),
                "subsidized",
            ),
            (serde_json::to_value(Billing::Unbilled).unwrap(), "unbilled"),
            (
                serde_json::to_value(CapabilityKind::ToolCalling).unwrap(),
                "tool_calling",
            ),
            (
                serde_json::to_value(CapabilityKind::StreamUsage).unwrap(),
                "stream_usage",
            ),
            (
                serde_json::to_value(CapabilityKind::CachePricing).unwrap(),
                "cache_pricing",
            ),
            (
                serde_json::to_value(CapabilityKind::StructuredOutput).unwrap(),
                "structured_output",
            ),
            (
                serde_json::to_value(CapabilityKind::Cancellation).unwrap(),
                "cancellation",
            ),
        ] {
            assert_eq!(value, serde_json::json!(text));
        }
    }

    #[test]
    fn non_empty_cannot_be_empty() {
        assert!(NonEmpty::<u8>::from_vec(vec![]).is_none());
        let n = NonEmpty::from_vec(vec![1, 2]).unwrap();
        assert_eq!((n.len(), n.is_empty(), *n.first()), (2, false, 1));
        assert_eq!(n.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(n.into_vec(), vec![1, 2]);
        let single = NonEmpty::from_vec(vec![7]).unwrap();
        assert_eq!((single.len(), *single.first()), (1, 7));
    }
}

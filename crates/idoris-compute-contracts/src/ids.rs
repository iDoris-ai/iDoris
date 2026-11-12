//! 标识符与时间的 newtype。
//!
//! 目的有两个：
//!
//! 1. **类型区分**：把 [`AccountId`] 传给要 [`MembershipId`] 的函数，编译不过。
//!    （见各 newtype 上的 `compile_fail` 文档测试。）
//! 2. **字符集收紧**：id 会进日志、响应头、SQL 键和 `QUOTA:<节点>` 这样的复合字符串，
//!    所以统一只允许 URL/头安全的字符，并且禁止 `@`、空白、`/`、`#`：假名 id 里
//!    出现邮箱形状的值，几乎一定是把个人信息漏进来了（设计稿 §3「假名 id」、§9
//!    「iDoris 不存孩子的姓名、年龄」）。这只是形状检查，证明不了「确实是假名」。
//!
//! 反序列化走 `TryFrom<String>`，所以从 JSON/YAML/数据库读出来的 id 一样会被校验。
//!
//! ```compile_fail
//! use idoris_compute_contracts::ids::{AccountId, MembershipId};
//! fn needs_membership(_: &MembershipId) {}
//! let account = AccountId::new("acct-1").unwrap();
//! needs_membership(&account); // 不同的 newtype，不能互相传
//! ```
//!
//! 对照组：同一段代码传对类型就能编译，说明上面的失败确实是类型不匹配造成的。
//!
//! ```
//! use idoris_compute_contracts::ids::MembershipId;
//! fn needs_membership(_: &MembershipId) {}
//! let membership = MembershipId::new("mem-1").unwrap();
//! needs_membership(&membership);
//! ```

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 一般 id 的最大长度（字节）。
pub const MAX_ID_LEN: usize = 128;
/// 幂等键的最大长度：装得下 `subsidy:` + 主体 id（<=128）+ `:YYYY-MM`。
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 192;
/// 节点 id 的最大长度。节点 id 会出现在 `QUOTA:<节点>` 里，所以更短、更严。
pub const MAX_NODE_ID_LEN: usize = 64;

/// 标识符校验失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },
    #[error("{kind} is too long: {len} bytes > {max}")]
    TooLong {
        kind: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{kind} contains invalid character {ch:?} at byte {index}")]
    InvalidChar {
        kind: &'static str,
        ch: char,
        index: usize,
    },
    #[error("invalid attempt id {0:?}: expected \"<request_id>#<index>\"")]
    InvalidAttemptId(String),
    #[error("invalid billing month {0:?}: expected \"YYYY-MM\" with year 2000..=9999")]
    InvalidBillingMonth(String),
}

fn validate_token(
    value: &str,
    kind: &'static str,
    max: usize,
    allow_colon_tilde: bool,
) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if value.len() > max {
        return Err(IdError::TooLong {
            kind,
            len: value.len(),
            max,
        });
    }
    for (index, ch) in value.char_indices() {
        let ok = ch.is_ascii_alphanumeric()
            || matches!(ch, '.' | '_' | '-')
            || (allow_colon_tilde && matches!(ch, ':' | '~'));
        if !ok {
            return Err(IdError::InvalidChar { kind, ch, index });
        }
    }
    Ok(())
}

macro_rules! token_id {
    (
        $(#[$meta:meta])*
        $name:ident, kind = $kind:literal, max = $max:expr, colon_tilde = $ct:expr
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// 校验并构造；规则见模块文档。
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_token(&value, $kind, $max, $ct)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(value: String) -> Result<Self, IdError> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

token_id!(
    /// 主体（Principal）：开通方提供的稳定假名 id（设计稿 §3）。
    /// 例：eduai 的学生 id 哈希、Agent24 云账号。不透明，不含任何可还原身份的信息。
    PrincipalId, kind = "principal id", max = MAX_ID_LEN, colon_tilde = true
);
token_id!(
    /// 成员关系（Membership）id。一个主体可以有多个成员关系。
    MembershipId, kind = "membership id", max = MAX_ID_LEN, colon_tilde = true
);
token_id!(
    /// 付款账户（Account）id。
    AccountId, kind = "account id", max = MAX_ID_LEN, colon_tilde = true
);
token_id!(
    /// 凭证（Credential）id。注意这是凭证的**标识**，不是密钥本身；密钥不出现在任何契约类型里。
    CredentialId, kind = "credential id", max = MAX_ID_LEN, colon_tilde = true
);
token_id!(
    /// 额度（Allocation）id。
    AllocationId, kind = "allocation id", max = MAX_ID_LEN, colon_tilde = true
);
token_id!(
    /// 一次用户请求的 id（对应 `X-iDoris-Request-Id` / Record-Id 一类的关联 id）。
    RequestId, kind = "request id", max = MAX_ID_LEN, colon_tilde = true
);
token_id!(
    /// 节点 id，只出现在 `QUOTA:<节点>` 里。不允许 `:`，否则复合字符串无法无歧义地拆开。
    NodeId, kind = "node id", max = MAX_NODE_ID_LEN, colon_tilde = false
);
token_id!(
    /// 供应商 id（许可表里的 `provider`），如 `deepinfra`。
    ProviderId, kind = "provider id", max = MAX_ID_LEN, colon_tilde = false
);
token_id!(
    /// 幂等键。免费额度发放用 `subsidy:<主体>:<YYYY-MM>`（设计稿 §4：幂等键 `(成员, 月份)`），
    /// 见 [`crate::account::subsidy_idempotency_key`]。比一般 id 长，因为要装下一个主体 id。
    IdempotencyKey, kind = "idempotency key", max = MAX_IDEMPOTENCY_KEY_LEN, colon_tilde = true
);
token_id!(
    /// 价格版本：一次尝试计价所用价目表的版本标签（设计稿 §6：每次尝试记录「价格版本」）。
    /// 价目表变更后，已经发生的尝试仍按当时的版本结算和对账。
    PriceVersion, kind = "price version", max = MAX_NODE_ID_LEN, colon_tilde = true
);
token_id!(
    /// 具体端点名（许可表里的 `endpoint`）：同一个供应商、同一个模型下，不同的 API 面或路由方式，
    /// 例如 `openrouter-zdr` 与 `openrouter-default` 是两个端点，留存不同。只是标签，不含 URL。
    EndpointName, kind = "endpoint name", max = MAX_ID_LEN, colon_tilde = false
);

/// 模型 id。
///
/// 供应商的模型名里常见 `/` 和 `:`（`deepseek-ai/DeepSeek-V3.2`、`qwen3:32b`），所以字符集比
/// 其它 id 宽：在 `A-Za-z0-9 . _ - : ~ /` 之间。仍然禁止空白、`@`、`#`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelId(String);

impl ModelId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        const KIND: &str = "model id";
        if value.is_empty() {
            return Err(IdError::Empty { kind: KIND });
        }
        if value.len() > MAX_ID_LEN {
            return Err(IdError::TooLong {
                kind: KIND,
                len: value.len(),
                max: MAX_ID_LEN,
            });
        }
        for (index, ch) in value.char_indices() {
            let ok = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ':' | '~' | '/');
            if !ok {
                return Err(IdError::InvalidChar {
                    kind: KIND,
                    ch,
                    index,
                });
            }
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ModelId {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, IdError> {
        Self::new(value)
    }
}
impl From<ModelId> for String {
    fn from(value: ModelId) -> String {
        value.0
    }
}
impl FromStr for ModelId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::new(s)
    }
}
impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 端点的键：供应商 + 模型 + 具体端点（设计稿 §8 许可表的「端点」列）。
///
/// 许可表里的条目、路由候选、尝试记录里的「供应商、模型」都用它标识，三处对得上才算同一个端点。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointKey {
    pub provider: ProviderId,
    pub model: ModelId,
    pub endpoint: EndpointName,
}

impl EndpointKey {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Result<Self, IdError> {
        Ok(Self {
            provider: ProviderId::new(provider)?,
            model: ModelId::new(model)?,
            endpoint: EndpointName::new(endpoint)?,
        })
    }
}

impl fmt::Display for EndpointKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}@{}", self.provider, self.model, self.endpoint)
    }
}

/// 请求内第几次尝试（从 0 开始）。一次请求的尝试数有硬上限，见
/// [`crate::request::MAX_ATTEMPTS_PER_REQUEST`]，所以 `u8` 够用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptIndex(pub u8);

/// 尝试 id = 请求 id + 尝试序号。线上形式 `"<request_id>#<index>"`。
///
/// 「用尝试 ID 和不可变账目做幂等游标」（设计稿 §9 对账）：尝试 id 由请求 id 和序号确定，
/// 同一次尝试无论被重放多少次，id 都一样；`#` 不在 id 字符集里，所以拆分无歧义。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AttemptId {
    request: RequestId,
    index: AttemptIndex,
}

impl AttemptId {
    pub fn new(request: RequestId, index: AttemptIndex) -> Self {
        Self { request, index }
    }

    pub fn request(&self) -> &RequestId {
        &self.request
    }

    pub fn index(&self) -> AttemptIndex {
        self.index
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.request, self.index.0)
    }
}

impl FromStr for AttemptId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        let (request, index) = s
            .rsplit_once('#')
            .ok_or_else(|| IdError::InvalidAttemptId(s.to_string()))?;
        // 序号只接受规范的十进制：不接受 `+1`、前导零、空串。
        let canonical = !index.is_empty()
            && index.bytes().all(|b| b.is_ascii_digit())
            && (index == "0" || !index.starts_with('0'));
        if !canonical {
            return Err(IdError::InvalidAttemptId(s.to_string()));
        }
        let index: u8 = index
            .parse()
            .map_err(|_| IdError::InvalidAttemptId(s.to_string()))?;
        Ok(Self {
            request: RequestId::new(request)?,
            index: AttemptIndex(index),
        })
    }
}

impl TryFrom<String> for AttemptId {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, IdError> {
        value.parse()
    }
}

impl From<AttemptId> for String {
    fn from(value: AttemptId) -> String {
        value.to_string()
    }
}

/// UTC 毫秒时间戳（自 Unix epoch）。与 `idoris-tenancy` 的 `now_ms` 同一口径。
///
/// 本 crate 里所有依赖「现在」的函数都显式接收 `now: TimestampMs`，没有任何地方读系统时钟：
/// 这样纯函数才是纯的，边界（刚好过期那一毫秒）才测得出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimestampMs(pub i64);

impl TimestampMs {
    pub const fn new(ms: i64) -> Self {
        Self(ms)
    }

    pub const fn millis(self) -> i64 {
        self.0
    }

    /// 溢出返回 `None`，不回绕、不 panic。
    pub fn checked_add_ms(self, delta_ms: i64) -> Option<Self> {
        self.0.checked_add(delta_ms).map(Self)
    }

    /// `self - earlier`（毫秒）；溢出返回 `None`。
    pub fn checked_since(self, earlier: Self) -> Option<i64> {
        self.0.checked_sub(earlier.0)
    }
}

/// 账期（月），线上形式 `"YYYY-MM"`，与 `idoris-tenancy` 的 `billing_period_key` 输出同形。
///
/// 这里**不做时区换算**：账期按租户的 `billing_timezone` 算出来是 tenancy 的事，本类型只负责
/// 把算出来的结果当作有类型的键用（免费额度幂等键、宽限次数键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BillingMonth {
    year: u16,
    month: u8,
}

impl BillingMonth {
    pub fn new(year: u16, month: u8) -> Result<Self, IdError> {
        if !(2000..=9999).contains(&year) || !(1..=12).contains(&month) {
            return Err(IdError::InvalidBillingMonth(format!(
                "{year:04}-{month:02}"
            )));
        }
        Ok(Self { year, month })
    }

    pub fn year(self) -> u16 {
        self.year
    }

    pub fn month(self) -> u8 {
        self.month
    }
}

impl fmt::Display for BillingMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

impl FromStr for BillingMonth {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        let bad = || IdError::InvalidBillingMonth(s.to_string());
        let bytes = s.as_bytes();
        // 严格 `YYYY-MM`：恰好 7 个字节、全是 ASCII 数字加一个 `-`。
        let well_formed = bytes.len() == 7
            && bytes[4] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || b.is_ascii_digit());
        if !well_formed {
            return Err(bad());
        }
        let year: u16 = s[0..4].parse().map_err(|_| bad())?;
        let month: u8 = s[5..7].parse().map_err(|_| bad())?;
        Self::new(year, month)
    }
}

impl TryFrom<String> for BillingMonth {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, IdError> {
        value.parse()
    }
}

impl From<BillingMonth> for String {
    fn from(value: BillingMonth) -> String {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn valid_tokens_round_trip_through_json() {
        for raw in [
            "a",
            "acct-1",
            "eduai:3fa9c0",
            "A.b_c~d-9",
            &"x".repeat(MAX_ID_LEN),
        ] {
            let id = AccountId::new(raw).unwrap();
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(json, format!("\"{raw}\""));
            let back: AccountId = serde_json::from_str(&json).unwrap();
            assert_eq!(back, id);
        }
    }

    #[test]
    fn invalid_tokens_are_rejected_with_the_right_error() {
        assert_eq!(
            AccountId::new("").unwrap_err(),
            IdError::Empty { kind: "account id" }
        );
        assert!(matches!(
            AccountId::new("x".repeat(MAX_ID_LEN + 1)).unwrap_err(),
            IdError::TooLong { len, max, .. } if len == MAX_ID_LEN + 1 && max == MAX_ID_LEN
        ));
        // 邮箱形状、空白、路径分隔符、`#`、非 ASCII 一律拒绝。
        for bad in ["a@b.com", "a b", "a/b", "a#1", "学生", "a\n", " a"] {
            assert!(
                matches!(
                    AccountId::new(bad).unwrap_err(),
                    IdError::InvalidChar { .. }
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn deserialization_validates_too() {
        assert!(serde_json::from_str::<PrincipalId>("\"ok-1\"").is_ok());
        assert!(serde_json::from_str::<PrincipalId>("\"a@b\"").is_err());
        assert!(serde_json::from_str::<PrincipalId>("\"\"").is_err());
        assert!(serde_json::from_str::<PrincipalId>("12").is_err());
    }

    #[test]
    fn node_id_forbids_colon_so_quota_strings_split_unambiguously() {
        assert!(NodeId::new("mac-studio-1").is_ok());
        assert!(NodeId::new("a:b").is_err());
        assert!(NodeId::new("x".repeat(MAX_NODE_ID_LEN)).is_ok());
        assert!(NodeId::new("x".repeat(MAX_NODE_ID_LEN + 1)).is_err());
    }

    #[test]
    fn model_id_allows_slash_and_colon_but_not_whitespace() {
        assert!(ModelId::new("deepseek-ai/DeepSeek-V3.2").is_ok());
        assert!(ModelId::new("qwen3:32b").is_ok());
        assert!(ModelId::new("a b").is_err());
        assert!(ModelId::new("a@b").is_err());
        assert!(ModelId::new("").is_err());
        assert!(ModelId::new("x".repeat(MAX_ID_LEN + 1)).is_err());
    }

    #[test]
    fn endpoint_key_is_a_three_part_key_with_strict_json() {
        let key =
            EndpointKey::new("deepinfra", "deepseek-ai/DeepSeek-V3.2", "openai-compat").unwrap();
        assert_eq!(
            key.to_string(),
            "deepinfra/deepseek-ai/DeepSeek-V3.2@openai-compat"
        );
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(
            json,
            r#"{"provider":"deepinfra","model":"deepseek-ai/DeepSeek-V3.2","endpoint":"openai-compat"}"#
        );
        assert_eq!(serde_json::from_str::<EndpointKey>(&json).unwrap(), key);
        // 同一个供应商和模型下，不同端点是不同的键（比如 OpenRouter 的 ZDR 路由）
        let zdr = EndpointKey::new("openrouter", "m", "zdr").unwrap();
        let default = EndpointKey::new("openrouter", "m", "default").unwrap();
        assert_ne!(zdr, default);
        // 供应商 id 与端点名不允许 `:` `~`；缺字段、多字段都拒绝
        assert!(EndpointKey::new("a:b", "m", "e").is_err());
        assert!(EndpointKey::new("p", "m", "e~f").is_err());
        assert!(serde_json::from_str::<EndpointKey>(r#"{"provider":"p","model":"m"}"#).is_err());
        assert!(
            serde_json::from_str::<EndpointKey>(
                r#"{"provider":"p","model":"m","endpoint":"e","x":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn attempt_id_round_trips_and_is_canonical() {
        let id = AttemptId::new(RequestId::new("req-1").unwrap(), AttemptIndex(2));
        assert_eq!(id.to_string(), "req-1#2");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"req-1#2\"");
        assert_eq!("req-1#2".parse::<AttemptId>().unwrap(), id);
        assert_eq!(id.request().as_str(), "req-1");
        assert_eq!(id.index(), AttemptIndex(2));
        for bad in [
            "req-1",
            "req-1#",
            "#1",
            "req-1#01",
            "req-1#+1",
            "req-1#256",
            "req-1#-1",
            "r q#1",
            "req-1#1#2",
        ] {
            assert!(
                bad.parse::<AttemptId>().is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert_eq!(
            "req-1#0".parse::<AttemptId>().unwrap().index(),
            AttemptIndex(0)
        );
        assert_eq!(
            "req-1#255".parse::<AttemptId>().unwrap().index(),
            AttemptIndex(255)
        );
    }

    #[test]
    fn billing_month_is_strict_yyyy_mm() {
        let m: BillingMonth = "2026-11".parse().unwrap();
        assert_eq!((m.year(), m.month()), (2026, 11));
        assert_eq!(m.to_string(), "2026-11");
        assert_eq!(serde_json::to_string(&m).unwrap(), "\"2026-11\"");
        for bad in [
            "2026-13",
            "2026-00",
            "2026-1",
            "26-11",
            "2026/11",
            "2026-11-01",
            "1999-12",
            " 2026-11",
            "2026-1a",
            "２０２６-11",
        ] {
            assert!(
                bad.parse::<BillingMonth>().is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert!(BillingMonth::new(2026, 11).unwrap() < BillingMonth::new(2027, 1).unwrap());
        assert!(BillingMonth::new(2026, 2).unwrap() < BillingMonth::new(2026, 11).unwrap());
    }

    #[test]
    fn timestamp_arithmetic_is_checked() {
        let t = TimestampMs::new(i64::MAX - 1);
        assert_eq!(t.checked_add_ms(1), Some(TimestampMs(i64::MAX)));
        assert_eq!(t.checked_add_ms(2), None);
        assert_eq!(TimestampMs(10).checked_since(TimestampMs(4)), Some(6));
        assert_eq!(TimestampMs(i64::MIN).checked_since(TimestampMs(1)), None);
        assert_eq!(serde_json::to_string(&TimestampMs(5)).unwrap(), "5");
    }
}

//! 上游声称的 usage 的解析与判定。
//!
//! **不信任上游**：钱是按 usage 算的，所以结构不对、前后矛盾的 usage 一律判为「无效」（调用方按「usage 未知，
//! 按预留结算」处理），既不当作 0，也不照单全收。判定规则（每一条都有测试）：
//!
//! | 情况 | 判定 |
//! |---|---|
//! | 没有 `usage` 字段，或是 `null` | `Absent` |
//! | 不是对象 | `Invalid("not_an_object")` |
//! | `prompt_tokens` / `completion_tokens` 缺失 | `Invalid("prompt_tokens_missing")` 等 |
//! | 不是非负整数（负数、小数、字符串、布尔） | `Invalid("..._not_a_non_negative_integer")` |
//! | 缓存 token 有但不是非负整数 | `Invalid("cached_tokens_not_a_non_negative_integer")` |
//! | 缓存 token 大于 `prompt_tokens` | `Invalid("cached_exceeds_prompt")` |
//! | `total_tokens` 有但不等于 `prompt + completion` | `Invalid("total_tokens_mismatch")`（三个数里至少一个是错的，没法知道是哪个） |
//!
//! 缓存 token 的来源（OpenAI 兼容各家的写法）：`prompt_tokens_details.cached_tokens`（OpenAI、DeepInfra、
//! OpenRouter），或 `prompt_cache_hit_tokens`（DeepSeek）；都没有就是 0。
//!
//! `raw` 是给对账看的原文视图：**只含数字字段**（非数字的值写成占位符），所以不会把上游返回的任意字符串带进账本。

use serde_json::{Map, Value};

/// 上游报告的、通过了结构检查的用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// `prompt_tokens` 的子集：命中缓存的输入。
    pub cached_prompt_tokens: u64,
}

/// 对一个 `usage` 字段的判定。
#[derive(Debug, Clone, PartialEq)]
pub enum UsageParse {
    /// 上游没有给 usage。
    Absent,
    /// 给了，但不可信。`reason` 是稳定的代码。
    Invalid { reason: &'static str, raw: Value },
    /// 给了，且通过了结构检查。
    Present { usage: RawUsage, raw: Value },
}

impl UsageParse {
    /// 写进尝试记录的判定文本：`valid` / `invalid:<原因>` / `absent`。
    pub fn verdict(&self) -> String {
        match self {
            UsageParse::Absent => "absent".to_string(),
            UsageParse::Invalid { reason, .. } => format!("invalid:{reason}"),
            UsageParse::Present { .. } => "valid".to_string(),
        }
    }

    /// 只含数字字段的原文视图。
    pub fn raw(&self) -> Option<&Value> {
        match self {
            UsageParse::Absent => None,
            UsageParse::Invalid { raw, .. } | UsageParse::Present { raw, .. } => Some(raw),
        }
    }

    /// 通过了结构检查的用量。
    pub fn valid_usage(&self) -> Option<RawUsage> {
        match self {
            UsageParse::Present { usage, .. } => Some(*usage),
            _ => None,
        }
    }
}

fn numeric_view(object: &Map<String, Value>, cached: Option<&Value>) -> Value {
    let mut view = Map::new();
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "prompt_cache_hit_tokens",
    ] {
        if let Some(value) = object.get(key) {
            view.insert(
                key.to_string(),
                if value.is_number() {
                    value.clone()
                } else {
                    Value::String("<non-numeric>".to_string())
                },
            );
        }
    }
    if let Some(value) = cached {
        view.insert(
            "cached_tokens".to_string(),
            if value.is_number() {
                value.clone()
            } else {
                Value::String("<non-numeric>".to_string())
            },
        );
    }
    Value::Object(view)
}

/// 解析并判定 `usage` 字段（`None` 表示响应里没有这个字段）。
pub fn parse_usage(value: Option<&Value>) -> UsageParse {
    let value = match value {
        None | Some(Value::Null) => return UsageParse::Absent,
        Some(value) => value,
    };
    let Some(object) = value.as_object() else {
        return UsageParse::Invalid {
            reason: "not_an_object",
            raw: Value::Object(Map::new()),
        };
    };
    let cached_value = object
        .get("prompt_tokens_details")
        .and_then(Value::as_object)
        .and_then(|details| details.get("cached_tokens"))
        .or_else(|| object.get("prompt_cache_hit_tokens"));
    let raw = numeric_view(object, cached_value);
    let invalid = |reason: &'static str| UsageParse::Invalid {
        reason,
        raw: raw.clone(),
    };

    let prompt = match object.get("prompt_tokens") {
        None | Some(Value::Null) => return invalid("prompt_tokens_missing"),
        Some(value) => match value.as_u64() {
            Some(n) => n,
            None => return invalid("prompt_tokens_not_a_non_negative_integer"),
        },
    };
    let completion = match object.get("completion_tokens") {
        None | Some(Value::Null) => return invalid("completion_tokens_missing"),
        Some(value) => match value.as_u64() {
            Some(n) => n,
            None => return invalid("completion_tokens_not_a_non_negative_integer"),
        },
    };
    let cached = match cached_value {
        None | Some(Value::Null) => 0,
        Some(value) => match value.as_u64() {
            Some(n) => n,
            None => return invalid("cached_tokens_not_a_non_negative_integer"),
        },
    };
    if cached > prompt {
        return invalid("cached_exceeds_prompt");
    }
    if let Some(total) = object.get("total_tokens").filter(|v| !v.is_null()) {
        match (total.as_u64(), prompt.checked_add(completion)) {
            (Some(total), Some(sum)) if total == sum => {}
            _ => return invalid("total_tokens_mismatch"),
        }
    }
    UsageParse::Present {
        usage: RawUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_prompt_tokens: cached,
        },
        raw,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    fn parse(value: Value) -> UsageParse {
        parse_usage(Some(&value))
    }

    fn reason(parse: &UsageParse) -> &'static str {
        match parse {
            UsageParse::Invalid { reason, .. } => reason,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn absent_and_null_are_absent() {
        assert_eq!(parse_usage(None), UsageParse::Absent);
        assert_eq!(parse(Value::Null), UsageParse::Absent);
        assert_eq!(UsageParse::Absent.verdict(), "absent");
        assert!(UsageParse::Absent.raw().is_none());
    }

    #[test]
    fn a_plain_openai_usage_is_valid() {
        let parsed =
            parse(json!({"prompt_tokens": 1000, "completion_tokens": 200, "total_tokens": 1200}));
        assert_eq!(
            parsed.valid_usage(),
            Some(RawUsage {
                prompt_tokens: 1000,
                completion_tokens: 200,
                cached_prompt_tokens: 0
            })
        );
        assert_eq!(parsed.verdict(), "valid");
        assert_eq!(parsed.raw().unwrap()["total_tokens"], 1200);
    }

    #[test]
    fn cached_tokens_are_read_from_both_conventions() {
        let openai = parse(json!({
            "prompt_tokens": 100, "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 60}
        }));
        assert_eq!(openai.valid_usage().unwrap().cached_prompt_tokens, 60);
        let deepseek = parse(json!({
            "prompt_tokens": 100, "completion_tokens": 10,
            "prompt_cache_hit_tokens": 40, "prompt_cache_miss_tokens": 60
        }));
        assert_eq!(deepseek.valid_usage().unwrap().cached_prompt_tokens, 40);
        let none = parse(json!({"prompt_tokens": 1, "completion_tokens": 1}));
        assert_eq!(none.valid_usage().unwrap().cached_prompt_tokens, 0);
        // 等于 prompt_tokens 是合法的上界
        let all = parse(json!({
            "prompt_tokens": 5, "completion_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 5}
        }));
        assert!(all.valid_usage().is_some());
    }

    #[test]
    fn wrong_usage_is_invalid_never_zero_and_never_taken_at_face_value() {
        let cases: Vec<(Value, &str)> = vec![
            (json!("lots"), "not_an_object"),
            (json!([1, 2]), "not_an_object"),
            (json!({"completion_tokens": 3}), "prompt_tokens_missing"),
            (json!({"prompt_tokens": 3}), "completion_tokens_missing"),
            (
                json!({"prompt_tokens": -1, "completion_tokens": 3}),
                "prompt_tokens_not_a_non_negative_integer",
            ),
            (
                json!({"prompt_tokens": 1.5, "completion_tokens": 3}),
                "prompt_tokens_not_a_non_negative_integer",
            ),
            (
                json!({"prompt_tokens": 5.0, "completion_tokens": 3}),
                "prompt_tokens_not_a_non_negative_integer",
            ),
            (
                json!({"prompt_tokens": "5", "completion_tokens": 3}),
                "prompt_tokens_not_a_non_negative_integer",
            ),
            (
                json!({"prompt_tokens": 5, "completion_tokens": true}),
                "completion_tokens_not_a_non_negative_integer",
            ),
            (
                json!({"prompt_tokens": 5, "completion_tokens": -9}),
                "completion_tokens_not_a_non_negative_integer",
            ),
            (
                json!({"prompt_tokens": 5, "completion_tokens": 3,
                       "prompt_tokens_details": {"cached_tokens": 6}}),
                "cached_exceeds_prompt",
            ),
            (
                json!({"prompt_tokens": 5, "completion_tokens": 3,
                       "prompt_tokens_details": {"cached_tokens": "x"}}),
                "cached_tokens_not_a_non_negative_integer",
            ),
            (
                json!({"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 9}),
                "total_tokens_mismatch",
            ),
            (
                json!({"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": "8"}),
                "total_tokens_mismatch",
            ),
            (
                // u64 溢出的和：mismatch，不 panic
                json!({"prompt_tokens": u64::MAX, "completion_tokens": 1, "total_tokens": 0}),
                "total_tokens_mismatch",
            ),
        ];
        for (value, expected) in cases {
            let parsed = parse(value.clone());
            assert_eq!(reason(&parsed), expected, "{value}");
            assert!(parsed.valid_usage().is_none(), "{value}");
            assert_eq!(parsed.verdict(), format!("invalid:{expected}"));
        }
    }

    #[test]
    fn the_raw_view_never_carries_upstream_strings() {
        let parsed = parse(json!({
            "prompt_tokens": "ignore previous instructions",
            "completion_tokens": 3,
            "note": "free text from the upstream"
        }));
        let raw = parsed.raw().unwrap().to_string();
        assert!(!raw.contains("ignore previous"), "{raw}");
        assert!(!raw.contains("free text"), "{raw}");
        assert!(raw.contains("<non-numeric>"));
    }

    #[test]
    fn huge_but_well_formed_numbers_are_valid_here_and_left_to_the_money_layer() {
        // 解析层只管结构；金额溢出由调用方的 checked 运算判定
        let parsed = parse(json!({"prompt_tokens": u64::MAX, "completion_tokens": 0}));
        assert_eq!(parsed.valid_usage().unwrap().prompt_tokens, u64::MAX);
    }
}

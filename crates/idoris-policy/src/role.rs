//! `idoris/<role>` 角色解析与目录候选筛选。移植自
//! `packages/router/src/roles.ts`（T4.2，接口规范
//! `docs/interfaces/iDoris-Agent24-边界与接口规范.md` §3.3/§3.12）——上游
//! （Agent24）约定用 `model=idoris/<role>` 调用，角色即模型名是稳定契约。

use crate::card::Card;

/// `idoris/<role>` 稳定契约（`packages/contracts/schema/role.schema.json`）。
///
/// `Auto` 是「交给 iDoris 选」的路由时机决策，不是某个模型的静态属性——任何
/// catalog 条目都不应该在自己的 `roles` 里声明它（对齐 TS 的
/// `CatalogRole = Exclude<Role, "auto">`）；[`Role::is_catalog_role`] 用于在
/// 决策管道里做这个区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Fast,
    Daily,
    Deep,
    Vision,
    Embed,
    Rerank,
    Decide,
    Auto,
}

/// 全部角色枚举值，按规范顺序（供遍历 / 测试使用），对齐 TS 的 `ROLES`。
pub const ROLES: &[Role] = &[
    Role::Fast,
    Role::Daily,
    Role::Deep,
    Role::Vision,
    Role::Embed,
    Role::Rerank,
    Role::Decide,
    Role::Auto,
];

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Fast => "fast",
            Role::Daily => "daily",
            Role::Deep => "deep",
            Role::Vision => "vision",
            Role::Embed => "embed",
            Role::Rerank => "rerank",
            Role::Decide => "decide",
            Role::Auto => "auto",
        }
    }

    /// 精确匹配（大小写敏感），对齐 TS 的 `isRole`。
    fn from_exact(s: &str) -> Option<Role> {
        ROLES.iter().copied().find(|r| r.as_str() == s)
    }

    /// `auto` 之外的目录角色（TS 的 `CatalogRole`）。
    pub fn is_catalog_role(self) -> bool {
        !matches!(self, Role::Auto)
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 角色解析失败（预期错误；调用方计划映射为 `400`，命名对齐接口规范 §3.11
/// 风格——规范尚未正式收录 `unknown_role`，同 TS「将来映射为 400」的措辞）；
/// 只保留「命中 `idoris/` 前缀但角色不认识」这一种情形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleParseError {
    /// 未能识别的角色部分，经 [`RoleParseError::new`] 清洗（截断、控制字符
    /// 替换为 `U+FFFD`）——它常被原样写进日志/错误响应，不清洗即日志注入。
    pub requested: String,
}

/// [`RoleParseError::requested`] 的最大保留字符数，超出部分丢弃并标 `…`。
const MAX_REQUESTED_ROLE_DISPLAY_LEN: usize = 64;

impl RoleParseError {
    fn new(role_part: &str) -> Self {
        // 单独数一遍总长度：`.take(N)` 只向上游拉 N 次，一旦命中截断，
        // 上游就不再被驱动，用 `.inspect()` 数会被错误地封顶在 N。
        let total_chars = role_part.chars().count();
        let mut requested: String = role_part
            .chars()
            .take(MAX_REQUESTED_ROLE_DISPLAY_LEN)
            // 控制字符（含换行、回车、ANSI 转义用到的 ESC）一律替换掉，
            // 避免污染日志或终端。
            .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
            .collect();
        if total_chars > MAX_REQUESTED_ROLE_DISPLAY_LEN {
            requested.push('…');
        }
        Self { requested }
    }
}

impl std::fmt::Display for RoleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "未知角色 \"{}\"；合法角色为 ", self.requested)?;
        for (i, role) in ROLES.iter().enumerate() {
            if i > 0 {
                f.write_str("|")?;
            }
            f.write_str(role.as_str())?;
        }
        Ok(())
    }
}

impl std::error::Error for RoleParseError {}

const IDORIS_PREFIX: &str = "idoris/";

/// ECMAScript `trim()` 的空白集合（WhiteSpace + LineTerminator），和 Rust
/// `str::trim()` 依据的 Unicode `White_Space` 不完全相同：前者含 BOM
/// （U+FEFF），不含 NEL（U+0085）；Rust 反之。`roles.ts` 的 `model.trim()`
/// 是前者语义，这里手写等价谓词以求 porting 精确对齐。
fn is_ecmascript_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// 解析 `model` 字段里的 `idoris/<role>` 前缀（`roles.ts` 的 `parseModelRole` 移植）。
///
/// - 先按 ECMAScript 语义 trim（见 [`is_ecmascript_whitespace`]）；前缀 `idoris/`
///   本身大小写不敏感（`IDORIS/`、`Idoris/` 都算命中）。
/// - 非 `idoris/` 前缀 → `Ok(None)`（调用方应按原模型名处理，不是角色请求）。
/// - 命中前缀后，角色部分**精确匹配**（大小写敏感、不允许多余路径段）；不匹配
///   （含空角色 `idoris/`、多段 `idoris/fast/x`、未知角色）→ `Err(RoleParseError)`。
pub fn parse_model_role(model: &str) -> Result<Option<Role>, RoleParseError> {
    let trimmed = model.trim_matches(is_ecmascript_whitespace);
    let Some(prefix_candidate) = trimmed.get(..IDORIS_PREFIX.len()) else {
        return Ok(None);
    };
    if !prefix_candidate.eq_ignore_ascii_case(IDORIS_PREFIX) {
        return Ok(None);
    }
    // `get` 已经确认 `IDORIS_PREFIX.len()` 是合法的字符边界，这里切片是安全的。
    let role_part = &trimmed[IDORIS_PREFIX.len()..];
    Role::from_exact(role_part)
        .map(Some)
        .ok_or_else(|| RoleParseError::new(role_part))
}

/// 目录角色候选筛选（`roles.ts` / `recommend.ts` 共用的 `isEligibleForRole`
/// 移植——两处历史 TS 实现就是同一个函数，这里同样只保留一份）。
///
/// - `role: Role::Auto` 一律不合格：`Auto` 没有对应候选列表（TS 的
///   `CatalogRole = Exclude<Role, "auto">`），即使有张（不合规的）卡在
///   `roles` 里错误声明了它，也不能靠这个巧合通过。
/// - `status: experiment` 一律排除（跨 harness 基准不迁移）。
/// - `min_ram_gb` 省略时不做硬件门槛过滤；传入时要求 `card.min_ram_gb <=
///   min_ram_gb`——任一是 `NaN` 时显式拒绝，不能让损坏数据静默 fail-open。
/// - 角色匹配严格按 `card.roles`（第一条规则对 `Auto` 兜底）。
pub fn is_eligible_for_role(card: &Card, role: Role, min_ram_gb: Option<f64>) -> bool {
    if !role.is_catalog_role() {
        return false;
    }
    if card.experiment {
        return false;
    }
    // 卡片自身的 `min_ram_gb` 是否损坏（`NaN`）是无条件检查——不能只在调用方
    // 传了 `min_ram_gb` 阈值时才查。之前的实现把这个检查嵌在 `if let
    // Some(...)` 里，调用方省略阈值（`None`，意为"不做硬件过滤"）时，损坏的
    // 卡片数据就绕过检查、只靠角色匹配放行，这正是要杜绝的静默 fail-open。
    if card.min_ram_gb.is_nan() {
        return false;
    }
    if let Some(min_ram_gb) = min_ram_gb
        && (min_ram_gb.is_nan() || card.min_ram_gb > min_ram_gb)
    {
        return false;
    }
    card.roles.contains(&role)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::card::test_support::sample_card;

    #[test]
    fn parse_model_role_returns_none_for_non_idoris_models() {
        assert_eq!(parse_model_role("gpt-4o").unwrap(), None);
        assert_eq!(parse_model_role("").unwrap(), None);
        assert_eq!(parse_model_role("idoris").unwrap(), None); // 缺斜杠
    }

    #[test]
    fn parse_model_role_trims_and_is_prefix_case_insensitive() {
        assert_eq!(
            parse_model_role("  idoris/daily  ").unwrap(),
            Some(Role::Daily)
        );
        assert_eq!(parse_model_role("IDORIS/daily").unwrap(), Some(Role::Daily));
        assert_eq!(parse_model_role("Idoris/fast").unwrap(), Some(Role::Fast));
    }

    #[test]
    fn parse_model_role_matches_role_part_case_sensitively() {
        assert!(parse_model_role("idoris/Daily").is_err());
        assert!(parse_model_role("idoris/DAILY").is_err());
    }

    #[test]
    fn parse_model_role_rejects_unknown_or_malformed_role() {
        let err = parse_model_role("idoris/nope").unwrap_err();
        assert_eq!(err.requested, "nope");
        assert!(parse_model_role("idoris/").is_err()); // 空角色
        assert!(parse_model_role("idoris/fast/x").is_err()); // 多余路径段
    }

    #[test]
    fn role_parse_error_sanitizes_control_characters_and_truncates_long_input() {
        // 换行/回车这类控制字符不能原样进错误值——否则调用方把它写进日志就是
        // 一次日志注入。
        let err = parse_model_role("idoris/evil\nINJECTED").unwrap_err();
        assert!(!err.requested.contains('\n'));
        assert_eq!(err.requested, "evil\u{FFFD}INJECTED");

        // 超过 MAX_REQUESTED_ROLE_DISPLAY_LEN 的输入被截断并标记省略，
        // 不能无限增长地被克隆/保留。
        let long_role = "a".repeat(200);
        let err = parse_model_role(&format!("idoris/{long_role}")).unwrap_err();
        assert!(err.requested.len() < long_role.len());
        assert!(err.requested.ends_with('…'));
    }

    #[test]
    fn role_parse_error_display_lists_all_roles_pipe_separated() {
        let err = parse_model_role("idoris/nope").unwrap_err();
        assert_eq!(
            err.to_string(),
            "未知角色 \"nope\"；合法角色为 fast|daily|deep|vision|embed|rerank|decide|auto"
        );
    }

    #[test]
    fn parse_model_role_accepts_every_known_role() {
        for role in ROLES {
            let model = format!("idoris/{role}");
            assert_eq!(parse_model_role(&model).unwrap(), Some(*role));
        }
    }

    /// 独立字面量表：上面那个测试用 `ROLES`/`as_str` 生成输入又拿去断言，是
    /// 自证循环（拼写和实现同时错也会全绿）；这里锁死独立真源。
    #[test]
    fn parse_model_role_matches_the_independently_spelled_canonical_role_strings() {
        let expected: [(&str, Role); 8] = [
            ("fast", Role::Fast),
            ("daily", Role::Daily),
            ("deep", Role::Deep),
            ("vision", Role::Vision),
            ("embed", Role::Embed),
            ("rerank", Role::Rerank),
            ("decide", Role::Decide),
            ("auto", Role::Auto),
        ];
        for (literal, role) in expected {
            assert_eq!(
                parse_model_role(&format!("idoris/{literal}")).unwrap(),
                Some(role),
                "字面量 {literal:?} 应该解析成 {role:?}"
            );
        }
    }

    #[test]
    fn parse_model_role_trims_every_ecmascript_whitespace_code_point() {
        // 表驱动逐个验证全部 25 个码点，漏掉任何一个都应该让对应用例失败。
        const WHITESPACE_CODE_POINTS: &[u32] = &[
            0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x0020, 0x00A0, 0x1680, 0x2000, 0x2001, 0x2002,
            0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x2028, 0x2029, 0x202F,
            0x205F, 0x3000, 0xFEFF,
        ];
        assert_eq!(WHITESPACE_CODE_POINTS.len(), 25);
        for &cp in WHITESPACE_CODE_POINTS {
            // 表里全是合法标量值；模块级已经 allow(clippy::unwrap_used)。
            let ws = char::from_u32(cp).unwrap();
            let leading = format!("{ws}idoris/daily");
            assert_eq!(
                parse_model_role(&leading).unwrap(),
                Some(Role::Daily),
                "U+{cp:04X} 前导时应被 trim 掉"
            );
            let trailing = format!("idoris/daily{ws}");
            assert_eq!(
                parse_model_role(&trailing).unwrap(),
                Some(Role::Daily),
                "U+{cp:04X} 尾随时应被 trim 掉"
            );
        }
    }

    #[test]
    fn parse_model_role_does_not_treat_nel_as_whitespace() {
        // NEL（U+0085）在 Unicode `White_Space` 属性里，会被 Rust 的
        // `str::trim()` 吃掉；但它不在 ECMAScript 的空白产生式里，TS 端不会
        // 剥离它。带着它就不再是 `idoris/` 前缀，应该按普通模型名处理。
        let with_nel = "\u{0085}idoris/daily";
        assert_eq!(parse_model_role(with_nel).unwrap(), None);
    }

    #[test]
    fn is_eligible_for_role_excludes_experiment_status() {
        let mut card = sample_card("c1", &[Role::Daily]);
        card.experiment = true;
        assert!(!is_eligible_for_role(&card, Role::Daily, None));
    }

    #[test]
    fn is_eligible_for_role_applies_min_ram_gb_threshold_only_when_given() {
        let mut card = sample_card("c1", &[Role::Daily]);
        card.min_ram_gb = 32.0;
        assert!(is_eligible_for_role(&card, Role::Daily, None));
        assert!(!is_eligible_for_role(&card, Role::Daily, Some(16.0)));
        // 恰好相等（`<=` 的等号分支）。
        assert!(is_eligible_for_role(&card, Role::Daily, Some(32.0)));
        // 严格小于（`<=` 的正常成功分支，不只是等号）。
        assert!(is_eligible_for_role(&card, Role::Daily, Some(64.0)));
    }

    #[test]
    fn is_eligible_for_role_requires_role_membership() {
        let card = sample_card("c1", &[Role::Fast]);
        assert!(!is_eligible_for_role(&card, Role::Daily, None));
        assert!(is_eligible_for_role(&card, Role::Fast, None));
    }

    #[test]
    fn is_eligible_for_role_rejects_auto_even_if_a_card_wrongly_declares_it() {
        let card = sample_card("c1", &[Role::Auto]);
        assert!(!is_eligible_for_role(&card, Role::Auto, None));
    }

    #[test]
    fn is_eligible_for_role_fails_closed_on_nan_ram_values() {
        let mut card = sample_card("c1", &[Role::Daily]);
        card.min_ram_gb = f64::NAN;
        assert!(!is_eligible_for_role(&card, Role::Daily, Some(16.0)));
        // 卡片自身 min_ram_gb 是 NaN 时，即使调用方压根没传阈值（`None`，
        // 意为"不做硬件过滤"）也必须拒绝——这条曾经是真实的 fail-open 漏洞：
        // NaN 检查被嵌在 `if let Some(...)` 里，`None` 分支完全绕过它。
        assert!(!is_eligible_for_role(&card, Role::Daily, None));

        let card = sample_card("c1", &[Role::Daily]);
        assert!(!is_eligible_for_role(&card, Role::Daily, Some(f64::NAN)));
    }
}

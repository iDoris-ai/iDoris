//! 可逆脱敏引擎（docs/15 §1.4，M5-12）：稳定占位符替换、脱敏后二次扫描、
//! 回填与流式回填。纯函数，不接线——请求路径何时允许 REDACT 取决于调用方
//! 同意的线上表示，那是另一个决定。
//!
//! - 同一个原值在一次请求里永远得到同一个占位符 `[KIND_N]`。
//! - 脱敏后的文本再跑一遍完整 L0；只要还有命中就拒绝（fail-closed），
//!   绝不把「替换了一部分」的文本放出去。
//! - R-1/R-2：回填时，输出里出现映射表之外的占位符形状，或者已知占位符被
//!   改写（宽字符折叠、ASCII 字母数字大写归一化后匹配），拒绝回填，不猜。
//!   仅完整落在同一个原始字面量或合法回填条目内的匹配豁免；分隔文本也参与归一化。
//! - R-4：映射表只活在这个值里；`Debug` 不打印原值。

use std::fmt;

use idoris_contracts::inspect::{InspectAction, InspectResponse};

use super::{InspectCtx, inspect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactError {
    /// 判定不是 REDACT（BLOCK/ALLOW 都不该走脱敏）。
    NotRedactable,
    /// 脱敏后的文本仍有 L0 命中。
    ResidualFinding,
    /// span 不落在合法的字符边界上。
    InvalidSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreError {
    /// R-2：输出里有映射表之外的占位符。
    UnknownPlaceholder,
    /// R-1：已知占位符被模型改写。
    MangledPlaceholder,
}

/// 一次请求的脱敏结果：发出去的文本 + 只在内存里的回填表。
pub struct Redaction {
    pub text: String,
    /// `(占位符, 原值)`，按占位符首次出现的顺序。
    table: Vec<(String, String)>,
    /// Placeholder-shaped tokens the input already contained. They are the
    /// user's own text: `restore` must pass them through untouched instead of
    /// demanding they be in the table.
    literals: Vec<String>,
}

impl fmt::Debug for Redaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redaction")
            .field("text", &self.text)
            .field("placeholders", &self.table.len())
            .finish_non_exhaustive()
    }
}

/// 按 `verdict`（必须是对同一个 `input` 的 REDACT 判定）替换每个命中。
/// 重叠的命中合并成一段，用最先出现的那个类别命名。
pub fn redact(
    input: &str,
    verdict: &InspectResponse,
    ctx: &InspectCtx,
) -> Result<Redaction, RedactError> {
    if verdict.action != InspectAction::Redact {
        return Err(RedactError::NotRedactable);
    }
    let units: Vec<u16> = input.encode_utf16().collect();
    let mut regions: Vec<(usize, usize, String)> = Vec::new();
    for finding in &verdict.findings {
        let span = finding.span.ok_or(RedactError::NotRedactable)?;
        let start = usize::try_from(span.start).map_err(|_| RedactError::InvalidSpan)?;
        let end = start
            .checked_add(usize::try_from(span.length).map_err(|_| RedactError::InvalidSpan)?)
            .filter(|&end| end <= units.len())
            .ok_or(RedactError::InvalidSpan)?;
        match regions.last_mut() {
            Some(last) if start < last.1 => last.1 = last.1.max(end),
            _ => regions.push((start, end, label(finding.kind))),
        }
    }
    let mut out = Redaction {
        text: String::with_capacity(input.len()),
        table: Vec::new(),
        literals: placeholder_tokens(input),
    };
    let normalized_input = normalize(input);
    let mut counters: Vec<(String, usize)> = Vec::new();
    let mut cursor = 0;
    for (start, end, kind) in regions {
        out.text.push_str(&utf16(&units[cursor..start])?);
        let original = utf16(&units[start..end])?;
        let placeholder = match out.table.iter().find(|(_, value)| *value == original) {
            Some((placeholder, _)) => placeholder.clone(),
            None => {
                let mut n = match counters.iter_mut().find(|(k, _)| *k == kind) {
                    Some((_, n)) => {
                        *n += 1;
                        *n
                    }
                    None => {
                        counters.push((kind.clone(), 1));
                        1
                    }
                };
                // U1: reserve normalized input names, including bare and fullwidth
                // spellings, before minting placeholders.
                let placeholder = loop {
                    let candidate = format!("[{kind}_{n}]");
                    if normalized_input.contains(&normalize(&candidate))
                        || out.text.contains(&candidate)
                    {
                        n += 1;
                        continue;
                    }
                    break candidate;
                };
                out.table.push((placeholder.clone(), original));
                placeholder
            }
        };
        out.text.push_str(&placeholder);
        cursor = end;
    }
    out.text.push_str(&utf16(&units[cursor..])?);
    if !inspect(&out.text, ctx).findings.is_empty() {
        return Err(RedactError::ResidualFinding);
    }
    Ok(out)
}

impl Redaction {
    #[cfg(test)]
    pub(super) fn empty_for_test() -> Self {
        Self {
            text: String::new(),
            table: Vec::new(),
            literals: Vec::new(),
        }
    }

    /// Restore with the same scanner and completeness rule as streaming.
    pub fn restore(&self, output: &str) -> Result<String, RestoreError> {
        let mut stream = self.stream();
        let mut result = stream.push(output)?;
        result.push_str(&stream.finish()?);
        Ok(result)
    }

    pub fn stream(&self) -> StreamRestorer<'_> {
        let limit = self
            .table
            .iter()
            .map(|(p, _)| p.len())
            .chain(self.literals.iter().map(String::len))
            .max()
            .unwrap_or(0)
            .max(64);
        let names: Vec<_> = self.table.iter().map(|(p, _)| normalize(p)).collect();
        let max_name = names.iter().map(String::len).max().unwrap_or(0);
        StreamRestorer {
            redaction: self,
            token: None,
            normalized: String::new(),
            names,
            max_name,
            seen: vec![false; self.table.len()],
            limit,
            failed: None,
        }
    }
}

/// Bounded bracket classifier. After the spelling buffer fills, structural
/// state still follows the entire token: overflow never becomes an exemption.
struct Token {
    spelling: String,
    overflow: bool,
    uppercase: bool,
    reserved: bool,
    digits: bool,
    indexed: bool,
}

impl Token {
    fn new() -> Self {
        Self {
            spelling: "[".into(),
            overflow: false,
            uppercase: true,
            reserved: false,
            digits: false,
            indexed: false,
        }
    }

    fn advance(&mut self, c: char) {
        let c = fold_width(c);
        self.uppercase &= !c.is_ascii_lowercase();
        if c == '_' {
            self.reserved = !self.overflow
                && reserved_kind(
                    &self.spelling[1..]
                        .chars()
                        .map(fold_width)
                        .collect::<String>(),
                );
            self.indexed = self.overflow || self.spelling.len() > 1;
            self.digits = false;
        } else if c.is_ascii_digit() {
            self.digits = self.indexed;
        } else {
            self.indexed = false;
            self.digits = false;
        }
    }

    fn placeholder(&self) -> bool {
        self.digits && (self.uppercase || self.reserved)
    }
}

/// Incremental restoration over UTF-8 strings. Call `finish` to validate
/// completeness, even after the last `push` emitted text. All output is
/// provisional until `finish` succeeds; discard it on any error. A failed
/// `push` permanently poisons the restorer.
///
/// Held spelling is bounded by max(64, longest table entry or input literal).
/// Shape state and the normalized suffix remain bounded independently of output
/// length, including unclosed brackets and arbitrarily long separator runs.
pub struct StreamRestorer<'a> {
    redaction: &'a Redaction,
    token: Option<Token>,
    normalized: String,
    names: Vec<String>,
    max_name: usize,
    seen: Vec<bool>,
    limit: usize,
    failed: Option<RestoreError>,
}

impl StreamRestorer<'_> {
    pub fn push(&mut self, chunk: &str) -> Result<String, RestoreError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.scan(chunk);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }

    fn scan(&mut self, chunk: &str) -> Result<String, RestoreError> {
        let mut out = String::new();
        for c in chunk.chars() {
            if c == ']' && self.token.is_some() {
                let mut token = self.token.take().ok_or(RestoreError::MangledPlaceholder)?;
                if !token.overflow {
                    token.spelling.push(c);
                    if self.redaction.literals.contains(&token.spelling) {
                        self.track(&token.spelling, true)?;
                        out.push_str(&token.spelling);
                        continue;
                    }
                    if let Some(index) = self
                        .redaction
                        .table
                        .iter()
                        .position(|(p, _)| *p == token.spelling)
                    {
                        self.seen[index] = true;
                        self.track(&token.spelling, true)?;
                        out.push_str(&self.redaction.table[index].1);
                        continue;
                    }
                }
                if token.placeholder() {
                    return Err(if token.uppercase {
                        RestoreError::UnknownPlaceholder
                    } else {
                        RestoreError::MangledPlaceholder
                    });
                }
                if token.overflow {
                    self.plain("]", &mut out)?;
                } else {
                    self.plain(&token.spelling, &mut out)?;
                }
                continue;
            }
            if (fold_width(c).is_ascii_alphanumeric() || fold_width(c) == '_')
                && let Some(mut token) = self.token.take()
            {
                token.advance(c);
                if !token.overflow && token.spelling.len() + c.len_utf8() < self.limit {
                    token.spelling.push(c);
                } else {
                    if !token.overflow {
                        self.plain(&token.spelling, &mut out)?;
                        token.spelling.clear();
                        token.overflow = true;
                    }
                    self.plain(&c.to_string(), &mut out)?;
                }
                self.token = Some(token);
                continue;
            }
            // Advance only one opening bracket: malformed outer brackets never
            // hide a valid or malformed inner token.
            if let Some(token) = self.token.take() {
                self.plain(&token.spelling, &mut out)?;
            }
            if c == '[' {
                self.token = Some(Token::new());
            } else {
                self.plain(&c.to_string(), &mut out)?;
            }
        }
        Ok(out)
    }

    fn plain(&mut self, text: &str, out: &mut String) -> Result<(), RestoreError> {
        self.track(text, false)?;
        out.push_str(text);
        Ok(())
    }

    // Only a match wholly inside this exact literal/restoration event is exempt.
    // Its characters still participate in matches spanning either boundary.
    fn track(&mut self, text: &str, literal: bool) -> Result<(), RestoreError> {
        let mut inside = 0;
        for c in text
            .chars()
            .map(fold_width)
            .filter(char::is_ascii_alphanumeric)
        {
            inside += usize::from(literal);
            self.normalized.push(c.to_ascii_uppercase());
            if self
                .names
                .iter()
                .any(|name| self.normalized.ends_with(name) && name.len() > inside)
            {
                return Err(RestoreError::MangledPlaceholder);
            }
            if self.normalized.len() > self.max_name {
                self.normalized
                    .drain(..self.normalized.len() - self.max_name);
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<String, RestoreError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let mut tail = String::new();
        if let Some(token) = self.token.take() {
            self.plain(&token.spelling, &mut tail)?;
        }
        if self.seen.iter().any(|seen| !seen) {
            return Err(RestoreError::MangledPlaceholder);
        }
        Ok(tail)
    }
}

fn fold_width(c: char) -> char {
    if ('\u{ff01}'..='\u{ff5e}').contains(&c) {
        char::from_u32(c as u32 - 0xfee0).unwrap_or(c)
    } else {
        c
    }
}

fn normalize(text: &str) -> String {
    text.chars()
        .map(fold_width)
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

// Uppercase indexed identifiers reserve the generic placeholder namespace.
// Lowercase prose identifiers remain ordinary, except actual entity kinds.
fn reserved_kind(kind: &str) -> bool {
    use idoris_contracts::inspect::FindingKind::*;
    kind.eq_ignore_ascii_case("person")
        || [
            CnResidentId,
            CnMobile,
            BankCard,
            CnUnifiedSocialCreditCode,
            CnLicensePlate,
            CnPassport,
            HkIdentityCard,
            HkMacauTravelPermit,
            EmailAddress,
            ApiKey,
            PrivateKey,
            HighEntropySecret,
            CredentialPath,
            InputLimitExceeded,
        ]
        .iter()
        .any(|k| kind.eq_ignore_ascii_case(k.as_str()))
}

/// The same bracket grammar used by restoration, advancing one `[` at a time.
/// Record all input literals (including ordinary lowercase indexed identifiers)
/// before minting names. Their shapes are allowed; names crossing their
/// boundaries still undergo mangling checks.
fn placeholder_tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut token: Option<Token> = None;
    for c in text.chars() {
        if c == '[' {
            token = Some(Token::new());
        } else if let Some(mut current) = token.take() {
            if c == ']' {
                current.spelling.push(c);
                found.push(current.spelling);
            } else if fold_width(c).is_ascii_alphanumeric() || fold_width(c) == '_' {
                current.advance(c);
                current.spelling.push(c);
                token = Some(current);
            }
        }
    }
    found
}

fn label(kind: idoris_contracts::inspect::FindingKind) -> String {
    kind.as_str().to_ascii_uppercase()
}

fn utf16(units: &[u16]) -> Result<String, RedactError> {
    String::from_utf16(units).map_err(|_| RedactError::InvalidSpan)
}

#[cfg(test)]
mod scanner_bounds {
    use super::*;

    #[test]
    fn exemptions_belong_to_one_literal_event_only() {
        let r = Redaction {
            text: String::new(),
            table: vec![("[CN_MOBILE_1]".into(), "value".into())],
            literals: [
                "[cn_mobile_1]",
                "[ＣＮ＿ＭＯＢＩＬＥ＿１]",
                "[CN]",
                "[MOBILE]",
                "[1]",
                "[]",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        };
        for (text, accepted) in [
            ("[CN_MOBILE_1] [cn_mobile_1]", true),
            ("[CN_MOBILE_1] [ＣＮ＿ＭＯＢＩＬＥ＿１]", true),
            ("[CN_MOBILE_1] [CN][MOBILE][1]", false),
            ("[CN_MOBILE_1] CN_MOBILE_[1]", false),
            ("[CN_MOBILE_1] [CN_MOBILE_[]1]", false),
            ("[CN_MOBILE_1] cn_mobile_1", false),
            ("[CN_MOBILE_1] ＣＮ＿ＭＯＢＩＬＥ＿１", false),
            ("CN_MOBILE[CN_MOBILE_1]1", true),
        ] {
            super::super::redact_tests::assert_all_partitions(&r, text, accepted);
        }
    }

    #[test]
    fn restoration_events_keep_cross_boundary_detection() {
        let r = Redaction {
            text: String::new(),
            table: vec![
                ("[CN_MOBILE_1]".into(), "phone".into()),
                ("[1]".into(), "digit".into()),
            ],
            literals: Vec::new(),
        };
        for (text, accepted) in [
            ("[CN_MOBILE_1] [1]", true),
            ("[CN_MOBILE_1] CN_MOBILE[1]", false),
        ] {
            super::super::redact_tests::assert_all_partitions(&r, text, accepted);
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn overflow_keeps_bounded_state_without_losing_the_final_shape() {
        let r = Redaction {
            text: String::new(),
            table: vec![("[CN_MOBILE_1]".into(), "value".into())],
            literals: Vec::new(),
        };
        for prefix in ["[PERSON_", "[person_", "[A"] {
            let mut stream = r.stream();
            stream.push("[CN_MOBILE_1]").unwrap();
            stream.push(prefix).unwrap();
            for _ in 0..1_000 {
                stream.push("1111111111").unwrap();
                assert!(stream.token.as_ref().unwrap().spelling.len() <= stream.limit);
                assert!(stream.normalized.len() <= normalize("[CN_MOBILE_1]").len());
            }
            if prefix == "[A" {
                stream.push("_").unwrap();
            }
            assert!(stream.push("1]").is_err());
        }
    }
}

//! 可逆脱敏引擎（docs/15 §1.4，M5-12）：稳定占位符替换、脱敏后二次扫描、
//! 回填与流式回填。纯函数，不接线——请求路径何时允许 REDACT 取决于调用方
//! 同意的线上表示，那是另一个决定。
//!
//! - 同一个原值在一次请求里永远得到同一个占位符 `[KIND_N]`。
//! - 脱敏后的文本再跑一遍完整 L0；只要还有命中就拒绝（fail-closed），
//!   绝不把「替换了一部分」的文本放出去。
//! - R-1/R-2：回填时，输出里出现映射表之外的占位符形状，或者已知占位符被
//!   改写（去掉括号、下划线变空格/连字符），一律拒绝回填，不猜。
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
                // U1: a literal placeholder-shaped token in the input must not be
                // mints to the same name, or `restore` would write personal data
                // over the user's own text. Skip any index the input already uses.
                let placeholder = loop {
                    let candidate = format!("[{kind}_{n}]");
                    if input.contains(&candidate) || out.text.contains(&candidate) {
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
    /// 把模型输出里的占位符换回原值（R-1/R-2 见模块文档）。
    pub fn restore(&self, output: &str) -> Result<String, RestoreError> {
        // U3 / R-1: §1.4 asks for a set comparison - refuse when the count or type
        // of placeholders does not match. Every minted placeholder must appear in
        // the model output, and nothing placeholder-shaped outside the table may
        // survive (a swallowed, lowercased or de-bracketed placeholder is a
        // mangled one, not a clean pass-through).
        // `replace` first so an unknown placeholder is reported as Unknown, not as
        // a missing one; only a text whose tokens are all accounted for is then
        // judged on completeness.
        let restored = self.replace(output)?;
        self.reject_mangled(&restored)?;
        for (placeholder, _) in &self.table {
            if !output.contains(placeholder.as_str()) {
                return Err(RestoreError::MangledPlaceholder);
            }
        }
        self.reject_extra_placeholder_shapes(output)?;
        Ok(restored)
    }

    /// Any placeholder-shaped token in the output that is not exactly a table
    /// entry - wrong case, wrong digits, missing underscore - is a mangled
    /// placeholder and must be refused rather than passed through.
    fn reject_extra_placeholder_shapes(&self, output: &str) -> Result<(), RestoreError> {
        let mut rest = output;
        while let Some(open) = rest.find('[') {
            let tail = &rest[open..];
            let Some(close) = tail.find(']') else {
                break;
            };
            let candidate = &tail[..=close];
            if looks_like_placeholder_any_case(candidate)
                && !self
                    .table
                    .iter()
                    .any(|(placeholder, _)| placeholder == candidate)
                && !self.literals.iter().any(|literal| literal == candidate)
            {
                return Err(RestoreError::MangledPlaceholder);
            }
            rest = &tail[candidate.len()..];
        }
        Ok(())
    }

    /// Streaming variant of [`Self::replace`]: records which table placeholders
    /// were restored, and refuses a placeholder-shaped token that is neither a
    /// table entry nor one of the input's own literals — the same verdict batch
    /// `restore` reaches, so the two paths cannot disagree.
    fn replace_streaming(
        &self,
        output: &str,
        restored_counts: &mut Vec<(String, usize)>,
    ) -> Result<String, RestoreError> {
        let restored = self.replace(output)?;
        for (placeholder, _) in &self.table {
            let seen = output.matches(placeholder.as_str()).count();
            if seen > 0 {
                match restored_counts.iter_mut().find(|(p, _)| p == placeholder) {
                    Some((_, n)) => *n += seen,
                    None => restored_counts.push((placeholder.clone(), seen)),
                }
            }
        }
        self.reject_extra_placeholder_shapes(output)?;
        Ok(restored)
    }

    /// R-2：只认映射表里的占位符。
    fn replace(&self, output: &str) -> Result<String, RestoreError> {
        let mut rest = output;
        let mut restored = String::with_capacity(output.len());
        while let Some(open) = rest.find('[') {
            restored.push_str(&rest[..open]);
            let tail = &rest[open..];
            match tail.find(']').map(|close| &tail[..=close]) {
                Some(candidate) if self.literals.iter().any(|literal| literal == candidate) => {
                    // The user's own literal, not a minted placeholder.
                    restored.push_str(candidate);
                    rest = &tail[candidate.len()..];
                }
                Some(candidate) if looks_like_placeholder(candidate) => {
                    let (_, original) = self
                        .table
                        .iter()
                        .find(|(placeholder, _)| placeholder == candidate)
                        .ok_or(RestoreError::UnknownPlaceholder)?;
                    restored.push_str(original);
                    rest = &tail[candidate.len()..];
                }
                _ => {
                    restored.push('[');
                    rest = &tail[1..];
                }
            }
        }
        restored.push_str(rest);
        Ok(restored)
    }

    /// R-1：回填后的文本里不能再出现任何已知占位符的变形（去括号、`_`
    /// 变空格或连字符）。
    fn reject_mangled(&self, restored: &str) -> Result<(), RestoreError> {
        for (placeholder, _) in &self.table {
            let bare = &placeholder[1..placeholder.len() - 1];
            let variants = [
                bare.to_string(),
                bare.replace('_', " "),
                bare.replace('_', "-"),
            ];
            if variants
                .iter()
                .any(|variant| restored.contains(variant.as_str()))
            {
                return Err(RestoreError::MangledPlaceholder);
            }
        }
        Ok(())
    }

    /// 流式回填：占位符可能被切在两个 chunk 之间。
    pub fn stream(&self) -> StreamRestorer<'_> {
        StreamRestorer {
            redaction: self,
            pending: String::new(),
            emitted_tail: String::new(),
            restored: Vec::new(),
        }
    }
}

/// 占位符（含括号）最长不会超过这个长度；更长的未闭合 `[` 不再压住。
const MAX_PLACEHOLDER: usize = 64;

/// 逐 chunk 回填；未闭合的 `[` 之后的内容先压住，等下一块或 [`finish`]。
/// 变形检查跨 chunk 进行：上一块已输出文本的尾巴和这一块拼起来再查。
///
/// [`finish`]: StreamRestorer::finish
pub struct StreamRestorer<'a> {
    redaction: &'a Redaction,
    pending: String,
    emitted_tail: String,
    /// How many times each table placeholder was restored so far, so `finish`
    /// can apply batch `restore`'s completeness rule to the whole stream.
    restored: Vec<(String, usize)>,
}

impl StreamRestorer<'_> {
    /// The longest placeholder this redaction actually minted, so the stream
    /// bounds its held prefix by the table instead of a fixed constant.
    fn max_placeholder_len(&self) -> usize {
        self.redaction
            .table
            .iter()
            .map(|(placeholder, _)| placeholder.len())
            .max()
            .unwrap_or(0)
    }

    pub fn push(&mut self, chunk: &str) -> Result<String, RestoreError> {
        self.pending.push_str(chunk);
        let hold_from = match self.pending.rfind('[') {
            Some(open) if !self.pending[open..].contains(']') => {
                let held = &self.pending[open..];
                // A held segment that is still a plausible placeholder prefix but
                // has already outgrown any real placeholder can never become a
                // known one; batch `restore` refuses it, so streaming must too
                // rather than releasing the `[` as plain text.
                if held.len() > self.max_placeholder_len() && is_placeholder_prefix(held) {
                    return Err(RestoreError::UnknownPlaceholder);
                }
                if held.len() <= MAX_PLACEHOLDER {
                    open
                } else {
                    self.pending.len()
                }
            }
            _ => self.pending.len(),
        };
        let ready: String = self.pending.drain(..hold_from).collect();
        self.emit(&ready)
    }

    pub fn finish(mut self) -> Result<String, RestoreError> {
        let rest = std::mem::take(&mut self.pending);
        let tail = self.emit(&rest)?;
        // N1: the batch path refuses a text that lost a placeholder; streaming
        // must reach the same verdict, and `finish` is where the whole stream is
        // finally known.
        for (placeholder, _) in &self.redaction.table {
            if !self.restored.iter().any(|(seen, _)| seen == placeholder) {
                return Err(RestoreError::MangledPlaceholder);
            }
        }
        Ok(tail)
    }

    fn emit(&mut self, ready: &str) -> Result<String, RestoreError> {
        let restored = self
            .redaction
            .replace_streaming(ready, &mut self.restored)?;
        let window = format!("{}{restored}", self.emitted_tail);
        self.redaction.reject_mangled(&window)?;
        let keep = window
            .char_indices()
            .rev()
            .nth(MAX_PLACEHOLDER - 1)
            .map_or(0, |(at, _)| at);
        self.emitted_tail = window[keep..].to_string();
        Ok(restored)
    }
}

/// Placeholder shape ignoring case, so a lowercased or mis-cased placeholder is
/// still recognised as a mangled placeholder rather than plain text.
fn looks_like_placeholder_any_case(candidate: &str) -> bool {
    if !candidate.starts_with('[') || !candidate.ends_with(']') {
        return false;
    }
    let inner = &candidate[1..candidate.len() - 1];
    inner.rsplit_once('_').is_some_and(|(kind, n)| {
        !kind.is_empty()
            && kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Every placeholder-shaped token already present in `text`, so the engine never
/// mints a colliding name and `restore` can pass the user's own text through.
fn placeholder_tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let tail = &rest[open..];
        let Some(close) = tail.find(']') else {
            break;
        };
        let candidate = &tail[..=close];
        if looks_like_placeholder(candidate) {
            found.push(candidate.to_string());
        }
        rest = &tail[candidate.len()..];
    }
    found
}

/// True while the text after an unmatched `[` can still become a placeholder:
/// uppercase letters, digits and `_` only.
fn is_placeholder_prefix(held: &str) -> bool {
    held.strip_prefix('[').is_some_and(|inner| {
        inner
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// `[` + 大写字母/数字/下划线 + `_` + 数字 + `]`。
fn looks_like_placeholder(candidate: &str) -> bool {
    let inner = &candidate[1..candidate.len() - 1];
    inner.rsplit_once('_').is_some_and(|(kind, n)| {
        !kind.is_empty()
            && kind
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
    })
}

fn label(kind: idoris_contracts::inspect::FindingKind) -> String {
    kind.as_str().to_ascii_uppercase()
}

fn utf16(units: &[u16]) -> Result<String, RedactError> {
    String::from_utf16(units).map_err(|_| RedactError::InvalidSpan)
}

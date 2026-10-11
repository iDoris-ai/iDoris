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
    };
    let mut counters: Vec<(String, usize)> = Vec::new();
    let mut cursor = 0;
    for (start, end, kind) in regions {
        out.text.push_str(&utf16(&units[cursor..start])?);
        let original = utf16(&units[start..end])?;
        let placeholder = match out.table.iter().find(|(_, value)| *value == original) {
            Some((placeholder, _)) => placeholder.clone(),
            None => {
                let n = match counters.iter_mut().find(|(k, _)| *k == kind) {
                    Some((_, n)) => {
                        *n += 1;
                        *n
                    }
                    None => {
                        counters.push((kind.clone(), 1));
                        1
                    }
                };
                let placeholder = format!("[{kind}_{n}]");
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
        let restored = self.replace(output)?;
        self.reject_mangled(&restored)?;
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
}

impl StreamRestorer<'_> {
    pub fn push(&mut self, chunk: &str) -> Result<String, RestoreError> {
        self.pending.push_str(chunk);
        let hold_from = self
            .pending
            .rfind('[')
            .filter(|&open| {
                !self.pending[open..].contains(']') && self.pending.len() - open <= MAX_PLACEHOLDER
            })
            .unwrap_or(self.pending.len());
        let ready: String = self.pending.drain(..hold_from).collect();
        self.emit(&ready)
    }

    pub fn finish(mut self) -> Result<String, RestoreError> {
        let rest = std::mem::take(&mut self.pending);
        self.emit(&rest)
    }

    fn emit(&mut self, ready: &str) -> Result<String, RestoreError> {
        let restored = self.redaction.replace(ready)?;
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

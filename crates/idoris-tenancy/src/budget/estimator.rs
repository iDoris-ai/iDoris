//! Conservative token-count estimator, used to size a `reserve()` call
//! before the real usage is known.
//!
//! blog 教训（总体规划 §4.6 依据）：中文按"字符数/4"估算会低估
//! 2-2.4 倍——CJK 文本在 UTF-8 里通常是每字符 3 字节，`bytes/4` 对一个 CJK
//! 字符只算 0.75 个 token，而它实际接近 1 个。This estimator never uses
//! that shortcut.

/// Swappable token-count estimator. `model_family` lets a future
/// real-tokenizer implementation pick the right vocabulary; the
/// conservative default below ignores it — one conservative estimate for
/// every family, deliberately on the high side. This task only ships the
/// trait plus the conservative fallback; wiring in a real tokenizer is a
/// later task.
pub trait TokenEstimator: Send + Sync {
    fn estimate_tokens(&self, text: &str, model_family: &str) -> u64;
}

/// CJK-aware conservative estimator:
/// - every CJK codepoint counts as **>= 1 token** (the floor, not an
///   average — this is what keeps the estimate from ever under-counting
///   CJK text the way `chars/4` does);
/// - the remaining (non-CJK) text is priced at `bytes / 3` rounded up;
/// - the total is then multiplied by a 1.2 safety factor, rounded up.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConservativeTokenEstimator;

/// Whether `c` is priced at >= 1 token/char. Covers the BMP Han/Hangul/Kana
/// blocks plus the supplementary-plane Han extensions and compatibility
/// ideographs — Codex review flagged the first cut of this list as
/// incomplete (missing Extension B and beyond, Hangul Jamo, Bopomofo, Kana
/// extensions), which didn't change the *safety* property (those codepoints
/// still fell through to the `bytes/3` path, which happens not to
/// under-count 3- or 4-byte UTF-8 sequences either) but was a real
/// classification gap against the "every CJK codepoint" claim.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF     // Hangul Jamo
        | 0x3000..=0x303F   // CJK punctuation
        | 0x3040..=0x30FF   // Hiragana + Katakana
        | 0x3100..=0x312F   // Bopomofo
        | 0x31A0..=0x31BF   // Bopomofo Extended
        | 0x31C0..=0x31EF   // CJK Strokes
        | 0x31F0..=0x31FF   // Katakana Phonetic Extensions
        | 0x3400..=0x4DBF   // CJK Extension A
        | 0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0xAC00..=0xD7A3   // Hangul syllables
        | 0xF900..=0xFAFF   // CJK compatibility ideographs
        | 0xFF00..=0xFFEF   // Halfwidth/fullwidth forms
        | 0x1B000..=0x1B0FF // Kana Supplement
        | 0x1B100..=0x1B16F // Kana Extended-A/-B
        | 0x20000..=0x2A6DF // CJK Extension B
        | 0x2A700..=0x2EBEF // CJK Extension C/D/E/F
        | 0x2F800..=0x2FA1F // CJK Compatibility Ideographs Supplement
        | 0x30000..=0x3134F // CJK Extension G/H
    )
}

impl TokenEstimator for ConservativeTokenEstimator {
    fn estimate_tokens(&self, text: &str, _model_family: &str) -> u64 {
        let mut cjk_chars: u64 = 0;
        let mut non_cjk_bytes: u64 = 0;
        for c in text.chars() {
            if is_cjk(c) {
                cjk_chars += 1;
            } else {
                non_cjk_bytes += c.len_utf8() as u64;
            }
        }
        let non_cjk_tokens = non_cjk_bytes.div_ceil(3);
        let raw = cjk_chars + non_cjk_tokens;
        // `ceil(raw * 6 / 5)` computed as `raw + ceil(raw / 5)` instead of
        // `(raw * 6).div_ceil(5)` — algebraically identical (6/5 = 1 + 1/5)
        // but avoids the multiply-by-6 overflowing `u64` for `raw` values
        // near `u64::MAX / 6` (Codex review: a maximal-length `str` could
        // reach this in principle).
        raw + raw.div_ceil(5)
    }
}

/// Convenience free function using the default conservative estimator —
/// what most callers want; construct [`ConservativeTokenEstimator`]
/// directly (or a future real-tokenizer impl of [`TokenEstimator`]) to be
/// explicit about which estimator is in play.
pub fn estimate_tokens(text: &str, model_family: &str) -> u64 {
    ConservativeTokenEstimator.estimate_tokens(text, model_family)
}

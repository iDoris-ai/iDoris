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
#[cfg(test)]
mod tests {
    use super::*;

    /// Naive (flawed) `chars/4` estimator the blog post warns about, used
    /// here only as a negative-control baseline.
    fn naive_chars_over_4(text: &str) -> u64 {
        (text.chars().count() as u64).div_ceil(4)
    }

    /// `ceil(n_cjk_chars * 6 / 5)` — the exact value [`estimate_tokens`]
    /// computes for text made entirely of CJK characters (no non-CJK bytes
    /// at all), used to assert exact expected values instead of just
    /// relative comparisons.
    fn expected_cjk_only(n_chars: u64) -> u64 {
        (n_chars * 6).div_ceil(5)
    }

    /// `ceil(ceil(n_ascii_bytes / 3) * 6 / 5)` — the exact value for text
    /// made entirely of single-byte non-CJK characters.
    fn expected_ascii_only(n_bytes: u64) -> u64 {
        (n_bytes.div_ceil(3) * 6).div_ceil(5)
    }

    /// Negative control establishing the baseline is actually flawed: for
    /// real CJK text, `chars/4` must undercount relative to the character
    /// count itself (each CJK char is worth close to one full token, not a
    /// quarter of one). If this ever failed, the "estimator" tests below
    /// would be comparing against a baseline that isn't actually the
    /// problem the CJK-aware estimator exists to fix.
    #[test]
    fn naive_chars_over_4_baseline_really_does_undercount_cjk_text() {
        let text = "预算账本必须原子化处理并发预留否则会超支";
        let chars = text.chars().count() as u64;
        assert!(
            naive_chars_over_4(text) < chars,
            "the naive baseline should undercount CJK text relative to its own char count"
        );
    }

    /// The real, universally-true invariant this estimator provides: it
    /// never returns less than the flawed `chars/4` baseline would, for any
    /// input length (not just a specific sample text where a looser bound
    /// like `> naive * 2` happens to hold).
    #[test]
    fn conservative_estimate_is_never_below_the_naive_baseline() {
        for n in 0..40u64 {
            let text = "中".repeat(n as usize);
            let conservative = estimate_tokens(&text, "qwen");
            let naive = naive_chars_over_4(&text);
            assert!(
                conservative >= naive,
                "n={n}: conservative ({conservative}) must be >= naive ({naive})"
            );
        }
    }

    /// For a realistic CJK sentence, the conservative estimate should still
    /// be substantially — not just marginally — above the naive baseline,
    /// which is the practical point of not using `chars/4`.
    #[test]
    fn realistic_cjk_sentence_is_well_above_the_naive_baseline() {
        let text = "预算账本必须原子化处理并发预留否则会超支";
        let conservative = estimate_tokens(text, "qwen");
        let naive = naive_chars_over_4(text);
        assert!(
            conservative > naive * 2,
            "conservative ({conservative}) should be well above naive ({naive}) for a \
             realistic-length CJK sentence"
        );
    }

    #[test]
    fn cjk_only_text_matches_the_exact_formula() {
        for n in [0u64, 1, 2, 4, 5, 6, 20, 39] {
            let text = "中".repeat(n as usize);
            assert_eq!(
                estimate_tokens(&text, "qwen"),
                expected_cjk_only(n),
                "n={n}"
            );
        }
    }

    #[test]
    fn cjk_text_estimate_is_at_least_the_character_count() {
        let text = "中文测试文本"; // 6 CJK chars, no ASCII
        let chars = text.chars().count() as u64;
        assert!(estimate_tokens(text, "qwen") >= chars);
    }

    /// A single CJK character is still floored at >= 1 token, not rounded
    /// down to 0 — the smallest possible non-empty input.
    #[test]
    fn single_cjk_character_is_at_least_one_token() {
        assert!(estimate_tokens("中", "qwen") >= 1);
    }

    /// Full-width CJK punctuation is priced the same as other CJK
    /// characters (the `is_cjk` halfwidth/fullwidth-forms and CJK-
    /// punctuation ranges), not silently dropped or priced as free.
    #[test]
    fn cjk_punctuation_only_text_matches_the_exact_formula() {
        let text = "，。！？"; // 4 fullwidth/CJK punctuation marks
        let n = text.chars().count() as u64;
        assert_eq!(estimate_tokens(text, "qwen"), expected_cjk_only(n));
    }

    #[test]
    fn ascii_only_text_matches_the_exact_formula() {
        for n in [0u64, 1, 2, 3, 4, 6, 30] {
            let text = "a".repeat(n as usize);
            assert_eq!(
                estimate_tokens(&text, "qwen"),
                expected_ascii_only(n),
                "n={n}"
            );
        }
    }

    /// Negative control: pure ASCII text is priced by the `bytes/3` path,
    /// not the CJK floor — for the same character count it must land
    /// strictly below the CJK estimate (proven via the exact formulas
    /// above, not just this relative comparison).
    #[test]
    fn ascii_text_is_estimated_lower_per_char_than_cjk() {
        let ascii = "a".repeat(6); // 6 ASCII chars, 6 bytes
        let cjk = "中".repeat(6); // 6 CJK chars, 18 bytes
        assert!(estimate_tokens(&ascii, "qwen") < estimate_tokens(&cjk, "qwen"));
    }

    #[test]
    fn empty_text_estimates_zero_tokens() {
        assert_eq!(estimate_tokens("", "qwen"), 0);
    }

    /// Longer text exercises the accumulation path (not just small,
    /// easily-coincidental numbers) and must still match the exact formula.
    #[test]
    fn long_cjk_text_matches_the_exact_formula() {
        let text = "中".repeat(500);
        assert_eq!(estimate_tokens(&text, "qwen"), expected_cjk_only(500));
    }

    #[test]
    fn model_family_does_not_change_the_conservative_estimate() {
        let text = "hello 世界";
        assert_eq!(
            estimate_tokens(text, "gpt-5"),
            estimate_tokens(text, "qwen3-8b")
        );
    }
}

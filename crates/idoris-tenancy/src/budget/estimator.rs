//! Conservative token-count estimator, used to size a `reserve()` call
//! before the real usage is known.
//!
//! blog 教训（总体规划 §4.6 依据）：中文按"字符数/4"估算会低估
//! 2-2.4 倍——CJK 文本在 UTF-8 里通常是每字符 3 字节，`bytes/4` 对一个 CJK
//! 字符只算 0.75 个 token，而它实际接近 1 个。

/// Swappable token-count estimator. `model_family` lets a future
/// real-tokenizer implementation pick the right vocabulary; the
/// conservative default below ignores it — one conservative estimate for
/// every family, deliberately on the high side. This task only ships the
/// trait plus the conservative fallback; a real tokenizer (e.g. calling out
/// to `tiktoken`/a model's own BPE) is a later task and can slot in as
/// another `TokenEstimator` impl without touching call sites.
pub trait TokenEstimator: Send + Sync {
    fn estimate_tokens(&self, text: &str, model_family: &str) -> u64;
}

/// ASCII-vs-non-ASCII conservative estimator (Opus Tier-2 acceptance M3
/// replaces the original CJK-Unicode-range approach):
/// - the ASCII portion is priced at `bytes / 3` rounded up (ASCII bytes ==
///   ASCII chars, so this is also `chars / 3`);
/// - the non-ASCII portion is priced at `max(chars, ceil(bytes / 2))`, then
///   multiplied by a 1.2 safety factor rounded up.
///
/// The original implementation classified codepoints via an explicit list
/// of CJK Unicode ranges. That list, however carefully extended, is
/// necessarily incomplete: Thai, Devanagari, and other non-Latin scripts
/// fell through to the ASCII-style `bytes/3` path even though they are not
/// 1-byte-per-char in UTF-8, and a multi-codepoint emoji (e.g. a ZWJ family
/// sequence) is several rarely-1-token codepoints that a range list has no
/// principled way to price. Splitting on ASCII vs. not sidesteps the
/// classification problem entirely: *anything* outside the ASCII range
/// (which real tokenizers reliably spend >= 1 token per byte-heavy
/// codepoint on) gets the conservative `max(chars, bytes/2)` treatment,
/// with no per-script allowlist to keep up to date.
///
/// This is a **conservative upper-bound heuristic**, not a claim that it
/// never under-counts every possible input against every possible real
/// tokenizer — no fixed formula can promise that against an unknown future
/// vocabulary. Swap in a real tokenizer via [`TokenEstimator`] where that
/// stronger guarantee actually matters.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConservativeTokenEstimator;

impl TokenEstimator for ConservativeTokenEstimator {
    fn estimate_tokens(&self, text: &str, _model_family: &str) -> u64 {
        let mut ascii_bytes: u64 = 0;
        let mut non_ascii_chars: u64 = 0;
        let mut non_ascii_bytes: u64 = 0;
        for c in text.chars() {
            if c.is_ascii() {
                // An ASCII char is always exactly 1 byte.
                ascii_bytes += 1;
            } else {
                non_ascii_chars += 1;
                non_ascii_bytes += c.len_utf8() as u64;
            }
        }

        let ascii_tokens = ascii_bytes.div_ceil(3);

        let non_ascii_raw = non_ascii_chars.max(non_ascii_bytes.div_ceil(2));
        // `ceil(non_ascii_raw * 6 / 5)` computed as `raw + ceil(raw / 5)`
        // instead of `(raw * 6).div_ceil(5)` — algebraically identical
        // (6/5 = 1 + 1/5) but avoids the multiply-by-6 overflowing `u64` for
        // `raw` values near `u64::MAX / 6`.
        let non_ascii_tokens = non_ascii_raw + non_ascii_raw.div_ceil(5);

        // Both terms are independently conservative; summing (rather than
        // safety-factoring the combined total again) keeps the ASCII
        // portion's contribution unchanged from a pure-ASCII input.
        ascii_tokens + non_ascii_tokens
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

    /// Exact value for text made entirely of single-byte ASCII characters:
    /// `ceil(n_bytes / 3)`, no safety factor (the factor only applies to the
    /// non-ASCII portion since M3).
    fn expected_ascii_only(n_bytes: u64) -> u64 {
        n_bytes.div_ceil(3)
    }

    /// Exact value for the non-ASCII portion alone:
    /// `ceil(max(chars, ceil(bytes/2)) * 6 / 5)`.
    fn expected_non_ascii(chars: u64, bytes: u64) -> u64 {
        let raw = chars.max(bytes.div_ceil(2));
        raw + raw.div_ceil(5)
    }

    /// Negative control establishing the baseline is actually flawed: for
    /// real CJK text, `chars/4` must undercount relative to the character
    /// count itself (each CJK char is worth close to one full token, not a
    /// quarter of one).
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
    /// input length.
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
    /// be substantially — not just marginally — above the naive baseline.
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
            // "中" is 3 UTF-8 bytes.
            let expected = expected_non_ascii(n, n * 3);
            assert_eq!(estimate_tokens(&text, "qwen"), expected, "n={n}");
        }
    }

    /// A single CJK character is still floored at >= 1 token, not rounded
    /// down to 0 — the smallest possible non-empty input.
    #[test]
    fn single_cjk_character_is_at_least_one_token() {
        assert!(estimate_tokens("中", "qwen") >= 1);
    }

    /// Full-width CJK punctuation is priced the same as other non-ASCII
    /// characters — not silently dropped or priced as free.
    #[test]
    fn cjk_punctuation_only_text_matches_the_exact_formula() {
        let text = "，。！？"; // 4 fullwidth/CJK punctuation marks, 3 bytes each
        let n = text.chars().count() as u64;
        assert_eq!(estimate_tokens(text, "qwen"), expected_non_ascii(n, n * 3));
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
    /// not the non-ASCII floor — for the same character count it must land
    /// strictly below the CJK estimate.
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

    /// Longer text exercises the accumulation path and must still match the
    /// exact formula.
    #[test]
    fn long_cjk_text_matches_the_exact_formula() {
        let text = "中".repeat(500);
        assert_eq!(
            estimate_tokens(&text, "qwen"),
            expected_non_ascii(500, 1500)
        );
    }

    #[test]
    fn model_family_does_not_change_the_conservative_estimate() {
        let text = "hello 世界";
        assert_eq!(
            estimate_tokens(text, "gpt-5"),
            estimate_tokens(text, "qwen3-8b")
        );
    }

    /// Mixed ASCII + non-ASCII text: the two portions are priced
    /// independently and summed (M3) — pin the exact combined value down so
    /// a regression that re-couples them (e.g. safety-factoring the sum
    /// again) is caught.
    #[test]
    fn mixed_ascii_and_non_ascii_text_matches_the_exact_formula() {
        let text = "hello 世界"; // 6 ASCII bytes ("hello "), 2 CJK chars/6 bytes
        let expected = expected_ascii_only(6) + expected_non_ascii(2, 6);
        assert_eq!(estimate_tokens(text, "qwen"), expected);
    }

    /// Thai script: no CJK Unicode range covers it, and Thai is not
    /// 1-byte-per-char in UTF-8 (3 bytes/char) — the old range-list approach
    /// would have silently priced this via the ASCII-style `bytes/3` path
    /// despite it not being ASCII. The new ASCII-vs-not split prices it via
    /// the non-ASCII formula purely because it isn't ASCII, with no Thai-
    /// specific allowlist entry required.
    #[test]
    fn thai_script_text_matches_the_non_ascii_formula() {
        let text = "สวัสดีครับ";
        let chars = text.chars().count() as u64;
        let bytes = text.len() as u64;
        assert_eq!(
            estimate_tokens(text, "qwen"),
            expected_non_ascii(chars, bytes)
        );
    }

    /// Devanagari script (Hindi): same argument as Thai above — 3 bytes/char
    /// in UTF-8, no CJK range, previously mispriced as if it were ASCII.
    #[test]
    fn devanagari_script_text_matches_the_non_ascii_formula() {
        let text = "नमस्ते";
        let chars = text.chars().count() as u64;
        let bytes = text.len() as u64;
        assert_eq!(
            estimate_tokens(text, "qwen"),
            expected_non_ascii(chars, bytes)
        );
    }

    /// CJK Extension B (supplementary plane, 4 bytes/char in UTF-8): the old
    /// range list had to enumerate this explicitly (and the first cut of it
    /// didn't); the ASCII-vs-not split covers it automatically since it's
    /// simply "not ASCII", with no extension-plane bookkeeping needed.
    #[test]
    fn cjk_extension_b_character_matches_the_non_ascii_formula() {
        let text = "\u{20000}"; // CJK Extension B, first codepoint
        let chars = 1u64;
        let bytes = text.len() as u64; // 4 bytes
        assert_eq!(
            estimate_tokens(text, "qwen"),
            expected_non_ascii(chars, bytes)
        );
    }

    /// A multi-codepoint emoji ZWJ sequence (family: man, woman, girl, boy)
    /// is several 4-byte codepoints joined by U+200D ZERO WIDTH JOINER — a
    /// case no CJK range list has any principled way to price, and one a
    /// naive "count grapheme clusters as 1" approach would under-count
    /// relative to what a real BPE tokenizer spends on it (each codepoint,
    /// including the joiners, typically costs its own token or more).
    #[test]
    fn emoji_zwj_sequence_matches_the_non_ascii_formula() {
        let text = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let chars = text.chars().count() as u64; // 7 codepoints
        let bytes = text.len() as u64;
        assert_eq!(
            estimate_tokens(text, "qwen"),
            expected_non_ascii(chars, bytes)
        );
    }
}

//! Deterministic offline embedding, matching `packages/router/src/intent.ts`.

use std::num::NonZeroUsize;

/// Word + UTF-16 bigram signed hashing, followed by L2 normalization.
/// A nonzero dimension prevents an invalid bucket index; TS defaults to 256.
pub fn hashing_embedding(text: &str, dim: NonZeroUsize) -> Vec<f64> {
    let mut vector = vec![0.0; dim.get()];
    let normalized = text.to_lowercase();
    for token in normalized.split(js_whitespace).filter(|s| !s.is_empty()) {
        let units: Vec<u16> = token.encode_utf16().collect();
        for feature in std::iter::once(units.as_slice()).chain(units.windows(2)) {
            let hash = fnv1a(feature);
            vector[hash as usize % dim.get()] += if hash & 0x8000_0000 != 0 { -1.0 } else { 1.0 };
        }
    }
    let norm = vector.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm != 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn fnv1a(units: &[u16]) -> u32 {
    units.iter().fold(0x811c_9dc5, |hash, unit| {
        (hash ^ u32::from(*unit)).wrapping_mul(0x0100_0193)
    })
}

// ECMAScript's /\s/ and trim set: includes BOM, excludes NEL (U+0085).
fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// Cosine over the shorter length; either zero norm gives zero similarity.
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn embed(text: &str, dim: usize) -> Vec<f64> {
        hashing_embedding(text, NonZeroUsize::new(dim).unwrap())
    }

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    // Fixed outputs from TS fnv1a (charCodeAt + Math.imul), not Rust-derived.
    #[test]
    fn fnv_matches_ts_fixed_hashes() {
        for (text, expected) in [
            ("", 2166136261),
            ("a", 3826002220),
            ("hello", 1335831723),
            ("写一个函数", 840273095),
            ("😀", 3409036472),
            ("A😀中", 1088501276),
            ("𠮷", 2955023030),
        ] {
            assert_eq!(fnv1a(&text.encode_utf16().collect::<Vec<_>>()), expected);
        }
    }

    // TS hashingEmbedder(16): integers below / denominator = fixed vectors.
    #[test]
    fn signed_hashing_and_l2_match_ts_vectors() {
        for (text, counts, denominator) in [
            (
                "Hello hello",
                [0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, 1, 1, 0, 0, 0],
                5_f64.sqrt(),
            ),
            (
                "写一个函数",
                [1, 0, 0, 0, 0, 0, 0, 1, -1, 0, 0, 0, -1, 0, 0, -1],
                5_f64.sqrt(),
            ),
            ("😀", [0, 0, 0, 0, 0, 0, 0, 0, -1, 0, 0, 0, 0, 0, 0, 0], 1.0),
            (
                "A😀中",
                [0, 0, 0, 1, 0, 0, 1, 0, -1, 0, 0, 0, -1, 0, 0, 0],
                2.0,
            ),
            ("𠮷", [0, 0, 0, 0, 0, 0, -1, 0, 0, 0, 0, 0, 0, 0, 0, 0], 1.0),
        ] {
            let vector = embed(text, 16);
            assert_eq!(vector.len(), counts.len());
            for (actual, count) in vector.iter().zip(counts) {
                close(*actual, f64::from(count) / denominator);
            }
            close(vector.iter().map(|x| x * x).sum::<f64>(), 1.0);
        }
    }

    #[test]
    fn normalization_matches_ts_whitespace_and_lowercase() {
        let expected = [0, 0, 0, 1, 2, 1, 1, 0, 0, 0, 0, 3, 1, 1, 0, 0];
        for (actual, count) in embed("\u{feff}  HeLLo\t\n WORLD\u{a0}", 16)
            .iter()
            .zip(expected)
        {
            close(*actual, f64::from(count) / 18_f64.sqrt());
        }
        assert_eq!(
            embed("a\u{85}b", 16),
            vec![
                0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0
            ]
        );
        let expected = [0, 0, 0, 0, -1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for (actual, count) in embed("ΟΣ İ", 16).iter().zip(expected) {
            close(*actual, f64::from(count) / 2_f64.sqrt());
        }
    }

    #[test]
    fn zero_vectors_stay_finite() {
        for text in ["", " \t\n\u{feff}"] {
            assert_eq!(embed(text, 256), vec![0.0; 256]);
        }
        // TS signed buckets cancel completely at dimension 1.
        assert_eq!(embed("hello a a a a a", 1), vec![0.0]);
        assert_eq!(cosine(&[0.0; 4], &[1.0; 4]), 0.0);
        assert_eq!(cosine(&[1.0; 4], &[0.0; 4]), 0.0);
        assert_eq!(cosine(&[], &[1.0]), 0.0);
    }

    #[test]
    fn cosine_matches_ts_and_uses_shorter_length() {
        close(cosine(&[1.0, 2.0, 999.0], &[3.0, 4.0]), 0.9838699100999074);
        close(cosine(&[3.0, 4.0], &[1.0, 2.0, 999.0]), 0.9838699100999074);
        close(cosine(&[1.0, 0.0], &[-1.0, 0.0]), -1.0);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn different_text_is_not_an_identical_embedding() {
        let a = embed("写一个函数", 256);
        let b = embed("帮我发一封邮件", 256);
        assert_eq!(a, embed("写一个函数", 256));
        assert_ne!(a, b);
        close(cosine(&a, &a), 1.0);
        close(cosine(&a, &b), 0.0); // Fixed TS score, not just "below threshold".
        let a = embed("fix this bug", 16);
        close(cosine(&a, &embed("fix that bug", 16)), 0.7938566201357355);
        close(cosine(&a, &embed("send an email", 16)), 0.18190171877724973);
    }
}

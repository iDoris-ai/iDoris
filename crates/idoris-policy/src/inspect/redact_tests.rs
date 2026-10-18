#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{
    FindingKind, InspectAction, InspectFinding, InspectResponse, InspectSpan, Sensitivity,
};

use super::redact::{RedactError, RestoreError, redact};
use super::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn consenting() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn redacted(input: &str) -> super::redact::Redaction {
    let verdict = inspect(input, &consenting());
    assert_eq!(verdict.action, InspectAction::Redact, "{verdict:?}");
    redact(input, &verdict, &consenting()).unwrap()
}

#[test]
fn placeholders_are_stable_per_value_and_numbered_per_kind() {
    let input = "张三 13812345678，李四 13987654321，再打 13812345678 或写信 a.b@example.com";
    let r = redacted(input);
    assert_eq!(
        r.text,
        "张三 [CN_MOBILE_1]，李四 [CN_MOBILE_2]，再打 [CN_MOBILE_1] 或写信 [EMAIL_ADDRESS_1]"
    );
    let debug = format!("{r:?}");
    for original in ["13812345678", "13987654321", "a.b@example.com"] {
        assert!(!debug.contains(original), "{debug}");
    }
}

#[test]
fn overlapping_findings_become_one_region() {
    // The phone is both a mobile hit and the local part of an email hit.
    let r = redacted("联系 13812345678@qq.com 谢谢");
    assert_eq!(r.text, "联系 [CN_MOBILE_1] 谢谢");
}

#[test]
fn only_redact_verdicts_are_redactable() {
    for input in ["身份证 11010519491231002X", "今天天气不错"] {
        let verdict = inspect(input, &consenting());
        assert_ne!(verdict.action, InspectAction::Redact);
        assert_eq!(
            redact(input, &verdict, &consenting()).unwrap_err(),
            RedactError::NotRedactable
        );
    }
}

#[test]
fn a_verdict_that_misses_an_entity_fails_closed() {
    let input = "13812345678 和 13987654321";
    let partial = InspectResponse {
        sensitivity: Sensitivity::S2,
        findings: vec![InspectFinding {
            kind: FindingKind::CnMobile,
            span: Some(InspectSpan {
                start: 0,
                length: 11,
            }),
        }],
        action: InspectAction::Redact,
    };
    assert_eq!(
        redact(input, &partial, &consenting()).unwrap_err(),
        RedactError::ResidualFinding
    );
    let out_of_bounds = InspectResponse {
        findings: vec![InspectFinding {
            kind: FindingKind::CnMobile,
            span: Some(InspectSpan {
                start: 20,
                length: 11,
            }),
        }],
        ..partial
    };
    assert_eq!(
        redact(input, &out_of_bounds, &consenting()).unwrap_err(),
        RedactError::InvalidSpan
    );
}

#[test]
fn restore_round_trips_and_leaves_ordinary_brackets_alone() {
    let r = redacted("请联系 13812345678");
    assert_eq!(
        r.restore("好的，我会拨打 [CN_MOBILE_1]。参考 [1] 和 [note]。")
            .unwrap(),
        "好的，我会拨打 13812345678。参考 [1] 和 [note]。"
    );
}

#[test]
fn unknown_or_mangled_placeholders_refuse_to_restore() {
    let r = redacted("请联系 13812345678");
    assert_eq!(
        r.restore("拨打 [CN_MOBILE_2]").unwrap_err(),
        RestoreError::UnknownPlaceholder
    );
    assert_eq!(
        r.restore("拨打 [PERSON_1]").unwrap_err(),
        RestoreError::UnknownPlaceholder
    );
    for mangled in [
        "拨打 CN_MOBILE_1",
        "拨打 CN MOBILE 1",
        "拨打 [CN-MOBILE-1]",
        "拨打 [CN_MOBILE_1",
    ] {
        assert_eq!(
            r.restore(mangled).unwrap_err(),
            RestoreError::MangledPlaceholder,
            "{mangled}"
        );
    }
}

#[test]
fn streaming_restore_handles_placeholders_split_across_chunks() {
    let r = redacted("请联系 13812345678 或 a.b@example.com");
    let mut stream = r.stream();
    let mut out = String::new();
    for chunk in [
        "我会打 [CN_MO",
        "BILE_1] 并写信给 [EMAIL",
        "_ADDRESS_1",
        "]。[",
        "1] 完",
    ] {
        out.push_str(&stream.push(chunk).unwrap());
    }
    out.push_str(&stream.finish().unwrap());
    assert_eq!(out, "我会打 13812345678 并写信给 a.b@example.com。[1] 完");

    // A mangled label split across chunks is still caught.
    let mut stream = r.stream();
    stream.push("打 CN_MOB").unwrap();
    assert_eq!(
        stream.push("ILE_1 吧").unwrap_err(),
        RestoreError::MangledPlaceholder
    );

    // An unclosed `[` longer than any placeholder is not held forever.
    let mut stream = r.stream();
    let long = format!("[{}", "x".repeat(100));
    assert_eq!(stream.push(&long).unwrap(), long);
}

/// U1: a literal placeholder-shaped token in the input must survive the round
/// trip untouched. Before the fix, `redact` minted the same name and `restore`
/// wrote personal data over the user's own text.
#[test]
fn a_literal_placeholder_in_the_input_round_trips_byte_identically() {
    let input = "引用 [CN_MOBILE_1]，电话 13812345678";
    let verdict = inspect(input, &consenting());
    let redaction = redact(input, &verdict, &consenting()).unwrap();
    assert!(
        !redaction.text.contains("[CN_MOBILE_1]，电话 [CN_MOBILE_1]"),
        "the minted name collided with the literal: {}",
        redaction.text
    );
    assert_eq!(redaction.restore(&redaction.text).unwrap(), input);
}

/// U2: streaming must refuse exactly what batch `restore` refuses. An unmatched
/// `[` that has outgrown any real placeholder can never become a known one.
#[test]
fn streaming_refuses_an_overflowed_placeholder_prefix_like_batch() {
    let r = redacted("请联系 13812345678");
    let overflow = format!("[PERSON_{}]", "1".repeat(65));
    assert_eq!(
        r.restore(&overflow).unwrap_err(),
        RestoreError::UnknownPlaceholder
    );
    let mut stream = r.stream();
    let first = &overflow[..overflow.len() - 1];
    let second = "]";
    match stream.push(first) {
        Err(error) => assert_eq!(error, RestoreError::UnknownPlaceholder),
        Ok(_) => {
            assert_eq!(
                stream.push(second).unwrap_err(),
                RestoreError::UnknownPlaceholder,
                "stream must refuse what batch restore refuses"
            );
        }
    }
}

/// U3: §1.4 asks for a set comparison. A swallowed, lowercased or de-bracketed
/// placeholder is a mangled one and must be refused.
#[test]
fn restore_refuses_missing_lowercased_and_debracketed_placeholders() {
    let r = redacted("请联系 13812345678");
    for mangled in ["电话 已省略", "电话 [cn_mobile_1]", "电话 CN_MOBILE1"] {
        assert_eq!(
            r.restore(mangled).unwrap_err(),
            RestoreError::MangledPlaceholder,
            "{mangled}"
        );
    }
    // The rows that already erred must keep their verdicts.
    for (text, expected) in [
        ("CN_MOBILE_1", RestoreError::MangledPlaceholder),
        ("[CN_MOBILE_01]", RestoreError::UnknownPlaceholder),
    ] {
        assert_eq!(r.restore(text).unwrap_err(), expected, "{text}");
    }
}

/// N1: the streaming path must reach the same verdict as batch `restore` for the
/// three U3 rows. Before this, only the batch path refused them.
#[test]
fn streaming_refuses_the_same_mangled_placeholders_as_batch() {
    let r = redacted("请联系 13812345678");
    for mangled in [
        "电话 已省略",        // placeholder swallowed entirely
        "电话 [cn_mobile_1]", // wrong case
        "电话 CN_MOBILE1",    // de-bracketed and de-underscored
        // The real placeholder is present *and* a wrong-case twin is too: only
        // the placeholder-shape check can refuse this one.
        "电话 [CN_MOBILE_1] 与 [cn_mobile_1]",
    ] {
        assert_eq!(
            r.restore(mangled).unwrap_err(),
            RestoreError::MangledPlaceholder,
            "batch: {mangled}"
        );
        let mut stream = r.stream();
        let streamed = stream
            .push(mangled)
            .and_then(|first| stream.finish().map(|tail| format!("{first}{tail}")));
        assert_eq!(
            streamed.unwrap_err(),
            RestoreError::MangledPlaceholder,
            "stream: {mangled}"
        );
    }
}

fn stream_chunks(r: &super::redact::Redaction, chunks: &[&str]) -> Result<String, RestoreError> {
    let mut stream = r.stream();
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&stream.push(chunk)?);
    }
    out.push_str(&stream.finish()?);
    Ok(out)
}

pub(super) fn assert_all_partitions(r: &super::redact::Redaction, text: &str, accepted: bool) {
    let expected = r.restore(text);
    assert_eq!(expected.is_ok(), accepted, "batch: {text}");
    let boundaries: Vec<_> = (0..=text.len())
        .filter(|&at| text.is_char_boundary(at))
        .collect();
    for &a in &boundaries {
        assert_eq!(
            stream_chunks(r, &[&text[..a], &text[a..]]),
            expected,
            "two {a}: {text}"
        );
        for &b in boundaries.iter().filter(|&&b| b >= a) {
            assert_eq!(
                stream_chunks(r, &[&text[..a], &text[a..b], &text[b..]]),
                expected,
                "three {a}/{b}: {text}"
            );
        }
    }
    let chars: Vec<_> = text
        .char_indices()
        .map(|(at, c)| &text[at..at + c.len_utf8()])
        .collect();
    assert_eq!(stream_chunks(r, &chars), expected, "chars: {text}");
}

#[test]
fn fullwidth_and_literal_boundaries_cannot_hide_mangled_twins() {
    let r = redacted("电话 13812345678");
    for twin in [
        "[CN_MOBILE_１]",
        "[ＣＮ＿ＭＯＢＩＬＥ＿１]",
        "CN_MOBILE_１",
        "[ＣＮ_MOBILE_1]",
        "[CN＿MOBILE＿1]",
        "[cn＿mobile_１]",
        "ＣＮ＿mobile_1",
        "[CN_MOBＩLE_1]",
    ] {
        assert_all_partitions(&r, &format!("[CN_MOBILE_1] {twin}"), false);
    }
    for (literal, twin) in [
        ("[1]", "[CN_MOBILE_[1]]"),
        ("[]", "[CN_MOBILE_[]1]"),
        ("[１]", "[CN_MOBILE_[１]]"),
        ("[MOBILE]", "[CN_[MOBILE]_1]"),
        ("[CN] [MOBILE] [1]", "[CN][MOBILE][1]"),
        ("[CN_MOBILE_]", "[CN_MOBILE_]1"),
    ] {
        let input = format!("参考 {literal} 电话 13812345678");
        let r = redacted(&input);
        assert_all_partitions(&r, &r.text, true);
        assert_eq!(r.restore(&r.text).unwrap(), input);
        assert_all_partitions(&r, &format!("[CN_MOBILE_1] {twin}"), false);
    }
}

#[test]
fn fullwidth_lowercase_long_and_nested_literals_round_trip() {
    for literal in [
        "[ＣＮ＿ＭＯＢＩＬＥ＿１]".to_owned(),
        "[cn_mobile_1]".into(),
        "[[ＣＮ＿ＭＯＢＩＬＥ＿１]".into(),
        "[]".into(),
        "[1]".into(),
        format!("[ＰＥＲＳＯＮ＿{}]", "１".repeat(100)),
        format!("[[cn_mobile_{}]", "1".repeat(100)),
    ] {
        let input = format!("参考 {literal} 电话 13812345678");
        let r = redacted(&input);
        assert_eq!(r.restore(&r.text).unwrap(), input);
        assert_all_partitions(&r, &r.text, true);
    }
}

#[test]
fn allocator_skips_bare_normalized_name_collisions() {
    for literal in [
        "CN_MOBILE1",
        "cn_mobile_1",
        "cn mobile 1",
        "ＣＮ＿ＭＯＢＩＬＥ＿１",
    ] {
        let input = format!("引用 {literal} 电话 13812345678");
        let r = redacted(&input);
        assert!(r.text.ends_with("[CN_MOBILE_2]"), "{}", r.text);
        assert_all_partitions(&r, &r.text, true);
        assert_eq!(r.restore(&r.text).unwrap(), input);
    }
}

#[test]
fn parser_contract_at_every_two_and_three_chunk_utf8_boundary() {
    let r = redacted("电话 13812345678");
    let mut rows = vec![
        ("电话 已省略".to_owned(), false),
        ("电话 [cn_mobile_1]".into(), false),
        ("电话 CN_MOBILE1".into(), false),
        ("[CN_MOBILE_1] [cn_mobile_1]".into(), false),
        ("[CN_MOBILE_1] [person_１]".into(), false),
        ("[CN_MOBILE_1] [ｐｅｒｓｏｎ＿１]".into(), false),
        ("[CN_MOBILE_1] [ＣＮ＿ＭＯＢＩＬＥ＿９]".into(), false),
        ("[CN_MOBILE_1] [[cn_mobile_1]]".into(), false),
        ("[CN_MOBILE_1] [ [cn_mobile_1]".into(), false),
        ("[CN_MOBILE_1] [CN_MOBILE1]".into(), false),
        ("[CN_MOBILE_1] CN_MOBILE1".into(), false),
        ("[CN_MOBILE_1] [cn_mobile_1".into(), false),
        ("[CN_MOBILE_1] c.n---mobile 1".into(), false),
        ("[CN_MOBILE_1] x[row_1] [step_2]".into(), true),
        ("[CN_MOBILE_1] [IMPORTANT_NOTE]".into(), true),
        ("[CN_MOBILE_1] [IMPORTANT_NOTE_1]".into(), false),
        ("[CN_MOBILE_1] [CN_MOBILE_01]".into(), false),
        ("[CN_MOBILE_1] [CN_MOBILE_1]".into(), true),
        ("[[CN_MOBILE_1] 🌊 [note] 文 [".into(), true),
    ];
    for kind in ["person", "PERSON", "CN_MOBILE", "step", "IMPORTANT"] {
        rows.push((
            format!("[CN_MOBILE_1] [{kind}_{}]", "1".repeat(65)),
            kind == "step",
        ));
    }
    rows.push((format!("[CN_MOBILE_1] [{}]", "x".repeat(100)), true));
    rows.push((format!("[CN_MOBILE_1] [{}]", "A".repeat(100)), true));
    rows.push((format!("[CN_MOBILE_1] [{}_1]", "A".repeat(100)), false));
    for (text, accepted) in rows {
        let expected = r.restore(&text);
        assert_eq!(expected.is_ok(), accepted, "contract: {text}");
        let chars: Vec<_> = text
            .char_indices()
            .map(|(at, c)| &text[at..at + c.len_utf8()])
            .collect();
        assert_eq!(stream_chunks(&r, &chars), expected, "chars: {text}");
        for a in 0..=text.len() {
            if !text.is_char_boundary(a) {
                continue;
            }
            for b in a..=text.len() {
                if !text.is_char_boundary(b) {
                    continue;
                }
                let actual = stream_chunks(&r, &[&text[..a], &text[a..b], &text[b..]]);
                assert_eq!(actual.is_ok(), accepted, "split {a}/{b}: {text}");
                if accepted {
                    assert_eq!(actual, expected, "split {a}/{b}: {text}");
                }
            }
        }
    }
}

#[test]
fn literal_collisions_nested_literals_and_long_literals_round_trip_at_every_split() {
    for literal in [
        "[CN_MOBILE_1]",
        "[[CN_MOBILE_1]",
        "[cn_mobile_1]",
        "x[row_1]",
        "[step_2]",
        "[IMPORTANT_NOTE_1]",
        &format!("[PERSON_{}]", "1".repeat(100)),
    ] {
        let input = format!("🌊 引用 {literal} 电话 13812345678");
        let r = redacted(&input);
        assert_eq!(r.restore(&r.text).unwrap(), input);
        for at in 0..=r.text.len() {
            if r.text.is_char_boundary(at) {
                assert_eq!(
                    stream_chunks(&r, &[&r.text[..at], &r.text[at..]]).unwrap(),
                    input
                );
            }
        }
    }
}

#[test]
fn completeness_is_set_based_and_errors_poison_streams() {
    let r = redacted("13812345678 和 13987654321");
    assert!(r.restore("[CN_MOBILE_1] [CN_MOBILE_1]").is_err());
    assert!(
        r.restore("[CN_MOBILE_1] [CN_MOBILE_2] [CN_MOBILE_1]")
            .is_ok()
    );
    let mut stream = r.stream();
    assert!(stream.push("[CN_MOBILE_1] [cn_mobile_1]").is_err());
    assert!(stream.push("[CN_MOBILE_2]").is_err());
    assert!(stream.finish().is_err());
}

#[test]
fn empty_table_has_no_stream_only_rejections() {
    let r = super::redact::Redaction::empty_for_test();
    for text in ["见 [", "[step_2]", "[IMPORTANT_NOTE]", "[PERSON_1]"] {
        let expected = r.restore(text);
        for at in 0..=text.len() {
            if text.is_char_boundary(at) {
                assert_eq!(stream_chunks(&r, &[&text[..at], &text[at..]]), expected);
            }
        }
    }
}

#[test]
fn honest_multi_entity_round_trips_do_not_join_restored_names_with_digits() {
    let mut inputs = Vec::new();
    for count in [10, 11, 12, 20] {
        for suffix in [
            " 1楼，",
            " 是 2 号楼的；",
            " 第 2 页，共 15 人，合计 10 元；",
        ] {
            let input: String = (1..=count)
                .map(|i| format!("电话 138{i:08}{suffix}"))
                .collect();
            inputs.push(input);
        }
    }
    let address_book: String = (1..=12).map(|i| format!("电话 138{i:08}；")).collect();
    inputs.push(format!("{address_book}第一个号码 13800000001 是 2 号楼的"));
    for input in inputs {
        let r = redacted(&input);
        let expected = Ok(input.clone());
        assert_eq!(r.restore(&r.text), expected);
        for at in 0..=r.text.len() {
            if r.text.is_char_boundary(at) {
                assert_eq!(stream_chunks(&r, &[&r.text[..at], &r.text[at..]]), expected);
            }
        }
        let boundaries: Vec<_> = r
            .text
            .char_indices()
            .map(|(at, _)| at)
            .chain([r.text.len()])
            .collect();
        let chunks: Vec<_> = boundaries.windows(2).map(|w| &r.text[w[0]..w[1]]).collect();
        assert_eq!(stream_chunks(&r, &chunks), expected);
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(1_500))]
    #[test]
    fn real_redactions_round_trip_original_mixed_numeric_text(
        count in 1usize..=20,
        fragments in proptest::collection::vec(proptest::sample::select(vec![
            " 1楼，", " 是 2 号楼的；", " 第 2 页；", " 共 15 人；", " 合计 10 元；",
            " 价格 ￥１２０；", " 请参考 [1] 和 [2]；", " 引用 [ＣＮ＿ＭＯＢＩＬＥ＿１]；",
            " literal cn_mobile_1；", " invoice 2024-0918；", " 🌊[] 中文；"
        ]), 1..30),
        cuts in proptest::collection::vec(proptest::bool::ANY, 0..300),
    ) {
        let input: String = (1..=count)
            .map(|i| format!("电话 138{i:08}{}", fragments[(i - 1) % fragments.len()]))
            .collect();
        let r = redacted(&input);
        let expected = Ok(input);
        proptest::prop_assert_eq!(r.restore(&r.text), expected.clone());
        let mut chunks = Vec::new();
        let mut start = 0;
        for (n, (at, _)) in r.text.char_indices().enumerate() {
            if cuts.get(n).copied().unwrap_or(false) {
                chunks.push(&r.text[start..at]);
                start = at;
            }
        }
        chunks.push(&r.text[start..]);
        proptest::prop_assert_eq!(stream_chunks(&r, &chunks), expected);
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(10_000))]
    #[test]
    fn arbitrary_partitions_preserve_batch_verdict_and_output(
        pieces in proptest::collection::vec(proptest::sample::select(vec![
            "[", "]", "_", "CN", "mobile", "1", " ", "-", "🌊", "step_2", "PERSON", "[CN_MOBILE_1]", "１", "ＣＮ", "＿", "[1]", "[]", "\u{200b}"
        ]), 0..80),
        cuts in proptest::collection::vec(proptest::bool::ANY, 0..200),
    ) {
        for input in ["13812345678", "参考 [1] [] 电话 13812345678", "参考 [ＣＮ＿ＭＯＢＩＬＥ＿１] 电话 13812345678"] {
            let r = redacted(input);
            let text = format!("{} {}", r.text, pieces.concat());
            let expected = r.restore(&text);
            let mut chunks = Vec::new();
            let mut start = 0;
            for (n, (at, _)) in text.char_indices().enumerate() {
                if cuts.get(n).copied().unwrap_or(false) { chunks.push(&text[start..at]); start = at; }
            }
            chunks.push(&text[start..]);
            let actual = stream_chunks(&r, &chunks);
            proptest::prop_assert_eq!(actual.is_ok(), expected.is_ok());
            if expected.is_ok() { proptest::prop_assert_eq!(actual, expected); }
        }
    }
}

/// `push` accepts `&str`; transport splits inside a code point must first be
/// decoded without loss. Exercise every raw byte split through that adapter.
#[test]
fn transport_byte_splits_decode_without_changing_parser_verdicts() {
    let r = redacted("电话 13812345678");
    for text in [
        "🌊 电话 [CN_MOBILE_1]，见 [step_2]",
        "🌊 [CN_MOBILE_1] [[cn_mobile_1]]",
        "🌊 [CN_MOBILE_1] [IMPORTANT_NOTE]",
        "🌊 [CN_MOBILE_1] [person_11111111111111111111111111111111111111111111111111111111111111111]",
    ] {
        for a in 0..=text.len() {
            for b in a..=text.len() {
                let mut pending = Vec::new();
                let mut decoded = Vec::new();
                for chunk in [
                    &text.as_bytes()[..a],
                    &text.as_bytes()[a..b],
                    &text.as_bytes()[b..],
                ] {
                    pending.extend_from_slice(chunk);
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(s) => s.len(),
                        Err(e) => {
                            assert!(e.error_len().is_none());
                            e.valid_up_to()
                        }
                    };
                    decoded.push(std::str::from_utf8(&pending[..valid]).unwrap().to_owned());
                    pending.drain(..valid);
                }
                assert!(pending.is_empty());
                let refs: Vec<_> = decoded.iter().map(String::as_str).collect();
                assert_eq!(
                    stream_chunks(&r, &refs),
                    r.restore(text),
                    "raw bytes {a}/{b}"
                );
            }
        }
    }
}

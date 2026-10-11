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

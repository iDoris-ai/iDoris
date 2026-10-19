//! 常见香港身份证写法：全角括号、括号前一个 ASCII 空格、中文前后缀与 emoji。
//! 走公开的 `inspect`（`PrivacyClass::Any` + 允许出网 + 同意脱敏）。

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{FindingKind, InspectAction, InspectSpan, Sensitivity};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn open_ctx() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn assert_hkid(input: &str, expected: &str, start: u64) {
    let response = inspect(input, &open_ctx());
    assert_eq!(
        response.action,
        InspectAction::Block,
        "{input}: {response:?}"
    );
    assert_eq!(
        response.sensitivity,
        Sensitivity::S3,
        "{input}: {response:?}"
    );
    let hits: Vec<_> = response
        .findings
        .iter()
        .filter(|finding| finding.kind == FindingKind::HkIdentityCard)
        .collect();
    assert_eq!(hits.len(), 1, "{input}: {response:?}");
    let Some(span) = hits[0].span else {
        panic!("HKID finding missing span: {input}: {response:?}");
    };
    let length = expected.encode_utf16().count() as u64;
    assert_eq!(
        span,
        InspectSpan { start, length },
        "{input}: covered span must be exactly {expected:?}"
    );
    let units: Vec<u16> = input.encode_utf16().collect();
    let from = start as usize;
    let to = from + length as usize;
    assert!(to <= units.len(), "{input}");
    let mut covered = Vec::new();
    covered.extend(expected.encode_utf16());
    assert_eq!(&units[from..to], covered.as_slice(), "{input}");
}

fn assert_no_hkid(input: &str) {
    let response = inspect(input, &open_ctx());
    assert!(
        response
            .findings
            .iter()
            .all(|finding| finding.kind != FindingKind::HkIdentityCard),
        "{input}: {response:?}"
    );
}

#[test]
fn fullwidth_and_spaced_hkid_are_s3_block_with_exact_spans() {
    assert_hkid("A123456（3）", "A123456（3）", 0);
    assert_hkid("AB987654（3）", "AB987654（3）", 0);
    // 香港身份证号码： = 8 个 UTF-16；句号在括号之外。
    assert_hkid("香港身份证号码：A123456（3）。", "A123456（3）", 8);
    // "HKID: " = 6；span 含括号前的一个 ASCII 空格。
    assert_hkid("HKID: A123456 (3)", "A123456 (3)", 6);
    assert_hkid("HKID: A123456 (3) end", "A123456 (3)", 6);
    assert_hkid("AB987654 (3)", "AB987654 (3)", 0);
    assert_hkid("Z683365（A）", "Z683365（A）", 0);
    assert_hkid("z683365 (a)", "z683365 (a)", 0);
    // 两个 emoji 各占一个代理对。
    assert_hkid("🚗😀A123456（3）", "A123456（3）", 4);
    assert_hkid("🚗😀AB987654 (3)。", "AB987654 (3)", 4);
    assert_hkid("前缀A123456（3）后还有无关文字", "A123456（3）", 2);
}

#[test]
fn ascii_and_noparen_hkid_controls_stay_exact() {
    assert_hkid("A123456(3)", "A123456(3)", 0);
    assert_hkid("AB987654(3)", "AB987654(3)", 0);
    assert_hkid("C668668(9)", "C668668(9)", 0);
    assert_hkid("Z683365(A)", "Z683365(A)", 0);
    assert_hkid("A1234563", "A1234563", 0);
    assert_hkid("g123456a", "g123456a", 0);
    assert_hkid("HKID A123456(3) end", "A123456(3)", 5);
    assert_hkid("A123456（3）tail", "A123456（3）", 0);
    assert_hkid("A123456 (3)tail", "A123456 (3)", 0);
}

#[test]
fn malformed_hkid_parens_do_not_match_or_swallow_tail() {
    for input in [
        "A123456（3",
        "A123456(3",
        "A123456 （",
        "A123456 (",
        "A123456（",
        "A123456(",
        "（3）",
        "A123456（3)",
        "A123456(3）",
        "A123456  (3)",
        "A123456  （3）",
        "A123456\t(3)",
        "A123456\u{3000}（3）",
        "A123456（4）",
        "A123456 (4)",
        "AB987654（2）",
        "AB987654 (2)",
        "G123456(5)",
        "1A23456(3)",
        "A1234564",
    ] {
        assert_no_hkid(input);
    }
    // 闭括号不在校验位紧后方：不能把中间文字吞进 span，也不能因此命中。
    let mut delayed_fullwidth = String::from("A123456（3");
    delayed_fullwidth.push_str(&"x".repeat(64));
    delayed_fullwidth.push('）');
    assert_no_hkid(&delayed_fullwidth);
    let mut delayed_ascii = String::from("A123456 (3");
    delayed_ascii.push_str(&"y".repeat(64));
    delayed_ascii.push(')');
    assert_no_hkid(&delayed_ascii);
    // 无括号旧写法仍然只覆盖主体，不吞后面的残缺括号。
    assert_hkid("A1234563）不是", "A1234563", 0);
    assert_hkid("A1234563)tail", "A1234563", 0);
}

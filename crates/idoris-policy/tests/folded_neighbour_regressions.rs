//! 折叠粘连回归：折叠（全角 ASCII、横线变体）会把紧挨实体的字符粘成一个更长
//! 的 run，让精确长度、email TLD 结尾和 `.env` 结尾判断失败，从而放行 base 能
//! 抓到的实体。原文视图必须始终在并集里，「原文能抓到的，归一化后绝不能放行」。
//!
//! 同时钉住本 PR 的两个收益不能被这次修复回退：实体内部的零宽插入仍要命中，
//! 命中 span 必须盖住插入字符。

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

fn assert_one(
    input: &str,
    kind: FindingKind,
    action: InspectAction,
    sensitivity: Sensitivity,
    start: u64,
    length: u64,
) {
    let response = inspect(input, &open_ctx());
    assert_eq!(response.action, action, "{input:?}: {response:?}");
    assert_eq!(response.sensitivity, sensitivity, "{input:?}");
    assert_eq!(
        response.findings,
        [idoris_contracts::inspect::InspectFinding {
            kind,
            span: Some(InspectSpan { start, length }),
        }],
        "{input:?}"
    );
}

#[test]
fn fullwidth_digit_next_to_an_entity_does_not_glue_it_away() {
    // 全角数字贴在手机号边上：base 是 Redact，折叠视图单独会放行。
    assert_one(
        "\u{FF11}13812345678",
        FindingKind::CnMobile,
        InspectAction::Redact,
        Sensitivity::S2,
        1,
        11,
    );
    assert_one(
        "13812345678\u{FF19}",
        FindingKind::CnMobile,
        InspectAction::Redact,
        Sensitivity::S2,
        0,
        11,
    );
    assert_one(
        "\u{FF11}11010519491231002X",
        FindingKind::CnResidentId,
        InspectAction::Block,
        Sensitivity::S3,
        1,
        18,
    );
}

#[test]
fn fullwidth_letters_next_to_a_key_do_not_glue_it_away() {
    let key = format!("sk-{}", "a".repeat(40));
    assert_one(
        &format!("\u{FF41}{key}"),
        FindingKind::ApiKey,
        InspectAction::Block,
        Sensitivity::S3,
        1,
        43,
    );
    // 全角 `Key` 前缀本身也被折回半角，命中仍从原文下标 3 开始。
    assert_one(
        &format!("\u{FF2B}\u{FF45}\u{FF59}{key}"),
        FindingKind::ApiKey,
        InspectAction::Block,
        Sensitivity::S3,
        3,
        43,
    );
}

#[test]
fn hyphen_variants_and_fullwidth_tails_do_not_break_email_or_env_detection() {
    assert_one(
        "a@b.com\u{2010}9",
        FindingKind::EmailAddress,
        InspectAction::Redact,
        Sensitivity::S2,
        0,
        7,
    );
    assert_one(
        "a@b.com\u{FF19}",
        FindingKind::EmailAddress,
        InspectAction::Redact,
        Sensitivity::S2,
        0,
        7,
    );
    assert_one(
        ".env\u{2010}x",
        FindingKind::CredentialPath,
        InspectAction::Redact,
        Sensitivity::S2,
        0,
        4,
    );
    assert_one(
        ".env\u{FF58}",
        FindingKind::CredentialPath,
        InspectAction::Redact,
        Sensitivity::S2,
        0,
        4,
    );
}

#[test]
fn folded_views_still_catch_ignorables_inside_an_entity() {
    // 删除视图的收益：实体内部的零宽插入仍被命中，span 盖住插入字符。
    assert_one(
        "138\u{200B}1234\u{200B}5678",
        FindingKind::CnMobile,
        InspectAction::Redact,
        Sensitivity::S2,
        0,
        13,
    );
    // 保留视图的收益：可忽略字符当边界，实体从它之后开始。
    assert_one(
        "1\u{200E}13812345678",
        FindingKind::CnMobile,
        InspectAction::Redact,
        Sensitivity::S2,
        2,
        11,
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::Contract;
use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{
    FindingKind, InspectAction, InspectFinding, InspectResponse, InspectSpan, Sensitivity,
};
use proptest::prelude::*;

use super::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

/// 放行条件全部满足的上下文：声明 `any`、租户允许出网、同意脱敏。
fn open_ctx() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn kinds(response: &InspectResponse) -> Vec<FindingKind> {
    response.findings.iter().map(|f| f.kind).collect()
}

/// 把 span 指向的 UTF-16 片段还原成字符串——只在测试里做，用来证明 span 单位。
fn covered(input: &str, finding: &InspectFinding) -> String {
    let span = finding.span.unwrap();
    let units: Vec<u16> = input.encode_utf16().collect();
    let start = span.start as usize;
    String::from_utf16(&units[start..start + span.length as usize]).unwrap()
}

#[test]
fn gb11643_example_id_is_s3_with_exact_span() {
    for id in ["11010519491231002X", "11010519491231002x"] {
        let input = format!("身份证：{id}。");
        let response = inspect(&input, &open_ctx());
        assert_eq!(kinds(&response), [FindingKind::CnResidentId]);
        assert_eq!(response.sensitivity, Sensitivity::S3);
        assert_eq!(response.action, InspectAction::Block);
        assert_eq!(covered(&input, &response.findings[0]), id);
    }
}

#[test]
fn id_with_wrong_check_digit_is_downgraded_not_dropped() {
    let response = inspect("110105194912310021", &open_ctx());
    assert_eq!(kinds(&response), [FindingKind::CnResidentId]);
    assert_eq!(response.sensitivity, Sensitivity::S2);
    assert_eq!(response.action, InspectAction::Redact);
}

#[test]
fn legacy_fifteen_digit_id_is_found_without_checksum() {
    let response = inspect("旧证 110105491231002", &open_ctx());
    assert_eq!(kinds(&response), [FindingKind::CnResidentId]);
    assert_eq!(response.sensitivity, Sensitivity::S2);
}

#[test]
fn id_shaped_numbers_with_bad_structure_are_not_ids() {
    for input in [
        "990105199001010011", // 省级区划码 99 不存在，日期合法
        "110105194913310021", // 13 月
        "110105190002290021", // 1900 不是闰年
    ] {
        let response = inspect(input, &open_ctx());
        assert!(
            !kinds(&response).contains(&FindingKind::CnResidentId),
            "{input}: {response:?}"
        );
    }
}

#[test]
fn mobile_numbers_with_common_separators_and_country_code() {
    for input in [
        "13812345678",
        "138-1234-5678",
        "138 1234 5678",
        "+86 13812345678",
        "8613812345678",
    ] {
        let response = inspect(input, &open_ctx());
        assert!(
            kinds(&response).contains(&FindingKind::CnMobile),
            "{input}: {response:?}"
        );
        assert_eq!(response.sensitivity, Sensitivity::S2, "{input}");
    }
    for input in ["12812345678", "213812345678", "1381234567"] {
        let response = inspect(input, &open_ctx());
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

#[test]
fn bank_card_requires_luhn() {
    for input in [
        "6222021234567894",
        "6222 0212 3456 7894",
        "4111-1111-1111-1111",
        "6214830000000000007",
    ] {
        let response = inspect(input, &open_ctx());
        assert!(
            kinds(&response).contains(&FindingKind::BankCard),
            "{input}: {response:?}"
        );
        assert_eq!(response.sensitivity, Sensitivity::S3);
        assert_eq!(covered(input, &response.findings[0]), input);
    }
    let response = inspect("6222021234567891", &open_ctx());
    assert!(response.findings.is_empty(), "{response:?}");
}

#[test]
fn adjacent_entities_merged_into_one_group_are_each_found() {
    let input = "电话 13812345678 6222021234567894 结束";
    let response = inspect(input, &open_ctx());
    assert_eq!(
        kinds(&response),
        [FindingKind::CnMobile, FindingKind::BankCard]
    );
    assert_eq!(covered(input, &response.findings[0]), "13812345678");
    assert_eq!(covered(input, &response.findings[1]), "6222021234567894");
}

#[test]
fn spans_count_utf16_code_units() {
    let input = "😀手机13812345678";
    let response = inspect(input, &open_ctx());
    assert_eq!(
        response.findings,
        [InspectFinding {
            kind: FindingKind::CnMobile,
            span: Some(InspectSpan {
                start: 4,
                length: 11
            }),
        }]
    );
}

#[test]
fn ports_addresses_and_timestamps_are_not_findings() {
    let input = "listen 127.0.0.1:8080, port 65535, ts 1728640000000, order 20261011";
    let response = inspect(input, &open_ctx());
    assert!(response.findings.is_empty(), "{response:?}");
    assert_eq!(response.sensitivity, Sensitivity::S0);
    assert_eq!(response.action, InspectAction::Allow);
}

#[test]
fn oversize_input_is_blocked_without_scanning() {
    let ctx = InspectCtx {
        max_input_utf16: 16,
        ..open_ctx()
    };
    let response = inspect("a clean sentence!", &ctx);
    assert_eq!(
        response,
        InspectResponse {
            sensitivity: Sensitivity::S3,
            findings: vec![InspectFinding {
                kind: FindingKind::InputLimitExceeded,
                span: None
            }],
            action: InspectAction::Block,
        }
    );
    assert_eq!(
        inspect("a clean sentence", &ctx).action,
        InspectAction::Allow
    );
    // 上限按 UTF-16 计：16 个汉字是 48 字节，但只有 16 个 code unit。
    assert_eq!(inspect(&"字".repeat(16), &ctx).action, InspectAction::Allow);
}

#[test]
fn action_matrix_defaults_to_block() {
    let clean = "hello";
    let personal = "13812345678";
    let special = "11010519491231002X";
    let cases = [
        (open_ctx(), clean, InspectAction::Allow),
        (open_ctx(), personal, InspectAction::Redact),
        (open_ctx(), special, InspectAction::Block),
        (
            InspectCtx {
                redact_consent: false,
                ..open_ctx()
            },
            personal,
            InspectAction::Block,
        ),
        (
            InspectCtx {
                declared: PrivacyClass::LocalOnly,
                ..open_ctx()
            },
            clean,
            InspectAction::Block,
        ),
        (
            InspectCtx {
                remote_allowed: false,
                ..open_ctx()
            },
            clean,
            InspectAction::Block,
        ),
        (InspectCtx::fail_closed(), clean, InspectAction::Block),
    ];
    for (ctx, input, expected) in cases {
        assert_eq!(inspect(input, &ctx).action, expected, "{input} {ctx:?}");
    }
}

#[test]
fn response_never_carries_the_matched_text() {
    let input = "卡号 6222021234567894 手机 13812345678 证件 11010519491231002X";
    let response = inspect(input, &open_ctx());
    assert_eq!(response.findings.len(), 3);
    let wire = format!("{response:?}");
    for secret in [
        "6222021234567894",
        "13812345678",
        "11010519491231002X",
        "卡号",
    ] {
        assert!(!wire.contains(secret), "{wire}");
    }
}

fn valid_id() -> impl Strategy<Value = String> {
    (0usize..34, 1950u32..2030, 1u32..13, 1u32..29, 0u32..1000).prop_map(
        |(province, year, month, day, seq)| {
            let first17 = format!(
                "{}0105{year}{month:02}{day:02}{seq:03}",
                super::l0::PROVINCES[province]
            );
            let sum: u32 = first17
                .bytes()
                .zip([7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2])
                .map(|(b, w)| u32::from(b - b'0') * w)
                .sum();
            let check = b"10X98765432"[(sum % 11) as usize] as char;
            format!("{first17}{check}")
        },
    )
}

proptest! {
    #[test]
    fn any_input_yields_a_valid_in_bounds_response(input in any::<String>()) {
        let response = inspect(&input, &open_ctx());
        prop_assert!(response.validate().is_ok());
        let len = input.encode_utf16().count() as u64;
        let mut last = 0;
        for finding in &response.findings {
            let span = finding.span.unwrap();
            prop_assert!(span.start + span.length <= len);
            prop_assert!(span.start >= last);
            last = span.start;
        }
    }

    #[test]
    fn every_valid_id_in_non_digit_text_is_found_as_s3(
        id in valid_id(),
        before in "[^0-9 \\-]{0,8}",
        after in "[^0-9xX \\-]{0,8}",
    ) {
        let input = format!("{before}{id}{after}");
        let response = inspect(&input, &open_ctx());
        let found = response
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::CnResidentId && covered(&input, f) == id);
        prop_assert!(found, "{} {:?}", input, response);
        prop_assert_eq!(response.sensitivity, Sensitivity::S3);
        prop_assert_eq!(response.action, InspectAction::Block);
    }
}

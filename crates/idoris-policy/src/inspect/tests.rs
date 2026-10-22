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

/// Literal backslash-u prefixes must not hide a resident id.
///
/// Issue #534 records that on PR #490 these two inputs are S0 Allow: a
/// literal U+005C, `u`, and `110105195001010004`, plus the same input with
/// the backslash doubled. That tree's `normalize()` decodes `\uXXXX` in
/// place (`50caf46`, not an ancestor of this commit) and consumes the id's
/// own leading hex digits. This tree does not decode those escapes, so both
/// inputs are S3 Block and the span covers the id. The regression passes;
/// it is not ignored.
///
/// Shorter prefixes (`\` + `u` + one to three ASCII digits glued to the same
/// id) are longer digit runs and stay undetected. #534 records them. They
/// are not asserted here, and the generator is unchanged.
#[test]
fn literal_backslash_u_prefix_keeps_the_resident_id() {
    let id = "110105195001010004";
    for input in [format!("身份证：\\u{id}"), format!("身份证：\\\\u{id}")] {
        let response = inspect(&input, &open_ctx());
        let found = response.findings.iter().any(|finding| {
            finding.kind == FindingKind::CnResidentId && covered(&input, finding) == id
        });
        assert!(
            found
                && response.sensitivity == Sensitivity::S3
                && response.action == InspectAction::Block,
            "{input:?} sensitivity={:?} action={:?} findings={:?}",
            response.sensitivity,
            response.action,
            response.findings
        );
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

/// 每个输入都必须恰好命中一次 `kind`，且 span 覆盖 `expected` 片段。
fn assert_single(input: &str, kind: FindingKind, expected: &str, sensitivity: Sensitivity) {
    let response = inspect(input, &open_ctx());
    let matching: Vec<_> = response
        .findings
        .iter()
        .filter(|f| f.kind == kind)
        .collect();
    assert_eq!(matching.len(), 1, "{input}: {response:?}");
    assert_eq!(covered(input, matching[0]), expected, "{input}");
    assert_eq!(response.sensitivity, sensitivity, "{input}");
}

fn assert_none(input: &str, kind: FindingKind) {
    let response = inspect(input, &open_ctx());
    assert!(!kinds(&response).contains(&kind), "{input}: {response:?}");
}

#[test]
fn unified_social_credit_code_requires_mod31_check() {
    let ok = "91350100M000100Y43";
    assert_single(
        &format!("信用代码：{ok}"),
        FindingKind::CnUnifiedSocialCreditCode,
        ok,
        Sensitivity::S1,
    );
    assert_single(
        "9144030071526726XG",
        FindingKind::CnUnifiedSocialCreditCode,
        "9144030071526726XG",
        Sensitivity::S1,
    );
    assert_eq!(inspect(ok, &open_ctx()).action, InspectAction::Redact);
    assert_none("91350100M000100Y44", FindingKind::CnUnifiedSocialCreditCode);
    // 校验位对，但第 3–8 位行政区划不是数字。
    assert_none("91AB0100M000100Y4T", FindingKind::CnUnifiedSocialCreditCode);
}

#[test]
fn passports_and_travel_permits_by_structure() {
    for id in ["E12345678", "EA1234567", "G12345678", "e12345678"] {
        assert_single(
            &format!("护照 {id}。"),
            FindingKind::CnPassport,
            id,
            Sensitivity::S3,
        );
    }
    for id in ["H12345678", "M87654321"] {
        assert_single(
            &format!("回乡证{id}"),
            FindingKind::HkMacauTravelPermit,
            id,
            Sensitivity::S3,
        );
    }
    for input in [
        "EI1234567",
        "E1234567",
        "E123456789",
        "X12345678",
        "H1234567A",
    ] {
        assert_none(input, FindingKind::CnPassport);
        assert_none(input, FindingKind::HkMacauTravelPermit);
    }
}

#[test]
fn hong_kong_identity_card_requires_check_digit() {
    for id in [
        "A123456(3)",
        "C668668(9)",
        "Z683365(A)",
        "AB987654(3)",
        "A1234563",
        "g123456a",
    ] {
        assert_single(
            &format!("HKID {id} end"),
            FindingKind::HkIdentityCard,
            id,
            Sensitivity::S3,
        );
    }
    for input in [
        "A123456(4)",
        "A1234564",
        "AB987654(2)",
        "1A23456(3)",
        "G123456(5)",
    ] {
        assert_none(input, FindingKind::HkIdentityCard);
    }
}

#[test]
fn license_plates_regular_and_new_energy() {
    for plate in [
        "粤B12345",
        "京A·12345",
        "沪AD12345",
        "粤B 1234D",
        "川A12345",
    ] {
        assert_single(
            &format!("车牌{plate}停在门口"),
            FindingKind::CnLicensePlate,
            plate,
            Sensitivity::S2,
        );
    }
    for input in [
        "京ABCDEF",
        "京A1234",
        "京A1234567",
        "北京12345",
        "京a12345",
        "京I12345",
        "粤B12345x",
        "京112345",
    ] {
        assert_none(input, FindingKind::CnLicensePlate);
    }
}

#[test]
fn email_addresses_with_real_domains() {
    let email = "alice.zhang+tag@example.com.cn";
    for input in [format!("<{email}>."), format!("写信给 {email}.")] {
        assert_single(&input, FindingKind::EmailAddress, email, Sensitivity::S2);
    }
    assert_single(
        "写信到 a_b-c@mail-01.example.org。",
        FindingKind::EmailAddress,
        "a_b-c@mail-01.example.org",
        Sensitivity::S2,
    );
    for input in [
        "a@b",
        "user@localhost",
        "@example.com",
        "x@example.c",
        "x@example.c0m",
    ] {
        assert_none(input, FindingKind::EmailAddress);
    }
}

#[test]
fn new_rule_spans_count_utf16_code_units() {
    let input = "🚗😀粤B12345";
    let response = inspect(input, &open_ctx());
    assert_eq!(
        response.findings,
        [InspectFinding {
            kind: FindingKind::CnLicensePlate,
            span: Some(InspectSpan {
                start: 4,
                length: 7
            }),
        }]
    );
}

// 前缀拆开写，源码里不出现完整的 key 形态（避免误触发仓库的密钥扫描）；
// 随机部分是固定的假值，不对应任何真实凭据。
const FAKE_OPENAI: &str = concat!(
    "sk",
    "-proj-FwFvrdcwEcSaT3WQwn8QMH2iE6bao4pjmx5ZTDNsbiqASwXU"
);
const FAKE_GITHUB: &str = concat!("gh", "p_X74NuHG7OVWUJrSlMDg6V1MANv6NXmI3P3ay");
const FAKE_AWS_ID: &str = concat!("AK", "IA5HVJV2XAZM2TT6A8");
const FAKE_RAW_SECRET: &str = "zMCTnu5i8RRUKO7CCgpDdFZwTMKHPBZU1HmjS9jA";

#[test]
fn credential_assignments_score_only_the_value() {
    for prefix in [
        "DJANGO_SECRET_KEY_FOR_PRODUCTION_SETTINGS=",
        "STRIPE_SECRET=",
        "export DATABASE_PASSWORD=",
        "--password=",
        "API_KEY=",
        "TOKEN=",
        "K=",
        "password: ",
        "",
    ] {
        let input = format!("😀 {prefix}{FAKE_RAW_SECRET}");
        assert_single(
            &input,
            FindingKind::HighEntropySecret,
            FAKE_RAW_SECRET,
            Sensitivity::S3,
        );
        let response = inspect(&input, &open_ctx());
        assert_eq!(response.action, InspectAction::Block, "{prefix}");
        assert!(!kinds(&response).contains(&FindingKind::ApiKey));
    }
    let input = "DJANGO_SECRET_KEY_FOR_PRODUCTION_SETTINGS=development";
    let response = inspect(input, &open_ctx());
    assert!(response.findings.is_empty(), "{response:?}");
    assert_eq!(response.action, InspectAction::Allow);
}

#[test]
fn credential_assignment_keeps_base64_padding() {
    // Thirty body characters plus padding: dropping padding would fall below
    // the existing 32-unit threshold and regress a previously detected value.
    let value = "aA1bB2cC3dD4eE5fF6gG7hH8iI9jJ0==";
    for prefix in ["", "DJANGO_SECRET_KEY_FOR_PRODUCTION_SETTINGS="] {
        let input = format!("😀 {prefix}{value}");
        assert_single(
            &input,
            FindingKind::HighEntropySecret,
            value,
            Sensitivity::S3,
        );
        assert_eq!(inspect(&input, &open_ctx()).action, InspectAction::Block);
    }
}

#[test]
fn credential_windows_paths_preserve_original_spans() {
    for path in [
        r"C:\Users\alice\.aws\credentials",
        r"C:\Users\a\.ssh\config",
        r"C:\Users\a\.ssh\known_hosts",
        r"C:\Users\a\.kube\config",
        r"C:\Users\a\.docker\config.json",
        r"%USERPROFILE%\.aws\credentials",
        r"C:\Users\a\config\.env.local",
    ] {
        for written in [path.to_owned(), path.replace('\\', "/")] {
            let input = format!("😀 cat {written} please");
            // ':' and '%' delimit the existing ASCII path runs.
            let expected = written.rsplit([':', '%']).next().unwrap();
            assert_single(
                &input,
                FindingKind::CredentialPath,
                expected,
                Sensitivity::S2,
            );
            assert_eq!(inspect(&input, &open_ctx()).action, InspectAction::Redact);
        }
    }
    for path in [
        r"C:\Users\alice\notes.txt",
        r"C:\code\process.env.API_KEY",
        r"C:\config\.envrc",
    ] {
        let response = inspect(path, &open_ctx());
        assert!(response.findings.is_empty(), "{response:?}");
        assert_eq!(response.action, InspectAction::Allow);
    }
}

#[test]
fn credential_internal_equals_preserves_value_spans() {
    for (prefix, value) in [
        ("", "zMCTnu5i8RRUKO7CCgpDd=FZwTMKHPBZU1HmjS9jA"),
        ("", "aA1bB2cC3dD4eE5fF6gG7=hH8iI9jJ0kK1lL2mM3nN4"),
        ("PASSWORD=", "aA1bB2cC3dD4eE5fF6gG7=hH8iI9jJ0kK1lL2mM3nN4"),
        ("", "aA1bB2cC3dD4eE5fF6gG7=Q62rR7sS8tT9uU0vV1wW2="),
    ] {
        let input = format!("😀 {prefix}{value}");
        assert_single(
            &input,
            FindingKind::HighEntropySecret,
            value,
            Sensitivity::S3,
        );
        let response = inspect(&input, &open_ctx());
        assert_eq!(response.action, InspectAction::Block, "{input}");
        assert_eq!(
            response.findings[0].span.unwrap().start,
            (3 + prefix.len()) as u64
        );
    }
}

#[test]
fn credential_internal_equals_with_low_switch_piece() {
    for value in [
        "abcdefgh=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4",
        "Q1w2E3r4T5y6U7i8O9p0A1s2D3f4=abcdefgh",
        "abcd=Q1w2E3r4T5y6U7i8O9p0A1s2=efgh",
        "abcdefgh=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4==",
        "abc=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4", // Exactly 32 units.
        "Q1w2E3r4T5y6U7i8O9p0A1s2D3f4=abc",
    ] {
        for prefix in ["", "PASSWORD="] {
            let input = format!("🚗😀 {prefix}{value};");
            assert_single(
                &input,
                FindingKind::HighEntropySecret,
                value,
                Sensitivity::S3,
            );
            let response = inspect(&input, &open_ctx());
            assert_eq!(response.action, InspectAction::Block);
            assert_eq!(response.sensitivity, Sensitivity::S3);
            assert_eq!(
                response.findings[0].span.unwrap().start,
                (5 + prefix.len()) as u64
            );
        }
    }
    // Inclusive segmentation still obeys length and entropy thresholds.
    for input in [
        "ab=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4",   // 31 units.
        "a1B2a1B2a1B2a1B2=a1B2a1B2a1B2a1B2", // Low entropy.
    ] {
        let response = inspect(input, &open_ctx());
        assert_eq!(
            response.action,
            InspectAction::Allow,
            "{input}: {response:?}"
        );
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

#[test]
fn credential_internal_equals_at_every_split_keeps_inclusive_coverage() {
    // Every insertion retains the original scanner's high-entropy token.
    // Near the ends a long piece is itself detected; prefer that exact span.
    for split in 1..FAKE_RAW_SECRET.len() {
        let (left, right) = FAKE_RAW_SECRET.split_at(split);
        let value = format!("{left}={right}");
        let input = format!("🚗😀 {value};");
        let response = inspect(&input, &open_ctx());
        assert_eq!(
            response.action,
            InspectAction::Block,
            "{split}: {response:?}"
        );
        assert_eq!(response.sensitivity, Sensitivity::S3);
        assert_eq!(kinds(&response), [FindingKind::HighEntropySecret]);
        let finding = &response.findings[0];
        let span = finding.span.unwrap();
        assert!(span.start >= 5);
        assert!(span.start + span.length <= 5 + value.len() as u64);
        assert!([left, right, value.as_str()].contains(&covered(&input, finding).as_str()));
    }
}

#[test]
fn credential_union_does_not_widen_or_duplicate_existing_values() {
    let input = format!("😀 {FAKE_RAW_SECRET}={FAKE_RAW_SECRET}");
    let response = inspect(&input, &open_ctx());
    assert_eq!(response.action, InspectAction::Block);
    assert_eq!(response.sensitivity, Sensitivity::S3);
    assert_eq!(response.findings.len(), 2, "{response:?}");
    for (finding, start) in response.findings.iter().zip([3, 44]) {
        assert_eq!(finding.kind, FindingKind::HighEntropySecret);
        assert_eq!(finding.span.unwrap().start, start);
        assert_eq!(covered(&input, finding), FAKE_RAW_SECRET);
    }
}

#[test]
fn credential_serialized_env_escapes_keep_original_spans() {
    for (argument, expected) in [
        (r#"{"command":"cat .env\nnext_command"}"#, ".env"),
        (r#"{"command":"cat .env\n"}"#, ".env"),
        (r#"{"command":"cat ~/.env\tx"}"#, "~/.env"),
        (r#"{"command":"cat .env\r\nnext"}"#, ".env"),
        (r#"\"/app/.env\""#, "/app/.env"),
        (r"C:\app\.env\\", r"\app\.env"),
        (r#"{"command":"cat .env.local\"}"#, ".env.local"),
        (r#"{"command":"cat ~/.ssh/id_rsa\"}"#, "~/.ssh/id_rsa"),
        (
            r#"{"arguments":"{\"command\":\"cat /app/.env\"}"}"#,
            "/app/.env",
        ),
        (
            r#"{"arguments":"{\"command\":\"cat C:\\app\\.env\"}"}"#,
            r"\\app\\.env",
        ),
    ] {
        let input = format!("😀 {argument}");
        assert_single(
            &input,
            FindingKind::CredentialPath,
            expected,
            Sensitivity::S2,
        );
        let response = inspect(&input, &open_ctx());
        assert_eq!(response.action, InspectAction::Redact, "{input}");
        let start = input.find(expected).unwrap();
        assert_eq!(
            response.findings[0].span.unwrap().start,
            input[..start].encode_utf16().count() as u64
        );
    }
}

#[test]
fn credential_serialized_windows_separators() {
    for path in [
        r"C:\\Users\\a\\.aws\\credentials",
        r"C:\\Users\\a\\.kube\\config",
        r"C:\\Users\\a\\.docker\\config.json",
        r"C:\\Users\\a\\.ssh\\config",
    ] {
        let input = format!("😀 {{\"path\":\"{path}\"}}");
        assert_single(
            &input,
            FindingKind::CredentialPath,
            &path[2..],
            Sensitivity::S2,
        );
        assert_eq!(inspect(&input, &open_ctx()).action, InspectAction::Redact);
    }
}

#[test]
fn credential_paths_after_serialized_env_remain_covered() {
    for (text, paths) in [
        (
            r#""./.env\n./.aws/credentials\n./id_rsa""#,
            vec!["./.env", "./.aws/credentials", "./id_rsa"],
        ),
        (
            r#""./.aws/credentials\n./.env\n./id_rsa""#,
            vec!["./.aws/credentials", "./.env", "./id_rsa"],
        ),
        (
            r".env\n~/.aws/credentials",
            vec![".env", "~/.aws/credentials"],
        ),
    ] {
        assert_credential_paths_masked(text, &paths);
    }
}

#[test]
fn credential_paths_after_absolute_env_remain_covered() {
    for (text, paths) in [
        (
            r#""/app/.env\n/root/.ssh/id_rsa""#,
            vec!["/app/.env", "/root/.ssh/id_rsa"],
        ),
        (
            r"/app/.env\t/home/alice/.ssh/id_rsa",
            vec!["/app/.env", "/home/alice/.ssh/id_rsa"],
        ),
        (
            r"C:\app\.env\..\..\Users\a\.ssh\id_rsa",
            vec![r"\app\.env", r".ssh\id_rsa"],
        ),
    ] {
        assert_credential_paths_masked(text, &paths);
    }
}

fn assert_credential_paths_masked(text: &str, paths: &[&str]) {
    let input = format!("🚗😀 {text}");
    let response = inspect(&input, &open_ctx());
    assert_eq!(response.action, InspectAction::Redact);
    let mut masked: Vec<u16> = input.encode_utf16().collect();
    let mut covered_units = vec![false; masked.len()];
    for finding in &response.findings {
        assert_eq!(finding.kind, FindingKind::CredentialPath);
        let span = finding.span.unwrap();
        let start = span.start as usize;
        let end = start + span.length as usize;
        covered_units[start..end].fill(true);
        masked[start..end].fill(u16::from(b'*'));
    }
    let redacted = String::from_utf16(&masked).unwrap();
    for path in paths {
        let byte_start = input.find(path).unwrap();
        let start = input[..byte_start].encode_utf16().count();
        let end = start + path.encode_utf16().count();
        assert!(
            covered_units[start..end].iter().all(|covered| *covered),
            "path {path:?} left exposed: {redacted:?}; {response:?}"
        );
        assert!(!redacted.contains(path), "{redacted:?}");
    }
    assert!(redacted.starts_with("🚗😀 "));
}

#[test]
fn credential_ambiguous_assignments_fail_closed_on_aggregate_entropy() {
    // These can be assignments with individually sub-threshold values or opaque
    // credentials containing '='. The inclusive detector cannot distinguish the
    // intent: retain its conservative Block, but never invent a padded-name span.
    for token in [
        "aA1bB2cC3dD4eE5fF6gG7hH8iI9jJ0k=developmentconfiguration",
        "K=8vLr5VrExzWYr2qeBJNvhYsN63lysYi",
        "K=aVTZ23bG7tHZlX0SqxlZ3dzETRxOmw",
        "DJANGO_SECRET_KEY_FOR_PRODUCTION_SETTINGS=a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2",
    ] {
        let input = format!("😀 {token};");
        assert_single(
            &input,
            FindingKind::HighEntropySecret,
            token,
            Sensitivity::S3,
        );
        let response = inspect(&input, &open_ctx());
        assert_eq!(response.action, InspectAction::Block);
        assert_eq!(response.findings[0].span.unwrap().start, 3);
    }
    // A delimiter before a benign value is not trailing base64 padding. Here
    // the aggregate also falls below the switch threshold, so it remains Allow.
    let response = inspect(
        "aA1bB2cC3dD4eE5fF6gG7hH8iI9jJ0k=developmentconfigurationdevelopmentconfiguration",
        &open_ctx(),
    );
    assert_eq!(response.action, InspectAction::Allow);
    assert!(response.findings.is_empty(), "{response:?}");
}

#[test]
fn credential_benign_windows_paths_and_escapes_are_allowed() {
    for input in [
        r"C:\Windows\System32\drivers\etc\hosts",
        r"\\?\C:\temp\file.txt",
        r"split on \ and / please",
        r"C:\code\process.env.API_KEY",
        r"escape \n.envelope",
    ] {
        let response = inspect(input, &open_ctx());
        assert_eq!(
            response.action,
            InspectAction::Allow,
            "{input}: {response:?}"
        );
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

#[test]
fn prefixed_keys_are_found_by_rule_set_a() {
    for key in [FAKE_OPENAI, FAKE_GITHUB, FAKE_AWS_ID] {
        assert_single(
            &format!("token={key};"),
            FindingKind::ApiKey,
            key,
            Sensitivity::S3,
        );
        assert_eq!(inspect(key, &open_ctx()).action, InspectAction::Block);
    }
    // 短的、看起来像前缀的普通词不算。
    for input in ["sk-learn", "pip install sk-video", "AKIA123", "ghp_short"] {
        assert_none(input, FindingKind::ApiKey);
    }
}

#[test]
fn rule_sets_a_and_b_are_independent() {
    // 真实格式的长 key：两组各自命中同一个位置。
    let response = inspect(FAKE_OPENAI, &open_ctx());
    assert_eq!(
        kinds(&response),
        [FindingKind::ApiKey, FindingKind::HighEntropySecret]
    );
    // 没有前缀的随机串只有 B 组兜得住；太短的 AWS id 只有 A 组认得。
    assert_single(
        &format!("secret: {FAKE_RAW_SECRET}"),
        FindingKind::HighEntropySecret,
        FAKE_RAW_SECRET,
        Sensitivity::S3,
    );
    assert_none(FAKE_RAW_SECRET, FindingKind::ApiKey);
    assert_none(FAKE_AWS_ID, FindingKind::HighEntropySecret);
}

#[test]
fn identifiers_paths_and_urls_are_not_high_entropy() {
    for input in [
        "AbstractSingletonProxyFactoryBean2024Impl",
        "getUserAccountBalanceForPeriodV2Handler",
        "XMLHttpRequestEventTargetPrototypeHandler",
        "com_example_service_UserAccountServiceImpl_v2",
        "my-very-long-kubernetes-deployment-name-v2",
        "/api/v1/organizations/12345/projects/67890/members",
        "x86_64-apple-darwin/release/deps/idoris_policy-9f8e7d6c5b4a3210",
        "550e8400-e29b-41d4-a716-446655440000",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        // 类别切换很密，但只有 4 个不同字符：熵门挡住。
        "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2",
        // 32 个字符以下 B 组不出手，已知的短格式交给 A 组。
        "req_8Zk3Lm9Xa2Wp7Rt5Yb1Nc4",
    ] {
        assert_none(input, FindingKind::HighEntropySecret);
    }
}

#[test]
fn private_key_blocks_including_truncated_ones() {
    let block = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjE\n-----END OPENSSH PRIVATE KEY-----";
    assert_single(
        &format!("key:\n{block}\nthanks"),
        FindingKind::PrivateKey,
        block,
        Sensitivity::S3,
    );
    let truncated = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA";
    assert_single(
        &format!("看这个 {truncated}"),
        FindingKind::PrivateKey,
        truncated,
        Sensitivity::S3,
    );
    assert_none(
        "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----",
        FindingKind::PrivateKey,
    );
}

#[test]
fn credential_file_paths() {
    for path in [
        "~/.ssh/id_ed25519",
        ".env",
        "./config/.env.local",
        "certs/server.pem",
        "~/.ssh/authorized_keys",
        "~/.aws/credentials",
        "login.keychain-db",
    ] {
        assert_single(
            &format!("cat {path} please"),
            FindingKind::CredentialPath,
            path,
            Sensitivity::S2,
        );
    }
    for input in [
        "process.env.API_KEY",
        "a .envrc file",
        "the environment",
        "item.key",
    ] {
        assert_none(input, FindingKind::CredentialPath);
    }
}

#[test]
fn full_width_and_dash_variants_are_folded_before_scanning() {
    let cases = [
        ("１３８１２３４５６７８", FindingKind::CnMobile),
        ("138－1234－5678", FindingKind::CnMobile),
        ("138–1234—5678", FindingKind::CnMobile),
        (
            "6222\u{3000}0212\u{3000}3456\u{3000}7894",
            FindingKind::BankCard,
        ),
        (
            "１１０１０５１９４９１２３１００２Ｘ",
            FindingKind::CnResidentId,
        ),
        (
            "ａｌｉｃｅ＠ｅｘａｍｐｌｅ．ｃｏｍ",
            FindingKind::EmailAddress,
        ),
    ];
    for (evasive, kind) in cases {
        let input = format!("看这里：{evasive}。");
        let response = inspect(&input, &open_ctx());
        let hit = response.findings.iter().find(|f| f.kind == kind);
        // The span points at the caller's original text, not the folded copy.
        assert_eq!(
            hit.map(|f| covered(&input, f)),
            Some(evasive.to_string()),
            "{input}: {response:?}"
        );
    }
}

#[test]
fn zero_width_characters_inside_an_entity_do_not_hide_it() {
    let input = "身份证 1101051949\u{200B}1231\u{2060}002X 和 sk\u{200D}-proj-FwFvrdcwEcSaT3WQwn8QMH2iE6bao4pjmx5ZTDNsbiqASwXU";
    let response = inspect(input, &open_ctx());
    let id = response
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::CnResidentId)
        .unwrap();
    assert_eq!(covered(input, id), "1101051949\u{200B}1231\u{2060}002X");
    assert!(
        kinds(&response).contains(&FindingKind::ApiKey),
        "{response:?}"
    );
    assert_eq!(response.sensitivity, Sensitivity::S3);
}

#[test]
fn one_layer_of_encoding_does_not_hide_an_entity() {
    let cases = [
        // base64 of "身份证 11010519491231002X"
        (
            "6Lqr5Lu96K+BIDExMDEwNTE5NDkxMjMxMDAyWA==",
            FindingKind::CnResidentId,
            Sensitivity::S3,
        ),
        // base64 and unpadded base64url of "13812345678"
        ("MTM4MTIzNDU2Nzg=", FindingKind::CnMobile, Sensitivity::S2),
        ("MTM4MTIzNDU2Nzg", FindingKind::CnMobile, Sensitivity::S2),
        // base64url (with `-`) of "电话13812345678"
        (
            "55S16K-dMTM4MTIzNDU2Nzg",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        // percent-encoded and hex-encoded "13812345678"
        (
            "%31%33%38%31%32%33%34%35%36%37%38",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        (
            "3133383132333435363738",
            FindingKind::CnMobile,
            Sensitivity::S2,
        ),
        // hex of "card 6222021234567894"
        (
            "636172642036323232303231323334353637383934",
            FindingKind::BankCard,
            Sensitivity::S3,
        ),
    ];
    for (encoded, kind, sensitivity) in cases {
        let input = format!("payload={encoded}&x=1");
        let response = inspect(&input, &open_ctx());
        let hit = response
            .findings
            .iter()
            .find(|f| f.kind == kind)
            .unwrap_or_else(|| panic!("{encoded}: {response:?}"));
        assert_eq!(covered(&input, hit), encoded);
        assert_eq!(response.sensitivity, sensitivity, "{encoded}");
    }
}

#[test]
fn harmless_encoded_text_and_plain_words_stay_clean() {
    for input in [
        // base64 of "hello world"
        "aGVsbG8gd29ybGQ=",
        "configuration-management_v2",
        "deadbeefcafebabe0011223344556677",
        "q=%E4%BD%A0%E5%A5%BD",
        // single escape that does not hide a secret stays clean
        "100%25 of users",
    ] {
        let response = inspect(input, &open_ctx());
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

/// E1: hex of (bad-checksum ID + valid ID) must keep S3/Block, not collapse to S2/Redact.
#[test]
fn encoded_mixed_id_checksums_keep_max_sensitivity() {
    let plain = "11010519491231003X 11010519491231002X";
    let plain_response = inspect(plain, &open_ctx());
    assert_eq!(plain_response.sensitivity, Sensitivity::S3);
    assert_eq!(plain_response.action, InspectAction::Block);

    let hex = "31313031303531393439313233313030335820313130313035313934393132333130303258";
    let response = inspect(hex, &open_ctx());
    assert!(
        kinds(&response).contains(&FindingKind::CnResidentId),
        "{response:?}"
    );
    assert_eq!(response.sensitivity, Sensitivity::S3, "{response:?}");
    assert_eq!(response.action, InspectAction::Block, "{response:?}");
}

/// E2: a single percent-escape must not hide an `sk-` API key as Allow.
#[test]
fn single_percent_escape_does_not_hide_api_key() {
    let encoded = "%73k-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let response = inspect(encoded, &open_ctx());
    assert!(
        kinds(&response).contains(&FindingKind::ApiKey),
        "{response:?}"
    );
    assert_eq!(response.sensitivity, Sensitivity::S3, "{response:?}");
    assert_eq!(response.action, InspectAction::Block, "{response:?}");
}

/// E3: a malformed escape after a valid percent-encoded phone must not wipe the hit.
#[test]
fn malformed_percent_escape_does_not_discard_valid_prefix() {
    let phone = "%31%33%38%31%32%33%34%35%36%37%38";
    let good = inspect(phone, &open_ctx());
    assert_eq!(good.action, InspectAction::Redact, "{good:?}");
    assert_eq!(good.sensitivity, Sensitivity::S2, "{good:?}");

    let with_junk = format!("{phone}%ZZ");
    let response = inspect(&with_junk, &open_ctx());
    assert!(
        kinds(&response).contains(&FindingKind::CnMobile),
        "{response:?}"
    );
    assert_eq!(response.sensitivity, Sensitivity::S2, "{response:?}");
    assert_eq!(response.action, InspectAction::Redact, "{response:?}");
}

/// Default_Ignorable insertions that the closed 6-codepoint list missed.
/// Each must match the clean string's verdict and keep the insert inside the span.
#[test]
fn default_ignorable_insertions_do_not_hide_id_or_mobile() {
    // TAG SPACE, LANGUAGE TAG, VS17, VS16, INVISIBLE SEPARATOR, LRM,
    // COMBINING GRAPHEME JOINER, MONGOLIAN VOWEL SEPARATOR, soft hyphen, ZWNJ.
    let inserts = [
        '\u{E0020}',
        '\u{E0001}',
        '\u{E0100}',
        '\u{FE0F}',
        '\u{2063}',
        '\u{200E}',
        '\u{034F}',
        '\u{180E}',
        '\u{00AD}',
        '\u{200C}',
    ];
    let clean_id = "11010519491231002X";
    let clean_mobile = "13812345678";
    let clean_id_resp = inspect(clean_id, &open_ctx());
    let clean_mobile_resp = inspect(clean_mobile, &open_ctx());
    assert_ne!(clean_id_resp.action, InspectAction::Allow);
    assert_ne!(clean_mobile_resp.action, InspectAction::Allow);

    for insert in inserts {
        let noisy_id = format!("1101{insert}0519491231002X");
        let id_resp = inspect(&noisy_id, &open_ctx());
        assert_eq!(
            id_resp.action, clean_id_resp.action,
            "id insert {insert:?}: {id_resp:?}"
        );
        assert_ne!(id_resp.action, InspectAction::Allow, "id insert {insert:?}");
        let id_hit = id_resp
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::CnResidentId)
            .unwrap_or_else(|| panic!("id insert {insert:?}: {id_resp:?}"));
        assert_eq!(
            covered(&noisy_id, id_hit),
            noisy_id,
            "id span must cover insert {insert:?}"
        );

        let noisy_mobile = format!("138{insert}12345678");
        let mobile_resp = inspect(&noisy_mobile, &open_ctx());
        assert_eq!(
            mobile_resp.action, clean_mobile_resp.action,
            "mobile insert {insert:?}: {mobile_resp:?}"
        );
        assert_ne!(
            mobile_resp.action,
            InspectAction::Allow,
            "mobile insert {insert:?}"
        );
        let mobile_hit = mobile_resp
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::CnMobile)
            .unwrap_or_else(|| panic!("mobile insert {insert:?}: {mobile_resp:?}"));
        let covered_mobile = covered(&noisy_mobile, mobile_hit);
        assert!(
            covered_mobile.contains(insert) || covered_mobile == noisy_mobile,
            "mobile span must cover insert {insert:?}: got {covered_mobile:?}"
        );
    }
}

/// 可忽略字符夹在两个 ASCII 记号之间时，不能把它们粘成一个更长的 run。
/// 纯删除会让这些行变成 Allow；实体内部的插入仍然要能命中，span 盖住插入。
#[test]
fn ignorable_between_ascii_tokens_does_not_join_away_the_entity() {
    let key = format!("sk-{}", "a".repeat(40));
    let rows = [
        (
            format!("Key\u{2068}{key}\u{2069}"),
            InspectAction::Block,
            FindingKind::ApiKey,
            key.clone(),
        ),
        (
            "\u{2068}1\u{2069}\u{2068}13812345678\u{2069}".to_string(),
            InspectAction::Redact,
            FindingKind::CnMobile,
            "13812345678".to_string(),
        ),
        (
            "1\u{200E}13812345678".to_string(),
            InspectAction::Redact,
            FindingKind::CnMobile,
            "13812345678".to_string(),
        ),
        (
            "2\u{200F}13812345678".to_string(),
            InspectAction::Redact,
            FindingKind::CnMobile,
            "13812345678".to_string(),
        ),
        (
            "13812345678\u{200E}9".to_string(),
            InspectAction::Redact,
            FindingKind::CnMobile,
            "13812345678".to_string(),
        ),
        (
            format!("a\u{200E}{key}"),
            InspectAction::Block,
            FindingKind::ApiKey,
            key.clone(),
        ),
    ];
    for (input, action, kind, entity) in rows {
        let response = inspect(&input, &open_ctx());
        assert_ne!(
            response.action,
            InspectAction::Allow,
            "{input:?} {response:?}"
        );
        assert_eq!(response.action, action, "{input:?} {response:?}");
        let hit = response
            .findings
            .iter()
            .find(|f| f.kind == kind && covered(&input, f) == entity)
            .unwrap_or_else(|| {
                panic!("{input:?} missing {kind:?} covering {entity}: {response:?}")
            });
        assert_eq!(covered(&input, hit), entity);
    }

    let split = "138\u{200B}1234\u{200B}5678";
    let response = inspect(split, &open_ctx());
    assert_eq!(response.action, InspectAction::Redact, "{response:?}");
    let hit = response
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::CnMobile)
        .unwrap();
    assert_eq!(covered(split, hit), split);
}

#[test]
fn soft_hyphen_and_zwnj_arms_each_have_a_dedicated_case() {
    // Removing either arm alone must turn these red (mutation targets).
    let soft = "138\u{00AD}12345678";
    let zwnj = "138\u{200C}12345678";
    for input in [soft, zwnj] {
        let response = inspect(input, &open_ctx());
        assert!(
            kinds(&response).contains(&FindingKind::CnMobile),
            "{input}: {response:?}"
        );
        assert_ne!(response.action, InspectAction::Allow, "{input}");
        let hit = response
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::CnMobile)
            .unwrap();
        assert_eq!(covered(input, hit), input);
    }
}

#[test]
fn lone_high_surrogate_is_not_treated_as_default_ignorable() {
    // U+DB40 is a high surrogate used by TAG / VS supplementary planes.
    // A lone unit must be kept so we never drop half a pair.
    let units = [
        u16::from(b'1'),
        u16::from(b'3'),
        u16::from(b'8'),
        0xDB40,
        u16::from(b'1'),
        u16::from(b'2'),
        u16::from(b'3'),
        u16::from(b'4'),
        u16::from(b'5'),
        u16::from(b'6'),
        u16::from(b'7'),
        u16::from(b'8'),
    ];
    let normalized = super::normalize::normalize(&units);
    assert!(
        normalized.units.contains(&0xDB40),
        "lone high surrogate must survive: {:?}",
        normalized.units
    );
    assert_eq!(normalized.units.len(), units.len());
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

#[test]
fn normalized_fullwidth_neighbours_follow_current_folding() {
    let id = "110105195001010004";
    for digit in ['０'] {
        // Main folds fullwidth digits before scanning, so a neighbouring
        // fullwidth digit no longer hides the ID.
        for input in [format!("{digit}{id}"), format!("{id}{digit}")] {
            let response = inspect(&input, &open_ctx());
            assert!(
                kinds(&response).contains(&FindingKind::CnResidentId),
                "{input}: {response:?}"
            );
        }
        let input = format!("{digit}:身份证{id}!{digit}");
        let response = inspect(&input, &open_ctx());
        assert!(
            response
                .findings
                .iter()
                .any(|f| { f.kind == FindingKind::CnResidentId && covered(&input, f) == id })
        );
        assert_eq!(response.sensitivity, Sensitivity::S3);
        assert_eq!(response.action, InspectAction::Block);
    }
}

#[test]
fn mathematical_digit_neighbour_does_not_hide_the_id() {
    let id = "110105195001010004";
    let digit = "\u{1D7CE}";
    for input in [format!("{digit}{id}"), format!("{id}{digit}")] {
        let response = inspect(&input, &open_ctx());
        assert!(
            response.findings.iter().any(|finding| {
                finding.kind == FindingKind::CnResidentId && covered(&input, finding) == id
            }),
            "{input}: {response:?}"
        );
    }
    let input = format!("{digit}:\u{8eab}\u{4efd}\u{8bc1}{id}!{digit}");
    let response = inspect(&input, &open_ctx());
    assert!(
        response.findings.iter().any(|finding| {
            finding.kind == FindingKind::CnResidentId && covered(&input, finding) == id
        }),
        "{input}: {response:?}"
    );
}

fn sensitivity_rank(sensitivity: Sensitivity) -> u8 {
    match sensitivity {
        Sensitivity::S0 => 0,
        Sensitivity::S1 => 1,
        Sensitivity::S2 => 2,
        Sensitivity::S3 => 3,
    }
}

fn action_rank(action: InspectAction) -> u8 {
    match action {
        InspectAction::Allow => 0,
        InspectAction::Redact => 1,
        InspectAction::Block => 2,
    }
}

/// 单视图的动作。坐标只用于把命中送进和 `inspect` 相同的 L2 判定。
fn action_of_view(normalized: &super::normalize::Normalized, ctx: &InspectCtx) -> InspectAction {
    let mut hits = super::l0::scan(&normalized.units);
    for hit in &mut hits {
        (hit.start, hit.length) = normalized.original_span(hit.start, hit.length);
    }
    let sensitivity = hits
        .iter()
        .map(|hit| hit.sensitivity)
        .max()
        .unwrap_or(Sensitivity::S0);
    super::decide_action(&hits, sensitivity, ctx)
}

/// Findings of one view, mapped back to original coordinates.
fn findings_of_view(normalized: &super::normalize::Normalized) -> Vec<(FindingKind, u64, u64)> {
    let mut hits = super::l0::scan(&normalized.units);
    for hit in &mut hits {
        (hit.start, hit.length) = normalized.original_span(hit.start, hit.length);
    }
    hits.iter()
        .map(|hit| (hit.kind, hit.start as u64, hit.length as u64))
        .collect()
}

proptest! {
    /// The union must never be looser than *any* of the three views, and it must
    /// keep every entity each view found. Generated inputs deliberately mix the
    /// three reasons the views exist:
    ///
    /// * a gluing char next to the entity (fullwidth digit, hyphen variant) -
    ///   only the **raw** view sees through it, because folding joins the two;
    /// * an ignorable before a fullwidth entity - only the **keep** view sees it,
    ///   because the raw view has no folding and the strip view joins;
    /// * an ignorable inside the entity - only the **strip** view sees it.
    ///
    /// So deleting any single view makes this property fail, not just the two it
    /// used to compare.
    #[test]
    fn union_is_never_looser_than_any_single_view(
        entity in prop_oneof![
            Just("13812345678".to_string()),
            Just("11010519491231002X".to_string()),
            Just(format!("sk-{}", "a".repeat(40))),
            Just("a@b.com".to_string()),
        ],
        lead in prop_oneof![Just(""), Just("1"), Just("１"), Just("a"), Just("ａ"), Just("-"), Just("\u{2010}")],
        trail in prop_oneof![Just(""), Just("9"), Just("１"), Just("x"), Just("ｘ"), Just("-")],
        // One representative per `Default_Ignorable_Code_Point` range. This is
        // coverage, not a guard on the table: dropping a range from
        // `normalize::is_default_ignorable` does NOT fail this property (the strip
        // view stops dropping it too, so the union stays a superset). The table
        // is guarded by `every_default_ignorable_range_still_breaks_an_entity_insertion`.
        inner in proptest::collection::vec(
            prop_oneof![
                Just('\u{00AD}'),   // 0x00AD
                Just('\u{034F}'),   // 0x034F
                Just('\u{061C}'),   // 0x061C
                Just('\u{115F}'),   // 0x115F..=0x1160
                Just('\u{17B4}'),   // 0x17B4..=0x17B5
                Just('\u{180B}'),   // 0x180B..=0x180F
                Just('\u{200B}'),   // 0x200B..=0x200F
                Just('\u{200E}'),
                Just('\u{200F}'),
                Just('\u{202A}'),   // 0x202A..=0x202E
                Just('\u{2060}'),   // 0x2060..=0x206F
                Just('\u{2068}'),
                Just('\u{2069}'),
                Just('\u{3164}'),   // 0x3164
                Just('\u{FE00}'),   // 0xFE00..=0xFE0F
                Just('\u{FEFF}'),   // 0xFEFF
                Just('\u{FFA0}'),   // 0xFFA0
                Just('\u{FFF0}'),   // 0xFFF0..=0xFFF8
                Just('\u{1BCA0}'),  // 0x1BCA0..=0x1BCA3 (non-BMP)
                Just('\u{1D173}'),  // 0x1D173..=0x1D17A (non-BMP)
                Just('\u{E0020}'),  // 0xE0000..=0xE0FFF (TAG, non-BMP)
            ],
            0..3,
        ),
        fold_entity in any::<bool>(),
        // A boundary-only ignorable *between* the lead and the entity. This is
        // the shape that only the keep view catches: the raw view has no
        // folding, and the strip view joins the lead to the entity.
        pre in prop_oneof![
            Just(""),
            Just("\u{200B}"),
            Just("\u{200E}"),
            Just("\u{2068}"),
        ],
    ) {
        // Sometimes widen the entity to fullwidth so folding is required at all.
        let entity = if fold_entity {
            entity.chars().map(|c| match c {
                '0'..='9' => char::from_u32(c as u32 - b'0' as u32 + 0xFF10).unwrap_or(c),
                'a'..='z' => char::from_u32(c as u32 - b'a' as u32 + 0xFF41).unwrap_or(c),
                'A'..='Z' => char::from_u32(c as u32 - b'A' as u32 + 0xFF21).unwrap_or(c),
                other => other,
            }).collect::<String>()
        } else {
            entity
        };
        let mut text = String::from(lead);
        text.push_str(pre);
        // Insert the ignorables inside the entity when it is long enough.
        let mut inserted = 0usize;
        for (index, ch) in entity.chars().enumerate() {
            if inserted < inner.len() && index == (inserted + 1) * 4 {
                text.push(inner[inserted]);
                inserted += 1;
            }
            text.push(ch);
        }
        while inserted < inner.len() {
            text.push(inner[inserted]);
            inserted += 1;
        }
        text.push_str(trail);

        let units: Vec<u16> = text.encode_utf16().collect();
        let ctx = open_ctx();
        let merged = inspect(&text, &ctx);
        let merged_findings: Vec<(FindingKind, u64, u64)> = merged
            .findings
            .iter()
            .map(|finding| {
                let span = finding.span.unwrap_or(idoris_contracts::inspect::InspectSpan {
                    start: 0,
                    length: 0,
                });
                (finding.kind, span.start, span.length)
            })
            .collect();

        let views = [
            ("raw", super::normalize::identity(&units)),
            ("keep", super::normalize::fold_keeping_ignorables(&units)),
            ("strip", super::normalize::normalize(&units)),
        ];
        for (name, view) in views {
            let view_findings = findings_of_view(&view);
            let view_sensitivity = {
                let mut hits = super::l0::scan(&view.units);
                for hit in &mut hits {
                    (hit.start, hit.length) = view.original_span(hit.start, hit.length);
                }
                hits.iter().map(|hit| hit.sensitivity).max().unwrap_or(Sensitivity::S0)
            };
            prop_assert!(
                action_rank(merged.action) >= action_rank(action_of_view(&view, &ctx)),
                "{name} is stricter than the union: view {:?} merged {:?} input {text:?}",
                action_of_view(&view, &ctx),
                merged.action
            );
            prop_assert!(
                sensitivity_rank(merged.sensitivity) >= sensitivity_rank(view_sensitivity),
                "{name} sensitivity is stricter than the union: input {text:?}"
            );
            for finding in view_findings {
                prop_assert!(
                    merged_findings.contains(&finding),
                    "{name} finding {finding:?} missing from the union {merged_findings:?} input {text:?}"
                );
            }
        }
    }

    #[test]
    fn zero_width_noise_never_hides_a_valid_id(
        id in valid_id(),
        noise in proptest::collection::vec((0usize..18, prop_oneof![
            Just('\u{200B}'), Just('\u{200C}'), Just('\u{200D}'), Just('\u{200E}'),
            Just('\u{2063}'), Just('\u{FEFF}'), Just('\u{00AD}'), Just('\u{034F}'),
            Just('\u{FE0F}'), Just('\u{E0020}'),
        ]), 0..6),
    ) {
        let mut chars: Vec<char> = id.chars().collect();
        for (at, zw) in noise.iter().rev() {
            chars.insert((*at).min(chars.len()), *zw);
        }
        let noisy: String = chars.into_iter().collect();
        let response = inspect(&noisy, &open_ctx());
        let found = response.findings.iter().any(|f| {
            f.kind == FindingKind::CnResidentId
                && covered(&noisy, f).chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>() == id
        });
        prop_assert!(found, "{:?} {:?}", noisy, response);
    }

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
        before in r"[^\p{N}\p{Z}\p{Pd}−\\]{0,8}",
        after in r"[^\p{N}\p{Z}\p{Pd}−\\xXｘＸхХΧ]{0,8}",
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

/// Absolute guard for the `Default_Ignorable_Code_Point` drop list.
///
/// The union property above compares the union against each view, so it cannot
/// notice a range disappearing from `is_default_ignorable`: when a code point is
/// no longer dropped, the *strip* view stops dropping it too and the property
/// still holds — even though the "ignorable inserted inside an entity" capability
/// is gone. This test pins the capability itself, with one hard-coded
/// representative per range so the table and the guard cannot drift together.
#[test]
fn every_default_ignorable_range_still_breaks_an_entity_insertion() {
    // Independent copy of the ranges; deliberately not derived from the table.
    const RANGES: &[char] = &[
        '\u{00AD}',
        '\u{034F}',
        '\u{061C}',
        '\u{115F}',
        '\u{17B4}',
        '\u{180B}',
        '\u{200B}',
        '\u{200E}',
        '\u{200F}',
        '\u{202A}',
        '\u{2060}',
        '\u{2068}',
        '\u{2069}',
        '\u{3164}',
        '\u{FE00}',
        '\u{FEFF}',
        '\u{FFA0}',
        '\u{FFF0}',
        '\u{1BCA0}',
        '\u{1D173}',
        '\u{E0020}',
    ];
    let ctx = open_ctx();
    for ch in RANGES {
        let mobile = format!("138{ch}1234{ch}5678");
        let response = inspect(&mobile, &ctx);
        assert_ne!(
            response.action,
            InspectAction::Allow,
            "U+{:04X} inserted inside a mobile was not detected: {response:?}",
            *ch as u32
        );
        assert!(
            kinds(&response).contains(&FindingKind::CnMobile),
            "U+{:04X} must still yield CnMobile: {response:?}",
            *ch as u32
        );

        let id = format!("110105{ch}19491231002X");
        let response = inspect(&id, &ctx);
        assert!(
            kinds(&response).contains(&FindingKind::CnResidentId),
            "U+{:04X} inserted inside a resident id was not detected: {response:?}",
            *ch as u32
        );
    }
}

#[test]
fn opaque_data_urls_block_both_consents_without_fabricated_findings() {
    for input in [
        "data:image/png;base64,YQ==",
        "![label](data:image/png;base64,YQ==)",
        "<img src=\"data:image/png;base64,YQ==\">",
        "DaTa:Application/X-OPAQUE;BaSe64,YQ==",
        "data:application/x-!opaque;base64,YQ==",
        "data:text/plain;name=a!b;base64,YQ==",
        "data:application/x-~opaque;base64,YQ==",
        "data:;base64,YQ==",
    ] {
        for redact_consent in [false, true] {
            let ctx = InspectCtx {
                redact_consent,
                ..open_ctx()
            };
            let response = inspect(input, &ctx);
            assert_eq!(response.action, InspectAction::Block, "{input}");
            assert_eq!(response.sensitivity, Sensitivity::S3, "{input}");
            assert!(
                response.findings.is_empty(),
                "{input}: {:?}",
                response.findings
            );
        }
    }
}

#[test]
fn opaque_header_alphabet_matches_accepted_ascii_predicate() {
    for byte in 0u8..=127 {
        let c = char::from(byte);
        let expected = (33..=126).contains(&byte) && !b"\"()<>@,:\\[]?".contains(&byte);
        assert_eq!(super::is_data_url_header_char(c), expected, "{byte}");
        if expected {
            let input = format!("data:application/x-{c};base64,YQ==");
            let response = inspect(&input, &open_ctx());
            assert_eq!(response.action, InspectAction::Block, "{input}");
            assert_eq!(response.sensitivity, Sensitivity::S3, "{input}");
        }
    }
    for c in ['é', '中', '😀'] {
        assert!(!super::is_data_url_header_char(c));
    }
}

#[test]
fn opaque_non_payloads_and_non_base64_headers_do_not_trigger() {
    for input in [
        "ordinary data text",
        "metadata: ordinary",
        "data:text/plain,hello",
        "data:text/plain;base64=YQ==",
        "data:text/plain;base64",
        "data:text/plain;base64;,YQ==",
        "data:image/png;base64,",
        "const src = `data:image/png;base64,${b64}`;",
        "'data:image/jpeg;base64,' + data",
        "Prefix the payload with data:image/png;base64, and then the bytes",
    ] {
        let response = inspect(input, &open_ctx());
        assert_eq!(response.action, InspectAction::Allow, "{input}");
        assert_eq!(response.sensitivity, Sensitivity::S0, "{input}");
        assert!(response.findings.is_empty());
    }
    assert_eq!(
        inspect("metadata: ordinary", &open_ctx()).action,
        InspectAction::Allow
    );
}

/// Known gap, not a lookalike claim. Node's WHATWG `data:` processor decodes
/// these to `a`, because it strips whitespace and accepts a broader header.
/// This linear scan still ends the header at space, controls, `:`, `?`, and
/// non-ASCII, so these stay Allow until a single-pass decoder replaces it.
#[test]
fn opaque_browser_decoded_forms_are_a_known_gap() {
    for input in [
        "data:text/plain:bad;base64,YQ==",
        "data:text/\u{00e9};base64,YQ==",
        "data:text/plain\n;base64,YQ==",
        "data:text/plain\0;base64,YQ==",
        "data:text/plain ;base64,YQ==",
        "data:text/plain?x;base64,YQ==",
        "data:image/png; base64,YQ==",
        "data:image/png;base64 ,YQ==",
        "data:text/plain;charset=\"utf-8\";base64,YQ==",
    ] {
        let response = inspect(input, &open_ctx());
        assert_eq!(response.action, InspectAction::Allow, "{input}");
        assert_eq!(response.sensitivity, Sensitivity::S0, "{input}");
    }
}

#[test]
fn opaque_header_work_is_bounded_and_input_limit_precedes_helper() {
    for (repeats, expected) in [(512, 2555), (4096, 20475)] {
        super::reset_data_url_header_char_checks();
        assert!(!super::contains_base64_data_url(&"data:".repeat(repeats)));
        assert_eq!(super::data_url_header_char_checks(), expected);
    }
    let input = format!("data:application/x-{};base64,YQ==", "a".repeat(100_000));
    let ctx = InspectCtx {
        max_input_utf16: input.len() + 1,
        ..open_ctx()
    };
    super::reset_data_url_header_char_checks();
    assert_eq!(inspect(&input, &ctx).action, InspectAction::Block);
    assert_eq!(super::data_url_header_char_checks(), 100_022);
    super::reset_data_url_header_char_checks();
    let limited = inspect(
        &input,
        &InspectCtx {
            max_input_utf16: 16,
            ..ctx
        },
    );
    assert_eq!(super::data_url_header_char_checks(), 0);
    assert_eq!(
        limited.findings,
        vec![InspectFinding {
            kind: FindingKind::InputLimitExceeded,
            span: None
        }]
    );
}

#[test]
fn opaque_data_url_preserves_real_findings_and_supplementary_utf16_spans() {
    for prefix in ["", "😀 label: "] {
        let clean = format!("{prefix}11010519491231002X data:text/plain,hello");
        let opaque = format!("{prefix}11010519491231002X data:text/plain;base64,YQ==");
        for redact_consent in [false, true] {
            let ctx = InspectCtx {
                redact_consent,
                ..open_ctx()
            };
            let expected = inspect(&clean, &ctx).findings;
            assert_eq!(
                expected,
                vec![InspectFinding {
                    kind: FindingKind::CnResidentId,
                    span: Some(InspectSpan {
                        start: prefix.encode_utf16().count() as u64,
                        length: 18
                    }),
                }]
            );
            let response = inspect(&opaque, &ctx);
            assert_eq!(response.findings, expected);
            assert_eq!(response.sensitivity, Sensitivity::S3);
            assert_eq!(response.action, InspectAction::Block);
        }
    }
}

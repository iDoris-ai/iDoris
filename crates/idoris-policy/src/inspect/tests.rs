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
    ] {
        let response = inspect(input, &open_ctx());
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

/// Regressions from the 2026-10-11 adversarial review: every input below
/// used to come back clean.
#[test]
fn review_regressions_grouped_split_and_invisible_variants() {
    // F4 (ids split across lines or parts) is deliberately not covered:
    // joining on line breaks flagged most numeric tables (second review)
    // and cannot reach across JSON parts anyway.
    let cases: [(&str, &[FindingKind]); 8] = [
        // F3: neighbouring grouped numbers
        (
            "张三 110101 19900307 7379 138 1234 5678",
            &[FindingKind::CnResidentId, FindingKind::CnMobile],
        ),
        // F5: invisible separators and other digit scripts
        ("138\u{00A0}1234\u{00A0}5678", &[FindingKind::CnMobile]),
        ("138\u{200E}1234\u{200E}5678", &[FindingKind::CnMobile]),
        ("138  1234  5678", &[FindingKind::CnMobile]),
        (
            "\u{0661}\u{0663}\u{0668}\u{0661}\u{0662}\u{0663}\u{0664}\u{0665}\u{0666}\u{0667}\u{0668}",
            &[FindingKind::CnMobile],
        ),
        // F6: encodings
        (
            "see id/MTEwMTAxMTk5MDAzMDc3MzdY",
            &[FindingKind::CnResidentId],
        ),
        ("0x3133383132333435363738", &[FindingKind::CnMobile]),
        (
            "\\u0031\\u0033\\u0038\\u0031\\u0032\\u0033\\u0034\\u0035\\u0036\\u0037\\u0038",
            &[FindingKind::CnMobile],
        ),
    ];
    for (input, expected) in cases {
        let found = kinds(&inspect(input, &open_ctx()));
        for kind in expected {
            assert!(found.contains(kind), "{input:?}: {found:?}");
        }
    }
}

#[test]
fn other_digit_scripts_and_latin_homoglyphs_are_folded() {
    let thai_phone = "เบอร์ ๑๓๘๑๒๓๔๕๖๗๘";
    assert!(kinds(&inspect(thai_phone, &open_ctx())).contains(&FindingKind::CnMobile));
    let devanagari_id: String = "11010519491231002"
        .chars()
        .map(|d| char::from_u32(0x0966 + d.to_digit(10).unwrap()).unwrap())
        .chain(['X'])
        .collect();
    let response = inspect(&devanagari_id, &open_ctx());
    assert!(
        kinds(&response).contains(&FindingKind::CnResidentId),
        "{response:?}"
    );
    assert_eq!(
        covered(&devanagari_id, &response.findings[0]),
        devanagari_id
    );
    // Cyrillic DZE for `s` in the prefix, Cyrillic A and K in AKIA.
    let spoofed_openai = FAKE_OPENAI.replacen('s', "\u{0455}", 1);
    let spoofed_aws = FAKE_AWS_ID.replacen("AK", "\u{0410}\u{041A}", 1);
    for key in [spoofed_openai, spoofed_aws] {
        assert!(
            kinds(&inspect(&key, &open_ctx())).contains(&FindingKind::ApiKey),
            "{key}"
        );
    }
    // Ordinary Russian and Greek text stays clean.
    for text in [
        "Привет, как дела? Сегодня хорошая погода.",
        "Καλημέρα, τι κάνεις;",
    ] {
        assert!(inspect(text, &open_ctx()).findings.is_empty(), "{text}");
    }
}

#[test]
fn lists_of_short_numbers_are_not_grouped_into_entities() {
    for input in [
        "sort: 3 1 4 1 5 9 2 6 5 3 5 8 9 7 9 3 2 3 8 4 6",
        "scores 12 34 56 78 90 12 34 56 78 90",
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n0\n1\n2\n3\n4\n5\n6\n7\n8\n9",
        // Second review: flattened logit_bias, a TSV table, ports, years.
        "logit_bias\n198\n-100\n50256\n-100\n15339\n5\n1734\n10",
        "1\t1520\t45600\t31200\n2\t1830\t52100\t29900\n3\t2210\t61200\t33400",
        "ports 8080 8443 9090 3000",
        "years 2019 2020 2021 2022 2023",
        "ids 1203 4567 8901 2345 6789",
    ] {
        let response = inspect(input, &open_ctx());
        assert!(response.findings.is_empty(), "{input:?}: {response:?}");
    }
    // Real groupings still join.
    for input in [
        "6222 0212 3456 7894",
        "138 1234 5678",
        "110101 19900307 7379",
    ] {
        assert!(!inspect(input, &open_ctx()).findings.is_empty(), "{input}");
    }
}

#[test]
fn escapes_and_math_digits_are_folded_in_place() {
    let escaped = "\\u0031\\u0033\\u0038\\u0031\\u0032\\u0033\\u0034\\u0035\\u0036\\u0037\\u0038";
    for input in [
        format!("电话 {escaped}\\uD800"),
        "\\u0031\\u0033\\u0038-\\u0031\\u0032\\u0033\\u0034-\\u0035\\u0036\\u0037\\u0038"
            .to_string(),
        "\\u0031\\u0033\\u0038 1234 5678".to_string(),
        "电话 𝟏𝟑𝟖𝟏𝟐𝟑𝟒𝟓𝟔𝟕𝟖".to_string(),
    ] {
        let response = inspect(&input, &open_ctx());
        assert!(
            kinds(&response).contains(&FindingKind::CnMobile),
            "{input}: {response:?}"
        );
    }
    // The span covers whole escapes and whole surrogate pairs.
    let input = format!("tel {escaped}!");
    let response = inspect(&input, &open_ctx());
    assert_eq!(covered(&input, &response.findings[0]), escaped);
    let bold = "𝟏𝟑𝟖𝟏𝟐𝟑𝟒𝟓𝟔𝟕𝟖";
    let response = inspect(bold, &open_ctx());
    assert_eq!(covered(bold, &response.findings[0]), bold);
}

#[test]
fn third_review_groupings_and_numeric_rows() {
    // Any written grouping of one number is still that number.
    for (input, kind) in [
        ("138-12345678", FindingKind::CnMobile),
        ("138 12345678", FindingKind::CnMobile),
        ("1381234 5678", FindingKind::CnMobile),
        ("1381 234 5678", FindingKind::CnMobile),
        ("110101 1949 1231 0023", FindingKind::CnResidentId),
        ("110101 194912310023", FindingKind::CnResidentId),
        (
            "\\uD835\\uDFCF\\uD835\\uDFD1\\uD835\\uDFD6\\uD835\\uDFCF\\uD835\\uDFD0\\uD835\\uDFD1\\uD835\\uDFD2\\uD835\\uDFD3\\uD835\\uDFD4\\uD835\\uDFD5\\uD835\\uDFD6",
            FindingKind::CnMobile,
        ),
    ] {
        assert!(
            kinds(&inspect(input, &open_ctx())).contains(&kind),
            "{input}"
        );
    }
    // Rows of numbers do not decompose into entities.
    for input in [
        "[4439 2537 8993 1464 7386 8090 1034 8297]",
        "[3412 5590 6012 4471 9032 1188 2765 4410 3307 6621 5129 8840 1203 4456 7781 3302 1509 6654 4421 3398]",
    ] {
        let response = inspect(input, &open_ctx());
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

#[test]
fn fourth_review_entities_with_short_neighbours() {
    // Neighbours of up to three digits only: allowing four (a year, a room)
    // flagged ~10% of rows of five 4-digit numbers as cards.
    for (input, expected) in [
        ("4111 1111 1111 1111 12/28", &[FindingKind::BankCard][..]),
        ("4111 1111 1111 1111 123", &[FindingKind::BankCard]),
        ("4111 1111 1111 1111 12 28", &[FindingKind::BankCard]),
        ("2026-10-11 20:19 138 1234 5678", &[FindingKind::CnMobile]),
        ("28 138 1234 5678", &[FindingKind::CnMobile]),
        ("0086 138 1234 5678", &[FindingKind::CnMobile]),
        ("0086-13812345678", &[FindingKind::CnMobile]),
        ("008613812345678", &[FindingKind::CnMobile]),
        ("138 1234 5678 139 8765 4321 7", &[FindingKind::CnMobile]),
    ] {
        let found = kinds(&inspect(input, &open_ctx()));
        for kind in expected {
            assert!(found.contains(kind), "{input}: {found:?}");
        }
    }
    let both = inspect("138 1234 5678 139 8765 4321 7", &open_ctx());
    assert_eq!(
        both.findings
            .iter()
            .filter(|f| f.kind == FindingKind::CnMobile)
            .count(),
        2
    );
    for input in [
        "59 15 33 28 80 70 89 61",
        "1 3 5 7 9 11 13 15",
        "合计 15 20 30 45 60 7",
        "16 17 18 19 20 1",
    ] {
        let response = inspect(input, &open_ctx());
        assert!(response.findings.is_empty(), "{input}: {response:?}");
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
    fn zero_width_noise_never_hides_a_valid_id(
        id in valid_id(),
        noise in proptest::collection::vec((0usize..18, prop_oneof![Just('\u{200B}'), Just('\u{200D}'), Just('\u{FEFF}')]), 0..6),
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

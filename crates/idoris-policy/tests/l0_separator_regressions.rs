use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{
    FindingKind, InspectAction, InspectFinding, InspectSpan, Sensitivity,
};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn open_ctx() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn assert_entity(text: &str, canonical: &str, kind: FindingKind) {
    let expected = inspect(canonical, &open_ctx());
    let input = format!("😀号码：{text}。结束");
    let actual = inspect(&input, &open_ctx());
    assert_eq!(actual.action, expected.action, "{text}: {actual:?}");
    assert_eq!(actual.sensitivity, expected.sensitivity, "{text}");
    assert_eq!(
        actual.findings,
        [InspectFinding {
            kind,
            span: Some(InspectSpan {
                start: 5,
                length: text.encode_utf16().count() as u64,
            }),
        }],
        "{text}"
    );
}

#[test]
fn tab_period_slash_cards_match_canonical_block_and_utf16_span() {
    let canonical = "6222021234567894";
    let control = inspect(canonical, &open_ctx());
    assert_eq!(control.action, InspectAction::Block);
    assert_eq!(control.sensitivity, Sensitivity::S3);
    for separator in ["\t", ".", "/", " ", "-"] {
        let text = ["6222", "0212", "3456", "7894"].join(separator);
        assert_entity(&text, canonical, FindingKind::BankCard);
    }
}

#[test]
fn tab_period_slash_phones_match_canonical_redact_and_utf16_span() {
    let canonical = "13812345678";
    let control = inspect(canonical, &open_ctx());
    assert_eq!(control.action, InspectAction::Redact);
    assert_eq!(control.sensitivity, Sensitivity::S2);
    for separator in ["\t", ".", "/", " ", "-"] {
        let text = ["138", "1234", "5678"].join(separator);
        assert_entity(&text, canonical, FindingKind::CnMobile);
    }
}

#[test]
fn extended_cards_country_prefix_and_mixed_separators_keep_exact_spans() {
    for text in ["86.138.1234.5678", "138\t1234/5678"] {
        assert_entity(text, "8613812345678", FindingKind::CnMobile);
    }
    assert_entity(
        "6214/8300/0000/0000/007",
        "6214830000000000007",
        FindingKind::BankCard,
    );
    assert_entity(
        "6222.0212/3456\t7894",
        "6222021234567894",
        FindingKind::BankCard,
    );
}

#[test]
fn formatted_entities_survive_neighboring_numeric_segments() {
    let input = "7 138.1234.5678 6222/0212/3456/7894 42";
    let response = inspect(input, &open_ctx());
    assert_eq!(response.action, InspectAction::Block);
    assert_eq!(response.sensitivity, Sensitivity::S3);
    assert_eq!(
        response.findings,
        [
            InspectFinding {
                kind: FindingKind::CnMobile,
                span: Some(InspectSpan {
                    start: 2,
                    length: 13
                }),
            },
            InspectFinding {
                kind: FindingKind::BankCard,
                span: Some(InspectSpan {
                    start: 16,
                    length: 19
                }),
            },
        ]
    );
}

#[test]
fn dates_ipv4_decimals_and_unrelated_short_groups_stay_public() {
    for input in [
        "hello",
        "2026.10.15 2026/10/15 2026-10-15 15/10/2026",
        "127.0.0.1 192.168.100.100 255.255.255.255",
        "138.12345678 62220212.34567894 1234.5678",
        // These concatenate to a valid mobile/card, but have no supported grouping.
        "13.81.23.45.678",
        "6/22/20/21/23/45/67/89/4",
        "13\t81\t23\t45\t678",
        "6\t22\t20\t21\t23\t45\t67\t89\t4",
        "1.2.3.4.5.6.7.8.9.0.1.2.3.4.5.6.7.8.9.0",
        // A valid card subwindow must not turn a numeric table into an entity.
        "1111.6222.0212.3456.7894.2222",
        "1111/6222/0212/3456/7894/2222",
        "1111\t6222\t0212\t3456\t7894\t2222",
    ] {
        let response = inspect(input, &open_ctx());
        assert_eq!(
            response.action,
            InspectAction::Allow,
            "{input}: {response:?}"
        );
        assert_eq!(response.sensitivity, Sensitivity::S0, "{input}");
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

#[test]
fn invalid_checksum_prefix_and_malformed_separators_are_not_entities() {
    for input in [
        "6222\t0212\t3456\t7891",
        "6222.0212.3456.7891",
        "6222/0212/3456/7891",
        "128.1234.5678",
        "138..1234.5678",
        "6222//0212/3456/7894",
        "6222\t\t0212\t3456\t7894",
        "138\n1234\n5678",
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
fn local_only_and_missing_consent_remain_fail_closed() {
    for input in [
        "hello",
        "6222\t0212\t3456\t7894",
        "6222.0212.3456.7894",
        "6222/0212/3456/7894",
        "6222 0212 3456 7894",
        "138.1234.5678",
    ] {
        for ctx in [
            InspectCtx {
                declared: PrivacyClass::LocalOnly,
                ..open_ctx()
            },
            InspectCtx {
                remote_allowed: false,
                ..open_ctx()
            },
        ] {
            assert_eq!(inspect(input, &ctx).action, InspectAction::Block, "{input}");
        }
    }
    let ctx = InspectCtx {
        redact_consent: false,
        ..open_ctx()
    };
    assert_eq!(inspect("138.1234.5678", &ctx).action, InspectAction::Block);
}

#[test]
fn long_short_segment_stream_does_not_create_entities() {
    let input = "1.".repeat(100_000);
    let response = inspect(&input, &open_ctx());
    assert_eq!(response.action, InspectAction::Allow);
    assert!(response.findings.is_empty());
}

#[test]
fn canonical_neighbor_and_country_code_keep_all_expected_spans() {
    let input = "138.1234.5678 6222021234567894";
    let response = inspect(input, &open_ctx());
    assert_eq!(response.action, InspectAction::Block);
    assert_eq!(response.findings.len(), 2);
    assert_eq!(
        response.findings[0].span,
        Some(InspectSpan {
            start: 0,
            length: 13
        })
    );
    assert_eq!(
        response.findings[1].span,
        Some(InspectSpan {
            start: 14,
            length: 16
        })
    );
    let response = inspect("86/13812345678", &open_ctx());
    assert_eq!(response.action, InspectAction::Redact);
    // Preserve the legacy standalone match as well as the prefixed match.
    assert_eq!(
        response.findings,
        [
            InspectFinding {
                kind: FindingKind::CnMobile,
                span: Some(InspectSpan {
                    start: 0,
                    length: 14
                })
            },
            InspectFinding {
                kind: FindingKind::CnMobile,
                span: Some(InspectSpan {
                    start: 3,
                    length: 11
                })
            },
        ]
    );
}

#[test]
fn unrelated_four_digit_neighbor_prevents_new_window_matching() {
    // Same conservative boundary as 7c6cee4: a room number is not a short edge.
    assert_eq!(
        inspect("1203 138.1234.5678", &open_ctx()).action,
        InspectAction::Allow
    );
    // A matching 3/4/4 row is intrinsically ambiguous and is treated as a phone.
    assert_eq!(
        inspect("138.1234.5678", &open_ctx()).action,
        InspectAction::Redact
    );
}

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

fn finding(kind: FindingKind, start: u64, length: u64) -> InspectFinding {
    InspectFinding {
        kind,
        span: Some(InspectSpan { start, length }),
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
        // Underscore and comma are out of scope for this separator pass.
        "138_1234_5678",
        "138,1234,5678",
        "6222_0212_3456_7894",
        "6222,0212,3456,7894",
        // Formatted ids with a bad province or an impossible date stay public.
        "990105.19491231.0003",
        "110105.19491331.0003",
        "110105.19000229.0003",
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
    // Deliberate false positive: an entity-shaped numeric row is reported even
    // when the neighbor is an unrelated four-digit room number. This used to
    // expect Allow (7c6cee4 / aced94d full-chain veto).
    let response = inspect("1203 138.1234.5678", &open_ctx());
    assert_eq!(response.action, InspectAction::Redact);
    assert_eq!(response.sensitivity, Sensitivity::S2);
    assert_eq!(response.findings, [finding(FindingKind::CnMobile, 5, 13)]);
    // A matching 3/4/4 row is intrinsically ambiguous and is treated as a phone.
    assert_eq!(
        inspect("138.1234.5678", &open_ctx()).action,
        InspectAction::Redact
    );
}

#[test]
fn g1_edge_digit_keeps_formatted_phone_and_card_span() {
    for (input, start) in [("138-1234-5678 1", 0u64), ("1 138-1234-5678", 2)] {
        let granted = inspect(input, &open_ctx());
        assert_eq!(granted.action, InspectAction::Redact, "{input}");
        assert_eq!(granted.sensitivity, Sensitivity::S2, "{input}");
        assert_eq!(
            granted.findings,
            [finding(FindingKind::CnMobile, start, 13)],
            "{input}"
        );
        let missing = inspect(
            input,
            &InspectCtx {
                redact_consent: false,
                ..open_ctx()
            },
        );
        assert_eq!(missing.action, InspectAction::Block, "{input}");
        assert_eq!(missing.sensitivity, Sensitivity::S2, "{input}");
        assert_eq!(missing.findings, granted.findings, "{input}");
    }
    for (input, start) in [
        ("6222 0212 3456 7894 1", 0u64),
        ("12 6222 0212 3456 7894", 3),
    ] {
        for consent in [true, false] {
            let response = inspect(
                input,
                &InspectCtx {
                    redact_consent: consent,
                    ..open_ctx()
                },
            );
            assert_eq!(response.action, InspectAction::Block, "{input}");
            assert_eq!(response.sensitivity, Sensitivity::S3, "{input}");
            assert_eq!(
                response.findings,
                [finding(FindingKind::BankCard, start, 19)],
                "{input}"
            );
        }
    }
}

#[test]
fn g2_period_tab_slash_resident_ids_are_s3_exact_span() {
    let specs: &[(&[&str], &str)] = &[
        (&["110105", "19491231", "0003"], "110105194912310003"),
        (&["110105", "19491231", "002X"], "11010519491231002X"),
        (&["440308", "19900101", "1239"], "440308199001011239"),
        (&["440308", "1990", "0101", "1239"], "440308199001011239"),
        (&["110105", "1949", "1231", "002X"], "11010519491231002X"),
    ];
    for (parts, canonical) in specs {
        for separator in ["\t", ".", "/"] {
            assert_entity(&parts.join(separator), canonical, FindingKind::CnResidentId);
        }
    }
    assert_entity(
        "110105.19491231.002x",
        "11010519491231002x",
        FindingKind::CnResidentId,
    );
    let downgraded = inspect("110105.19491231.0001", &open_ctx());
    assert_eq!(downgraded.action, InspectAction::Redact);
    assert_eq!(downgraded.sensitivity, Sensitivity::S2);
    assert_eq!(
        downgraded.findings,
        [finding(FindingKind::CnResidentId, 0, 20)]
    );
}

#[test]
fn taiwan_resident_830000_follows_id_checksum_rules() {
    assert_entity(
        "830000199001011234",
        "830000199001011234",
        FindingKind::CnResidentId,
    );
    assert_entity(
        "830000.19900101.1234",
        "830000199001011234",
        FindingKind::CnResidentId,
    );
    assert_entity(
        "830000/1990/0101/1234",
        "830000199001011234",
        FindingKind::CnResidentId,
    );
    assert_entity(
        "830000\t19900101\t1234",
        "830000199001011234",
        FindingKind::CnResidentId,
    );
    let downgraded = inspect("830000199001011230", &open_ctx());
    assert_eq!(downgraded.action, InspectAction::Redact);
    assert_eq!(downgraded.sensitivity, Sensitivity::S2);
    assert_eq!(
        downgraded.findings,
        [finding(FindingKind::CnResidentId, 0, 18)]
    );
    let formatted = inspect("830000.19900101.1230", &open_ctx());
    assert_eq!(formatted.action, InspectAction::Redact);
    assert_eq!(formatted.sensitivity, Sensitivity::S2);
    assert_eq!(
        formatted.findings,
        [finding(FindingKind::CnResidentId, 0, 20)]
    );
    let missing = inspect(
        "830000199001011230",
        &InspectCtx {
            redact_consent: false,
            ..open_ctx()
        },
    );
    assert_eq!(missing.action, InspectAction::Block);
    assert_eq!(missing.findings, downgraded.findings);
    // 83 only means the 18-digit 830000 resident ID. Other 83xxxx addresses,
    // 15-digit legacy numbers, and 8-digit permits stay public.
    for input in [
        "830105199001011231",
        "830105.19900101.1231",
        "831100199001011230",
        "830000900101123",
        "830105900101123",
        "83000019",
        "01234567",
    ] {
        let response = inspect(input, &open_ctx());
        assert_eq!(response.action, InspectAction::Allow, "{input}");
        assert!(response.findings.is_empty(), "{input}: {response:?}");
    }
}

#[test]
fn g3_entity_subwindows_ignore_unrelated_neighbors() {
    let phones = [
        ("1203 138.1234.5678", 5u64),
        ("1 2 3 138.1234.5678", 6),
        ("1203 138 1234 5678", 5),
        ("1203 138-1234-5678", 5),
    ];
    for (input, start) in phones {
        let response = inspect(input, &open_ctx());
        assert_ne!(response.action, InspectAction::Allow, "{input}");
        assert_eq!(response.action, InspectAction::Redact, "{input}");
        assert_eq!(response.sensitivity, Sensitivity::S2, "{input}");
        assert_eq!(
            response.findings,
            [finding(FindingKind::CnMobile, start, 13)],
            "{input}: {response:?}"
        );
    }
    let cards = [
        ("6222.0212.3456.7894 2028", 0u64, 19u64),
        ("2026.10.15 6222.0212.3456.7894", 11, 19),
        ("6222\t0212\t3456\t7894\t1000", 0, 19),
    ];
    for (input, start, length) in cards {
        let response = inspect(input, &open_ctx());
        assert_ne!(response.action, InspectAction::Allow, "{input}");
        assert_eq!(response.action, InspectAction::Block, "{input}");
        assert_eq!(response.sensitivity, Sensitivity::S3, "{input}");
        assert_eq!(
            response.findings,
            [finding(FindingKind::BankCard, start, length)],
            "{input}: {response:?}"
        );
    }
    let both = inspect("138.1234.5678 7 139.1234.5678", &open_ctx());
    assert_eq!(both.action, InspectAction::Redact);
    assert_eq!(both.sensitivity, Sensitivity::S2);
    assert_eq!(
        both.findings,
        [
            finding(FindingKind::CnMobile, 0, 13),
            finding(FindingKind::CnMobile, 16, 13),
        ]
    );
    // Both 16-digit windows are Luhn-valid. Flagging the 1111-prefixed row is
    // the deliberate false positive of accepting entity-shaped subwindows.
    for input in [
        "1111.6222.0212.3456.7894.2222",
        "1111/6222/0212/3456/7894/2222",
        "1111\t6222\t0212\t3456\t7894\t2222",
    ] {
        let response = inspect(input, &open_ctx());
        assert_eq!(response.action, InspectAction::Block, "{input}");
        assert_eq!(response.sensitivity, Sensitivity::S3, "{input}");
        assert_eq!(
            response.findings,
            [
                finding(FindingKind::BankCard, 0, 19),
                finding(FindingKind::BankCard, 5, 19),
            ],
            "{input}: {response:?}"
        );
    }
}

#[test]
fn short_lead_run_is_not_glued_into_a_card_length_entity() {
    // `12` plus four four-digit groups reaches card length. The guard keeps the
    // short run out of the window, so the card is reported from its own start
    // and never as one entity spanning all five groups.
    let response = inspect("12 6222 0212 3456 7894", &open_ctx());
    assert_eq!(
        response.findings,
        [InspectFinding {
            kind: FindingKind::BankCard,
            span: Some(InspectSpan {
                start: 3,
                length: 19
            }),
        }],
        "{response:?}"
    );
    // A country-code phone keeps its short run: the window never reaches card
    // length, so `86.138.1234.5678` is still one 13-digit phone.
    let phone = inspect("86.138.1234.5678", &open_ctx());
    assert_eq!(
        phone.findings,
        [InspectFinding {
            kind: FindingKind::CnMobile,
            span: Some(InspectSpan {
                start: 0,
                length: 16
            }),
        }],
        "{phone:?}"
    );
    // The prefixed span from the legacy test is unchanged by the guard.
    let prefixed = inspect("😀号码：86.138.1234.5678。结束", &open_ctx());
    assert_eq!(
        prefixed.findings,
        [InspectFinding {
            kind: FindingKind::CnMobile,
            span: Some(InspectSpan {
                start: 5,
                length: 16
            }),
        }],
        "{prefixed:?}"
    );
}

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{FindingKind, InspectAction, Sensitivity};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn context(consent: bool) -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: consent,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn positive(input: &str, start: usize, length: usize) {
    let units: Vec<u16> = input.encode_utf16().collect();
    for consent in [false, true] {
        let response = inspect(input, &context(consent));
        assert_eq!(
            response.sensitivity,
            Sensitivity::S3,
            "{input:?}: {response:?}"
        );
        assert_eq!(response.action, InspectAction::Block, "{input:?}");
        assert!(
            response.findings.iter().any(|finding| {
                finding.kind == FindingKind::CnResidentId
                    && finding.span.as_ref().is_some_and(|span| {
                        span.start == start as u64 && span.length == length as u64
                    })
            }),
            "{input:?}: {response:?}"
        );
        for finding in &response.findings {
            if let Some(span) = &finding.span {
                assert!(span.start + span.length <= units.len() as u64);
            }
        }
        assert!(
            String::from_utf16(&units[start..start + length])
                .is_ok_and(|slice| !slice.ends_with('\n')),
            "invalid source slice or trailing newline in span"
        );
    }
}

fn no_id(input: &str) {
    for consent in [false, true] {
        let response = inspect(input, &context(consent));
        assert!(
            !response
                .findings
                .iter()
                .any(|f| f.kind == FindingKind::CnResidentId),
            "{input:?}: {response:?}"
        );
    }
}

#[test]
fn actual_445_positives_and_main_checksum_controls() {
    for input in [
        "1101051949\n1231002X",
        "110105194912\n31002X\n",
        "1101051949\n1231002x",
        "110105194912\n310003",
        "8300001990\n01011234",
    ] {
        positive(input, 0, 19);
        let joined = input.replace('\n', "");
        let response = inspect(&joined, &context(false));
        assert_eq!(
            response.sensitivity,
            Sensitivity::S3,
            "Main validator: {joined}"
        );
    }
    positive("1101051949\n1231002X\n7", 0, 19);
    positive("7\n1101051949\n1231002X", 2, 19);
}

#[test]
fn exact_utf16_crlf_and_existing_origin_maps() {
    positive("😀证件 1101051949\r\n1231002X trailing padding", 5, 20);
    for ignored in ["\u{200b}", "\u{2060}", "\u{e0100}"] {
        let input = format!("😀证件 110105{ignored}1949\n1231002X padding");
        positive(&input, 5, 19 + ignored.encode_utf16().count());
        let input = format!("{ignored}１１０１０５１９４９\n１２３１００２Ｘ");
        positive(&input, ignored.encode_utf16().count(), 19);
    }
    positive("1101051949%0A1231002X", 0, 21);
    positive("1101051949%0D%0A1231002X", 0, 24);
    // Existing hex/base64 decoders map to their entire token.
    positive("313130313035313934390a3132333130303258", 0, 38);
    positive("MTEwMTA1MTk0OQoxMjMxMDAyWA==", 0, 28);
}

#[test]
fn invalid_checksum_structure_and_boundaries_do_not_gain_ids() {
    for input in [
        "1101051949\n12310021",
        "1101011990\n0307737X",
        "8301051990\n01011231",
        "8311001990\n01011230",
        "1101051949\n13310021",
        "1101051900\n02290021",
        "2 1101051949\n1231002X",
        "1101051949\n1231002X 2",
        "110105194912\n310003 2",
        "21101051949\n1231002X",
        "1101051949\n1231002X9",
        "1101051949\n1231002x9",
        "1101051949\n1231002X\t9",
        "1101051949\n1231002X -9",
        "1101051949    \n1231002X",
        "1101051949\n1231002X0",
        "1101051949\n1231002X 2\n3",
        "1110105194912\n310003",
        "\"1101051949\",\n\"1231002X\"",
        "1101051949\0\n1231002X",
        "year qty\n2023 1203\n2024 1305\n2025 1411\n2026 1520",
        "1\t197\t4567\n6\t770\t56489",
        "1 197 4567\n6 770 56489",
        "2023 12 03 7\n2024 13 05 9\n2025 14 11 8",
    ] {
        no_id(input);
    }
}

#[test]
fn exact_segment_and_gap_limits_without_two_line_cap() {
    let numeric = "110105194912310003"
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>();
    let limit = numeric.join(" \n-");
    assert_eq!(limit.encode_utf16().count(), 69);
    positive(&limit, 0, 69);
    no_id(&limit.replacen(" \n-", "  \n-", 1));
    let x = "11010519491231002"
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" \r\n")
        + "X";
    assert_eq!(x.encode_utf16().count(), 66);
    positive(&x, 0, 66);
    no_id(&x.replacen(" \r\n", "  \r\n", 1));
    no_id(&(limit.clone() + " 9"));
    positive(&(limit + "\n9"), 0, 69);
}

#[test]
fn crossline_does_not_classify_other_numeric_kinds() {
    for input in [
        "138\n1234 5678",
        "1350\n2210\n980",
        "6222 0212\n3456 7894",
        "11010549\n1231002",
    ] {
        let response = inspect(input, &context(false));
        assert!(response.findings.is_empty(), "{input:?}: {response:?}");
        assert_eq!(response.action, InspectAction::Allow);
    }
}

#[test]
fn all_four_old_same_line_cards_remain_blocked() {
    for input in [
        "card 6222 0212 3456 7894\n2. thanks",
        "6222021234567894\n2024",
        "card 6222 0212 3456 7894\r\n1",
        "card 6222 0212 3456 7894 \n1",
    ] {
        let response = inspect(input, &context(true));
        assert_eq!(response.action, InspectAction::Block, "{input:?}");
        assert!(
            response
                .findings
                .iter()
                .any(|f| f.kind == FindingKind::BankCard)
        );
    }
}

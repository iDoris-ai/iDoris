use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{FindingKind, InspectAction, InspectResponse, Sensitivity};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn response(text: &str, consent: bool) -> InspectResponse {
    inspect(
        text,
        &InspectCtx {
            declared: PrivacyClass::Any,
            remote_allowed: true,
            redact_consent: consent,
            max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
        },
    )
}
fn has(r: &InspectResponse, start: u64, length: u64) -> bool {
    r.findings.iter().any(|f| {
        f.kind == FindingKind::CnMobile
            && f.span
                .is_some_and(|s| s.start == start && s.length == length)
    })
}
#[test]
fn country_positive_layouts_and_response_policy() {
    for (text, length) in [
        ("008613812345678", 15),
        ("0086 13812345678", 16),
        ("0086 138 12345678", 17),
        ("0086 138 1234 5678", 18),
    ] {
        for consent in [true, false] {
            let r = response(text, consent);
            assert!(has(&r, 0, length), "full country span: {text:?}: {r:?}");
            assert_eq!(r.sensitivity, Sensitivity::S2);
            assert_eq!(
                r.action,
                if consent {
                    InspectAction::Redact
                } else {
                    InspectAction::Block
                }
            );
        }
    }
}
#[test]
fn country_separators_and_utf16_mapping() {
    for sep in [" ", "-", "\t", ".", "/", "　", "–"] {
        for parts in [
            vec!["0086", "13812345678"],
            vec!["0086", "138", "12345678"],
            vec!["0086", "138", "1234", "5678"],
        ] {
            let body = parts.join(sep);
            let text = format!("🔒卡 {body}");
            let r = response(&text, true);
            assert!(
                has(&r, 4, body.encode_utf16().count() as u64),
                "UTF16 start and span: {text:?}: {r:?}"
            );
        }
    }
    for (text, start, length) in [
        ("🔒卡 008613812345678", 4, 15),
        ("🔒卡 0086 138\u{e0100} 1234 5678", 4, 20),
        ("0086 138\u{200b} 1234 5678", 0, 19),
        ("００８６　１３８　１２３４　５６７８", 0, 18),
        ("7 0086 138 1234 5678 9", 2, 18),
        ("0086-138.1234/5678", 0, 18),
    ] {
        assert!(
            has(&response(text, true), start, length),
            "mapped full span: {text:?}"
        );
    }
}
#[test]
fn country_rejects_only_the_new_full_tuple() {
    for text in [
        "008513812345678",
        "018613812345678",
        "0008613812345678",
        "008612812345678",
        "008623812345678",
        "00861381234567",
        "0086138123456789",
        "9008613812345678",
        "0086 138 1234 56789",
        "0086 13 8123 45678",
        "0086 1381 2345 678",
        "0086  138 1234 5678",
        "0086_138_1234_5678",
        "0086,138,1234,5678",
        "0086a138a1234a5678",
        "0086\n138 1234 5678",
        "0086\r\n138 1234 5678",
        "00086 138 1234 5678",
        "0086 138 1234 56789",
        "0086 1 3 8 1 2 3 4 5 6 7 8",
    ] {
        let r = response(text, true);
        assert!(
            !has(&r, 0, text.encode_utf16().count() as u64),
            "no illicit full country tuple: {text:?}: {r:?}"
        );
    }
}
#[test]
fn country_preserves_formatted_inner_and_taiwan() {
    for text in [
        "0086 138 1234 5678",
        "0086.138.1234.5678",
        "0086/138/1234/5678",
        "0086\t138\t1234\t5678",
    ] {
        let r = response(text, true);
        assert!(has(&r, 5, 13), "retain old inner span: {text:?}: {r:?}");
        assert!(has(&r, 0, 18), "new outer span: {text:?}: {r:?}");
    }
    for (text, sensitivity) in [
        ("830000199001011234", Sensitivity::S3),
        ("830000199001011235", Sensitivity::S2),
    ] {
        let r = response(text, true);
        assert_eq!(r.sensitivity, sensitivity);
        assert!(
            r.findings
                .iter()
                .any(|f| f.kind == FindingKind::CnResidentId)
        );
    }
}
#[test]
fn country_decoded_maps() {
    for (text, start, length) in [
        ("303038363133383132333435363738", 0, 30),
        ("MDA4NjEzODEyMzQ1Njc4", 0, 20),
        ("%30%30%38%36%31%33%38%31%32%33%34%35%36%37%38", 0, 45),
    ] {
        let r = response(text, true);
        assert!(
            has(&r, start, length),
            "decoded country map: {text:?}: {r:?}"
        );
        assert_eq!(r.sensitivity, Sensitivity::S2);
    }
}

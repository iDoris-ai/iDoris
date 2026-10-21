use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{InspectAction, InspectResponse};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn context() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn assert_parity(plain: &InspectResponse, encoded: &str) {
    let result = inspect(encoded, &context());
    assert_eq!(result.action, plain.action, "{encoded}: {result:?}");
    assert_eq!(
        result.sensitivity, plain.sensitivity,
        "{encoded}: {result:?}"
    );
    for finding in &plain.findings {
        assert!(
            result.findings.iter().any(|f| f.kind == finding.kind),
            "{encoded}: {result:?}"
        );
    }
}

#[test]
fn encoded_entities_keep_all_plain_normalization_views() {
    for (text, base64) in [
        ("１11010519491231002X", "77yRMTEwMTA1MTk0OTEyMzEwMDJY"),
        (
            "ａsk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "772Bc2stYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYQ==",
        ),
        (
            "Ｋｅｙsk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "77yr772F772Zc2stYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYQ==",
        ),
        ("１13812345678", "77yRMTM4MTIzNDU2Nzg="),
        ("13812345678９", "MTM4MTIzNDU2NzjvvJk="),
        ("a@b.com‐9", "YUBiLmNvbeKAkDk="),
        ("a@b.com９", "YUBiLmNvbe+8mQ=="),
    ] {
        let plain = inspect(text, &context());
        assert_ne!(plain.action, InspectAction::Allow, "control {text}");
        let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
        let percent: String = text.bytes().map(|b| format!("%{b:02X}")).collect();
        for encoded in [base64, &hex, &percent] {
            assert_parity(&plain, encoded);
        }
    }
}

#[test]
fn percent_decoding_preserves_entity_separators_and_original_spans() {
    for (plain, encoded) in [
        ("110105 19491231 002X", "110105 19491231 002%58"),
        ("110105 19491231 002X", "110105 %319491231 002X"),
        ("6222 0212 3456 7894", "6222 0212 3456 789%34"),
        ("A123456(3)", "A123456(%33)"),
        ("a@b.com", "a@b%2Ecom"),
        ("138 1234 5678", "138 1234 567%38"),
    ] {
        let control = inspect(plain, &context());
        assert_ne!(control.action, InspectAction::Allow, "control {plain}");
        assert_parity(&control, encoded);
        let wrapped = format!("😀 note: {encoded} end");
        let result = inspect(&wrapped, &context());
        let Some(span) = result
            .findings
            .iter()
            .find(|f| f.kind == control.findings[0].kind)
            .and_then(|f| f.span)
        else {
            panic!("missing finding: {result:?}");
        };
        let units: Vec<u16> = wrapped.encode_utf16().collect();
        let covered = String::from_utf16_lossy(
            &units[span.start as usize..(span.start + span.length) as usize],
        );
        assert_eq!(covered, encoded, "{wrapped}: {result:?}");
    }
}

#[test]
fn decoded_malformed_hkid_does_not_gain_folded_coverage() {
    for text in ["A123456（3)", "A123456　(3)"] {
        let expected = inspect(text, &context());
        assert_eq!(expected.action, InspectAction::Allow);
        let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
        assert_parity(&expected, &hex);
    }
}

//! Written layouts from PR #531 on the current scanner.
//!
//! Positives already matched on `050478d` through whole-group digits or
//! `formatted_window`. This test keeps that recall, and keeps the table rows
//! public: `3-3-5` and `4-4-3` are not phones.

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{FindingKind, InspectAction, InspectResponse};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn open_ctx() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn response(input: &str) -> InspectResponse {
    inspect(input, &open_ctx())
}

#[test]
fn written_layouts_are_found_and_table_rows_stay_public() {
    for (input, kind) in [
        ("138-12345678", FindingKind::CnMobile),
        ("1381 2345678", FindingKind::CnMobile),
        ("110105 19491231002X", FindingKind::CnResidentId),
        ("110105 194912310021", FindingKind::CnResidentId),
        ("622202 1234567890128", FindingKind::BankCard),
        ("622202 123456789012", FindingKind::BankCard),
        ("6222 0212 3456 7890 128", FindingKind::BankCard),
        ("2200123456789019", FindingKind::BankCard),
        ("2200 1234 5678 9019", FindingKind::BankCard),
        ("9792123456789018", FindingKind::BankCard),
        ("6222 0212 3456 7894", FindingKind::BankCard),
        ("电话：138 123 45678", FindingKind::CnMobile),
        ("手机 138-123-45678", FindingKind::CnMobile),
        ("请联系 1391 2345 678", FindingKind::CnMobile),
        ("我的号码是138 123 45678，加微信", FindingKind::CnMobile),
    ] {
        let response = response(input);
        assert!(
            response.findings.iter().any(|finding| finding.kind == kind),
            "{input:?} must be {kind:?}: {response:?}"
        );
    }

    for input in [
        "138 123 45678",
        "2023 1203 2024",
        "1 2 3 4 5 6 7 8",
        "1998 2005 100",
        "1949 1978 200",
        "共 1500 2000 300 元",
        "年份 1998 2005 100 1999 2006 200",
        "139 000 00000",
        "1500 2000 301",
    ] {
        let response = response(input);
        assert_eq!(
            response.action,
            InspectAction::Allow,
            "{input:?} must stay public: {response:?}"
        );
        assert!(response.findings.is_empty(), "{input:?}: {response:?}");
    }
}

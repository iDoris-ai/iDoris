//! Digit-script and Latin-homoglyph folds from PR #448, as a fourth scan view.
//!
//! The raw view stays in the union. A Tamil zero or a Cyrillic neighbour must
//! not erase a finding that view already has.

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{FindingKind, InspectFinding, InspectResponse};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn open_ctx() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn covered(input: &str, finding: &InspectFinding) -> Option<String> {
    let span = finding.span?;
    let units: Vec<u16> = input.encode_utf16().collect();
    let start = usize::try_from(span.start).ok()?;
    let length = usize::try_from(span.length).ok()?;
    let end = start.checked_add(length)?;
    String::from_utf16(units.get(start..end)?).ok()
}

fn covers(input: &str, response: &InspectResponse, kind: FindingKind, text: &str) -> bool {
    response
        .findings
        .iter()
        .any(|finding| finding.kind == kind && covered(input, finding).as_deref() == Some(text))
}

#[test]
fn decimal_digit_and_latin_homoglyph_fold_to_findings() {
    // Thai digits for 13812345678. The raw view has no ASCII run here.
    let thai = "๑๓๘๑๒๓๔๕๖๗๘";
    let thai_response = inspect(thai, &open_ctx());
    assert!(
        covers(thai, &thai_response, FindingKind::CnMobile, thai),
        "{thai_response:?}"
    );

    // Cyrillic small letter dze (U+0455) stands in for `s` in `sk-`.
    let key = format!("\u{0455}k-{}", "a".repeat(20));
    let key_response = inspect(&key, &open_ctx());
    assert!(
        covers(&key, &key_response, FindingKind::ApiKey, &key),
        "{key_response:?}"
    );
}

#[test]
fn raw_view_secret_survives_beside_non_latin_text() {
    let secret = "AKIA5HVJV2XAZM2TT6A8";
    // `а` folds to `a` and would glue onto the key if this were the only view.
    let input = format!("доступа{secret}");
    let response = inspect(&input, &open_ctx());
    assert!(
        covers(&input, &response, FindingKind::ApiKey, secret),
        "{response:?}"
    );
}

#[test]
fn tamil_zero_does_not_erase_a_following_resident_id() {
    let id = "11010519491231002X";
    let input = format!("\u{0BE6}{id}");
    let response = inspect(&input, &open_ctx());
    assert!(
        covers(&input, &response, FindingKind::CnResidentId, id),
        "{response:?}"
    );
}

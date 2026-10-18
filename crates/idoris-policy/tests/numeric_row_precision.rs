use idoris_contracts::{common::PrivacyClass, inspect::InspectAction};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

#[test]
fn deterministic_four_digit_rows_keep_precision() {
    let ctx = InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    };
    let mut seed = 0x45220261018u64;
    for width in [8, 20] {
        let mut flagged = 0;
        for _ in 0..5000 {
            let row = (0..width)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (1000 + ((seed >> 32) % 9000)).to_string()
                })
                .collect::<Vec<_>>()
                .join(" ");
            flagged += usize::from(inspect(&row, &ctx).action != InspectAction::Allow);
        }
        println!("{width} four-digit columns: {flagged}/5000 flagged");
        // This distribution cannot form standard phone/ID shapes; cards are
        // allowed only by complete decomposition. Keep a bounded FP budget.
        assert!(flagged <= if width == 8 { 50 } else { 5 });
    }
}

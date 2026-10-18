use idoris_contracts::{common::PrivacyClass, inspect::InspectAction};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

#[test]
fn multiline_precision_covers_single_narrow_wide_and_csv_shapes() {
    let ctx = InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    };
    type Shape<'a> = (&'a str, &'a [(u64, u64)], &'a str);
    let shapes: &[Shape<'_>] = &[
        ("single_1_9999", &[(1, 9999)], " "),
        ("single_10_999", &[(10, 999)], " "),
        ("single_1_99999", &[(1, 99999)], " "),
        ("narrow_tab", &[(1, 99), (1, 9999)], "\t"),
        ("narrow_space", &[(1, 999), (1, 999)], " "),
        ("three_tab", &[(1, 12), (1, 999), (1, 99999)], "\t"),
        ("three_space", &[(1, 12), (1, 999), (1, 99999)], " "),
        ("three_csv", &[(1, 12), (1, 999), (1, 99999)], ","),
    ];
    let mut seed = 0x44520261018u64;
    for &(name, columns, separator) in shapes {
        for rows in [5, 12, 50] {
            let mut flagged = 0;
            for _ in 0..200 {
                let text = (0..rows)
                    .map(|_| {
                        columns
                            .iter()
                            .map(|&(min, max)| {
                                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                                (min + ((seed >> 32) % (max - min + 1))).to_string()
                            })
                            .collect::<Vec<_>>()
                            .join(separator)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                flagged += usize::from(inspect(&text, &ctx).action != InspectAction::Allow);
            }
            println!("{name}, rows={rows}: {flagged}/200");
            // Each row has <11 digits. Cross-line checksum-valid IDs may
            // coincidentally occur; cap the documented residual at 1% here.
            assert!(flagged <= 2, "{name}, rows={rows}: {flagged}/200");
        }
    }
}

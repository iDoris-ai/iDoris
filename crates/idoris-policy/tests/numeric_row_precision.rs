use idoris_contracts::{common::PrivacyClass, inspect::InspectAction};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn ctx() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

#[test]
fn deterministic_four_digit_rows_keep_precision() {
    let ctx = ctx();
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

fn next_value(seed: &mut u64, dist: &str) -> u64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    let r = *seed >> 32;
    match dist {
        "mixed_3_4_digit" => {
            if r.is_multiple_of(2) {
                100 + (r % 900)
            } else {
                1000 + (r % 9000)
            }
        }
        "range_1_2500" => 1 + (r % 2500),
        "range_100_9999" => 100 + (r % 9900),
        "range_1000_9999" => 1000 + (r % 9000),
        _ => unreachable!("unknown distribution {dist}"),
    }
}

// Exercise both separator classes and their normalized Unicode equivalents.
const SEPARATORS: [(&str, &str); 4] = [
    (" ", "ascii_space"),
    ("-", "ascii_hyphen"),
    ("\u{2013}", "unicode_en_dash"),
    ("\u{3000}", "unicode_ideographic_space"),
];

fn gen_row(seed: &mut u64, width: usize, dist: &str, sep: &str) -> String {
    (0..width)
        .map(|_| next_value(seed, dist).to_string())
        .collect::<Vec<_>>()
        .join(sep)
}

// These deterministic fixture rates describe each distribution separately;
// they are not population estimates or evidence of zero false positives.
#[test]
fn mixed_shape_rows_precision_matrix() {
    let ctx = ctx();
    let dists_and_bounds: [(&str, usize, usize); 4] = [
        // (distribution, bound at width=8, bound at width=20)
        ("mixed_3_4_digit", 309, 800),
        ("range_1_2500", 344, 861),
        ("range_100_9999", 195, 479),
        ("range_1000_9999", 15, 0),
    ];
    for (dist, bound8, bound20) in dists_and_bounds {
        for (width, bound) in [(8usize, bound8), (20usize, bound20)] {
            for (sep, sep_name) in SEPARATORS {
                let mut seed = 0x45220261018u64;
                let mut flagged = 0usize;
                for _ in 0..5000 {
                    let row = gen_row(&mut seed, width, dist, sep);
                    flagged += usize::from(inspect(&row, &ctx).action != InspectAction::Allow);
                }
                println!("{dist} width={width} sep={sep_name}: {flagged}/5000 flagged");
                assert_eq!(
                    flagged, bound,
                    "{dist} width={width} sep={sep_name}: expected exactly {bound}/5000 \
                     (pinned); a lower count is a real precision improvement (update the \
                     pin), a higher count is a regression"
                );
            }
        }
    }
}

#[test]
fn standard_shape_recall_matrix() {
    use idoris_contracts::inspect::FindingKind;

    let fixtures = [
        ("Room 1203 138 1234 5678", 1, 0),
        ("qty 3 138 1234 5678", 1, 0),
        ("138 1234 5678 1", 1, 0),
        ("138 1234 5678 1 139 8765 4321", 2, 0),
        ("Room 1203 138 1234 5678 1", 1, 0),
        ("qty 3 138 1234 5678 139 8765 4321 2", 2, 0),
        ("1203 138 1234 5678 1", 1, 0),
        ("3 138 1234 5678 139 8765 4321 2", 2, 0),
        ("Room 1203 110101 19491231 0023", 0, 1),
        ("Room 1203 110105 19491231 002X", 0, 1),
        ("qty 3 110105 19491231 002X 9", 0, 1),
        ("1203 110101 19491231 0023 1", 0, 1),
    ];
    for (sep, name) in SEPARATORS {
        let mut mobiles = 0;
        let mut ids = 0;
        for (input, expected_mobiles, expected_ids) in fixtures {
            let input = input.replace(' ', sep);
            let response = inspect(&input, &ctx());
            let count = |kind| response.findings.iter().filter(|f| f.kind == kind).count();
            assert_eq!(count(FindingKind::CnMobile), expected_mobiles, "{input}");
            assert_eq!(count(FindingKind::CnResidentId), expected_ids, "{input}");
            mobiles += expected_mobiles;
            ids += expected_ids;
        }
        println!("recall sep={name}: 12/12 fixtures, mobiles={mobiles}/11 ids={ids}/4");
    }
    // A group may mix separators rather than consistently use just one.
    let mixed = "Room\u{3000}1203-138\u{2013}1234 5678-1";
    assert_eq!(inspect(mixed, &ctx()).findings.len(), 1);
    println!("recall mixed separators: 1/1 fixture, mobiles=1/1");
}

#[test]
fn numeric_row_false_positives_remain_an_explicit_cost() {
    for input in [
        "10 160 1920 1080 60",
        "2024 150 1200 3000 17",
        "100 200 135 1500 2500 300",
        "4 130 1024 2048 1",
    ] {
        for (sep, _) in SEPARATORS {
            let input = input.replace(' ', sep);
            assert_eq!(
                inspect(&input, &ctx()).action,
                InspectAction::Redact,
                "{input}"
            );
            let no_consent = InspectCtx {
                redact_consent: false,
                ..ctx()
            };
            assert_eq!(
                inspect(&input, &no_consent).action,
                InspectAction::Block,
                "{input}"
            );
        }
    }
}

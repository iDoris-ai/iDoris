#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{FindingKind, InspectFinding};
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

fn ctx() -> InspectCtx {
    InspectCtx {
        declared: PrivacyClass::Any,
        remote_allowed: true,
        redact_consent: true,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    }
}

fn luhn_sum(digits: &[u8]) -> u32 {
    digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &digit)| {
            let digit = u32::from(digit);
            match (i % 2 == 1, digit * 2) {
                (false, _) => digit,
                (true, doubled) if doubled > 9 => doubled - 9,
                (true, doubled) => doubled,
            }
        })
        .sum()
}

fn luhn_valid(digits: &[u8]) -> bool {
    luhn_sum(digits).is_multiple_of(10)
}

/// Check digit that makes `body || [check]` pass Luhn. `body` itself is not
/// required to be valid.
fn check_digit(body: &[u8]) -> u8 {
    let mut with_zero = body.to_vec();
    with_zero.push(0);
    let sum = luhn_sum(&with_zero);
    ((10 - (sum % 10)) % 10) as u8
}

fn parse_digits(text: &str) -> Vec<u8> {
    text.bytes().map(|b| b - b'0').collect()
}

/// Extend `body` by one Luhn check digit.
fn extend(body: &[u8]) -> Vec<u8> {
    let mut out = body.to_vec();
    out.push(check_digit(body));
    out
}

fn format_groups(digits: &[u8], groups: &[usize], sep: &str) -> String {
    let mut parts = Vec::new();
    let mut at = 0;
    for &len in groups {
        parts.push(
            digits[at..at + len]
                .iter()
                .map(|d| char::from(b'0' + d))
                .collect::<String>(),
        );
        at += len;
    }
    assert_eq!(at, digits.len());
    parts.join(sep)
}

fn covered(input: &str, finding: &InspectFinding) -> String {
    let span = finding.span.expect("finding span");
    let units: Vec<u16> = input.encode_utf16().collect();
    let start = usize::try_from(span.start).unwrap();
    let end = start + usize::try_from(span.length).unwrap();
    String::from_utf16(&units[start..end]).unwrap()
}

fn cards(input: &str) -> Vec<String> {
    inspect(input, &ctx())
        .findings
        .iter()
        .filter(|finding| finding.kind == FindingKind::BankCard)
        .map(|finding| covered(input, finding))
        .collect()
}

fn mobiles(input: &str) -> usize {
    inspect(input, &ctx())
        .findings
        .iter()
        .filter(|finding| finding.kind == FindingKind::CnMobile)
        .count()
}

fn assert_one_card(input: &str, expected: &str) {
    let found = cards(input);
    assert_eq!(found, vec![expected.to_string()], "{input}");
}

#[test]
fn generator_matches_known_luhn_cards_and_builds_nested_prefixes() {
    let visa16 = parse_digits("4111111111111111");
    let union16 = parse_digits("6222021234567894");
    let union19 = parse_digits("6214830000000000007");
    assert!(luhn_valid(&visa16) && luhn_valid(&union16) && luhn_valid(&union19));
    assert_eq!(extend(&visa16[..15]), visa16);
    assert_eq!(extend(&union16[..15]), union16);
    assert_eq!(extend(&union19[..18]), union19);

    // Each added digit is the check digit of the number so far, so every
    // prefix length from 16 through 19 is independently Luhn-valid.
    let body15 = parse_digits("622202123456789");
    let d16 = extend(&body15);
    let d17 = extend(&d16);
    let d18 = extend(&d17);
    let d19 = extend(&d18);
    assert_eq!(d16, union16);
    for digits in [&d16, &d17, &d18, &d19] {
        assert!(luhn_valid(digits), "{digits:?}");
    }
    // The check digit that extends the published 16-digit card is 5, so the
    // generated 17-digit number is the published shortest-window fixture.
    // 18 and 19 are further extensions that fixture does not cover.
    let fixture17 = parse_digits("62220212345678945");
    assert_eq!(d17, fixture17);
    assert_ne!(d18, fixture17);
    assert_ne!(d19, fixture17);
}

#[test]
fn longest_written_window_keeps_full_span_for_supported_lengths() {
    let body15 = parse_digits("622202123456789");
    let d16 = extend(&body15);
    let d17 = extend(&d16);
    let d18 = extend(&d17);
    let d19 = extend(&d18);
    let prefix16 = format_groups(&d16, &[4, 4, 4, 4], " ");

    for sep in [" ", "-", "\u{2013}", "\u{3000}"] {
        assert_one_card(
            &format_groups(&d16, &[4, 4, 4, 4], sep),
            &format_groups(&d16, &[4, 4, 4, 4], sep),
        );
        let g17 = format_groups(&d17, &[4, 4, 4, 4, 1], sep);
        assert_one_card(&g17, &g17);
        assert_ne!(cards(&g17), vec![prefix16.replace(' ', sep)]);
        let g18 = format_groups(&d18, &[4, 4, 4, 4, 2], sep);
        assert_one_card(&g18, &g18);
        let g19 = format_groups(&d19, &[4, 4, 4, 4, 3], sep);
        assert_one_card(&g19, &g19);
        assert_one_card(
            &format_groups(&d19, &[6, 13], sep),
            &format_groups(&d19, &[6, 13], sep),
        );
    }

    for digits in [&d16, &d17, &d18, &d19] {
        let text: String = digits.iter().map(|d| char::from(b'0' + d)).collect();
        assert_one_card(&text, &text);
    }

    // 13/14/15 are real Luhn-valid IIN-eligible lengths. This scanner's card
    // gate is 16..=19, same as published #452. Not recalling them is the
    // current contract, not a longest-window regression.
    let mut short = parse_digits("411111111111");
    for len in [13, 14, 15] {
        short = extend(&short);
        assert_eq!(short.len(), len);
        assert!(luhn_valid(&short), "{len}");
        let text: String = short.iter().map(|d| char::from(b'0' + d)).collect();
        assert!(
            cards(&text).is_empty(),
            "unsupported length {len} recalled {text}"
        );
        let grouped = if len == 13 {
            format_groups(&short, &[4, 4, 5], " ")
        } else if len == 14 {
            format_groups(&short, &[4, 6, 4], " ")
        } else {
            format_groups(&short, &[4, 4, 4, 3], " ")
        };
        assert!(
            cards(&grouped).is_empty(),
            "unsupported grouped length {len} recalled {grouped}"
        );
    }
}

#[test]
fn invalid_longer_window_falls_back_to_the_shorter_valid_card() {
    let d16 = extend(&parse_digits("622202123456789"));
    // Three extra digits that are not the valid 17/18/19 check chain.
    let mut invalid19 = d16.clone();
    invalid19.extend_from_slice(&[0, 0, 1]);
    assert!(luhn_valid(&d16));
    assert!(!luhn_valid(&invalid19));
    let input = format_groups(&invalid19, &[4, 4, 4, 4, 3], " ");
    let expected = format_groups(&d16, &[4, 4, 4, 4], " ");
    assert_one_card(&input, &expected);
    let hyphen = format_groups(&invalid19, &[4, 4, 4, 4, 3], "-");
    assert_one_card(&hyphen, &format_groups(&d16, &[4, 4, 4, 4], "-"));
}

#[test]
fn source_span_starts_after_a_label_and_uses_utf16_units() {
    let d19 = {
        let d16 = extend(&parse_digits("622202123456789"));
        let d17 = extend(&d16);
        let d18 = extend(&d17);
        extend(&d18)
    };
    let card = format_groups(&d19, &[4, 4, 4, 4, 3], " ");
    let ascii = format!("label {card}");
    assert_one_card(&ascii, &card);
    let cjk = format!("卡 {card}");
    assert_one_card(&cjk, &card);
    let response = inspect(&cjk, &ctx());
    let span = response.findings[0].span.expect("span");
    // "卡" is one UTF-16 unit, then one space.
    assert_eq!(span.start, 2, "{cjk}");
    assert_eq!(span.length as usize, card.encode_utf16().count());
}

#[test]
fn numeric_prefix_guard_drops_supported_cards_and_keeps_phone_rescue() {
    let d16 = extend(&parse_digits("622202123456789"));
    let card_space = format_groups(&d16, &[4, 4, 4, 4], " ");
    let card_hyphen = format_groups(&d16, &[4, 4, 4, 4], "-");

    // Supported card form after 1 or 2 short unmatched runs. Isolated #453
    // skipped those runs and recalled the card. This aggregate does not.
    // That is a named recall loss relative to 597, and it matches the
    // parent rule that card windows are not a clutter exception.
    for input in [
        format!("7 {card_space}"),
        format!("7-{card_hyphen}"),
        format!("7 8 {card_space}"),
        format!("12-28-{card_hyphen}"),
        format!("1203 {card_space}"),
    ] {
        assert!(
            cards(&input).is_empty(),
            "numeric-prefix guard recalled a card: {input} -> {:?}",
            cards(&input)
        );
    }

    // The same short prefix must not erase a standard phone shape.
    assert_eq!(mobiles("7 138 1234 5678"), 1);
    assert_eq!(mobiles("7-138-1234-5678"), 1);
    assert_eq!(mobiles("12 28 138 1234 5678"), 1);

    // Parent negative: this split is not a supported phone shape, even
    // though skipping "99" would expose the supported grouping 1381 234 5678.
    assert_eq!(mobiles("99 1381 234 5678"), 0);

    // Published parent negative for cards: a short neighbour on both sides
    // stays Allow. Requiring decomposition from run 0 preserves it.
    let both = format!("1 {card_space} 2");
    assert!(cards(&both).is_empty(), "{both}");
    assert_eq!(
        inspect(&both, &ctx()).action,
        idoris_contracts::inspect::InspectAction::Allow
    );
}

#[test]
fn same_class_adjacent_and_disjoint_cards_keep_both_spans() {
    let first = "6222 0212 3456 7894 5";
    let second = "4111 1111 1111 1111";
    for separator in [" ", "; ", "\n"] {
        let input = format!("{first}{separator}{second}");
        assert_eq!(cards(&input), [first, second], "{input}");
    }
}

#[test]
fn supplementary_label_uses_two_utf16_units() {
    let card = "6222 0212 3456 7894 120";
    let input = format!("🔒卡 {card}");
    assert_one_card(&input, card);
    let response = inspect(&input, &ctx());
    let span = response.findings[0].span.unwrap();
    assert_eq!(span.start, 4);
    assert_eq!(span.length as usize, card.encode_utf16().count());
    assert_eq!(
        response.sensitivity,
        idoris_contracts::inspect::Sensitivity::S3
    );
}

#[test]
fn measured_parent_rows_retain_phone_and_card_recall() {
    for sep in [" ", "-", "\u{2013}", "\u{3000}"] {
        let phone_row = "1871 490 2709 3537 216 4257 708 7703".replace(' ', sep);
        let response = inspect(&phone_row, &ctx());
        assert!(
            response.findings.iter().any(|f| {
                f.kind == FindingKind::CnMobile
                    && covered(&phone_row, f) == "1871 490 2709".replace(' ', sep)
            }),
            "{phone_row}: {response:?}"
        );
        let card_row = "5543 2200 1209 3651 1425 8214 121 4846".replace(' ', sep);
        assert!(
            cards(&card_row).contains(&"5543 2200 1209 3651".replace(' ', sep)),
            "{card_row}"
        );
    }
}

#[test]
fn all_24_reported_phone_rows_keep_parent_mobile_classification() {
    // Some rows contain multiple eligible windows. Preserve the parent's
    // decomposition semantics, rather than rescuing every possible window.
    for row in [
        "4495 436 6025 980 3991 1339 654 5517",
        "1871 490 2709 3537 216 4257 708 7703",
        "1931 558 4113 2677 582 6595 976 1071",
        "1357 1503 732 3117 600 820 584 8597",
        "1501 657 4609 3948 3424 463 2613 1561",
        "351 1592 650 1865 1609 1756 1533 750",
        "2475 1912 2099 166 707 1710 230 2383",
        "343 973 2425 1316 240 1428 1965 372",
        "1562 945 1742 2304 97 2337 407 1415",
        "1993 618 1990 2315 600 1860 1208 1320",
        "1817 679 1446 39 1816 473 1223 1910",
        "317 1928 374 1182 1391 1591 1023 145",
        "348 1351 430 645 563 1652 811 1355",
        "1976 857 1949 594 667 1672 2314 485",
        "320 1579 308 1231 983 1503 1460 328",
        "681 634 1327 1859 741 1352 974 1018",
        "551 1112 1399 711 1635 1746 611 1159",
        "570 579 567 799 1475 1777 418 1098",
        "1663 300 1834 60 1846 2189 681 421",
        "1976 328 1140 419 46 1927 1389 2327",
        "1644 381 1521 2404 2138 954 1359 717",
        "1596 576 2146 62 1178 2050 1680 2476",
        "596 2066 320 1419 2388 1691 916 1646",
        "666 1382 987 1226 801 1380 1489 899",
    ] {
        for sep in [" ", "-", "\u{2013}", "\u{3000}"] {
            let row = row.replace(' ', sep);
            assert!(
                inspect(&row, &ctx())
                    .findings
                    .iter()
                    .any(|f| { f.kind == FindingKind::CnMobile }),
                "{row}: lost parent mobile classification"
            );
        }
    }
}

#[test]
fn prototype_only_rows_remain_parent_negatives() {
    for row in [
        "1534 559 2303 1983 672 2287 725 578",
        "1411 215 2165 1679 450 1706 745 16",
    ] {
        assert_eq!(
            inspect(row, &ctx()).action,
            idoris_contracts::inspect::InspectAction::Allow
        );
    }
}

#[test]
fn longer_card_alternative_requires_a_valid_remaining_decomposition() {
    // The parent chooses two 16-digit cards. A valid 19-digit alternative
    // consumes 302 and leaves the valid country-prefixed phone 8613812345678.
    // Retain the second parent card too: it overlaps but is not contained.
    let row = "6222 0212 3456 7894 302 861 3812 345678";
    assert_eq!(
        cards(row),
        ["6222 0212 3456 7894 302", "302 861 3812 345678"]
    );
}

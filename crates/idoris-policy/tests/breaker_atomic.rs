#![allow(clippy::unwrap_used)]
use idoris_policy::breaker::{
    Admission, AdmissionTicket, Breaker, BreakerBook, CredentialClass, parse_retry_after_ms,
};

fn take(b: &mut Breaker, now: u64) -> AdmissionTicket {
    b.admit(now).ticket.unwrap()
}
fn open(class: CredentialClass) -> Breaker {
    let mut b = Breaker::new(class);
    for _ in 0..class.failure_threshold() {
        let t = take(&mut b, 0);
        b.on_failure(t, 0);
    }
    b
}
fn rejected(b: &mut Breaker, now: u64, remaining: u64) {
    assert_eq!(
        b.admit(now).admission,
        Admission::Rejected {
            retry_after_ms: remaining
        }
    );
}
#[test]
fn atomic_threshold_and_probe_headers_preserve_full_window() {
    for class in [CredentialClass::Local, CredentialClass::ApiKey] {
        for header in [0, 1000] {
            let mut b = Breaker::new(class);
            for _ in 1..class.failure_threshold() {
                let t = take(&mut b, 0);
                b.on_failure(t, 0);
            }
            let t = take(&mut b, 0);
            b.on_failure_with_retry_after(t, 0, Some(header));
            rejected(&mut b, 1000, class.recovery_ms() - 1000);
            let t = take(&mut b, class.recovery_ms());
            b.on_failure_with_retry_after(t, class.recovery_ms(), Some(header));
            rejected(&mut b, class.recovery_ms(), class.recovery_ms());
        }
    }
}
#[test]
fn below_threshold_zero_short_and_long_headers() {
    for ms in [0, 1000, 120_000] {
        let mut b = Breaker::new(CredentialClass::Local);
        let t = take(&mut b, 0);
        b.on_failure_with_retry_after(t, 0, Some(ms));
        if ms > 0 {
            rejected(&mut b, ms - 1, 1);
        }
        assert_eq!(b.admit(ms).admission, Admission::Probe);
    }
    let mut b = open(CredentialClass::Local);
    let t = take(&mut b, 15_000);
    b.on_failure_with_retry_after(t, 15_000, Some(120_000));
    rejected(&mut b, 15_000, 120_000);
}
#[test]
fn late_cohort_extends_without_shortening() {
    let mut b = Breaker::new(CredentialClass::Local);
    let late = take(&mut b, 0);
    let short = take(&mut b, 0);
    for _ in 0..2 {
        let t = take(&mut b, 0);
        b.on_failure(t, 0);
    }
    b.on_failure_with_retry_after(late, 1000, Some(120_000));
    b.on_failure_with_retry_after(short, 2000, Some(1));
    rejected(&mut b, 15_000, 106_000);
}
#[test]
fn old_nonprobe_cannot_settle_halfopen_or_recovered_generation() {
    for recover in [false, true] {
        let mut b = Breaker::new(CredentialClass::Local);
        let old = take(&mut b, 0);
        for _ in 0..2 {
            let t = take(&mut b, 0);
            b.on_failure(t, 0);
        }
        let probe = take(&mut b, 15_000);
        if recover {
            b.on_success(probe);
        }
        b.on_failure_with_retry_after(old, 15_000, Some(u64::MAX));
        if recover {
            assert_eq!(b.admit(15_000).admission, Admission::Allowed);
        } else {
            rejected(&mut b, 15_000, 0);
        }
    }
}
#[test]
fn combined_method_rejects_foreign_instances_and_clones() {
    for clone in [false, true] {
        let mut owner = Breaker::new(CredentialClass::Local);
        let mut foreign = if clone {
            owner.clone()
        } else {
            Breaker::new(CredentialClass::Local)
        };
        let t = take(&mut owner, 0);
        foreign.on_failure_with_retry_after(t, 0, Some(u64::MAX));
        assert_eq!(foreign.admit(0).admission, Admission::Allowed);
        let t = take(&mut foreign, 0);
        owner.on_failure_with_retry_after(t, 0, Some(u64::MAX));
        assert_eq!(owner.admit(0).admission, Admission::Allowed);
    }
}
#[test]
fn combined_book_rejects_foreign_books_keys_classes_and_missing_entries() {
    let class = CredentialClass::Local;
    let mut a = BreakerBook::new();
    let mut b = BreakerBook::new();
    for (key, c) in [("a", class), ("b", class), ("a", CredentialClass::ApiKey)] {
        let _ = a.admit(&key, c, 0);
    }
    let t = a.admit(&"a", class, 0).ticket.unwrap();
    b.on_failure_with_retry_after(&"a", class, t, 0, Some(u64::MAX));
    assert_eq!(b.admit(&"a", class, 0).admission, Admission::Allowed);
    for (key, c) in [
        ("b", class),
        ("a", CredentialClass::ApiKey),
        ("missing", class),
    ] {
        let t = a.admit(&"a", class, 0).ticket.unwrap();
        a.on_failure_with_retry_after(&key, c, t, 0, Some(u64::MAX));
        assert_eq!(a.admit(&key, c, 0).admission, Admission::Allowed);
    }
    let t = b.admit(&"a", class, 0).ticket.unwrap();
    a.on_failure_with_retry_after(&"a", class, t, 0, Some(u64::MAX));
    assert_eq!(a.admit(&"a", class, 0).admission, Admission::Allowed);
}
#[test]
fn strict_ascii_saturating_delta_seconds_and_uncapped_core() {
    for raw in [
        "+7",
        "+0",
        "+0007",
        "-0",
        "",
        "1 0",
        "1.0",
        "７",
        "Wed, 21 Oct 2015 07:28:00 GMT",
    ] {
        assert_eq!(parse_retry_after_ms(raw), None, "{raw}");
    }
    for (raw, ms) in [
        (" 0007 ", 7000),
        ("0", 0),
        ("301", 301_000),
        ("18446744073709552", u64::MAX),
        ("18446744073709551616", u64::MAX),
    ] {
        assert_eq!(parse_retry_after_ms(raw), Some(ms));
    }
    assert_eq!(parse_retry_after_ms(&"9".repeat(1000)), Some(u64::MAX));
    let mut b = Breaker::new(CredentialClass::Local);
    let t = take(&mut b, 0);
    b.on_retry_after(t, 0, 900_000);
    let late = b.admit(900_000).ticket.unwrap();
    b.on_failure_with_retry_after(late, 900_000, Some(300_000));
    rejected(&mut b, 900_000, 300_000);
}

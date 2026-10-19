#![allow(clippy::unwrap_used)]

use idoris_policy::breaker::{
    Admission, AdmissionTicket, Admit, Breaker, BreakerBook, CredentialClass, PairReject,
};

fn ticket(admit: Admit) -> AdmissionTicket {
    assert!(admit.admitted());
    admit.ticket.unwrap()
}

fn trip<'a>(book: &mut BreakerBook<&'a str>, key: &&'a str, class: CredentialClass, now: u64) {
    for _ in 0..class.failure_threshold() {
        let owned = ticket(book.admit(key, class, now));
        book.on_failure(key, class, owned, now);
    }
}

fn parity(breaker: &mut Breaker, now: u64, expected: Admission) -> Admit {
    for _ in 0..4 {
        assert_eq!(breaker.readiness(now), expected);
    }
    let admit = breaker.admit(now);
    assert_eq!(admit.admission, expected);
    assert_eq!(admit.ticket.is_some(), expected.admitted());
    admit
}

#[test]
fn readiness_matches_admit_across_states_classes_and_boundaries() {
    for class in [
        CredentialClass::Local,
        CredentialClass::ApiKey,
        CredentialClass::OAuth,
    ] {
        let mut b = Breaker::new(class);
        for _ in 0..class.failure_threshold() {
            let owned = ticket(parity(&mut b, 10, Admission::Allowed));
            b.on_failure(owned, 10);
        }
        let end = 10 + class.recovery_ms();
        let _ = parity(&mut b, end - 1, Admission::Rejected { retry_after_ms: 1 });
        let probe = ticket(parity(&mut b, end, Admission::Probe));
        let _ = parity(&mut b, end + 1, Admission::Rejected { retry_after_ms: 0 });
        b.on_failure(probe, end + 1);
        let probe = ticket(parity(
            &mut b,
            end + 1 + class.recovery_ms(),
            Admission::Probe,
        ));
        b.on_success(probe);
        let _ = parity(&mut b, u64::MAX, Admission::Allowed);
    }
}

#[test]
fn repeated_absent_book_readiness_allows_a_real_pair() {
    let mut book = BreakerBook::new();
    let class = CredentialClass::Local;
    for _ in 0..10 {
        assert_eq!(book.readiness(&"a", class, 0), Admission::Allowed);
    }
    let [a, b] = book.admit_pair((&"a", class), (&"b", class), 0).unwrap();
    assert_eq!([a.admission, b.admission], [Admission::Allowed; 2]);
    book.on_failure(&"a", class, ticket(a), 0);
    book.on_success(&"b", class, ticket(b));
    let owned = ticket(book.admit(&"a", class, 0));
    book.on_failure(&"a", class, owned, 0);
    assert!(!book.readiness(&"a", class, 0).admitted());
    assert_eq!(book.readiness(&"b", class, 0), Admission::Allowed);
}

#[test]
fn successful_pair_claims_both_expired_probes_in_order() {
    let mut book = BreakerBook::new();
    let class = CredentialClass::Local;
    trip(&mut book, &"a", class, 0);
    trip(&mut book, &"b", class, 0);
    for _ in 0..4 {
        assert_eq!(book.readiness(&"a", class, 15_000), Admission::Probe);
    }
    let [a, b] = book
        .admit_pair((&"a", class), (&"b", class), 15_000)
        .unwrap();
    assert_eq!([a.admission, b.admission], [Admission::Probe; 2]);
    assert!(matches!(
        book.admit_pair((&"a", class), (&"b", class), 15_000),
        Err(PairReject::Gate { .. })
    ));
    book.on_success(&"a", class, ticket(a));
    book.on_failure(&"b", class, ticket(b), 15_000);
    assert_eq!(book.readiness(&"a", class, 15_000), Admission::Allowed);
    assert_eq!(
        book.readiness(&"b", class, 15_000),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
}

#[test]
fn either_rejection_order_preserves_the_other_expired_probe() {
    for reversed in [false, true] {
        let mut book = BreakerBook::new();
        let class = CredentialClass::Local;
        trip(&mut book, &"ready", class, 0);
        trip(&mut book, &"blocked", class, 1);
        let (first, second) = if reversed {
            ("blocked", "ready")
        } else {
            ("ready", "blocked")
        };
        let result = book.admit_pair((&first, class), (&second, class), 15_000);
        let Err(PairReject::Gate { first, second }) = result else {
            panic!("pair must refuse")
        };
        let expected = if reversed {
            [Admission::Rejected { retry_after_ms: 1 }, Admission::Probe]
        } else {
            [Admission::Probe, Admission::Rejected { retry_after_ms: 1 }]
        };
        assert_eq!([first, second], expected);
        let admit = book.admit(&"ready", class, 15_000);
        assert_eq!(admit.admission, Admission::Probe);
        book.on_success(&"ready", class, ticket(admit));
        assert_eq!(book.readiness(&"ready", class, 15_000), Admission::Allowed);
    }
}

#[test]
fn claimed_probe_refusal_preserves_its_ticket_and_other_gate() {
    let mut book = BreakerBook::new();
    let class = CredentialClass::Local;
    trip(&mut book, &"a", class, 0);
    trip(&mut book, &"b", class, 0);
    let probe = ticket(book.admit(&"a", class, 15_000));
    assert!(matches!(
        book.admit_pair((&"a", class), (&"b", class), 15_000),
        Err(PairReject::Gate {
            first: Admission::Rejected { retry_after_ms: 0 },
            second: Admission::Probe
        })
    ));
    book.on_success(&"a", class, probe);
    let [a, b] = book
        .admit_pair((&"a", class), (&"b", class), 15_000)
        .unwrap();
    assert_eq!(
        [a.admission, b.admission],
        [Admission::Allowed, Admission::Probe]
    );
    book.on_success(&"a", class, ticket(a));
    book.on_success(&"b", class, ticket(b));
}

#[test]
fn exact_duplicate_refuses_before_probe_claim_but_classes_are_distinct() {
    let mut book = BreakerBook::new();
    let class = CredentialClass::Local;
    assert!(matches!(
        book.admit_pair((&"missing", class), (&"missing", class), 0),
        Err(PairReject::Duplicate)
    ));
    trip(&mut book, &"same", class, 0);
    assert!(matches!(
        book.admit_pair((&"same", class), (&"same", class), 15_000),
        Err(PairReject::Duplicate)
    ));
    let [local, api] = book
        .admit_pair((&"same", class), (&"same", CredentialClass::ApiKey), 15_000)
        .unwrap();
    assert_eq!(
        [local.admission, api.admission],
        [Admission::Probe, Admission::Allowed]
    );
    book.on_success(&"same", class, ticket(local));
    book.on_retry_after(
        &"same",
        CredentialClass::ApiKey,
        ticket(api),
        15_000,
        50_000,
    );
    assert_eq!(book.readiness(&"same", class, 15_000), Admission::Allowed);
    assert_eq!(
        book.readiness(&"same", CredentialClass::ApiKey, 15_000),
        Admission::Rejected {
            retry_after_ms: 50_000
        }
    );
}

#[test]
fn outstanding_normal_ticket_still_settles_after_pair() {
    let mut book = BreakerBook::new();
    let class = CredentialClass::Local;
    let early = ticket(book.admit(&"a", class, 0));
    let [a, b] = book.admit_pair((&"a", class), (&"b", class), 0).unwrap();
    book.on_failure(&"a", class, ticket(a), 0);
    book.on_success(&"a", class, early);
    let owned = ticket(book.admit(&"a", class, 0));
    book.on_failure(&"a", class, owned, 0);
    assert_eq!(book.readiness(&"a", class, 0), Admission::Allowed);
    book.on_failure(&"b", class, ticket(b), 0);
    assert_eq!(book.readiness(&"b", class, 0), Admission::Allowed);
}

#[test]
fn pair_preserves_existing_open_cohort_extension_authority() {
    let mut book = BreakerBook::new();
    let class = CredentialClass::Local;
    let early = ticket(book.admit(&"a", class, 0));
    let [a, b] = book.admit_pair((&"a", class), (&"b", class), 0).unwrap();
    book.on_retry_after(&"a", class, ticket(a), 0, 20_000);
    assert!(matches!(
        book.admit_pair((&"a", class), (&"b", class), 0),
        Err(PairReject::Gate { .. })
    ));
    book.on_retry_after(&"a", class, early, 1, 80_000);
    assert_eq!(
        book.readiness(&"a", class, 20_000),
        Admission::Rejected {
            retry_after_ms: 60_001
        }
    );
    book.on_success(&"b", class, ticket(b));
}

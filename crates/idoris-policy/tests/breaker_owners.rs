//! Tickets settle only the instance that issued them, through public APIs.
#![allow(clippy::unwrap_used)]

use idoris_policy::breaker::{Admission, AdmissionTicket, Breaker, BreakerBook, CredentialClass};

const CLASS: CredentialClass = CredentialClass::Local;

fn ticket(breaker: &mut Breaker) -> AdmissionTicket {
    breaker.admit(0).ticket.unwrap()
}

fn fail(breaker: &mut Breaker) {
    let own = ticket(breaker);
    breaker.on_failure(own, 0);
}

fn report(breaker: &mut Breaker, foreign: AdmissionTicket, outcome: u8) {
    match outcome {
        0 => breaker.on_success(foreign),
        1 => breaker.on_failure(foreign, 0),
        2 => breaker.on_retry_after(foreign, 0, 120_000),
        _ => unreachable!(),
    }
}

fn check_instances(make: impl Fn() -> (Breaker, Breaker)) {
    for outcome in 0..3 {
        let (mut owner, mut foreign) = make();
        // Success must not erase a real failure; failure must not count;
        // Retry-After must not open the target.
        if outcome == 0 {
            fail(&mut owner);
        }
        report(&mut owner, ticket(&mut foreign), outcome);
        if outcome != 0 {
            assert_eq!(owner.admit(0).admission, Admission::Allowed);
        }
        fail(&mut owner);
        if outcome == 0 {
            assert_eq!(
                owner.admit(0).admission,
                Admission::Rejected {
                    retry_after_ms: 15_000
                }
            );
        } else {
            assert_eq!(owner.admit(0).admission, Admission::Allowed);
        }
    }
}

#[test]
fn standalone_instances_reject_all_foreign_outcomes() {
    check_instances(|| (Breaker::new(CLASS), Breaker::new(CLASS)));
}

#[test]
fn clones_have_independent_ticket_ownership_in_both_directions() {
    check_instances(|| {
        let original = Breaker::new(CLASS);
        let cloned = original.clone();
        (original, cloned)
    });
    check_instances(|| {
        let original = Breaker::new(CLASS);
        (original.clone(), original)
    });
}

#[test]
fn books_keys_and_classes_reject_all_foreign_outcomes() {
    for route in 0..3 {
        for outcome in 0..3 {
            let mut book = BreakerBook::new();
            let mut other = BreakerBook::new();
            let key = "target";
            let own = book.admit(&key, CLASS, 0).ticket.unwrap();
            if outcome == 0 {
                book.on_failure(&key, CLASS, own, 0);
            }
            let foreign = match route {
                0 => other.admit(&key, CLASS, 0),
                1 => book.admit(&"other-key", CLASS, 0),
                2 => book.admit(&key, CredentialClass::ApiKey, 0),
                _ => unreachable!(),
            }
            .ticket
            .unwrap();
            match outcome {
                0 => book.on_success(&key, CLASS, foreign),
                1 => book.on_failure(&key, CLASS, foreign, 0),
                2 => book.on_retry_after(&key, CLASS, foreign, 0, 120_000),
                _ => unreachable!(),
            }
            if outcome != 0 {
                assert_eq!(book.admit(&key, CLASS, 0).admission, Admission::Allowed);
            }
            let own = book.admit(&key, CLASS, 0).ticket.unwrap();
            book.on_failure(&key, CLASS, own, 0);
            let expected = if outcome == 0 {
                Admission::Rejected {
                    retry_after_ms: 15_000,
                }
            } else {
                Admission::Allowed
            };
            assert_eq!(
                book.admit(&key, CLASS, 0).admission,
                expected,
                "route={route}, outcome={outcome}"
            );
        }
    }
}

#[test]
fn foreign_probes_cannot_settle_owner_probe_but_owner_can() {
    for outcome in 0..3 {
        let mut owner = Breaker::new(CLASS);
        fail(&mut owner);
        fail(&mut owner);
        let mut foreign = owner.clone();
        let own = owner.admit(15_000).ticket.unwrap();
        let alien = foreign.admit(15_000).ticket.unwrap();
        report(&mut owner, alien, outcome);
        assert_eq!(
            owner.admit(15_000).admission,
            Admission::Rejected { retry_after_ms: 0 }
        );
        owner.on_success(own);
        assert_eq!(owner.admit(15_000).admission, Admission::Allowed);
    }
}

#[test]
fn only_owner_cohort_can_extend_open_window() {
    let mut owner = Breaker::new(CLASS);
    let own_late = ticket(&mut owner);
    let mut cloned = owner.clone();
    let foreign_late = ticket(&mut cloned);
    let opener = ticket(&mut owner);
    owner.on_retry_after(opener, 0, 30_000);
    owner.on_retry_after(foreign_late, 0, 120_000);
    assert_eq!(
        owner.admit(0).admission,
        Admission::Rejected {
            retry_after_ms: 30_000
        }
    );
    owner.on_retry_after(own_late, 0, 60_000);
    assert_eq!(
        owner.admit(0).admission,
        Admission::Rejected {
            retry_after_ms: 60_000
        }
    );
}

#[test]
fn clone_retains_state_and_accepts_its_own_new_tickets() {
    let mut original = Breaker::new(CLASS);
    fail(&mut original);
    let mut cloned = original.clone();
    fail(&mut cloned);
    assert_eq!(
        cloned.admit(0).admission,
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    assert_eq!(original.admit(0).admission, Admission::Allowed);
    let mut open_clone = cloned.clone();
    assert_eq!(
        open_clone.admit(14_999).admission,
        Admission::Rejected { retry_after_ms: 1 }
    );
    let probe = open_clone.admit(15_000).ticket.unwrap();
    open_clone.on_retry_after(probe, 15_000, 1);
    assert_eq!(
        open_clone.admit(15_000).admission,
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
}

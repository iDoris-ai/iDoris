//! Public state transitions for one-shot breaker admission tickets.
//!
//! Reports move an [`AdmissionTicket`]. A ticket from an older generation
//! cannot close `Retry-After`, cannot reopen a recovered breaker, and cannot
//! finish a half-open probe. `BreakerBook` keys on credential class as well
//! as the caller's key.

use idoris_policy::breaker::{
    Admission, AdmissionTicket, Admit, Breaker, BreakerBook, CredentialClass,
};

fn ticket_of(admit: Admit) -> AdmissionTicket {
    assert!(admit.admitted(), "{:?}", admit.admission);
    let Some(ticket) = admit.ticket else {
        panic!("admitted request must carry a ticket");
    };
    ticket
}

fn retry_after_ms(admit: Admit) -> u64 {
    assert!(
        admit.ticket.is_none(),
        "rejected admit must not issue a ticket"
    );
    match admit.admission {
        Admission::Rejected { retry_after_ms } => retry_after_ms,
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn preopen_success_cannot_cancel_120s_retry_after() {
    let mut breaker = Breaker::new(CredentialClass::ApiKey);
    let early = ticket_of(breaker.admit(0));
    let opener = ticket_of(breaker.admit(0));
    breaker.on_retry_after(opener, 0, 120_000);
    breaker.on_success(early);
    assert_eq!(retry_after_ms(breaker.admit(0)), 120_000);
    assert_eq!(retry_after_ms(breaker.admit(119_999)), 1);
    assert_eq!(breaker.admit(120_000).admission, Admission::Probe);
}

#[test]
fn preopen_failure_after_successful_probe_cannot_reopen() {
    let mut breaker = Breaker::new(CredentialClass::Local);
    let early = ticket_of(breaker.admit(0));
    for _ in 0..CredentialClass::Local.failure_threshold() {
        let ticket = ticket_of(breaker.admit(0));
        breaker.on_failure(ticket, 0);
    }
    assert_eq!(retry_after_ms(breaker.admit(0)), 15_000);
    let admit = breaker.admit(15_000);
    assert_eq!(admit.admission, Admission::Probe);
    breaker.on_success(ticket_of(admit));
    breaker.on_failure(early, 15_000);
    let (real_admission, real) = {
        let admit = breaker.admit(15_000);
        (admit.admission, ticket_of(admit))
    };
    assert_eq!(real_admission, Admission::Allowed);
    breaker.on_failure(real, 15_000);
    assert_eq!(breaker.admit(15_000).admission, Admission::Allowed);
}

#[test]
fn halfopen_second_admit_rejected_and_nonprobe_success_leaves_halfopen() {
    let mut breaker = Breaker::new(CredentialClass::Local);
    let early = ticket_of(breaker.admit(0));
    for _ in 0..CredentialClass::Local.failure_threshold() {
        let ticket = ticket_of(breaker.admit(0));
        breaker.on_failure(ticket, 0);
    }
    let admit = breaker.admit(15_000);
    assert_eq!(admit.admission, Admission::Probe);
    let probe = ticket_of(admit);
    assert_eq!(retry_after_ms(breaker.admit(15_000)), 0);
    breaker.on_success(early);
    assert_eq!(retry_after_ms(breaker.admit(15_001)), 0);
    breaker.on_success(probe);
    assert_eq!(breaker.admit(15_002).admission, Admission::Allowed);
}

#[test]
fn normal_cycle_works() {
    let mut breaker = Breaker::new(CredentialClass::Local);
    let first = breaker.admit(0);
    assert_eq!(first.admission, Admission::Allowed);
    breaker.on_failure(ticket_of(first), 0);
    let second = breaker.admit(0);
    assert_eq!(second.admission, Admission::Allowed);
    breaker.on_failure(ticket_of(second), 0);
    assert_eq!(retry_after_ms(breaker.admit(0)), 15_000);
    let probe = breaker.admit(15_000);
    assert_eq!(probe.admission, Admission::Probe);
    breaker.on_success(ticket_of(probe));
    assert_eq!(breaker.admit(15_000).admission, Admission::Allowed);
}

#[test]
fn cross_class_same_key_does_not_share_a_breaker() {
    let mut book: BreakerBook<&str> = BreakerBook::new();
    let key = "shared";
    for _ in 0..CredentialClass::OAuth.failure_threshold() {
        let ticket = ticket_of(book.admit(&key, CredentialClass::OAuth, 0));
        book.on_failure(&key, CredentialClass::OAuth, ticket, 0);
    }
    assert_eq!(
        retry_after_ms(book.admit(&key, CredentialClass::OAuth, 0)),
        CredentialClass::OAuth.recovery_ms()
    );
    let api = book.admit(&key, CredentialClass::ApiKey, 0);
    assert_eq!(api.admission, Admission::Allowed);
    book.on_failure(&key, CredentialClass::ApiKey, ticket_of(api), 0);
    assert!(book.admit(&key, CredentialClass::ApiKey, 0).admitted());
    assert_eq!(
        retry_after_ms(book.admit(&key, CredentialClass::OAuth, 0)),
        CredentialClass::OAuth.recovery_ms()
    );
}

#[test]
fn an_abandoned_probe_frees_the_slot_without_a_new_cooldown() {
    let mut breaker = Breaker::new(CredentialClass::Local);
    // 打开断路器，等冷却过去。
    for _ in 0..CredentialClass::Local.failure_threshold() {
        let ticket = ticket_of(breaker.admit(0));
        breaker.on_failure(ticket, 0);
    }
    let after = CredentialClass::Local.recovery_ms();
    let probe = ticket_of(breaker.admit(after));
    // 探测名额被占着：别的请求被拒。
    assert_eq!(retry_after_ms(breaker.admit(after)), 0);
    // 探测没有发出去：名额还回来，马上可以再探测（没有新的冷却）。
    breaker.on_abandon(probe);
    assert_eq!(breaker.admit(after).admission, Admission::Probe);
}

#[test]
fn abandoning_a_closed_ticket_or_a_stale_one_changes_nothing() {
    let mut breaker = Breaker::new(CredentialClass::Local);
    let closed = ticket_of(breaker.admit(0));
    breaker.on_abandon(closed);
    assert!(breaker.admit(0).admitted());
    // 别的实例的票据不动这个断路器。
    let mut other = Breaker::new(CredentialClass::Local);
    for _ in 0..CredentialClass::Local.failure_threshold() {
        let ticket = ticket_of(other.admit(0));
        other.on_failure(ticket, 0);
    }
    let after = CredentialClass::Local.recovery_ms();
    let foreign = ticket_of(other.admit(after));
    let mut third = Breaker::new(CredentialClass::Local);
    for _ in 0..CredentialClass::Local.failure_threshold() {
        let ticket = ticket_of(third.admit(0));
        third.on_failure(ticket, 0);
    }
    let _mine = ticket_of(third.admit(after));
    third.on_abandon(foreign);
    assert_eq!(
        retry_after_ms(third.admit(after)),
        0,
        "the probe slot is still taken"
    );
}

//! Local dispatch: `idoris_policy::decide()` → (if needed) `Supervisor`
//! load → `Supervisor` chat. This crate's decision pipeline (R2-B) and
//! model-management Supervisor (R2-A) are both Rust-native additions with
//! no 1:1 TS "dispatch.ts" to port against for the actual local-execution
//! steps — only the response-header contract (interface spec §3.12) is
//! shared ground truth, and that's applied by the axum handler in `lib.rs`
//! that calls `dispatch_local`, not here (this module has no axum/HTTP
//! dependency on purpose, so it's testable without spinning up the app).

use idoris_backend::{BackendError, ChatMessage, ChatRequest, ChatResponse, SupervisorHandle};
use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::Locality;
use idoris_policy::{
    AdmissionStatus, Card, Decision, PolicyCtx, ROLES, ReasonCode, Rejection, RequestProfile,
    decide, effective_served_locality,
};
use idoris_tenancy::budget::{BudgetError, BudgetLedger, ReservationId};
use tokio_util::sync::CancellationToken;

use crate::budget;
use crate::profile::ParsedProfile;

/// Fallback for a card that doesn't declare its own `load_policy` — cards
/// should normally declare one (interface spec §3.3); this only covers one
/// that omits it, so a missing field doesn't turn into a panic/`expect`.
fn default_load_policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
        admission: Admission::Coexist,
    }
}

/// A single, fixed placeholder memory size for every card's Supervisor
/// "model" entry. This crate doesn't yet have a real per-model memory
/// figure at the component-card layer (that's catalog/idoris-recommender
/// data, not wired into R2-D) — every local candidate is treated as
/// equally cheap to load for now.
const PLACEHOLDER_MEMORY_GB: f64 = 1.0;

/// R2-D simplification: every loaded component card is eligible for every
/// catalog role and always `Ready` at `decide()` time — role→catalog
/// mapping isn't wired yet, and real admission state is only known after
/// asking the Supervisor (post-selection, see [`dispatch_local`]).
/// `estimated_cost_minor` comes from [`budget::estimate_cost_minor`].
fn candidate(component: &ComponentCard, prompt: &str) -> Card {
    Card {
        component: component.clone(),
        roles: ROLES
            .iter()
            .copied()
            .filter(|r| r.is_catalog_role())
            .collect(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: budget::estimate_cost_minor(&component.provider.cost, prompt),
        admission_status: AdmissionStatus::Ready,
    }
}

/// A local backend failure, or a budget-ledger failure gating a *paid*
/// candidate — both only occur after a candidate was already chosen.
#[derive(Debug)]
pub enum DispatchFailure {
    Backend(BackendError),
    Budget(BudgetError),
}

/// What [`dispatch_local`] returns on a successful `decide()`.
/// `served_locality` is set even when `result` is `Err` (interface spec
/// §3.12). `actual_cost_minor` is `Some` only after a successful `settle`
/// on a paid candidate — `None` for free/failed or durably queued settlement.
#[derive(Debug)]
pub struct ChatOutcome {
    pub decision: Decision,
    pub served_locality: Locality,
    pub result: Result<ChatResponse, DispatchFailure>,
    pub actual_cost_minor: Option<i64>,
}

#[derive(Debug)]
pub enum DispatchError {
    /// `decide()` rejected the request before any candidate was chosen —
    /// no backend was contacted, no `X-iDoris-Served-Locality` applies.
    Rejection(Rejection),
    /// Defensive-only: `decide()` returned a `chosen_id` absent from the
    /// candidate slice it was itself given. Should be unreachable by
    /// construction; kept as a typed error instead of `expect()` so a
    /// latent bug fails closed with a 500, never a panic.
    Internal(String),
}

/// What [`select`] returns: everything a caller needs to route between the
/// two request-execution paths (R2-G) *before* committing to either one.
#[derive(Debug)]
pub struct Selected {
    pub decision: Decision,
    pub served_locality: Locality,
    pub card: ComponentCard,
    pub load_policy: LoadPolicy,
    pub estimated_cost_minor: i64,
}

/// Runs `decide()` and resolves the chosen candidate — the same selection
/// step [`dispatch_local`] runs internally, factored out so a caller can
/// inspect *which* candidate would be used without committing to the
/// Supervisor execution path. R2-G: a chosen candidate whose
/// `load_policy.mode` is [`LoadMode::Resident`] and whose `form` is
/// `http_service` is a generic OpenAI-compatible backend (including a
/// conformance fixture pointing at a fake upstream) — the caller forwards
/// to it directly via `proxy::ChatProxy` instead of calling
/// [`dispatch_local`], which always goes through the Supervisor and is only
/// correct for a real oMLX-shaped backend (`LoadMode::OnDemand`/
/// `EvictToLoad`, needing an explicit load/unload lifecycle a plain HTTP
/// passthrough backend has no equivalent of — see `proxy.rs`'s module doc).
///
/// Known, accepted duplication: [`dispatch_local`] calls `decide()` again
/// internally rather than taking a pre-computed [`Selected`] — avoiding a
/// larger signature change to an already width-tested function for this PR.
/// `decide()` is a small, pure, side-effect-free function over an in-memory
/// candidate list; the caller (`lib.rs`'s `chat_completions`) calls this
/// twice only on the Supervisor-path branch, never on the (more latency-
/// sensitive, real-network-call) proxy-path branch.
pub fn select(
    cards: &[ComponentCard],
    profile: &ParsedProfile,
    prompt: &str,
) -> Result<Selected, DispatchError> {
    let candidates: Vec<Card> = cards.iter().map(|c| candidate(c, prompt)).collect();
    let request_profile = RequestProfile {
        task: profile.task.clone(),
        role: profile.role,
        tenant_id: profile.tenant_id.clone(),
        content_tightening: None,
    };
    let ctx = PolicyCtx {
        min_ram_gb: None,
        budget: None,
    };
    let decision = decide(&request_profile, &candidates, &ctx).map_err(DispatchError::Rejection)?;
    let Some(chosen) = candidates.iter().find(|c| c.id() == decision.chosen_id) else {
        return Err(DispatchError::Internal(format!(
            "decide() returned chosen_id {:?} absent from its own candidate list",
            decision.chosen_id
        )));
    };
    let served_locality = effective_served_locality(chosen);
    let load_policy = chosen
        .component
        .load_policy
        .unwrap_or_else(default_load_policy);
    let estimated_cost_minor = chosen.estimated_cost_minor.unwrap_or(0);
    Ok(Selected {
        decision,
        served_locality,
        card: chosen.component.clone(),
        load_policy,
        estimated_cost_minor,
    })
}

/// Whether `card` should be forwarded directly (R2-G's `proxy::ChatProxy`)
/// rather than dispatched through the Supervisor — see [`select`]'s doc.
pub fn is_resident_http_service(card: &ComponentCard) -> bool {
    card.form == idoris_contracts::component_card::Form::HttpService
        && card
            .load_policy
            .is_some_and(|lp| lp.mode == LoadMode::Resident)
}

/// RAII guard: on `Drop`, releases the reservation unless [`Self::take`]
/// already removed it. This covers two cases a scattering of explicit
/// `release()` calls at each early-`return` site cannot: an `Err` return
/// (the ordinary case) *and* this whole `async fn`'s future being dropped
/// mid-`.await` — a client disconnecting mid-request, or the task being
/// cancelled some other way. Rust's cancellation model is drop-based: no
/// code "after" an interrupted `.await` point ever runs, but every live
/// value's `Drop` impl still does, which is exactly what a reservation
/// leaking real budget on a lost connection needs (R0 finding: the TS
/// reference's `req.on("close")` cancellation listener never actually
/// fires, since by the time it's attached the request body — and with it,
/// that stream's own `close` — has already completed; this guard doesn't
/// depend on any such listener at all).
struct ReservationGuard<'a> {
    ledger: Option<&'a BudgetLedger>,
    tenant_id: Option<&'a str>,
    id: Option<ReservationId>,
}

impl ReservationGuard<'_> {
    /// Takes the id for settling — after this, `Drop` is a no-op.
    fn take(&mut self) -> Option<ReservationId> {
        self.id.take()
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if let (Some(ledger), Some(id)) = (self.ledger, self.id.take())
            && let Err(err) = budget::release(ledger, self.tenant_id, &id)
        {
            eprintln!(
                "budget reservation release deferred: reservation={} error={err}",
                id.0
            );
        }
    }
}

/// Cancels `token` on `Drop` — the other half of the same fix: propagating
/// "this whole future was dropped" into the `CancellationToken` passed to
/// the Supervisor/upstream call, so an adapter that itself honors
/// cancellation (per [`idoris_backend::RuntimeAdapter::chat`]'s contract)
/// can stop early. Cancelling an already-completed call's token is a
/// harmless no-op, so this needs no "disarm" — unlike [`ReservationGuard`],
/// there's no outcome where cancelling *after* a normal finish is wrong.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Runs the local decision + execution path for one request: builds
/// decision-time [`Card`]s from `cards` (R2-D simplification, see
/// `candidate`), calls [`idoris_policy::decide`], reserves budget for a
/// *paid* candidate (`budget_ledger: None` fails closed via
/// [`DispatchFailure::Budget`] exactly as an unconfigured ledger would —
/// free candidates are unaffected), loads the chosen model via the
/// Supervisor if not already loaded, calls `chat`, then settles (success)
/// or releases (failure) the reservation. `prompt` is the caller's own
/// concatenated message text, passed in rather than recomputed here so
/// there's one place deciding how "the prompt" is derived from `messages`.
/// `cancel` is caller-owned (e.g. tied to the HTTP request's lifetime) --
/// this function additionally cancels it if its own future is dropped
/// mid-`.await` (see `CancelOnDrop`), so a caller doesn't have to get
/// that part right itself to still get correct propagation.
pub async fn dispatch_local(
    cards: &[ComponentCard],
    supervisor: Option<&SupervisorHandle>,
    budget_ledger: Option<&BudgetLedger>,
    profile: &ParsedProfile,
    prompt: &str,
    messages: Vec<ChatMessage>,
    cancel: CancellationToken,
) -> Result<ChatOutcome, DispatchError> {
    let tenant_id = profile.tenant_id.as_deref();

    // Scoped so `candidates`/`ctx` (which holds a `PolicyCtx<'_>` — not
    // `Send` because `dyn BudgetView` isn't `Sync` — see its own doc) are
    // dropped before any `.await` below; otherwise the whole function's
    // future would stop being `Send`, which axum's `Handler` trait requires.
    let (decision, served_locality, model_id, load_policy, cost, estimated_cost_minor) = {
        let candidates: Vec<Card> = cards.iter().map(|c| candidate(c, prompt)).collect();
        let request_profile = RequestProfile {
            task: profile.task.clone(),
            role: profile.role,
            tenant_id: profile.tenant_id.clone(),
            content_tightening: None,
        };
        let ctx = PolicyCtx {
            min_ram_gb: None,
            budget: None,
        };
        let decision =
            decide(&request_profile, &candidates, &ctx).map_err(DispatchError::Rejection)?;

        let Some(chosen) = candidates.iter().find(|c| c.id() == decision.chosen_id) else {
            return Err(DispatchError::Internal(format!(
                "decide() returned chosen_id {:?} absent from its own candidate list",
                decision.chosen_id
            )));
        };
        let served_locality = effective_served_locality(chosen);
        let load_policy = chosen
            .component
            .load_policy
            .unwrap_or_else(default_load_policy);
        // decide()'s pricing stage already excluded any None/negative
        // estimate, so this is always Some(v >= 0); unwrap_or(0) is
        // defense in depth, not a path expected to actually trigger.
        let estimated_cost_minor = chosen.estimated_cost_minor.unwrap_or(0);
        (
            decision,
            served_locality,
            chosen.id().to_string(),
            load_policy,
            chosen.component.provider.cost,
            estimated_cost_minor,
        )
    };

    let is_paid = budget::is_paid(Some(estimated_cost_minor));
    let mut reservation_guard = ReservationGuard {
        ledger: budget_ledger,
        tenant_id,
        id: None,
    };
    if is_paid {
        match budget_ledger {
            None => {
                return Ok(ChatOutcome {
                    decision,
                    served_locality,
                    result: Err(DispatchFailure::Budget(budget::ledger_unavailable_error(
                        tenant_id, &model_id,
                    ))),
                    actual_cost_minor: None,
                });
            }
            Some(ledger) => {
                match budget::reserve(ledger, tenant_id, &model_id, estimated_cost_minor) {
                    Ok(id) => {
                        let durable_tenant = tenant_id.unwrap_or(budget::PERSONAL_TENANT_ID);
                        let begin_result = ledger.begin_settlement(durable_tenant, &id);
                        reservation_guard.id = Some(id);
                        if let Err(err) = begin_result {
                            return Ok(ChatOutcome {
                                decision,
                                served_locality,
                                result: Err(DispatchFailure::Budget(err)),
                                actual_cost_minor: None,
                            });
                        }
                    }
                    Err(err) => {
                        return Ok(ChatOutcome {
                            decision,
                            served_locality,
                            result: Err(DispatchFailure::Budget(err)),
                            actual_cost_minor: None,
                        });
                    }
                }
            }
        }
    }
    // From here on, `reservation_guard`'s Drop releases the reservation on
    // any early return *and* on this future being dropped mid-`.await`
    // (client disconnect) — see its doc. Only the success path below
    // disarms it (via `take`) to settle instead.

    let Some(supervisor) = supervisor else {
        return Ok(ChatOutcome {
            decision,
            served_locality,
            result: Err(DispatchFailure::Backend(
                BackendError::supervisor_unavailable(),
            )),
            actual_cost_minor: None,
        });
    };

    // Cancelled on Drop too (in addition to whatever the caller does with
    // its own clone of `cancel`), so this future being dropped mid-`.await`
    // propagates into the same token the Supervisor/adapter call below
    // receives (see CancelOnDrop's doc) — an adapter that itself honors
    // cancellation can stop early.
    let _cancel_guard = CancelOnDrop(cancel.clone());

    let status = supervisor.status().await;
    let already_loaded = matches!(&status, Ok(s) if s.loaded.iter().any(|m| m == &model_id));
    if !already_loaded
        && let Err(err) = supervisor
            .load(model_id.clone(), PLACEHOLDER_MEMORY_GB, load_policy)
            .await
    {
        return Ok(ChatOutcome {
            decision,
            served_locality,
            result: Err(DispatchFailure::Backend(err)),
            actual_cost_minor: None,
        });
    }

    let chat_result = supervisor
        .chat(
            ChatRequest {
                model: model_id,
                messages,
            },
            cancel,
        )
        .await;

    match chat_result {
        Err(err) => Ok(ChatOutcome {
            decision,
            served_locality,
            result: Err(DispatchFailure::Backend(err)),
            actual_cost_minor: None,
        }),
        Ok(response) => {
            // Disarm release: a completed call must never be refunded.
            // Busy/Storage after journaling keeps the successful response;
            // failure to persist the actual cost fails closed.
            let actual_cost_minor = match (budget_ledger, reservation_guard.take()) {
                (Some(ledger), Some(id)) => {
                    let actual = budget::estimate_actual_cost_minor(
                        &cost,
                        prompt,
                        &response.content,
                        estimated_cost_minor,
                    );
                    match budget::settle(ledger, tenant_id, &id, actual) {
                        Ok(charged) => charged,
                        Err(err) => {
                            return Ok(ChatOutcome {
                                decision,
                                served_locality,
                                result: Err(DispatchFailure::Budget(err)),
                                actual_cost_minor: None,
                            });
                        }
                    }
                }
                _ => None,
            };
            Ok(ChatOutcome {
                decision,
                served_locality,
                result: Ok(response),
                actual_cost_minor,
            })
        }
    }
}

/// Debug-formatted, comma-joined reason codes — observability-only, not
/// yet a formal wire contract (see `X-iDoris-Reason` in `lib.rs`).
pub fn reason_header_value(reasons: &[ReasonCode]) -> String {
    reasons
        .iter()
        .map(|r| format!("{r:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};
    use idoris_contracts::TaskProfile;
    use idoris_contracts::common::PrivacyClass;
    use idoris_contracts::common::Tier;
    use idoris_contracts::component_card::{Egress, Form};
    use idoris_contracts::provider::{Cost, Family, ProviderDescriptor};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    fn local_card(id: &str) -> ComponentCard {
        ComponentCard {
            provider: ProviderDescriptor {
                id: id.to_string(),
                family: Family::Local,
                tier: Tier::Local,
                capabilities: vec![idoris_contracts::common::Capability::Chat],
                privacy_class: PrivacyClass::LocalOnly,
                cost: Cost {
                    input_per_m: 0.0,
                    output_per_m: 0.0,
                },
                locality: Locality::Loopback,
                extensions: None,
            },
            form: Form::HttpService,
            endpoint: "http://127.0.0.1:8740".to_string(),
            version_pin: "0.0.0".to_string(),
            privacy_class: PrivacyClass::LocalOnly,
            allowed_egress: vec![Egress::Loopback],
            fallback_policy: idoris_contracts::common::FallbackPolicy::FailClosed,
            fail_closed: true,
            load_policy: None,
            extensions: None,
        }
    }

    fn empty_profile() -> ParsedProfile {
        ParsedProfile {
            task: TaskProfile::default(),
            role: None,
            tenant_id: None,
            intent_source: crate::profile::IntentSource::Default,
        }
    }

    #[tokio::test]
    async fn no_cards_rejects_as_local_only_unavailable() {
        let err = dispatch_local(
            &[],
            None,
            None,
            &empty_profile(),
            "",
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            DispatchError::Rejection(Rejection::LocalOnlyUnavailable)
        ));
    }

    #[tokio::test]
    async fn a_candidate_with_no_supervisor_reports_supervisor_unavailable() {
        let outcome = dispatch_local(
            &[local_card("a")],
            None,
            None,
            &empty_profile(),
            "",
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.served_locality, Locality::Loopback);
        match outcome.result.unwrap_err() {
            DispatchFailure::Backend(err) => {
                assert_eq!(err.reason_code(), "supervisor_unavailable")
            }
            other => panic!("expected a Backend failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn succeeds_end_to_end_against_a_mock_supervisor() {
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "a".to_string(),
            memory_gb: 1.0,
        }]));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }];
        let outcome = dispatch_local(
            &[local_card("a")],
            Some(&supervisor),
            None,
            &empty_profile(),
            "hello",
            messages,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.served_locality, Locality::Loopback);
        assert_eq!(outcome.actual_cost_minor, None); // free candidate: nothing to charge
        let response = outcome.result.unwrap();
        assert_eq!(response.model, "a");
        assert!(response.content.contains("hello"));
    }

    fn paid_card(id: &str) -> ComponentCard {
        let mut card = local_card(id);
        card.provider.cost = Cost {
            input_per_m: 1_000_000.0,
            output_per_m: 2_000_000.0,
        };
        card
    }

    // TempDir must outlive the BudgetLedger using its path.
    fn configured_ledger(limit_minor: i64) -> (tempfile::TempDir, BudgetLedger) {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger = BudgetLedger::open(dir.path().join("b.sqlite3")).unwrap();
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                limit_minor,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .unwrap();
        (dir, ledger)
    }

    #[derive(Default)]
    struct TestClock(AtomicI64);

    impl idoris_tenancy::budget::Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    // "No ledger wired" (trivial branch) is covered at the budget.rs unit
    // level; these focus on the two integration paths below.
    #[tokio::test]
    async fn paid_candidate_over_budget_is_rejected_and_nothing_is_charged() {
        let (_dir, ledger) = configured_ledger(1);
        let outcome = dispatch_local(
            &[paid_card("p")],
            None,
            Some(&ledger),
            &empty_profile(),
            "hi",
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        match outcome.result.unwrap_err() {
            DispatchFailure::Budget(BudgetError::Exceeded { .. }) => {}
            other => panic!("expected Budget(Exceeded), got {other:?}"),
        }
        assert_eq!(
            ledger.tenant_balance(budget::PERSONAL_TENANT_ID).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn paid_candidate_settles_the_actual_cost_on_success() {
        let (_dir, ledger) = configured_ledger(1_000_000);
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "p".to_string(),
            memory_gb: 1.0,
        }]));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let outcome = dispatch_local(
            &[paid_card("p")],
            Some(&supervisor),
            Some(&ledger),
            &empty_profile(),
            "hi",
            messages,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(outcome.result.is_ok());
        let charged = outcome.actual_cost_minor.unwrap();
        assert!(charged > 0);
        assert_eq!(
            ledger.tenant_balance(budget::PERSONAL_TENANT_ID).unwrap(),
            1_000_000 - charged
        );
    }

    #[tokio::test]
    async fn concurrent_paid_dispatches_succeed_for_same_and_different_tenants() {
        for (first_tenant, second_tenant) in [("acme", "acme"), ("acme", "other")] {
            let dir = tempfile::TempDir::new().unwrap();
            let ledger = BudgetLedger::open(dir.path().join("b.sqlite3")).unwrap();
            for tenant in ["acme", "other"] {
                ledger
                    .configure_tenant(
                        tenant,
                        1_000_000,
                        "UTC",
                        idoris_tenancy::budget::SpendGate::PaidOnly,
                    )
                    .unwrap();
            }
            let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
                id: "p".to_string(),
                memory_gb: 1.0,
            }]));
            adapter.set_chat_delay("p", std::time::Duration::from_millis(100));
            let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
            let cards = [paid_card("p")];
            let first_profile = ParsedProfile {
                tenant_id: Some(first_tenant.to_string()),
                ..empty_profile()
            };
            let second_profile = ParsedProfile {
                tenant_id: Some(second_tenant.to_string()),
                ..empty_profile()
            };
            let mut first = Box::pin(dispatch_local(
                &cards,
                Some(&supervisor),
                Some(&ledger),
                &first_profile,
                "hi",
                vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                CancellationToken::new(),
            ));
            std::future::poll_fn(|cx| {
                assert!(first.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            let mut second = Box::pin(dispatch_local(
                &cards,
                Some(&supervisor),
                Some(&ledger),
                &second_profile,
                "hi",
                vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                CancellationToken::new(),
            ));
            let second_poll =
                std::future::poll_fn(|cx| std::task::Poll::Ready(second.as_mut().poll(cx))).await;
            assert!(
                second_poll.is_pending(),
                "second paid request must reserve and reach upstream while the first is in flight; got {second_poll:?}"
            );

            let (first_result, second_result) = tokio::join!(first, second);
            let first_outcome = first_result.unwrap();
            let second_outcome = second_result.unwrap();
            assert!(first_outcome.result.is_ok());
            assert!(second_outcome.result.is_ok());
            let first_charge = first_outcome.actual_cost_minor.unwrap();
            let second_charge = second_outcome.actual_cost_minor.unwrap();
            assert!(first_charge > 0 && second_charge > 0);
            assert_eq!(
                ledger.tenant_balance(first_tenant).unwrap(),
                1_000_000
                    - first_charge
                    - if first_tenant == second_tenant {
                        second_charge
                    } else {
                        0
                    }
            );
            if first_tenant != second_tenant {
                assert_eq!(
                    ledger.tenant_balance(second_tenant).unwrap(),
                    1_000_000 - second_charge
                );
            }
        }
    }

    #[tokio::test]
    async fn failed_intent_prevents_upstream_dispatch_and_releases_reservation() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("b.sqlite3");
        let ledger =
            BudgetLedger::open_with(&path, Arc::new(idoris_tenancy::budget::SystemClock), 1)
                .unwrap();
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                1_000_000,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .unwrap();
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "p".to_string(),
            memory_gb: 1.0,
        }]));
        let supervisor = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let journal =
            rusqlite::Connection::open(path.with_added_extension("settlements.sqlite3")).unwrap();
        journal
            .execute_batch(
                "CREATE TRIGGER fail_intent BEFORE INSERT ON settlement_intents
                 BEGIN SELECT RAISE(ABORT, 'injected intent failure'); END;",
            )
            .unwrap();

        let outcome = dispatch_local(
            &[paid_card("p")],
            Some(&supervisor),
            Some(&ledger),
            &empty_profile(),
            "hi",
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(outcome.result, Err(DispatchFailure::Budget(_))));
        assert!(adapter.event_log().is_empty());
        let main = rusqlite::Connection::open(&path).unwrap();
        let reservations: (i64, i64) = main
            .query_row(
                "SELECT count(*), sum(status='released') FROM reservations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(reservations, (1, 1));
        assert_eq!(
            ledger.tenant_balance(budget::PERSONAL_TENANT_ID).unwrap(),
            1_000_000
        );
    }

    #[tokio::test]
    async fn busy_journal_preserves_intent_across_ttl_and_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("b.sqlite3");
        let clock = Arc::new(TestClock::default());
        let ledger = BudgetLedger::open_with_busy_timeout(
            &path,
            clock.clone(),
            1,
            std::time::Duration::ZERO,
        )
        .unwrap();
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                1_000_000,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .unwrap();
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "p".to_string(),
            memory_gb: 1.0,
        }]));
        adapter.set_chat_delay("p", std::time::Duration::from_millis(100));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let cards = [paid_card("p")];
        let profile = empty_profile();
        let mut call = Box::pin(dispatch_local(
            &cards,
            Some(&supervisor),
            Some(&ledger),
            &profile,
            "hi",
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            CancellationToken::new(),
        ));
        // First poll reserves budget, then yields to the Supervisor. Lock
        // only afterward so reserve succeeds but settle deterministically fails.
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let blocker =
            rusqlite::Connection::open(path.with_added_extension("settlements.sqlite3")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let outcome = call.await.unwrap();
        assert!(matches!(outcome.result, Err(DispatchFailure::Budget(_))));
        assert_eq!(outcome.actual_cost_minor, None);
        let main = rusqlite::Connection::open(&path).unwrap();
        let actual: i64 = main
            .query_row(
                "SELECT actual_cost_minor FROM reservations WHERE actual_cost_minor IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let expected_actual = budget::estimate_actual_cost_minor(
            &paid_card("p").provider.cost,
            "hi",
            "mock reply to: hi",
            0,
        );
        assert_eq!(actual, expected_actual);
        drop(main);

        // The reservation itself has expired, but the durable pre-call
        // intent still blocks another paid request after the journal write
        // failed. This also proves the first operation happened before the
        // upstream await: the lock was acquired only after the first poll.
        clock.0.store(2, Ordering::SeqCst);
        assert!(budget::reserve(&ledger, None, "p", 500).is_err());

        drop(ledger);
        // Restart while the journal is still locked. The durable intent and
        // fallback actual keep admission closed until recovery can finish.
        let restarted = BudgetLedger::open_with_busy_timeout(
            &path,
            clock.clone(),
            1,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert!(budget::reserve(&restarted, None, "p", 500).is_err());
        drop(restarted);
        blocker.execute_batch("ROLLBACK").unwrap();

        // With the journal available, retry can complete the settlement.
        // Reopening repeatedly must not apply the same charge twice, and
        // admission must resume once recovery has cleared the intent.
        for _ in 0..2 {
            let recovered = BudgetLedger::open_with(path.clone(), clock.clone(), 1).unwrap();
            let balance = recovered
                .tenant_balance(budget::PERSONAL_TENANT_ID)
                .unwrap();
            assert_eq!(balance, 1_000_000 - actual);
            let id = budget::reserve(&recovered, None, "p", 500).unwrap();
            budget::release(&recovered, None, &id).unwrap();
        }
    }

    #[tokio::test]
    async fn busy_journal_and_main_db_keep_durable_intent_fail_closed_after_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("b.sqlite3");
        let clock = Arc::new(TestClock::default());
        let ledger = BudgetLedger::open_with_busy_timeout(
            &path,
            clock.clone(),
            1,
            std::time::Duration::ZERO,
        )
        .unwrap();
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                1_000_000,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .unwrap();
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "p".to_string(),
            memory_gb: 1.0,
        }]));
        adapter.set_chat_delay("p", std::time::Duration::from_millis(20));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let cards = [paid_card("p")];
        let profile = empty_profile();
        let mut call = Box::pin(dispatch_local(
            &cards,
            Some(&supervisor),
            Some(&ledger),
            &profile,
            "hi",
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            CancellationToken::new(),
        ));
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let journal_lock =
            rusqlite::Connection::open(path.with_added_extension("settlements.sqlite3")).unwrap();
        journal_lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let main_lock = rusqlite::Connection::open(&path).unwrap();
        main_lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let outcome = call.await.unwrap();
        assert!(matches!(outcome.result, Err(DispatchFailure::Budget(_))));
        let main = rusqlite::Connection::open(&path).unwrap();
        let actuals: i64 = main
            .query_row(
                "SELECT count(*) FROM reservations WHERE actual_cost_minor IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actuals, 0);
        drop(main);

        // Neither durable outcome store accepted the amount. After TTL and
        // process restart, the intent alone must continue blocking spends.
        clock.0.store(2, Ordering::SeqCst);
        drop(ledger);
        main_lock.execute_batch("ROLLBACK").unwrap();
        let restarted = BudgetLedger::open_with_busy_timeout(
            &path,
            clock.clone(),
            1,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert!(budget::reserve(&restarted, None, "p", 500).is_err());
        drop(restarted);
        journal_lock.execute_batch("ROLLBACK").unwrap();
        let recovered = BudgetLedger::open_with(path, clock, 1).unwrap();
        assert!(budget::reserve(&recovered, None, "p", 500).is_err());
    }

    #[tokio::test]
    async fn busy_main_database_journals_and_recovers_the_charge() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("b.sqlite3");
        let clock = Arc::new(TestClock::default());
        let ledger = BudgetLedger::open_with_busy_timeout(
            &path,
            clock.clone(),
            1,
            std::time::Duration::ZERO,
        )
        .unwrap();
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                1_000_000,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .unwrap();
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "p".to_string(),
            memory_gb: 1.0,
        }]));
        adapter.set_chat_delay("p", std::time::Duration::from_millis(100));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let cards = [paid_card("p")];
        let profile = empty_profile();
        let mut call = Box::pin(dispatch_local(
            &cards,
            Some(&supervisor),
            Some(&ledger),
            &profile,
            "hi",
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            CancellationToken::new(),
        ));
        let call_poll =
            std::future::poll_fn(|cx| std::task::Poll::Ready(call.as_mut().poll(cx))).await;
        assert!(
            call_poll.is_pending(),
            "expected request to remain in flight: {call_poll:?}"
        );
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let outcome = call.await.unwrap();
        assert!(outcome.result.is_ok());
        assert_eq!(outcome.actual_cost_minor, None);
        let journal =
            rusqlite::Connection::open(path.with_added_extension("settlements.sqlite3")).unwrap();
        let (pending, actual): (i64, i64) = journal
            .query_row(
                "SELECT count(*), actual_cost_minor FROM pending_settlements",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(pending, 1);
        assert_eq!(
            actual,
            budget::estimate_actual_cost_minor(
                &paid_card("p").provider.cost,
                "hi",
                "mock reply to: hi",
                0,
            )
        );
        drop(ledger);
        blocker.execute_batch("ROLLBACK").unwrap();
        clock.0.store(2, Ordering::SeqCst);
        for _ in 0..2 {
            let recovered = BudgetLedger::open_with(&path, clock.clone(), 1).unwrap();
            assert_eq!(
                recovered
                    .tenant_balance(budget::PERSONAL_TENANT_ID)
                    .unwrap(),
                1_000_000 - actual
            );
        }
        let pending: i64 = journal
            .query_row("SELECT count(*) FROM pending_settlements", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, 0);
    }

    /// R0 finding: the TS reference's cancellation propagation
    /// (`server.ts`'s `req.on("close")`) never actually fires, since the
    /// listener is attached after the request body — and with it, that
    /// stream's own `close` — has already completed. This asserts the
    /// Rust replacement actually works: dropping `dispatch_local`'s future
    /// mid-`.await` (simulated deterministically via `tokio::time::timeout`,
    /// which drops the inner future when it elapses -- the same mechanism
    /// a real client disconnect would trigger if this handler's own future
    /// is dropped) must still release the budget reservation and cancel
    /// the caller-supplied token, even though no explicit `return` in
    /// `dispatch_local` ever runs.
    #[tokio::test]
    async fn dropping_the_future_mid_chat_releases_the_reservation_and_cancels_the_token() {
        let (_dir, ledger) = configured_ledger(1_000_000);
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "p".to_string(),
            memory_gb: 1.0,
        }]));
        adapter.set_chat_delay("p", std::time::Duration::from_secs(5));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let cancel = CancellationToken::new();
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            dispatch_local(
                &[paid_card("p")],
                Some(&supervisor),
                Some(&ledger),
                &empty_profile(),
                "hi",
                messages,
                cancel.clone(),
            ),
        )
        .await;
        assert!(
            outcome.is_err(),
            "expected the call to still be in flight (mock chat_delay is 5s) when the 200ms timeout fired"
        );

        // Not stuck reserved forever -- ReservationGuard's Drop released it
        // even though none of dispatch_local's own `return`s ran.
        assert_eq!(
            ledger.tenant_balance(budget::PERSONAL_TENANT_ID).unwrap(),
            1_000_000
        );
        // The same token the Supervisor/adapter call received is cancelled.
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_release_under_main_db_lock_is_retried_by_live_worker() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("b.sqlite3");
        let ledger = Arc::new(
            BudgetLedger::open_with_busy_timeout(
                &path,
                Arc::new(idoris_tenancy::budget::SystemClock),
                60_000,
                std::time::Duration::ZERO,
            )
            .unwrap(),
        );
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                1_000_000,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .unwrap();
        let (_app, mut retries) = crate::build_observed_app(crate::AppState {
            budget_ledger: Some(ledger.clone()),
            ..crate::AppState::default()
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), retries.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "p".to_string(),
            memory_gb: 1.0,
        }]));
        adapter.set_chat_delay("p", std::time::Duration::from_secs(5));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let cards = [paid_card("p")];
        let profile = empty_profile();
        let mut call = Box::pin(dispatch_local(
            &cards,
            Some(&supervisor),
            Some(&ledger),
            &profile,
            "hi",
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            CancellationToken::new(),
        ));
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        drop(call);

        let err = tokio::time::timeout(std::time::Duration::from_secs(3), retries.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            err.is_err(),
            "worker should report the locked-main-db retry failure"
        );
        let main = rusqlite::Connection::open(&path).unwrap();
        let active: i64 = main
            .query_row(
                "SELECT count(*) FROM reservations WHERE status='active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 1);
        drop(main);
        blocker.execute_batch("ROLLBACK").unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if retries
                    .recv()
                    .await
                    .expect("worker stopped before recovery")
                    .is_ok()
                {
                    break;
                }
            }
        })
        .await
        .expect("worker should automatically retry the known release");
        assert_eq!(
            ledger.tenant_balance(budget::PERSONAL_TENANT_ID).unwrap(),
            1_000_000
        );
        let main = rusqlite::Connection::open(&path).unwrap();
        let released: i64 = main
            .query_row(
                "SELECT count(*) FROM reservations WHERE status='released'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(released, 1);
        drop(main);
        let journal =
            rusqlite::Connection::open(path.with_added_extension("settlements.sqlite3")).unwrap();
        let intents: i64 = journal
            .query_row("SELECT count(*) FROM settlement_intents", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(intents, 0);
        let id = budget::reserve(&ledger, None, "p", 500).unwrap();
        budget::release(&ledger, None, &id).unwrap();
    }
}

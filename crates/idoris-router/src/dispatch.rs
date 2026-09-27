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
use tokio_util::sync::CancellationToken;

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

/// R2-D simplification (documented, revisited once idoris-recommender's
/// catalog is wired in): every loaded component card is treated as
/// eligible for every catalog role and always `Ready` for admission
/// purposes at `decide()` time — role→component catalog mapping isn't
/// wired yet, and *real* admission state is only known after actually
/// asking the Supervisor, which only happens after `decide()` has already
/// picked a candidate (see [`dispatch_local`]). `estimated_cost_minor` is
/// `Some(0)` when the provider's declared cost is exactly zero and `None`
/// (price unknown → excluded, invariant #3) otherwise — real per-request
/// cost estimation from actual token counts is R2-D task 4's job.
fn candidate(component: &ComponentCard) -> Card {
    let free =
        component.provider.cost.input_per_m == 0.0 && component.provider.cost.output_per_m == 0.0;
    Card {
        component: component.clone(),
        roles: ROLES
            .iter()
            .copied()
            .filter(|r| r.is_catalog_role())
            .collect(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: if free { Some(0) } else { None },
        admission_status: AdmissionStatus::Ready,
    }
}

/// What [`dispatch_local`] returns on a successful `decide()` — the
/// decision plus which locality actually served (or attempted to serve)
/// the request, alongside the backend's own result. Callers need
/// `served_locality` even when `result` is `Err`: once a candidate is
/// chosen, `X-iDoris-Served-Locality` must be set regardless of what
/// happens next (interface spec §3.12).
#[derive(Debug)]
pub struct ChatOutcome {
    pub decision: Decision,
    pub served_locality: Locality,
    pub result: Result<ChatResponse, BackendError>,
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

/// Runs the local decision + execution path for one request: builds
/// decision-time [`Card`]s from `cards` (R2-D simplification, see
/// `candidate`), calls [`idoris_policy::decide`] with no budget context
/// (atomic reserve/settle around a *paid* candidate is R2-D task 4's job,
/// layered on top of this function rather than inside it), then — if a
/// local backend is wired (`supervisor.is_some()`) — loads the chosen
/// model if the Supervisor doesn't already report it loaded, and finally
/// calls `chat`.
pub async fn dispatch_local(
    cards: &[ComponentCard],
    supervisor: Option<&SupervisorHandle>,
    profile: &ParsedProfile,
    messages: Vec<ChatMessage>,
) -> Result<ChatOutcome, DispatchError> {
    // Scoped so `candidates`/`ctx` (which holds a `PolicyCtx<'_>` — not
    // `Send` because `dyn BudgetView` isn't `Sync` — see its own doc) are
    // dropped before any `.await` below; otherwise the whole function's
    // future would stop being `Send`, which axum's `Handler` trait requires.
    let (decision, served_locality, model_id, load_policy) = {
        let candidates: Vec<Card> = cards.iter().map(candidate).collect();
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
        (
            decision,
            served_locality,
            chosen.id().to_string(),
            load_policy,
        )
    };

    let Some(supervisor) = supervisor else {
        return Ok(ChatOutcome {
            decision,
            served_locality,
            result: Err(BackendError::supervisor_unavailable()),
        });
    };

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
            result: Err(err),
        });
    }

    let result = supervisor
        .chat(
            ChatRequest {
                model: model_id,
                messages,
            },
            CancellationToken::new(),
        )
        .await;
    Ok(ChatOutcome {
        decision,
        served_locality,
        result,
    })
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
        let err = dispatch_local(&[], None, &empty_profile(), Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DispatchError::Rejection(Rejection::LocalOnlyUnavailable)
        ));
    }

    #[tokio::test]
    async fn a_candidate_with_no_supervisor_reports_supervisor_unavailable() {
        let outcome = dispatch_local(&[local_card("a")], None, &empty_profile(), Vec::new())
            .await
            .unwrap();
        assert_eq!(outcome.served_locality, Locality::Loopback);
        let err = outcome.result.unwrap_err();
        assert_eq!(err.reason_code(), "supervisor_unavailable");
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
            &empty_profile(),
            messages,
        )
        .await
        .unwrap();
        assert_eq!(outcome.served_locality, Locality::Loopback);
        let response = outcome.result.unwrap();
        assert_eq!(response.model, "a");
        assert!(response.content.contains("hello"));
    }
}

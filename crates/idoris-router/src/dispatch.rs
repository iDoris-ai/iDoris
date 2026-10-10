//! Local dispatch: `idoris_policy::decide()` → (if needed) `Supervisor`
//! load → `Supervisor` chat. This crate's decision pipeline (R2-B) and
//! model-management Supervisor (R2-A) are both Rust-native additions with
//! no 1:1 TS "dispatch.ts" to port against for the actual local-execution
//! steps — only the response-header contract (interface spec §3.12) is
//! shared ground truth, and that's applied by the axum handler in `lib.rs`
//! that calls `dispatch_local`, not here (this module has no axum/HTTP
//! dependency on purpose, so it's testable without spinning up the app).

use idoris_backend::{
    BackendError, BackendStatus, ChatMessage, ChatRequest, ChatResponse, SupervisorHandle,
};
use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::Locality;
use idoris_policy::{
    AdmissionStatus, Card, Decision, PolicyCtx, ROLES, ReasonCode, Rejection, RequestProfile,
    decide_with_affinity, effective_served_locality,
};
use idoris_tenancy::budget::{BudgetError, BudgetLedger, ReservationId};
use tokio_util::sync::CancellationToken;

use crate::budget;
use crate::profile::ParsedProfile;

/// The single lifecycle Supervisor and the backend identity it executes.
/// Keep these together so replacing a card cannot silently retarget dispatch.
#[derive(Debug, Clone)]
pub struct BoundSupervisor {
    handle: SupervisorHandle,
    provider_id: String,
    endpoint: String,
    locality: Locality,
    admission_model: Option<(String, f64)>,
}

impl BoundSupervisor {
    pub(crate) fn new(card: &ComponentCard, handle: SupervisorHandle) -> Self {
        Self {
            handle,
            provider_id: card.provider.id.clone(),
            endpoint: card.endpoint.clone(),
            locality: card.provider.locality,
            admission_model: None,
        }
    }

    pub(crate) fn with_admission_model(
        mut self,
        model_id: impl Into<String>,
        memory_gb: f64,
    ) -> Self {
        self.admission_model = Some((model_id.into(), memory_gb));
        self
    }

    pub(crate) fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub(crate) async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.handle.status().await
    }

    /// Constructs the adapter from the same card used for the binding.
    pub fn spawn_omlx(card: &ComponentCard) -> Result<Self, String> {
        let adapter = idoris_upstream::OmlxAdapter::new(idoris_upstream::OmlxAdapterConfig {
            base_url: card.endpoint.clone(),
            ..idoris_upstream::OmlxAdapterConfig::default()
        })
        .map_err(|err| format!("无法构造 oMLX 适配器（{}）：{err}", card.provider.id))?;
        let handle = idoris_backend::Supervisor::spawn(
            std::sync::Arc::new(adapter),
            idoris_backend::SupervisorConfig::default(),
        )
        .map_err(|err| format!("无法启动 Supervisor（{}）：{err}", card.provider.id))?;
        Ok(Self::new(card, handle))
    }

    fn matches(&self, card: &ComponentCard) -> bool {
        self.provider_id == card.provider.id
            && self.endpoint == card.endpoint
            && self.locality == card.provider.locality
    }
}

/// Apply the YAML policy before either execution path selects a candidate.
/// Privacy and deterministic selection remain enforced by the policy pipeline;
/// `capability` and `load` are metadata until their dedicated tasks wire them.
#[derive(Debug, Clone)]
pub struct PolicyCards {
    pub cards: Vec<ComponentCard>,
    pub route: crate::routing_policy::RouteDecision,
}

pub fn policy_cards(
    cards: &[ComponentCard],
    policy: &idoris_contracts::RoutingPolicy,
    profile: &ParsedProfile,
) -> PolicyCards {
    let route = crate::routing_policy::decide(policy, &profile.task);
    let cards = cards
        .iter()
        .filter(|card| route.tiers.contains(&card.provider.tier))
        .cloned()
        .collect();
    PolicyCards { cards, route }
}

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

/// A local backend failure, or a budget-ledger failure gating a selected
/// candidate — both only occur after a candidate was already chosen.
#[derive(Debug)]
pub enum DispatchFailure {
    Backend(BackendError),
    Budget(BudgetError),
}

/// What [`dispatch_local`] returns on a successful `decide()`.
/// `served_locality` is set even when `result` is `Err` (interface spec
/// §3.12). `actual_cost_minor` is `Some` only after a successful `settle`
/// on a paid candidate — `None` for free/failed/settle-also-failed
/// (best-effort; the response still succeeds either way).
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
    select_with_affinity(cards, profile, prompt, None)
}

pub fn select_with_affinity(
    cards: &[ComponentCard],
    profile: &ParsedProfile,
    prompt: &str,
    affinity_key: Option<&str>,
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
    let decision = decide_with_affinity(&request_profile, &candidates, &ctx, affinity_key)
        .map_err(DispatchError::Rejection)?;
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
pub(crate) struct ReservationGuard<'a> {
    ledger: Option<&'a BudgetLedger>,
    tenant_id: Option<&'a str>,
    id: Option<ReservationId>,
    release_on_drop: bool,
}

impl<'a> ReservationGuard<'a> {
    pub(crate) fn reserve(
        ledger: Option<&'a BudgetLedger>,
        tenant_id: Option<&'a str>,
        provider_id: &str,
        estimated_cost_minor: i64,
    ) -> Result<Self, BudgetError> {
        let id = match ledger {
            Some(ledger) => Some(budget::reserve(
                ledger,
                tenant_id,
                provider_id,
                estimated_cost_minor,
            )?),
            None if budget::is_paid(Some(estimated_cost_minor)) => {
                return Err(budget::ledger_unavailable_error(tenant_id, provider_id));
            }
            None => None,
        };
        Ok(Self {
            ledger,
            tenant_id,
            id,
            release_on_drop: true,
        })
    }

    /// Takes the id for settling — after this, `Drop` is a no-op.
    fn take(&mut self) -> Option<ReservationId> {
        self.id.take()
    }

    /// Settles a completed request using the ledger's replay-safe path.
    /// `None` means this guard never held a real reservation.
    pub(crate) fn settle_replayable(
        &mut self,
        actual_cost_minor: i64,
    ) -> Result<Option<i64>, BudgetError> {
        match (self.ledger, self.take()) {
            (Some(ledger), Some(id)) => {
                budget::settle_replayable(ledger, self.tenant_id, &id, actual_cost_minor).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Disarms Drop without releasing the reservation. Use only when an
    /// upstream call completed but its actual cost cannot be established;
    /// retaining the reservation until its TTL expires is safer than
    /// silently declaring the paid call free.
    pub(crate) fn retain_until_expiry(&mut self) {
        let _ = self.take();
    }

    /// Once a paid POST may have crossed the process boundary, dropping the
    /// caller future must not turn an uncertain execution into a free call.
    pub(crate) fn retain_on_drop(&mut self) {
        self.release_on_drop = false;
    }

    /// Releases only when upstream execution is proven not to have happened
    /// (or this caller only observed a replay).
    pub(crate) fn release_now(&mut self) -> Result<(), BudgetError> {
        match (self.ledger, self.take()) {
            (Some(ledger), Some(id)) => budget::release(ledger, self.tenant_id, &id),
            _ => Ok(()),
        }
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if self.release_on_drop
            && let (Some(ledger), Some(id)) = (self.ledger, self.id.take())
        {
            let _ = budget::release(ledger, self.tenant_id, &id);
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

#[derive(Debug, Clone, Copy)]
pub struct DispatchInput<'a> {
    pub requested_model: Option<&'a str>,
    pub prompt: &'a str,
    pub affinity_key: Option<&'a str>,
}

impl<'a> DispatchInput<'a> {
    pub fn new(prompt: &'a str) -> Self {
        Self {
            requested_model: None,
            prompt,
            affinity_key: None,
        }
    }

    pub fn with_model(requested_model: Option<&'a str>, prompt: &'a str) -> Self {
        Self {
            requested_model,
            prompt,
            affinity_key: None,
        }
    }

    pub fn with_affinity(mut self, affinity_key: Option<&'a str>) -> Self {
        self.affinity_key = affinity_key;
        self
    }
}

/// Runs the local decision + execution path for one request: builds
/// decision-time [`Card`]s from `cards` (R2-D simplification, see
/// `candidate`), calls [`idoris_policy::decide`], reserves budget for a
/// selected candidate whenever a ledger is wired, including zero cost
/// (`SpendGate` is enforced by the ledger). With no ledger, paid candidates
/// fail closed and free candidates remain usable. Loads the chosen model
/// via the Supervisor if not already loaded, calls `chat`, then settles (success)
/// or releases (failure) the reservation. `prompt` is the caller's own
/// concatenated message text, passed in rather than recomputed here so
/// there's one place deciding how "the prompt" is derived from `messages`.
/// `cancel` is caller-owned (e.g. tied to the HTTP request's lifetime) --
/// this function additionally cancels it if its own future is dropped
/// mid-`.await` (see `CancelOnDrop`), so a caller doesn't have to get
/// that part right itself to still get correct propagation.
pub async fn dispatch_local(
    cards: &[ComponentCard],
    supervisor: Option<&BoundSupervisor>,
    budget_ledger: Option<&BudgetLedger>,
    profile: &ParsedProfile,
    input: DispatchInput<'_>,
    messages: Vec<ChatMessage>,
    cancel: CancellationToken,
) -> Result<ChatOutcome, DispatchError> {
    let tenant_id = profile.tenant_id.as_deref();
    let prompt = input.prompt;
    let requested_model = input.requested_model;
    let affinity_key = input.affinity_key;

    // Scoped so `candidates`/`ctx` (which holds a `PolicyCtx<'_>` — not
    // `Send` because `dyn BudgetView` isn't `Sync` — see its own doc) are
    // dropped before any `.await` below; otherwise the whole function's
    // future would stop being `Send`, which axum's `Handler` trait requires.
    let (
        decision,
        served_locality,
        provider_id,
        backend_model_id,
        load_policy,
        cost,
        estimated_cost_minor,
    ) = {
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
        let decision = decide_with_affinity(&request_profile, &candidates, &ctx, affinity_key)
            .map_err(DispatchError::Rejection)?;

        let Some(chosen) = candidates.iter().find(|c| c.id() == decision.chosen_id) else {
            return Err(DispatchError::Internal(format!(
                "decide() returned chosen_id {:?} absent from its own candidate list",
                decision.chosen_id
            )));
        };
        let served_locality = effective_served_locality(chosen);
        // Fail closed before budget reservation or any Supervisor operation.
        if supervisor.is_some_and(|bound| !bound.matches(&chosen.component)) {
            return Ok(ChatOutcome {
                decision,
                served_locality,
                result: Err(DispatchFailure::Backend(
                    BackendError::supervisor_unavailable(),
                )),
                actual_cost_minor: None,
            });
        }
        let load_policy = chosen
            .component
            .load_policy
            .unwrap_or_else(default_load_policy);
        // decide()'s pricing stage already excluded any None/negative
        // estimate, so this is always Some(v >= 0); unwrap_or(0) is
        // defense in depth, not a path expected to actually trigger.
        let estimated_cost_minor = chosen.estimated_cost_minor.unwrap_or(0);
        let provider_id = chosen.id().to_string();
        let backend_model_id = if profile.role.is_none() {
            requested_model.unwrap_or(&provider_id).to_string()
        } else {
            provider_id.clone()
        };
        (
            decision,
            served_locality,
            provider_id,
            backend_model_id,
            load_policy,
            chosen.component.provider.cost,
            estimated_cost_minor,
        )
    };

    let is_paid = budget::is_paid(Some(estimated_cost_minor));
    let mut reservation_guard = match ReservationGuard::reserve(
        budget_ledger,
        tenant_id,
        &provider_id,
        estimated_cost_minor,
    ) {
        Ok(guard) => guard,
        Err(err) => {
            return Ok(ChatOutcome {
                decision,
                served_locality,
                result: Err(DispatchFailure::Budget(err)),
                actual_cost_minor: None,
            });
        }
    };
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

    let admission_memory_override = supervisor
        .admission_model
        .as_ref()
        .filter(|(model_id, _)| model_id == &backend_model_id)
        .map(|(_, memory_gb)| *memory_gb);
    let supervisor = &supervisor.handle;
    // Process-owned mlx_lm.server/llama.cpp bind a trusted config footprint.
    // Every other lifecycle runtime (notably oMLX) must provide its exact
    // selected model plus a positive pre-load footprint via its catalog.
    // A mismatched bound model intentionally has no override, forcing this
    // preflight before Supervisor eviction planning can mutate lifecycle state.
    let catalog = if admission_memory_override.is_none() {
        let catalog = match supervisor.list().await {
            Ok(models) => models,
            Err(err) => {
                return Ok(ChatOutcome {
                    decision,
                    served_locality,
                    result: Err(DispatchFailure::Backend(err)),
                    actual_cost_minor: None,
                });
            }
        };
        if backend_model_id != provider_id
            && !catalog.iter().any(|model| model.id == backend_model_id)
        {
            return Ok(ChatOutcome {
                decision,
                served_locality,
                result: Err(DispatchFailure::Backend(BackendError::model_not_found(
                    backend_model_id,
                ))),
                actual_cost_minor: None,
            });
        }
        Some(catalog)
    } else {
        None
    };
    let admission_memory_gb = match admission_memory_override {
        Some(memory_gb) => memory_gb,
        None => {
            let Some(model) = catalog
                .as_ref()
                .and_then(|models| models.iter().find(|model| model.id == backend_model_id))
            else {
                return Ok(ChatOutcome {
                    decision,
                    served_locality,
                    result: Err(DispatchFailure::Backend(BackendError::model_not_found(
                        backend_model_id,
                    ))),
                    actual_cost_minor: None,
                });
            };
            model.memory_gb
        }
    };
    if !admission_memory_gb.is_finite() || admission_memory_gb <= 0.0 {
        return Ok(ChatOutcome {
            decision,
            served_locality,
            result: Err(DispatchFailure::Backend(BackendError::Upstream {
                message: "selected lifecycle model has no positive pre-load memory footprint"
                    .into(),
            })),
            actual_cost_minor: None,
        });
    };
    // `status.loaded` reports engine residency, which may have been
    // inherited after a Supervisor restart while the model is still in
    // Error (not yet adopted and policy-checked). Always pass through the
    // Supervisor's idempotent load path before chatting so it can establish
    // readiness and apply the requested policy.
    if let Err(err) = supervisor
        .load(backend_model_id.clone(), admission_memory_gb, load_policy)
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
                model: backend_model_id,
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
            // Settling is best-effort (see `ChatOutcome::actual_cost_minor`'s
            // doc): a successful chat response is never withheld just
            // because the ledger write afterward had a problem.
            let actual_cost_minor = match (budget_ledger, reservation_guard.take()) {
                (Some(ledger), Some(id)) => {
                    let actual = budget::estimate_actual_cost_minor(
                        &cost,
                        prompt,
                        &response.content,
                        estimated_cost_minor,
                    );
                    budget::settle(ledger, tenant_id, &id, actual)
                        .ok()
                        .filter(|_| is_paid)
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

    use idoris_backend::{
        BackendStatus, GlobalCapacityLedger, MockAdapter, ModelInfo, RuntimeAdapter, Supervisor,
        SupervisorConfig,
    };
    use idoris_contracts::TaskProfile;
    use idoris_contracts::common::PrivacyClass;
    use idoris_contracts::common::Tier;
    use idoris_contracts::component_card::{Egress, Form};
    use idoris_contracts::provider::{Cost, Family, ProviderDescriptor};
    use std::sync::Arc;

    use super::*;

    struct FailingListAdapter {
        inner: MockAdapter,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for FailingListAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            self.inner.load_fence_path()
        }

        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Err(BackendError::Upstream {
                message: "catalog unavailable".into(),
            })
        }

        async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            self.inner.load(id, policy).await
        }

        async fn unload(&self, id: &str) -> Result<(), BackendError> {
            self.inner.unload(id).await
        }

        async fn status(&self) -> Result<BackendStatus, BackendError> {
            self.inner.status().await
        }

        async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
            self.inner.probe_ready(id).await
        }

        async fn chat(
            &self,
            req: ChatRequest,
            cancel: CancellationToken,
        ) -> Result<ChatResponse, BackendError> {
            self.inner.chat(req, cancel).await
        }
    }

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

    fn tier_card(id: &str, tier: Tier) -> ComponentCard {
        let mut card = local_card(id);
        card.provider.tier = tier;
        card
    }

    fn routing_policy(yaml: &str) -> idoris_contracts::RoutingPolicy {
        serde_yaml::from_str(yaml).expect("test routing policy should parse")
    }

    #[test]
    fn policy_cards_retains_explicit_rule_decision_and_existing_filtering() {
        let policy = routing_policy(
            r#"
routing_policy:
  version: 1
  rules:
    - if: { intent: banner }
      then:
        tiers: [remote, local]
        fail_closed: false
        capability: vision
        load: resident
  default: { tiers: [lora], fail_closed: false }
"#,
        );
        let mut profile = empty_profile();
        profile.task.intent = Some("banner".to_string());
        profile.task.privacy = Some(PrivacyClass::Any);
        let cards = vec![
            tier_card("lora", Tier::Lora),
            tier_card("remote", Tier::Remote),
            tier_card("local", Tier::Local),
        ];

        let filtered = policy_cards(&cards, &policy, &profile);

        assert_eq!(
            filtered
                .cards
                .iter()
                .map(|card| card.provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["remote", "local"]
        );
        assert_eq!(
            filtered.route.matched_rule,
            crate::routing_policy::MatchedRule::Rule(0)
        );
        assert_eq!(filtered.route.tiers, vec![Tier::Remote, Tier::Local]);
        assert!(!filtered.route.fail_closed);
        assert_eq!(
            filtered.route.capability,
            Some(idoris_contracts::common::Capability::Vision)
        );
        assert_eq!(filtered.route.load, Some(LoadMode::Resident));
    }

    #[test]
    fn policy_cards_retains_privacy_filtered_default_decision() {
        let policy = routing_policy(
            r#"
routing_policy:
  version: 1
  rules:
    - if: { intent: banner }
      then: { tiers: [remote], fail_closed: false }
  default:
    tiers: [remote, lora, local]
    fail_closed: false
    capability: coding
    load: evict_to_load
"#,
        );
        let mut profile = empty_profile();
        profile.task.intent = Some("chat".to_string());
        profile.task.privacy = Some(PrivacyClass::LocalOnly);
        let cards = vec![
            tier_card("remote", Tier::Remote),
            tier_card("local", Tier::Local),
            tier_card("lora", Tier::Lora),
        ];

        let filtered = policy_cards(&cards, &policy, &profile);

        assert_eq!(
            filtered
                .cards
                .iter()
                .map(|card| card.provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["local", "lora"]
        );
        assert_eq!(
            filtered.route.matched_rule,
            crate::routing_policy::MatchedRule::Default
        );
        assert_eq!(filtered.route.tiers, vec![Tier::Lora, Tier::Local]);
        assert!(filtered.route.fail_closed);
        assert_eq!(
            filtered.route.capability,
            Some(idoris_contracts::common::Capability::Coding)
        );
        assert_eq!(filtered.route.load, Some(LoadMode::EvictToLoad));
    }

    #[tokio::test]
    async fn no_cards_rejects_as_local_only_unavailable() {
        let err = dispatch_local(
            &[],
            None,
            None,
            &empty_profile(),
            DispatchInput::new(""),
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
            DispatchInput::new(""),
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
        let supervisor = BoundSupervisor::new(&local_card("a"), supervisor);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }];
        let outcome = dispatch_local(
            &[local_card("a")],
            Some(&supervisor),
            None,
            &empty_profile(),
            DispatchInput::new("hello"),
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

    #[tokio::test]
    async fn catalog_memory_drives_global_admission_before_adapter_load() {
        let card = local_card("a");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "a".to_string(),
            memory_gb: 6.0,
        }]));
        let ledger = Arc::new(GlobalCapacityLedger::new(4.0).unwrap());
        let supervisor = Supervisor::spawn_with_startup_capacity_tracking(
            adapter.clone(),
            SupervisorConfig::default(),
            ledger.clone(),
            "a",
        )
        .unwrap();
        let supervisor = BoundSupervisor::new(&card, supervisor).with_admission_model("a", 6.0);

        let outcome = dispatch_local(
            std::slice::from_ref(&card),
            Some(&supervisor),
            None,
            &empty_profile(),
            DispatchInput::new("hello"),
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();

        match outcome.result.unwrap_err() {
            DispatchFailure::Backend(err) => assert_eq!(err.reason_code(), "oom"),
            other => panic!("expected Backend(oom), got {other:?}"),
        }
        assert_eq!(adapter.load_call_count("a"), 0);
        assert_eq!(ledger.snapshot().unwrap().allocations["runtime:a"], 0.0);
    }

    #[tokio::test]
    async fn concrete_model_is_dispatched_separately_from_provider_id() {
        let card = local_card("omlx");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "Qwen3-0.6B-4bit".to_string(),
            memory_gb: 1.0,
        }]));
        let supervisor = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let supervisor = BoundSupervisor::new(&card, supervisor);
        let outcome = dispatch_local(
            std::slice::from_ref(&card),
            Some(&supervisor),
            None,
            &empty_profile(),
            DispatchInput::with_model(Some("Qwen3-0.6B-4bit"), "hello"),
            vec![ChatMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let response = outcome.result.unwrap();
        assert_eq!(response.model, "Qwen3-0.6B-4bit");
        assert_eq!(adapter.load_call_count("Qwen3-0.6B-4bit"), 1);
        assert_eq!(adapter.load_call_count("omlx"), 0);
    }

    #[tokio::test]
    async fn zero_or_non_finite_catalog_memory_never_reaches_adapter_load() {
        for memory_gb in [0.0, f64::NAN] {
            let card = local_card("omlx");
            let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
                id: "actual-model".into(),
                memory_gb,
            }]));
            let supervisor =
                Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
            let supervisor = BoundSupervisor::new(&card, supervisor);
            let outcome = dispatch_local(
                std::slice::from_ref(&card),
                Some(&supervisor),
                None,
                &empty_profile(),
                DispatchInput::with_model(Some("actual-model"), "hello"),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            match outcome.result.unwrap_err() {
                DispatchFailure::Backend(err) => assert_eq!(err.reason_code(), "upstream_error"),
                other => panic!("expected Backend(upstream_error), got {other:?}"),
            }
            assert_eq!(adapter.load_call_count("actual-model"), 0);
        }
    }

    #[tokio::test]
    async fn unknown_concrete_model_is_rejected_without_provider_fallback() {
        let card = local_card("omlx");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "Qwen3-0.6B-4bit".to_string(),
            memory_gb: 1.0,
        }]));
        let supervisor = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let supervisor = BoundSupervisor::new(&card, supervisor);
        let outcome = dispatch_local(
            std::slice::from_ref(&card),
            Some(&supervisor),
            None,
            &empty_profile(),
            DispatchInput::with_model(Some("missing-model"), ""),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        match outcome.result.unwrap_err() {
            DispatchFailure::Backend(err) => assert_eq!(err.reason_code(), "model_not_found"),
            other => panic!("expected Backend(model_not_found), got {other:?}"),
        }
        assert_eq!(adapter.load_call_count("omlx"), 0);
    }

    #[tokio::test]
    async fn unknown_concrete_model_never_evicts_an_existing_warm_model() {
        let card = local_card("omlx");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "warm".to_string(),
            memory_gb: 1.0,
        }]));
        let warm_policy = default_load_policy();
        adapter.load("warm", Some(&warm_policy)).await.unwrap();
        let supervisor = Supervisor::spawn(
            adapter.clone(),
            SupervisorConfig {
                budget_gb: 1.0,
                ..SupervisorConfig::default()
            },
        )
        .unwrap();
        let supervisor = BoundSupervisor::new(&card, supervisor);

        let outcome = dispatch_local(
            std::slice::from_ref(&card),
            Some(&supervisor),
            None,
            &empty_profile(),
            DispatchInput::with_model(Some("missing-model"), ""),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        match outcome.result.unwrap_err() {
            DispatchFailure::Backend(err) => assert_eq!(err.reason_code(), "model_not_found"),
            other => panic!("expected Backend(model_not_found), got {other:?}"),
        }
        assert_eq!(adapter.load_call_count("missing-model"), 0);
        assert_eq!(adapter.unload_call_count("warm"), 0);
        assert!(
            adapter
                .status()
                .await
                .unwrap()
                .loaded
                .contains(&"warm".to_string())
        );
    }

    #[tokio::test]
    async fn implicit_provider_id_never_evicts_bound_single_model_runtime() {
        let card = local_card("prov");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "m".to_string(),
            memory_gb: 8.0,
        }]));
        let warm_policy = LoadPolicy {
            mode: LoadMode::OnDemand,
            keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
            admission: Admission::RequiresEviction,
        };
        let supervisor = Supervisor::spawn(
            adapter.clone(),
            SupervisorConfig {
                budget_gb: 10.0,
                ..SupervisorConfig::default()
            },
        )
        .unwrap();
        supervisor
            .load("m", 8.0, warm_policy)
            .await
            .expect("warm model must load");
        let supervisor = BoundSupervisor::new(&card, supervisor).with_admission_model("m", 8.0);

        let outcome = dispatch_local(
            std::slice::from_ref(&card),
            Some(&supervisor),
            None,
            &empty_profile(),
            DispatchInput::new("hello"),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        match outcome.result.unwrap_err() {
            DispatchFailure::Backend(err) => assert_eq!(err.reason_code(), "model_not_found"),
            other => panic!("expected Backend(model_not_found), got {other:?}"),
        }
        assert_eq!(adapter.load_call_count("prov"), 0);
        assert_eq!(adapter.unload_call_count("m"), 0);
        assert!(
            adapter
                .status()
                .await
                .unwrap()
                .loaded
                .contains(&"m".to_string())
        );
    }

    #[tokio::test]
    async fn catalog_failure_fails_closed_before_any_concrete_model_load() {
        let card = local_card("omlx");
        let adapter = Arc::new(FailingListAdapter {
            inner: MockAdapter::new(vec![ModelInfo {
                id: "known".to_string(),
                memory_gb: 1.0,
            }]),
        });
        let supervisor = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let supervisor = BoundSupervisor::new(&card, supervisor);
        let outcome = dispatch_local(
            std::slice::from_ref(&card),
            Some(&supervisor),
            None,
            &empty_profile(),
            DispatchInput::with_model(Some("known"), ""),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        match outcome.result.unwrap_err() {
            DispatchFailure::Backend(err) => assert_eq!(err.reason_code(), "upstream_error"),
            other => panic!("expected Backend(upstream_error), got {other:?}"),
        }
        assert_eq!(adapter.inner.load_call_count("known"), 0);
    }

    #[tokio::test]
    async fn dispatch_adopts_inherited_residency_and_applies_policy() {
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "a".to_string(),
            memory_gb: 1.0,
        }]));
        let inherited_policy = LoadPolicy {
            mode: LoadMode::OnDemand,
            keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
            admission: Admission::Coexist,
        };
        // Simulate an engine that kept the model resident while its
        // Supervisor was restarted.
        adapter.load("a", Some(&inherited_policy)).await.unwrap();

        let mut card = local_card("a");
        let requested_policy = LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        };
        card.load_policy = Some(requested_policy);
        let supervisor = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let supervisor = BoundSupervisor::new(&card, supervisor);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }];

        for _ in 0..2 {
            let outcome = dispatch_local(
                &[card.clone()],
                Some(&supervisor),
                None,
                &empty_profile(),
                DispatchInput::new("hello"),
                messages.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let response = outcome.result.unwrap();
            assert_eq!(response.model, "a");
            assert!(response.content.contains("hello"));
        }

        assert_eq!(
            adapter.effective_policy("a").unwrap(),
            Some(requested_policy)
        );
        // One adapter load adopts the inherited model and applies the new
        // policy; the repeated dispatch is an idempotent no-op.
        assert_eq!(adapter.load_call_count("a"), 2); // one pre-spawn, one adoption
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

    #[tokio::test]
    async fn free_candidate_obeys_all_gate_before_loading() {
        let (_dir, ledger) = configured_ledger(0);
        ledger
            .configure_tenant("acme", 0, "UTC", idoris_tenancy::budget::SpendGate::All)
            .unwrap();
        let mut profile = empty_profile();
        profile.tenant_id = Some("acme".to_string());
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "a".to_string(),
            memory_gb: 1.0,
        }]));
        let supervisor = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let supervisor = BoundSupervisor::new(&local_card("a"), supervisor);
        let outcome = dispatch_local(
            &[local_card("a")],
            Some(&supervisor),
            Some(&ledger),
            &profile,
            DispatchInput::new("hi"),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome.result,
            Err(DispatchFailure::Budget(BudgetError::Exceeded { .. }))
        ));
        assert_eq!(adapter.load_call_count("a"), 0);
        assert_eq!(ledger.tenant_balance("acme").unwrap(), 0);
    }

    #[tokio::test]
    async fn free_candidate_obeys_paid_only_gate_and_preserves_no_charge_header() {
        let (_dir, ledger) = configured_ledger(0);
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "a".to_string(),
            memory_gb: 1.0,
        }]));
        let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let supervisor = BoundSupervisor::new(&local_card("a"), supervisor);
        let outcome = dispatch_local(
            &[local_card("a")],
            Some(&supervisor),
            Some(&ledger),
            &empty_profile(),
            DispatchInput::new("hi"),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(outcome.result.is_ok());
        assert_eq!(outcome.actual_cost_minor, None);
        assert_eq!(
            ledger.tenant_balance(budget::PERSONAL_TENANT_ID).unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn free_candidate_with_unconfigured_ledger_fails_closed() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger = BudgetLedger::open(dir.path().join("b.sqlite3")).unwrap();
        let outcome = dispatch_local(
            &[local_card("a")],
            None,
            Some(&ledger),
            &empty_profile(),
            DispatchInput::new("hi"),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome.result,
            Err(DispatchFailure::Budget(BudgetError::NotConfigured { .. }))
        ));
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
            DispatchInput::new("hi"),
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
        let supervisor = BoundSupervisor::new(&paid_card("p"), supervisor);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let outcome = dispatch_local(
            &[paid_card("p")],
            Some(&supervisor),
            Some(&ledger),
            &empty_profile(),
            DispatchInput::new("hi"),
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
        let supervisor = BoundSupervisor::new(&paid_card("p"), supervisor);
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
                DispatchInput::new("hi"),
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
}

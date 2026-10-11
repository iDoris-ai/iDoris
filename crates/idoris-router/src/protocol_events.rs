//! The Event Log chain for `/v1/embeddings`, `/v1/rerank` and
//! `/v1/messages`. M4 acceptance says any request's record id must find
//! its decision chain; until now only chat wrote one. These endpoints now
//! append the same facts as chat's proxy path — `request.received` (after
//! auth), `inspected`, `profiled`, `decided`, then `dispatched` right before
//! the upstream send and `completed` when that attempt terminates — with the
//! same correlation checks and the same fail-closed 503 when an append
//! fails. A streamed `/v1/messages` completes when its stream ends.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::http::HeaderMap;
use axum::response::Response;
use idoris_tenancy::event_log::EventLogStore;

use crate::correlation::{self, RequestCorrelation};
use crate::dispatch::{DispatchError, Selected};
use crate::inspect::Inspection;
use crate::profile::ParsedProfile;
use crate::proxy::{ExecutionDisposition, ForwardOutcome};
use crate::routing_policy::RouteDecision;
use crate::{
    AppState, CompletedEventContext, DispatchedEventContext, HEADER_REQUEST_ID, append_completed,
    append_decided, append_dispatched, append_profiled, append_request_received, budget,
    correlation_error_response, event_log_unavailable_response, event_request_id, header_text,
};

pub(crate) struct ProtocolEvents {
    log: Arc<EventLogStore>,
    tenant_id: String,
    record_id: String,
    correlation: RequestCorrelation,
    dispatched: AtomicBool,
}

impl ProtocolEvents {
    /// Validates correlation headers and appends `request.received`. `None`
    /// when no Event Log is configured (nothing is recorded, as for chat).
    pub(crate) async fn begin(
        state: &AppState,
        headers: &HeaderMap,
        parsed: &ParsedProfile,
        record_id: &str,
    ) -> Result<Option<Self>, Response> {
        let Some(log) = state.event_log.clone() else {
            return Ok(None);
        };
        let correlation = correlation::parse(headers).map_err(correlation_error_response)?;
        let tenant_id = match state.deploy_mode {
            idoris_contracts::DeployMode::Personal => budget::PERSONAL_TENANT_ID.to_string(),
            idoris_contracts::DeployMode::Tenant => parsed
                .tenant_id
                .clone()
                .ok_or_else(event_log_unavailable_response)?,
        };
        let request_id = event_request_id(header_text(headers, HEADER_REQUEST_ID), record_id);
        append_request_received(
            log.clone(),
            tenant_id.clone(),
            record_id.to_string(),
            request_id,
            &correlation,
        )
        .await
        .map_err(|()| event_log_unavailable_response())?;
        Ok(Some(Self {
            log,
            tenant_id,
            record_id: record_id.to_string(),
            correlation,
            dispatched: AtomicBool::new(false),
        }))
    }

    pub(crate) async fn inspected(&self, inspection: &Inspection) -> Result<(), Response> {
        crate::inspect::append_inspected(
            self.log.clone(),
            self.tenant_id.clone(),
            self.record_id.clone(),
            &self.correlation,
            inspection,
        )
        .await
        .map_err(|()| event_log_unavailable_response())
    }

    pub(crate) async fn profiled(&self, parsed: &ParsedProfile) -> Result<(), Response> {
        append_profiled(
            self.log.clone(),
            self.tenant_id.clone(),
            self.record_id.clone(),
            &self.correlation,
            parsed,
        )
        .await
        .map_err(|()| event_log_unavailable_response())
    }

    /// Called by the proxy right before the send; an error stops the send.
    pub(crate) async fn dispatched(&self, selected: &Selected) -> Result<(), ()> {
        let context = DispatchedEventContext {
            event_log: self.log.clone(),
            tenant_id: self.tenant_id.clone(),
            record_id: self.record_id.clone(),
            correlation: self.correlation.clone(),
        };
        append_dispatched(&context, selected).await?;
        self.dispatched.store(true, Ordering::Release);
        Ok(())
    }

    /// Every recorded send attempt gets a terminal result, including a
    /// failed connection. Cache hits, replays and preflight refusals do not.
    pub(crate) async fn completed(
        &self,
        selected: &Selected,
        outcome: &ForwardOutcome,
    ) -> Result<(), Response> {
        if !self.dispatched.load(Ordering::Acquire)
            || outcome.cached
            || outcome.execution == ExecutionDisposition::Replay
        {
            return Ok(());
        }
        let status = if (200..300).contains(&outcome.status) {
            "success"
        } else {
            "failure"
        };
        append_completed(&self.completed_context(), selected, status)
            .await
            .map_err(|()| event_log_unavailable_response())
    }

    pub(crate) fn completed_context(&self) -> CompletedEventContext {
        CompletedEventContext {
            event_log: self.log.clone(),
            tenant_id: self.tenant_id.clone(),
            record_id: self.record_id.clone(),
            correlation: self.correlation.clone(),
        }
    }

    /// A stream whose send attempt failed or received a buffered upstream error.
    pub(crate) async fn completed_failure(&self, selected: &Selected) -> Result<(), Response> {
        if !self.dispatched.load(Ordering::Acquire) {
            return Ok(());
        }
        append_completed(&self.completed_context(), selected, "failure")
            .await
            .map_err(|()| event_log_unavailable_response())
    }

    pub(crate) async fn decided(
        &self,
        route: &RouteDecision,
        selection: &Result<Selected, DispatchError>,
    ) -> Result<(), Response> {
        let rejection_reason = match selection {
            Ok(_) => None,
            Err(DispatchError::Rejection(rejection)) => Some(rejection.error_type()),
            Err(DispatchError::Internal(_)) => Some("internal_error"),
        };
        append_decided(
            self.log.clone(),
            self.tenant_id.clone(),
            self.record_id.clone(),
            &self.correlation,
            route,
            selection.as_ref().ok(),
            rejection_reason,
        )
        .await
        .map_err(|()| event_log_unavailable_response())
    }
}

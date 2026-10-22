//! Dispatch and terminal observations for protocol endpoint upstream sends.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{http::HeaderMap, response::Response};
use idoris_tenancy::event_log::EventLogStore;

use crate::{
    AppState, CompletedEventContext, DispatchedEventContext, HEADER_REQUEST_ID, append_completed,
    append_dispatched, budget, correlation, correlation_error_response,
    event_log_unavailable_response, event_request_id, header_text,
};
use crate::{
    dispatch::Selected,
    proxy::{ExecutionDisposition, ForwardOutcome},
};

pub(crate) struct ProtocolEvents {
    log: Arc<EventLogStore>,
    tenant_id: String,
    record_id: String,
    correlation: correlation::RequestCorrelation,
    dispatched: AtomicBool,
}

impl ProtocolEvents {
    pub(crate) async fn begin(
        state: &AppState,
        headers: &HeaderMap,
        tenant: Option<&str>,
        record_id: &str,
    ) -> Result<Option<Self>, Response> {
        let Some(log) = state.event_log.clone() else {
            return Ok(None);
        };
        let correlation = correlation::parse(headers).map_err(correlation_error_response)?;
        let tenant_id = match state.deploy_mode {
            idoris_contracts::DeployMode::Personal => budget::PERSONAL_TENANT_ID.to_string(),
            idoris_contracts::DeployMode::Tenant => tenant
                .map(str::to_string)
                .ok_or_else(event_log_unavailable_response)?,
        };
        let request_id = event_request_id(header_text(headers, HEADER_REQUEST_ID), record_id);
        let store = log.clone();
        let tenant_for_write = tenant_id.clone();
        let record = record_id.to_string();
        let ts_utc_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .ok_or_else(event_log_unavailable_response)?;
        let event = idoris_tenancy::event_log::NewEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            tenant_id: tenant_id.clone(),
            record_id: record,
            event_type: idoris_tenancy::event_log::EventType::RequestReceived,
            ts_utc_ms,
            request_id: Some(request_id),
            session_id: correlation.session_id.clone(),
            trace_id: correlation.trace_id.clone(),
            parent_id: correlation.parent_id.clone(),
            origin_record_id: None,
            metadata: Default::default(),
        };
        crate::run_event_log_write(move || {
            store.append(Some(&tenant_for_write), &event).map(|_| ())
        })
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

    pub(crate) async fn completed(
        &self,
        selected: &Selected,
        outcome: &ForwardOutcome,
    ) -> Result<(), ()> {
        if !self.dispatched.load(Ordering::Acquire)
            || outcome.cached
            || outcome.execution == ExecutionDisposition::Replay
        {
            return Ok(());
        }
        let context = self.completed_context();
        append_completed(
            &context,
            selected,
            if (200..300).contains(&outcome.status) {
                "success"
            } else {
                "failure"
            },
        )
        .await
    }

    pub(crate) fn completed_context(&self) -> CompletedEventContext {
        CompletedEventContext {
            event_log: self.log.clone(),
            tenant_id: self.tenant_id.clone(),
            record_id: self.record_id.clone(),
            correlation: self.correlation.clone(),
        }
    }

    pub(crate) async fn completed_failure(&self, selected: &Selected) -> Result<(), ()> {
        if !self.dispatched.load(Ordering::Acquire) {
            return Ok(());
        }
        append_completed(&self.completed_context(), selected, "failure").await
    }
}

use axum::Json;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use idoris_backend::ChatResponse;
use idoris_upstream::subscription::error::SubscriptionRelayError;

use super::source::SubscriptionSourceError;

pub fn success(
    chat: &ChatResponse,
    prompt: &str,
    record_id: &str,
    _stream_requested: bool,
) -> Response {
    let (body, prompt_tokens, completion_tokens) =
        crate::openai_chat_completion(&chat.content, &chat.model, prompt);
    let mut response = (StatusCode::OK, Json(body)).into_response();
    mark_executed(&mut response, record_id);
    response
        .extensions_mut()
        .insert(crate::usage::UsageFact::inference_with_tokens(
            Some(0),
            prompt_tokens,
            completion_tokens,
        ));
    response
}

pub fn relay_failure(error: &SubscriptionRelayError, record_id: &str) -> Response {
    let mut response = crate::error_envelope_with_reason(
        StatusCode::BAD_GATEWAY,
        "subscription_relay_failed",
        error.reason_code(),
        error.public_message(),
    );
    mark_executed(&mut response, record_id);
    response
}

pub fn source_rejection(error: SubscriptionSourceError, record_id: &str) -> Response {
    let mut response = crate::error_envelope_with_reason(
        StatusCode::FORBIDDEN,
        "subscription_source_not_loopback",
        error.reason_code(),
        error.to_string(),
    );
    mark_record_id(&mut response, record_id);
    response
}

fn mark_executed(response: &mut Response, record_id: &str) {
    mark_record_id(response, record_id);
    response.headers_mut().insert(
        crate::HEADER_SERVED_LOCALITY,
        HeaderValue::from_static("remote"),
    );
}

fn mark_record_id(response: &mut Response, record_id: &str) {
    if let Ok(value) = HeaderValue::from_str(record_id) {
        response
            .headers_mut()
            .insert(crate::HEADER_RECORD_ID, value);
    }
}

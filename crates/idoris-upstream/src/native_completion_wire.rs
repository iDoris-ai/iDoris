//! Shared OpenAI wire conversion for lifecycle adapters. Lifecycle admission,
//! transport deadlines and cancellation remain the adapter/Supervisor's job.
use idoris_backend::{BackendError, ChatRequest, ChatResponse};
use serde_json::{Value, json};

pub fn request_body(request: &ChatRequest, engine_model: &str) -> Value {
    let mut body = request
        .native
        .as_ref()
        .map(|native| native.parameters.clone())
        .unwrap_or_default();
    let messages = request
        .native
        .as_ref()
        .map(|native| native.messages.clone())
        .unwrap_or_else(|| {
            request
                .messages
                .iter()
                .map(|message| json!({"role": message.role, "content": message.content}))
                .collect()
        });
    body.insert("model".into(), json!(engine_model));
    body.insert("messages".into(), json!(messages));
    Value::Object(body)
}

/// Formal native completions must identify their actual backend model.
/// Selected/requested model identities are not evidence of what served a call.
pub fn response(raw: Value) -> Result<ChatResponse, BackendError> {
    let model = raw
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| BackendError::Upstream {
            message: "native runtime response missing a nonempty actual model".into(),
        })?
        .to_string();
    let invalid = || BackendError::Upstream {
        message: "native runtime response missing text or tool completion".into(),
    };
    let message = raw
        .pointer("/choices/0/message")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let content = match message.get("content") {
        Some(Value::String(content)) => content.clone(),
        None | Some(Value::Null)
            if message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty()) =>
        {
            String::new()
        }
        _ => return Err(invalid()),
    };
    Ok(ChatResponse {
        model,
        content,
        native: Some(raw),
    })
}

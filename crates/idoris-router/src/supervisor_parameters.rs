//! Validate the lossless native payload before budget reservation or lifecycle IO.
use serde_json::{Map, Value};

pub(crate) fn validate(object: &Map<String, Value>) -> Result<(), String> {
    for (field, value) in object {
        let valid = match field.as_str() {
            "model" | "messages" | "stream" => true,
            "max_tokens" => value.as_u64().is_some_and(|n| n > 0),
            "tools" => value
                .as_array()
                .is_some_and(|tools| tools.iter().all(valid_tool)),
            "response_format" => value.as_object().is_some_and(|format| {
                matches!(
                    format.get("type").and_then(Value::as_str),
                    Some("text" | "json_object")
                ) || (format.get("type").and_then(Value::as_str) == Some("json_schema")
                    && format.get("json_schema").is_some_and(Value::is_object))
            }),
            "chat_template_kwargs" => value.is_object(),
            "disable_thinking" => value.is_boolean(),
            _ => {
                return Err(format!(
                    "Supervisor does not support {field}; remove it or use a backend that supports it"
                ));
            }
        };
        if !valid {
            return Err(format!("Supervisor received invalid {field}"));
        }
    }
    if let Some(messages) = object.get("messages") {
        let Some(messages) = messages.as_array() else {
            return Err("Supervisor requires messages to be an array".into());
        };
        if !messages.iter().all(valid_message) {
            return Err("Supervisor requires text or tool messages with supported fields".into());
        }
    }
    Ok(())
}

fn valid_tool(tool: &Value) -> bool {
    tool.get("type").and_then(Value::as_str) == Some("function")
        && tool.get("function").is_some_and(|function| {
            function
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.is_empty())
                && function.get("parameters").is_none_or(Value::is_object)
        })
}

fn valid_message(message: &Value) -> bool {
    let Some(message) = message.as_object() else {
        return false;
    };
    if !message.keys().all(|key| {
        matches!(
            key.as_str(),
            "role" | "content" | "name" | "tool_calls" | "tool_call_id"
        )
    }) {
        return false;
    }
    if message.get("name").is_some_and(|name| !name.is_string()) {
        return false;
    }
    let role = message.get("role").and_then(Value::as_str);
    let calls = message.get("tool_calls");
    if let Some(calls) = calls {
        if role != Some("assistant")
            || !calls.as_array().is_some_and(|calls| {
                !calls.is_empty()
                    && calls.iter().all(|call| {
                        call.get("id").is_some_and(Value::is_string)
                            && call.get("type").and_then(Value::as_str) == Some("function")
                            && call.pointer("/function/name").is_some_and(Value::is_string)
                            && call
                                .pointer("/function/arguments")
                                .is_some_and(Value::is_string)
                    })
            })
        {
            return false;
        }
    }
    if role == Some("tool") {
        if !message.get("tool_call_id").is_some_and(Value::is_string) {
            return false;
        }
    } else if message.contains_key("tool_call_id") {
        return false;
    }
    role.is_some()
        && (message.get("content").is_some_and(Value::is_string)
            || (calls.is_some() && message.get("content").is_none_or(Value::is_null)))
}

pub(crate) fn payload(object: &Map<String, Value>) -> idoris_backend::NativeCompletionRequest {
    idoris_backend::NativeCompletionRequest {
        messages: object
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        parameters: object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "model" | "messages"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    }
}

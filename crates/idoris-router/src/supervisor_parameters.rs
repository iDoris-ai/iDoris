//! The lifecycle adapter currently carries only text messages. Reject any
//! option it would discard, before reserving budget or contacting the backend.
use serde_json::{Map, Value};

pub(crate) fn validate(object: &Map<String, Value>) -> Result<(), String> {
    for field in object.keys() {
        if !matches!(field.as_str(), "model" | "messages" | "stream") {
            return Err(format!(
                "Supervisor does not support {field}; remove it or use a backend that supports it"
            ));
        }
    }
    if let Some(messages) = object.get("messages") {
        let Some(messages) = messages.as_array() else {
            return Err("Supervisor requires messages to be an array of text messages".into());
        };
        for message in messages {
            let valid = message.as_object().is_some_and(|message| {
                message
                    .keys()
                    .all(|key| matches!(key.as_str(), "role" | "content"))
                    && message.get("role").is_some_and(Value::is_string)
                    && message.get("content").is_some_and(Value::is_string)
            });
            if !valid {
                return Err("Supervisor supports only messages with string role and content; tools, images and extra message fields are unsupported".into());
            }
        }
    }
    Ok(())
}

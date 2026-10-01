use crate::error::BoxError;
use crate::webc::WebStream;
use futures::Stream;
use reqwest::RequestBuilder;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Simple EventSource stream implementation that uses WebStream as a foundation.
pub struct EventSourceStream {
	inner: WebStream,
	opened: bool,
}

#[derive(Debug)]
pub enum Event {
	Open,
	Message(Message),
}

#[derive(Debug)]
pub struct Message {
	pub event: String,
	pub data: String,
}

fn parse_event_block(raw_event: &str) -> Option<Message> {
	let mut event = "message".to_string();
	let mut data_lines = Vec::new();

	for line in raw_event.lines() {
		if line.starts_with(':') {
			continue;
		}

		let (field, value) = line.split_once(':').unwrap_or((line, ""));
		// The SSE format removes exactly one optional ASCII space after the colon.
		let value = value.strip_prefix(' ').unwrap_or(value);

		match field {
			"event" => {
				event = if value.is_empty() {
					"message".to_string()
				} else {
					value.to_string()
				}
			}
			"data" => data_lines.push(value),
			_ => {}
		}
	}

	if data_lines.is_empty() {
		return None;
	}

	let data = data_lines.join("\n");
	Some(Message { event, data })
}

impl EventSourceStream {
	pub fn new(reqwest_builder: RequestBuilder) -> Self {
		// SSE event separator is `\n\n`, `\r\n\r\n`, or `\r\r`. WebStream's Sse mode
		// normalizes CR/CRLF to LF and splits on `\n\n`, so all three forms work.
		Self {
			inner: WebStream::new_with_sse(reqwest_builder),
			opened: false,
		}
	}
}

impl Stream for EventSourceStream {
	type Item = Result<Event, BoxError>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();

		// -- 1. Handle initial "Open" event
		if !this.opened {
			this.opened = true;
			return Poll::Ready(Some(Ok(Event::Open)));
		}

		// -- 2. Poll the inner WebStream for next event block
		loop {
			let nx = Pin::new(&mut this.inner).poll_next(cx);

			match nx {
				Poll::Ready(Some(Ok(raw_event))) => {
					// If no data found in this block, poll for the next one.
					let Some(message) = parse_event_block(&raw_event) else {
						continue;
					};

					return Poll::Ready(Some(Ok(Event::Message(message))));
				}
				Poll::Ready(Some(Err(e))) => {
					return Poll::Ready(Some(Err(e)));
				}
				Poll::Ready(None) => return Poll::Ready(None),
				Poll::Pending => return Poll::Pending,
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::parse_event_block;

	#[test]
	fn preserves_empty_data_fields_and_inter_field_newlines() {
		let message = parse_event_block("data:\ndata: [DONE]").unwrap();
		assert_eq!(message.data, "\n[DONE]");
	}

	#[test]
	fn accepts_colonless_data_fields() {
		let message = parse_event_block("data\ndata: [DONE]").unwrap();
		assert_eq!(message.data, "\n[DONE]");
	}

	#[test]
	fn removes_only_one_optional_space_after_colon() {
		let message = parse_event_block("data:  [DONE]").unwrap();
		assert_eq!(message.data, " [DONE]");
	}

	#[test]
	fn ordinary_done_payload_is_preserved() {
		let message = parse_event_block("data: [DONE]").unwrap();
		assert_eq!(message.data, "[DONE]");
	}

	#[test]
	fn dispatches_empty_data_values_but_skips_blocks_without_data_fields() {
		assert_eq!(parse_event_block("data:").unwrap().data, "");
		assert_eq!(parse_event_block("data").unwrap().data, "");
		assert!(parse_event_block(": comment\nevent: update").is_none());
	}

	#[test]
	fn preserves_leading_middle_and_trailing_empty_data_values() {
		let message = parse_event_block("data:\ndata: first\ndata\ndata: last\ndata:").unwrap();
		assert_eq!(message.data, "\nfirst\n\nlast\n");
	}

	#[test]
	fn preserves_trailing_data_whitespace_and_ignores_leading_field_whitespace() {
		let message = parse_event_block(" data: ignored\ndata: value  ").unwrap();
		assert_eq!(message.data, "value  ");
	}

	#[test]
	fn empty_event_value_uses_default_event_name() {
		let message = parse_event_block("event:\ndata: payload").unwrap();
		assert_eq!(message.event, "message");
	}
}

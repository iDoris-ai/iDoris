//! Guard the OpenAI-compatible direct SSE path against a clean HTTP EOF
//! without a complete `data: [DONE]` event. Bytes remain unchanged.

use std::io;

use axum::body::Bytes;
use futures_util::{Stream, StreamExt, stream};

pub(crate) fn ensure_terminated(
    inner: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send {
    stream::unfold(
        (Box::pin(inner), TerminalEvent::default(), false),
        |(mut inner, mut marker, finished)| async move {
            if finished {
                return None;
            }
            let (item, finished) = match inner.next().await {
                Some(Ok(bytes)) => {
                    marker.feed(&bytes);
                    (Ok(bytes), false)
                }
                Some(Err(err)) => (Err(io::Error::other(err)), true),
                None if marker.done => return None,
                None => (
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "unterminated upstream SSE",
                    )),
                    true,
                ),
            };
            Some((item, (inner, marker, finished)))
        },
    )
}

#[derive(Default)]
struct TerminalEvent {
    // Only a short marker can match; cap retained line bytes even when an
    // upstream sends arbitrarily large data lines. No response buffering.
    line: Vec<u8>,
    data_is_done: Option<bool>,
    after_cr: bool,
    done: bool,
}

impl TerminalEvent {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.done {
                break;
            }
            if byte == b'\n' && self.after_cr {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if matches!(byte, b'\r' | b'\n') {
                if self.line.is_empty() {
                    self.done = self.data_is_done.take() == Some(true);
                } else if let Some(data) = self.line.strip_prefix(b"data:") {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    // SSE joins multiple data fields with a newline, so
                    // even a second empty field makes this a different value.
                    self.data_is_done = Some(self.data_is_done.is_none() && data == b"[DONE]");
                } else if self.line == b"data" {
                    self.data_is_done = Some(false);
                }
                self.line.clear();
            } else if self.line.len() < 16 {
                self.line.push(byte);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn terminal_event_survives_every_byte_split_and_sse_line_ending() {
        for body in [
            "data: partial\n\ndata: [DONE]\n\n",
            "data:[DONE]\r\r",
            ": heartbeat\r\ndata: [DONE]\r\n\r\n",
        ] {
            for split in 0..=body.len() {
                let inner = stream::iter([
                    Ok(Bytes::copy_from_slice(&body.as_bytes()[..split])),
                    Ok(Bytes::copy_from_slice(&body.as_bytes()[split..])),
                ]);
                let guarded = ensure_terminated(inner);
                let result: Result<Vec<_>, _> =
                    guarded.collect::<Vec<_>>().await.into_iter().collect();
                assert_eq!(result.unwrap().concat(), body.as_bytes());
            }
        }
    }
}

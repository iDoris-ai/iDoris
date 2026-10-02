//! Guard the OpenAI-compatible direct SSE path against a clean HTTP EOF
//! without a complete `data: [DONE]` event. Bytes remain unchanged.

use std::io;

use axum::body::Bytes;
use futures_util::{Stream, StreamExt, stream};

pub(crate) fn ensure_terminated<E>(
    inner: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send
where
    E: std::error::Error + Send + Sync + 'static,
{
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

struct TerminalEvent {
    // Only a short marker can match; cap retained line bytes even when an
    // upstream sends arbitrarily large data lines. No response buffering.
    line: Vec<u8>,
    data_is_done: Option<bool>,
    after_cr: bool,
    done: bool,
    first_line: bool,
}

impl Default for TerminalEvent {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data_is_done: None,
            after_cr: false,
            done: false,
            first_line: true,
        }
    }
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
                if self.first_line {
                    if self.line.starts_with(b"\xef\xbb\xbf") {
                        self.line.drain(..3);
                    }
                    self.first_line = false;
                }
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
                    Ok::<_, reqwest::Error>(Bytes::copy_from_slice(&body.as_bytes()[..split])),
                    Ok(Bytes::copy_from_slice(&body.as_bytes()[split..])),
                ]);
                let guarded = ensure_terminated(inner);
                let result: Result<Vec<_>, _> =
                    guarded.collect::<Vec<_>>().await.into_iter().collect();
                assert_eq!(result.unwrap().concat(), body.as_bytes());
            }
        }
    }

    #[tokio::test]
    async fn ignores_one_bom_only_at_the_start_for_all_chunk_boundaries() {
        for ending in [b"\n".as_slice(), b"\r\n", b"\r"] {
            let mut terminated = b"\xef\xbb\xbfdata: [DONE]".to_vec();
            terminated.extend_from_slice(ending);
            terminated.extend_from_slice(ending);
            let mut unterminated = b"\xef\xbb\xbfdata:".to_vec();
            unterminated.extend_from_slice(ending);
            unterminated.extend_from_slice(b"data: [DONE]");
            unterminated.extend_from_slice(ending);
            unterminated.extend_from_slice(ending);

            for body in [&terminated, &unterminated] {
                for split in 0..=body.len() {
                    let chunks = [
                        Bytes::copy_from_slice(&body[..split]),
                        Bytes::new(),
                        Bytes::copy_from_slice(&body[split..]),
                    ];
                    let result: Vec<_> = ensure_terminated(stream::iter(
                        chunks.into_iter().map(Ok::<_, reqwest::Error>),
                    ))
                    .collect()
                    .await;
                    assert_guarded_bytes(result, body, body == &terminated);
                }

                let body_bytes = body.to_vec();
                let byte_chunks = body_bytes
                    .into_iter()
                    .map(|byte| Ok::<_, reqwest::Error>(Bytes::copy_from_slice(&[byte])));
                let result: Vec<_> = ensure_terminated(stream::iter(byte_chunks)).collect().await;
                assert_guarded_bytes(result, body, body == &terminated);
            }
        }
    }

    fn assert_guarded_bytes(result: Vec<Result<Bytes, io::Error>>, body: &[u8], terminated: bool) {
        let errors: Vec<_> = result
            .iter()
            .filter_map(|item| item.as_ref().err())
            .collect();
        if terminated {
            assert!(errors.is_empty());
        } else {
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].kind(), io::ErrorKind::UnexpectedEof);
            assert!(result.last().unwrap().is_err());
        }
        let bytes: Vec<_> = result.into_iter().filter_map(Result::ok).collect();
        assert_eq!(bytes.concat(), body);
    }

    #[tokio::test]
    async fn does_not_strip_later_or_second_bom() {
        for body in [
            b"data: [OTHER]\n\n\xef\xbb\xbfdata: [DONE]\n\n".as_slice(),
            b"\n\n\xef\xbb\xbfdata: [DONE]\n\n",
            b"\xef\xbb\xbf\xef\xbb\xbfdata: [DONE]\n\n",
        ] {
            let result: Vec<_> = ensure_terminated(stream::iter([Ok::<_, reqwest::Error>(
                Bytes::copy_from_slice(body),
            )]))
            .collect()
            .await;
            assert!(
                matches!(result.last(), Some(Err(err)) if err.kind() == io::ErrorKind::UnexpectedEof)
            );
        }
    }
}

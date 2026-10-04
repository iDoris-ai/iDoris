use axum::body::Bytes;
use futures_util::{Stream, StreamExt, stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    Completed,
    Error,
    Dropped,
}

struct Finalizer<F: FnOnce(StreamEnd)> {
    callback: Option<F>,
}

impl<F: FnOnce(StreamEnd)> Finalizer<F> {
    fn finish(&mut self, end: StreamEnd) {
        if let Some(callback) = self.callback.take() {
            callback(end);
        }
    }
}

impl<F: FnOnce(StreamEnd)> Drop for Finalizer<F> {
    fn drop(&mut self) {
        self.finish(StreamEnd::Dropped);
    }
}

pub(crate) fn finalize_stream<S, E, F>(
    inner: S,
    callback: F,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
    F: FnOnce(StreamEnd) + Send + 'static,
{
    stream::unfold(
        (
            Box::pin(inner),
            Finalizer {
                callback: Some(callback),
            },
        ),
        |(mut inner, mut finalizer)| async move {
            match inner.next().await {
                Some(Ok(bytes)) => Some((Ok(bytes), (inner, finalizer))),
                Some(Err(err)) => {
                    finalizer.finish(StreamEnd::Error);
                    Some((Err(err), (inner, finalizer)))
                }
                None => {
                    finalizer.finish(StreamEnd::Completed);
                    None
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::{Arc, Mutex};

    use futures_util::{StreamExt, stream};

    use super::*;

    fn callback(events: Arc<Mutex<Vec<StreamEnd>>>) -> impl FnOnce(StreamEnd) {
        move |end| events.lock().unwrap().push(end)
    }

    #[tokio::test]
    async fn complete_error_and_drop_each_finalize_once() {
        let completed = Arc::new(Mutex::new(Vec::new()));
        let stream = finalize_stream(
            stream::iter([Ok::<_, &'static str>(Bytes::from_static(b"x"))]),
            callback(completed.clone()),
        );
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(*completed.lock().unwrap(), [StreamEnd::Completed]);

        let errored = Arc::new(Mutex::new(Vec::new()));
        let stream = finalize_stream(
            stream::iter([Err::<Bytes, _>("boom")]),
            callback(errored.clone()),
        );
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(*errored.lock().unwrap(), [StreamEnd::Error]);

        let dropped = Arc::new(Mutex::new(Vec::new()));
        let stream = finalize_stream(
            stream::pending::<Result<Bytes, &'static str>>(),
            callback(dropped.clone()),
        );
        drop(stream);
        assert_eq!(*dropped.lock().unwrap(), [StreamEnd::Dropped]);
    }
}

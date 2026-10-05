use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use idoris_backend::{ChatRequest, ChatResponse};
use tokio::sync::{Mutex, Notify, oneshot};
use tokio_util::sync::CancellationToken;

use super::error::{SubscriptionErrorCode, SubscriptionRelayError};
use super::relay::SubscriptionRelay;

#[derive(Debug)]
struct ServiceState {
    accepting: bool,
    next_id: u64,
    active: HashMap<u64, CancellationToken>,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            accepting: true,
            next_id: 1,
            active: HashMap::new(),
        }
    }
}

struct Inner {
    relay: Arc<SubscriptionRelay>,
    state: Mutex<ServiceState>,
    drained: Notify,
    shutdown_wait: Duration,
}

#[derive(Clone)]
pub struct SubscriptionService {
    inner: Arc<Inner>,
}

impl SubscriptionService {
    pub fn new(relay: SubscriptionRelay, shutdown_wait: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                relay: Arc::new(relay),
                state: Mutex::new(ServiceState::default()),
                drained: Notify::new(),
                shutdown_wait,
            }),
        }
    }

    pub async fn chat(
        &self,
        request: ChatRequest,
        request_cancel: CancellationToken,
    ) -> Result<ChatResponse, SubscriptionRelayError> {
        let (id, service_cancel) = {
            let mut state = self.inner.state.lock().await;
            if !state.accepting {
                return Err(SubscriptionRelayError::new(
                    SubscriptionErrorCode::Cancelled,
                ));
            }
            let id = state.next_id;
            state.next_id = state.next_id.wrapping_add(1).max(1);
            let token = CancellationToken::new();
            state.active.insert(id, token.clone());
            (id, token)
        };

        let (tx, rx) = oneshot::channel();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let bridge_token = service_cancel.clone();
            let bridge = tokio::spawn(async move {
                request_cancel.cancelled().await;
                bridge_token.cancel();
            });
            let result = inner.relay.chat(request, service_cancel).await;
            bridge.abort();
            let _ = bridge.await;
            finish_request(&inner, id).await;
            let _ = tx.send(result);
        });

        rx.await
            .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CleanupFailed))?
    }

    pub async fn shutdown(&self) -> Result<(), SubscriptionRelayError> {
        let tokens = {
            let mut state = self.inner.state.lock().await;
            state.accepting = false;
            state.active.values().cloned().collect::<Vec<_>>()
        };
        for token in tokens {
            token.cancel();
        }

        let wait = async {
            loop {
                let notified = self.inner.drained.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.inner.state.lock().await.active.is_empty() {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(self.inner.shutdown_wait, wait)
            .await
            .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CleanupFailed))?;
        Ok(())
    }

    pub async fn active_requests(&self) -> usize {
        self.inner.state.lock().await.active.len()
    }

    pub async fn is_accepting(&self) -> bool {
        self.inner.state.lock().await.accepting
    }
}

async fn finish_request(inner: &Inner, id: u64) {
    let became_empty = {
        let mut state = inner.state.lock().await;
        let removed = state.active.remove(&id).is_some();
        removed && state.active.is_empty()
    };
    if became_empty {
        inner.drained.notify_waiters();
    }
}

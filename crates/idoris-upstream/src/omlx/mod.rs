//! oMLX `RuntimeAdapter` — mirrors `packages/adapters/omlx/
//! omlx-backend.ts` on `main` (FU-16) behavior, ported onto
//! `idoris_backend::RuntimeAdapter`. Talks to `http://127.0.0.1:8088` by
//! default. Split across submodules landing across several PRs on this
//! stack: [`http`] (GET/POST/PUT + timeout + safe errors), [`status`]
//! (this PR, `list`/`status` parsing), `pin` (resident/admin-session gap),
//! then the `OmlxAdapter` struct wiring it all into `RuntimeAdapter`.
//!
//! **The API key is read from an env var and never logged** — see
//! [`OMLX_API_KEY_ENV`] and `http`'s module doc.

mod http;
mod status;

use std::time::Duration;

use idoris_backend::BackendError;

/// Default oMLX base URL (local, custom port — see the TS reference's
/// design-doc citation, D7).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8088";

/// Default per-call timeout. Every oMLX call must have one — a hung local
/// process must not hang this adapter (and, transitively, the Supervisor's
/// single-flighted load/evict mutex) forever.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Env var oMLX's inference API key is read from. Never logged, never put
/// in an error message.
pub const OMLX_API_KEY_ENV: &str = "IDORIS_OMLX_API_KEY";

/// `BackendError` has no `upstream()` constructor (only `Upstream
/// { message }`) — shorthand shared by `http` and `status`.
pub(crate) fn upstream_error(message: impl Into<String>) -> BackendError {
    BackendError::Upstream {
        message: message.into(),
    }
}

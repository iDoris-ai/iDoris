//! Lazy, serialized admin sessions. Only the main key can log in; keys,
//! cookies, response bodies and reqwest errors never enter diagnostics.
//!
//! **A cookie belongs to the engine generation that issued it** (IDORIS-CORE S6, Codex final review H4): the
//! session is cached together with the endpoint generation it was obtained under, and **every** admin send
//! (a cached cookie's reuse included) is authorized right before it goes out ([`OmlxAdapter::credential`]):
//! work admitted for an engine that has since been replaced is refused before anything is written, and a
//! cookie issued by one engine is dropped and re-obtained rather than sent to the next one.

use idoris_backend::BackendError;
use reqwest::header::{COOKIE, HeaderValue, SET_COOKIE};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use super::{OmlxAdapter, http, upstream_error};

/// A logged-in admin session: the cookie and the engine generation it was issued under (`None` = the adapter has no
/// notion of generations: a fixed key).
pub(super) struct AdminSession {
    cookie: HeaderValue,
    pub(super) generation: Option<u64>,
}

#[derive(Debug)]
pub(super) enum AdminRequestError {
    Definite(BackendError),
    Unknown(BackendError),
}

impl AdminRequestError {
    pub(super) fn into_backend(self) -> BackendError {
        match self {
            Self::Definite(err) | Self::Unknown(err) => err,
        }
    }
}

impl OmlxAdapter {
    /// Log in. This is a send of its own: it is authorized (generation, blocked endpoint, the listener re-verified)
    /// right before the main key goes out, and the session it returns remembers the generation it was issued under.
    async fn admin_login(&self) -> Result<AdminSession, AdminRequestError> {
        let credential = self
            .credential()
            .await
            .map_err(AdminRequestError::Definite)?;
        let key = credential.key().ok_or_else(|| {
            AdminRequestError::Definite(upstream_error(
                "oMLX admin login requires the main API key",
            ))
        })?;
        let (client, request) = credential
            .client(&self.client)
            .post(format!(
                "{}/admin/api/login",
                credential.base_url(&self.base_url)
            ))
            .timeout(self.call_timeout)
            .json(&json!({"api_key": key}))
            .build_split();
        let request = request.map_err(|_| {
            AdminRequestError::Definite(upstream_error(
                "oMLX admin login could not be built: the request was not sent",
            ))
        })?;
        // The last look, after the request (with the main key in its body) is built: a refusal means nothing was sent.
        http::check_before_send(credential.authorization(), &request)
            .map_err(|refusal| AdminRequestError::Definite(upstream_error(refusal.to_string())))?;
        let response = client.execute(request).await.map_err(|_| {
            AdminRequestError::Unknown(upstream_error("oMLX admin login transport failure"))
        })?;
        admin_check_status(&response)?;
        let mut cookie = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|header| header.to_str().ok())
            .filter_map(|header| header.split(';').next())
            .find(|pair| {
                pair.strip_prefix("omlx_admin_session=")
                    .is_some_and(|value| !value.is_empty())
            })
            .and_then(|pair| HeaderValue::from_str(pair).ok())
            .ok_or_else(|| {
                AdminRequestError::Unknown(upstream_error(
                    "oMLX admin login returned no session cookie",
                ))
            })?;
        cookie.set_sensitive(true);
        let body: Value = response.json().await.map_err(|_| {
            AdminRequestError::Unknown(upstream_error("oMLX admin login returned invalid JSON"))
        })?;
        if body["success"].as_bool() != Some(true) {
            return Err(AdminRequestError::Unknown(upstream_error(
                "oMLX admin login was not successful",
            )));
        }
        Ok(AdminSession {
            cookie,
            generation: credential.generation,
        })
    }

    /// Hold the mutex through refresh and dispatch so concurrent callers
    /// cannot replace a fresh cookie with a stale login. A 401 clears the
    /// cache; at most one new login and one retry follow per request.
    pub(super) async fn admin_request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, BackendError> {
        self.admin_request_classified(method, path, body)
            .await
            .map_err(AdminRequestError::into_backend)
    }

    pub(super) async fn admin_request_classified(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, AdminRequestError> {
        // The login, the request, the retry after a 401 are one operation: one engine generation for all of them (Codex pre-review
        // of layer 2, M2; see `OmlxAdapter::operation`).
        self.operation(self.admin_request_unpinned(method, path, body))
            .await
    }

    async fn admin_request_unpinned(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, AdminRequestError> {
        let mut session = self.admin_session.lock().await;
        for attempt in 0..2 {
            // Authorize this send — a reuse of a cached cookie is a send like any other: the generation it was admitted
            // under, a blocked endpoint, the listener (re-verified now) are all checked before the cookie goes out.
            let mut credential = self
                .credential()
                .await
                .map_err(AdminRequestError::Definite)?;
            // A cookie belongs to the engine generation that issued it: it is never sent to the next engine.
            if session
                .as_ref()
                .is_some_and(|cached| cached.generation != credential.generation)
            {
                *session = None;
            }
            if session.is_none() {
                *session = Some(self.admin_login().await?);
                // The login took a round trip and the request that follows is a send of its own: authorize it again,
                // and refuse if the engine was replaced meanwhile (the session just issued is the old engine's).
                credential = self
                    .credential()
                    .await
                    .map_err(AdminRequestError::Definite)?;
                if session
                    .as_ref()
                    .is_some_and(|issued| issued.generation != credential.generation)
                {
                    *session = None;
                    return Err(AdminRequestError::Definite(upstream_error(
                        "the oMLX engine was replaced while its admin session was being established: the request was not sent",
                    )));
                }
            }
            let cookie = session
                .as_ref()
                .map(|session| session.cookie.clone())
                .ok_or_else(|| {
                    AdminRequestError::Unknown(upstream_error("oMLX admin session unavailable"))
                })?;
            let mut request = credential
                .client(&self.client)
                .request(
                    method.clone(),
                    format!("{}{path}", credential.base_url(&self.base_url)),
                )
                .timeout(self.call_timeout)
                .header(COOKIE, cookie);
            if let Some(body) = body {
                request = request.json(body);
            }
            let (client, request) = request.build_split();
            let request = request.map_err(|_| {
                AdminRequestError::Definite(upstream_error(
                    "oMLX admin request could not be built: the request was not sent",
                ))
            })?;
            // The last look, after the request (with the session cookie) is built: a refusal means nothing was sent — a definite local
            // refusal, so a settings change refused here is not reported as an unknown outcome.
            http::check_before_send(credential.authorization(), &request).map_err(|refusal| {
                AdminRequestError::Definite(upstream_error(refusal.to_string()))
            })?;
            let response = client.execute(request).await.map_err(|_| {
                AdminRequestError::Unknown(upstream_error("oMLX admin request transport failure"))
            })?;
            if response.status() == StatusCode::UNAUTHORIZED {
                *session = None;
                if attempt == 0 {
                    continue;
                }
            }
            admin_check_status(&response)?;
            return if method == Method::GET {
                response.json().await.map_err(|_| {
                    AdminRequestError::Unknown(upstream_error(
                        "oMLX admin request returned invalid JSON",
                    ))
                })
            } else {
                Ok(Value::Null)
            };
        }
        Err(AdminRequestError::Unknown(upstream_error(
            "oMLX admin session unavailable",
        )))
    }
}

fn admin_check_status(response: &reqwest::Response) -> Result<(), AdminRequestError> {
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        let err = upstream_error(format!(
            "oMLX admin request failed: HTTP {}",
            status.as_u16()
        ));
        if is_definite_rejection(status) {
            Err(AdminRequestError::Definite(err))
        } else {
            Err(AdminRequestError::Unknown(err))
        }
    }
}

fn is_definite_rejection(status: StatusCode) -> bool {
    status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT
}

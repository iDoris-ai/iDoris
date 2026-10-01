//! Lazy, serialized admin sessions. Only the main key can log in; keys,
//! cookies, response bodies and reqwest errors never enter diagnostics.

use idoris_backend::BackendError;
use reqwest::header::{COOKIE, HeaderValue, SET_COOKIE};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use super::{OmlxAdapter, upstream_error};

impl OmlxAdapter {
    async fn admin_login(&self) -> Result<HeaderValue, BackendError> {
        let key = self
            .api_key()
            .ok_or_else(|| upstream_error("oMLX admin login requires the main API key"))?;
        let response = self
            .client
            .post(format!("{}/admin/api/login", self.base_url))
            .timeout(self.call_timeout)
            .json(&json!({"api_key": key}))
            .send()
            .await
            .map_err(|_| upstream_error("oMLX admin login transport failure"))?;
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
            .ok_or_else(|| upstream_error("oMLX admin login returned no session cookie"))?;
        cookie.set_sensitive(true);
        let body: Value = response
            .json()
            .await
            .map_err(|_| upstream_error("oMLX admin login returned invalid JSON"))?;
        if body["success"].as_bool() != Some(true) {
            return Err(upstream_error("oMLX admin login was not successful"));
        }
        Ok(cookie)
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
        let mut session = self.admin_session.lock().await;
        for attempt in 0..2 {
            if session.is_none() {
                *session = Some(self.admin_login().await?);
            }
            let cookie = session
                .as_ref()
                .ok_or_else(|| upstream_error("oMLX admin session unavailable"))?;
            let mut request = self
                .client
                .request(method.clone(), format!("{}{path}", self.base_url))
                .timeout(self.call_timeout)
                .header(COOKIE, cookie.clone());
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .map_err(|_| upstream_error("oMLX admin request transport failure"))?;
            if response.status() == StatusCode::UNAUTHORIZED {
                *session = None;
                if attempt == 0 {
                    continue;
                }
            }
            admin_check_status(&response)?;
            return if method == Method::GET {
                response
                    .json()
                    .await
                    .map_err(|_| upstream_error("oMLX admin request returned invalid JSON"))
            } else {
                Ok(Value::Null)
            };
        }
        Err(upstream_error("oMLX admin session unavailable"))
    }
}

fn admin_check_status(response: &reqwest::Response) -> Result<(), BackendError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(upstream_error(format!(
            "oMLX admin request failed: HTTP {}",
            response.status().as_u16()
        )))
    }
}

//! Explicit authority for tenant routing fixtures; never compiled into the service.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use axum::http::header::AUTHORIZATION;
use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;

use crate::{AppState, HeaderValue, Next, Request, Router, auth, build_app, middleware};

pub(crate) fn tenant_test_app(mut state: AppState, tenant: &str) -> Router {
    let key = MintedVirtualKey::mint();
    let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
    store
        .insert_active(
            &key.key_id,
            key.hash,
            &VirtualKeyScope {
                tenant_id: Some(tenant.into()),
                owner: "tenant-routing-fixture".into(),
                allowed_privacy: vec![PrivacyClass::LocalOnly, PrivacyClass::Any],
                allowed_roles: idoris_policy::ROLES
                    .iter()
                    .map(|role| role.as_str().into())
                    .collect(),
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    state.virtual_key_authenticator = Some(auth::VirtualKeyAuthenticator::new(Arc::new(
        Mutex::new(store),
    )));
    let bearer = HeaderValue::from_str(&format!("Bearer {}", key.secret.expose_secret())).unwrap();
    // Routing fixtures receive an actual, tenant-bound credential selected
    // by the fixture author. Authentication-negative tests use build_app
    // directly, so this helper cannot hide an absent-authenticator regression.
    build_app(state).layer(middleware::from_fn(
        move |mut request: Request, next: Next| {
            let bearer = bearer.clone();
            async move {
                request.headers_mut().insert(AUTHORIZATION, bearer);
                next.run(request).await
            }
        },
    ))
}

//! Explicit authority for tenant routing fixtures; never compiled into the service.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use axum::http::header::AUTHORIZATION;
use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;

use crate::{AppState, HeaderValue, Request, Router, auth, build_app};

pub(crate) fn tenant_test_app(mut state: AppState, tenant: &str) -> (Router, MintedVirtualKey) {
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
    (build_app(state), key)
}

pub(crate) fn tenant_test_request(mut request: Request, key: &MintedVirtualKey) -> Request {
    let bearer = HeaderValue::from_str(&format!("Bearer {}", key.secret.expose_secret())).unwrap();
    assert!(!request.headers().contains_key(AUTHORIZATION));
    request.headers_mut().insert(AUTHORIZATION, bearer);
    request
}

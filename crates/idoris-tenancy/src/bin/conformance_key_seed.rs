//! Test-only helper for black-box Rust conformance runs.
//!
//! Seeds one deterministic, broad-scope virtual key into an isolated test
//! database. This binary is feature-gated behind `test-bins` and is never
//! part of a normal production build or release archive.

use std::path::PathBuf;

use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let db_path = PathBuf::from(args.next().ok_or("missing database path")?);
    if args.next().is_some() {
        return Err("expected exactly one database path".into());
    }

    let minted = MintedVirtualKey::mint();
    let store = VirtualKeyStore::open(&db_path)?;
    store.insert_active(
        &minted.key_id,
        minted.hash,
        &VirtualKeyScope {
            owner: "conformance".into(),
            allowed_privacy: vec![PrivacyClass::LocalOnly, PrivacyClass::Any],
            allowed_roles: [
                "fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            budget_ref: None,
            expires_at_ms: None,
            admin_scopes: Vec::new(),
        },
    )?;
    println!("{}", minted.secret.expose_secret());
    Ok(())
}

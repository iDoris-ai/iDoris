//! Offline `idoris key list` / `idoris key revoke <key_id>` (no secrets, no hashes).

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use idoris_tenancy::virtual_key::store::{VirtualKeyRecord, VirtualKeyStore};
use serde_json::json;

/// Matches `MintedVirtualKey::mint`: `vk_` + 32 lowercase hex (UUID simple).
pub fn valid_key_id(id: &str) -> bool {
    id.strip_prefix("vk_").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn now_ms() -> Result<i64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .filter(|ms| *ms > 0)
        .ok_or_else(|| "系统时间无效".to_string())
}

fn status(record: &VirtualKeyRecord, now_ms: i64) -> &'static str {
    if record.revoked_at_ms.is_some() {
        "revoked"
    } else if record.scope.expires_at_ms.is_some_and(|e| e <= now_ms) {
        "expired"
    } else {
        "active"
    }
}

fn emit(writer: &mut impl Write, value: &serde_json::Value) -> Result<(), String> {
    writeln!(writer, "{value}")
        .and_then(|_| writer.flush())
        .map_err(|_| "无法输出结果".to_string())
}

pub fn list(store: &VirtualKeyStore, writer: &mut impl Write) -> Result<(), String> {
    list_at(store, writer, now_ms()?)
}

fn list_at(store: &VirtualKeyStore, writer: &mut impl Write, now: i64) -> Result<(), String> {
    let keys = store
        .list()
        .map_err(|_| "无法读取 virtual key 列表".to_string())?;
    let keys: Vec<_> = keys
        .iter()
        .map(|k| {
            json!({
                "key_id": k.key_id,
                "owner": k.scope.owner,
                "tenant_id": k.scope.tenant_id,
                "allowed_privacy": k.scope.allowed_privacy,
                "allowed_roles": k.scope.allowed_roles,
                "expires_at_ms": k.scope.expires_at_ms,
                "revoked_at_ms": k.revoked_at_ms,
                "status": status(k, now),
            })
        })
        .collect();
    emit(writer, &json!({ "keys": keys }))
}

pub fn revoke(
    store: &VirtualKeyStore,
    key_id: &str,
    writer: &mut impl Write,
) -> Result<(), String> {
    if !valid_key_id(key_id) {
        return Err("key_id 格式无效".to_string());
    }
    let changed = store
        .revoke(key_id, now_ms()?)
        .map_err(|_| "无法撤销 virtual key".to_string())?;
    if changed {
        return emit(writer, &json!({"key_id": key_id, "revoked": true}));
    }
    let known = store
        .list()
        .map_err(|_| "无法读取 virtual key 列表".to_string())?
        .iter()
        .any(|k| k.key_id == key_id);
    if !known {
        return Err(format!("未找到 key_id：{key_id}"));
    }
    emit(
        writer,
        &json!({"key_id": key_id, "revoked": false, "already_revoked": true}),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use rusqlite::Connection;

    #[test]
    fn key_id_format_is_strict() {
        assert!(valid_key_id(&format!("vk_{}", "0f".repeat(16))));
        for bad in [
            "",
            "vk_",
            "vk_ABCDEF0123456789abcdef0123456789",
            "vk_0f",
            " vk_00000000000000000000000000000000",
            "vk_000000000000000000000000000000000",
        ] {
            assert!(!valid_key_id(bad), "{bad}");
        }
    }

    #[test]
    fn list_marks_expired_and_revoke_unknown_errors() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let mut out = Vec::new();
        list_at(&store, &mut out, 1).unwrap();
        assert_eq!(out, b"{\"keys\":[]}\n");
        let minted = idoris_tenancy::virtual_key::MintedVirtualKey::mint();
        let scope = idoris_tenancy::virtual_key::store::VirtualKeyScope {
            owner: "o".into(),
            tenant_id: None,
            allowed_privacy: vec![idoris_contracts::common::PrivacyClass::LocalOnly],
            allowed_roles: vec!["fast".into()],
            budget_ref: None,
            expires_at_ms: Some(1000),
            admin_scopes: vec![],
        };
        store
            .insert_active(&minted.key_id, minted.hash, &scope)
            .unwrap();
        for (now, want) in [(999, "active"), (1000, "expired")] {
            let mut out = Vec::new();
            list_at(&store, &mut out, now).unwrap();
            let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(json["keys"][0]["status"], want);
        }
        // Revoking an already-expired key (the common launcher cleanup) must
        // report `revoked`, not `expired`.
        let mut sink = Vec::new();
        revoke(&store, &minted.key_id, &mut sink).unwrap();
        let mut out = Vec::new();
        list_at(&store, &mut out, 2000).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["keys"][0]["status"], "revoked");
        let mut out = Vec::new();
        let err = revoke(&store, &format!("vk_{}", "a".repeat(32)), &mut out).unwrap_err();
        assert!(err.contains("未找到"));
    }
}

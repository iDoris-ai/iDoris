//! Local-only virtual-key issuance for `idoris key issue --spec-stdin`.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use serde::Deserialize;

const MAX_SPEC_BYTES: u64 = 16 * 1024;
const MAX_OWNER_BYTES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueSpec {
    owner: String,
    allowed_privacy: Vec<PrivacyClass>,
    allowed_roles: Vec<String>,
    expires_at_ms: Option<i64>,
    #[serde(default)]
    admin_scopes: Vec<String>,
}

pub fn issue_from_reader(
    store: &VirtualKeyStore,
    reader: &mut impl Read,
    writer: &mut impl Write,
) -> Result<(), String> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时间无效".to_string())?
        .as_millis()
        .try_into()
        .map_err(|_| "系统时间超出支持范围".to_string())?;
    issue_from_reader_at(store, reader, writer, now_ms)
}

fn issue_from_reader_at(
    store: &VirtualKeyStore,
    reader: &mut impl Read,
    writer: &mut impl Write,
    now_ms: i64,
) -> Result<(), String> {
    let mut raw = Vec::new();
    reader
        .take(MAX_SPEC_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| "无法读取 virtual-key spec".to_string())?;
    if raw.is_empty() || raw.len() as u64 > MAX_SPEC_BYTES {
        return Err("virtual-key spec 为空或超过 16 KiB".to_string());
    }
    let spec: IssueSpec =
        serde_json::from_slice(&raw).map_err(|_| "virtual-key spec JSON 无效".to_string())?;
    let scope = validated_scope(spec, now_ms)?;
    let minted = MintedVirtualKey::mint();
    store
        .insert_active(&minted.key_id, minted.hash, &scope)
        .map_err(|_| "无法持久化 virtual key".to_string())?;

    let payload = serde_json::to_vec(&serde_json::json!({
        "key_id": minted.key_id,
        "secret": minted.secret.expose_secret(),
    }))
    .map_err(|_| "无法序列化 virtual-key issuance".to_string())?;
    if writer
        .write_all(&payload)
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .is_err()
    {
        let _ = store.revoke(&minted.key_id, now_ms.max(1));
        return Err(format!("无法输出已撤销 virtual key {}", minted.key_id));
    }
    Ok(())
}

fn validated_scope(spec: IssueSpec, now_ms: i64) -> Result<VirtualKeyScope, String> {
    if now_ms <= 0 {
        return Err("系统时间无效".to_string());
    }
    if spec.owner.is_empty()
        || spec.owner.len() > MAX_OWNER_BYTES
        || spec.owner.trim() != spec.owner
        || spec.owner.chars().any(char::is_control)
    {
        return Err("virtual-key owner 无效".to_string());
    }
    if spec.allowed_privacy.is_empty()
        || spec
            .allowed_privacy
            .iter()
            .enumerate()
            .any(|(index, privacy)| spec.allowed_privacy[..index].contains(privacy))
    {
        return Err("allowed_privacy 不能为空或重复".to_string());
    }
    let mut roles = BTreeSet::new();
    if spec.allowed_roles.is_empty()
        || spec.allowed_roles.iter().any(|role| {
            role.trim() != role
                || !roles.insert(role.as_str())
                || !idoris_policy::ROLES
                    .iter()
                    .any(|known| known.as_str() == role)
        })
    {
        return Err("allowed_roles 包含空白、重复或未知角色".to_string());
    }
    if !spec.admin_scopes.is_empty() {
        return Err("M4 virtual key 暂不允许 admin_scopes".to_string());
    }
    if spec.expires_at_ms.is_some_and(|expires| expires <= now_ms) {
        return Err("expires_at_ms 必须晚于当前服务器时间".to_string());
    }
    Ok(VirtualKeyScope {
        owner: spec.owner,
        allowed_privacy: spec.allowed_privacy,
        allowed_roles: spec.allowed_roles,
        budget_ref: None,
        expires_at_ms: spec.expires_at_ms,
        admin_scopes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use idoris_tenancy::virtual_key::VirtualKeySecret;
    use rusqlite::Connection;

    #[test]
    fn issues_once_persists_only_hash_and_reopens_scope() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let mut output = Vec::new();
        issue_from_reader_at(
            &store,
            &mut br#"{"owner":"agent24","allowed_privacy":["local_only"],"allowed_roles":["fast"],"expires_at_ms":2000}"#.as_slice(),
            &mut output,
            1000,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let secret = VirtualKeySecret::parse(json["secret"].as_str().unwrap()).unwrap();
        let identity = store.authenticate(&secret, 1500).unwrap().unwrap();
        assert_eq!(identity.key_id, json["key_id"]);
        assert_eq!(identity.scope.owner, "agent24");
        assert!(output.ends_with(b"\n"));
    }

    #[test]
    fn rejects_unsafe_metadata_before_minting() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        for spec in [
            br#"{"owner":" agent24","allowed_privacy":["local_only"],"allowed_roles":["fast"]}"#.as_slice(),
            br#"{"owner":"agent24","allowed_privacy":[],"allowed_roles":["fast"]}"#.as_slice(),
            br#"{"owner":"agent24","allowed_privacy":["local_only"],"allowed_roles":["unknown"]}"#.as_slice(),
            br#"{"owner":"agent24","allowed_privacy":["local_only"],"allowed_roles":["fast","fast"]}"#.as_slice(),
            br#"{"owner":"agent24","allowed_privacy":["local_only"],"allowed_roles":["fast"],"expires_at_ms":1000}"#.as_slice(),
            br#"{"owner":"agent24","allowed_privacy":["local_only"],"allowed_roles":["fast"],"admin_scopes":["future"]}"#.as_slice(),
        ] {
            let mut output = Vec::new();
            assert!(issue_from_reader_at(&store, &mut &*spec, &mut output, 1000).is_err());
            assert!(output.is_empty());
        }
    }

    #[derive(Default)]
    struct FailFlushWriter(Vec<u8>);
    impl Write for FailFlushWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("blocked"))
        }
    }

    #[test]
    fn output_failure_revokes_inserted_key() {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let mut error_writer = FailFlushWriter::default();
        let error = issue_from_reader_at(
            &store,
            &mut br#"{"owner":"agent24","allowed_privacy":["local_only"],"allowed_roles":["fast"]}"#.as_slice(),
            &mut error_writer,
            1000,
        )
        .unwrap_err();
        assert!(!error.contains("idk_"));
        let json: serde_json::Value = serde_json::from_slice(&error_writer.0).unwrap();
        let secret = VirtualKeySecret::parse(json["secret"].as_str().unwrap()).unwrap();
        assert!(store.authenticate(&secret, 1001).unwrap().is_none());
    }
}

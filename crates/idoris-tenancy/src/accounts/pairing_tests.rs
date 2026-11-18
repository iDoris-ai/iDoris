//! 云账号配对与数据面闸门的测试：码的一次性、过期、重放、并发兑换；多设备各自可撤销；解绑；
//! 签发者的权力被收回后码作废；撤销（凭证、成员关系、主体、账户）即时让数据面闸门拒绝。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use idoris_compute_contracts::account::{AccountStatus, MembershipStatus};
use idoris_compute_contracts::control::ControlReason;
use idoris_compute_contracts::credential::{
    AdminScope, ControlOp, CredentialBinding, CredentialStatus,
};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_contracts::common::PrivacyClass;

use super::control_tests::{
    DAY_MS, Env, Issue, Minted, T0, aid, all_ops, cid, failure_reason, mid, pid, sc, ts,
};
use super::{
    CreatePairingCode, GateVerdict, MAX_ACTIVE_DEVICES_PER_MEMBERSHIP, MAX_PAIRING_TTL_MS,
    MIN_PAIRING_TTL_MS, Meta, PairingStatus, RedeemPairingCode, RevokePairingCode, Who,
};
use crate::virtual_key::MintedVirtualKey;

const TTL: i64 = 10 * 60 * 1000;

/// 配对码：库里只有哈希。测试里用一把「假密钥」当码：明文只在测试手里，拿来检查它没有漏进库和审计。
struct Code {
    pairing_id: String,
    plain: String,
    hash: [u8; 32],
}

fn new_code(tag: &str) -> Code {
    let key = MintedVirtualKey::mint();
    Code {
        pairing_id: format!("pc_{tag}_{}", &key.key_id[3..11]),
        plain: key.secret.expose_secret().to_string(),
        hash: *key.hash.as_bytes(),
    }
}

fn create_request(code: &Code, membership: &str, ttl: i64) -> CreatePairingCode {
    CreatePairingCode {
        pairing_id: code.pairing_id.clone(),
        code_hash: code.hash,
        membership: mid(membership),
        ttl_ms: ttl,
        allowed_privacy: vec![PrivacyClass::LocalOnly],
        allowed_roles: vec!["fast".into()],
        tenant_id: None,
        device_ttl_ms: None,
    }
}

/// 有 `issue_credential` 权限的「登录服务」：绑 eduai，权限覆盖整个 eduai 子树，数据面 local_only + fast。
fn login_service(env: &Env) -> Minted {
    env.issue_as_operator(Issue::service(
        "eduai",
        vec![
            sc(ControlOp::IssueCredential, "eduai", None),
            sc(ControlOp::RevokeCredential, "eduai", None),
            sc(ControlOp::FreezeCredential, "eduai", None),
            sc(ControlOp::Read, "eduai", None),
        ],
    ))
}

fn setup() -> (Env, Minted) {
    let env = Env::new();
    env.tree();
    let service = login_service(&env);
    (env, service)
}

fn create_code(env: &Env, who: &Who<'_>, code: &Code, membership: &str, now: i64) {
    let rid = env.rid();
    env.store
        .control_create_pairing_code(
            who,
            &env.meta(now, &rid),
            &create_request(code, membership, TTL),
        )
        .unwrap();
}

struct Device {
    key: MintedVirtualKey,
}

impl Device {
    fn id(&self) -> &str {
        &self.key.key_id
    }
    fn who(&self) -> Who<'_> {
        Who::Secret(&self.key.hash)
    }
}

fn redeem(env: &Env, code: &Code, label: &str, now: i64) -> Result<Device, ControlReason> {
    let key = MintedVirtualKey::mint();
    let rid = env.rid();
    let meta = Meta {
        now: ts(now),
        request_id: &rid,
    };
    env.store
        .control_redeem_pairing_code(
            &meta,
            &RedeemPairingCode {
                code_hash: code.hash,
                key_id: key.key_id.clone(),
                key_hash: key.hash,
                label: label.to_string(),
            },
        )
        .map(|_| Device { key })
        .map_err(|failure| failure.reason)
}

fn gate(env: &Env, key_id: &str, now: i64) -> GateVerdict {
    env.store.credential_gate(key_id, ts(now)).unwrap()
}

#[test]
fn a_pairing_code_becomes_one_restricted_device_credential() {
    let (env, service) = setup();
    let code = new_code("a");
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    let device = redeem(&env, &code, "Jason's MacBook", T0 + 1_000).unwrap();

    // 设备凭证：绑定到这个成员关系（付款账户 s1），没有任何管理权限，走现有的虚拟 key 认证
    let credential = env
        .store
        .get_credential(&cid(device.id()))
        .unwrap()
        .unwrap();
    assert_eq!(
        credential.binding(),
        &CredentialBinding::Member {
            membership: mid("m-kid1")
        }
    );
    assert!(credential.admin_scopes().is_empty());
    assert_eq!(credential.status(), CredentialStatus::Active);
    let identity = env
        .keys
        .authenticate(&device.key.secret, T0 + 2_000)
        .unwrap()
        .unwrap();
    assert_eq!(identity.key_id, device.id());
    assert_eq!(
        identity.scope.allowed_privacy,
        vec![PrivacyClass::LocalOnly]
    );
    assert_eq!(identity.scope.allowed_roles, vec!["fast".to_string()]);
    assert_eq!(
        identity.scope.tenant_id.as_deref(),
        Some("tenant-edu"),
        "inherited from the issuer"
    );
    assert_eq!(gate(&env, device.id(), T0 + 2_000), GateVerdict::Allowed);
    // 付款上下文就是该成员关系的付款账户
    let ctx = env
        .store
        .payment_context(&cid(device.id()), None, ts(T0 + 2_000))
        .unwrap();
    assert_eq!(ctx.payer().as_str(), "s1");
    // 设备记录、签发来源、码的状态
    let conn = env.raw();
    let (label, membership): (String, String) = conn
        .query_row(
            "SELECT label, membership_id FROM pairing_devices WHERE credential_id = ?1",
            [device.id()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (label.as_str(), membership.as_str()),
        ("Jason's MacBook", "m-kid1")
    );
    let (kind, by): (String, String) = conn
        .query_row(
            "SELECT issued_by_kind, issued_by_id FROM credential_issuance WHERE credential_id = ?1",
            [device.id()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), by.as_str()),
        ("pairing_code", code.pairing_id.as_str())
    );
    let record = super::pairing::load_pairing_by_id(&conn, &code.pairing_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.status, PairingStatus::Redeemed);
    assert_eq!(
        record.redeemed_credential.as_ref().map(|c| c.as_str()),
        Some(device.id())
    );
    // 审计：创建（凭证）、兑换（配对码，actor 是配对记录 id）
    let audit = env.audit();
    let create = audit
        .iter()
        .find(|r| r.op == "create_pairing_code")
        .unwrap();
    assert_eq!(
        (create.actor_id.as_str(), create.outcome.as_str()),
        (service.id(), "success")
    );
    assert_eq!(create.scope_account.as_deref(), Some("s1"));
    let redeemed = audit
        .iter()
        .find(|r| r.op == "redeem_pairing_code")
        .unwrap();
    assert_eq!(redeemed.actor_kind, "pairing_code");
    assert_eq!(redeemed.actor_id, code.pairing_id);
    assert_eq!(
        redeemed.target_id.as_deref(),
        Some(code.pairing_id.as_str())
    );
    assert_eq!(redeemed.scope_account.as_deref(), Some("s1"));
    assert_eq!(redeemed.related_id.as_deref(), Some("m-kid1"));
    assert_eq!(redeemed.outcome, "success");
}

#[test]
fn the_code_label_and_secrets_never_reach_the_database_in_the_clear_or_the_audit() {
    let (env, service) = setup();
    let code = new_code("b");
    create_code(&env, &service.who(), &code, "m-kid2", T0);
    let device = redeem(&env, &code, "very-secret-device-name", T0 + 1).unwrap();
    // 整个库文件（含 WAL）里搜：码明文、设备密钥明文都不能出现
    let mut haystack = Vec::new();
    for suffix in ["", "-wal"] {
        let mut path = env.path.clone().into_os_string();
        path.push(suffix);
        if let Ok(bytes) = std::fs::read(&path) {
            haystack.extend(bytes);
        }
    }
    let contains = |needle: &str| {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    };
    assert!(
        !contains(&code.plain),
        "the pairing code is stored only as a hash"
    );
    assert!(
        !contains(device.key.secret.expose_secret()),
        "device secrets are stored only as hashes"
    );
    assert!(!contains(service.secret()));
    // 设备显示名只在 pairing_devices，不进审计
    let conn = env.raw();
    let mut stmt = conn.prepare("SELECT * FROM control_audit").unwrap();
    let columns = stmt.column_count();
    let mut dump = String::new();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        for index in 0..columns {
            let cell: rusqlite::types::Value = row.get(index).unwrap();
            dump.push_str(&format!("{cell:?}|"));
        }
    }
    assert!(!dump.contains("very-secret-device-name"));
    assert!(!dump.contains(&code.plain));
    assert!(!dump.contains(device.key.secret.expose_secret()));
    // pairing_codes 里只有 32 字节的哈希
    let (len, hash): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT length(code_hash), code_hash FROM pairing_codes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(len, 32);
    assert_eq!(hash, code.hash.to_vec());
}

#[test]
fn a_code_is_short_lived_and_one_use_only() {
    let (env, service) = setup();
    // 过期：右端开 [created, created + ttl)
    let code = new_code("exp");
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    assert_eq!(
        redeem(&env, &code, "late", T0 + TTL).err(),
        Some(ControlReason::PairingCodeExpired)
    );
    assert_eq!(
        redeem(&env, &code, "later", T0 + TTL + DAY_MS).err(),
        Some(ControlReason::PairingCodeExpired)
    );
    let last = env.last_audit();
    assert_eq!(
        (
            last.actor_kind.as_str(),
            last.outcome.as_str(),
            last.reason_code.as_str()
        ),
        ("pairing_code", "rejected", "pairing_code_expired")
    );
    assert_eq!(last.actor_id, code.pairing_id);
    // 差一毫秒还能用
    let code = new_code("edge");
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    assert!(redeem(&env, &code, "just in time", T0 + TTL - 1).is_ok());
    // 重放：兑换过的码再兑换一次
    assert_eq!(
        redeem(&env, &code, "again", T0 + TTL - 1).err(),
        Some(ControlReason::PairingCodeUsed)
    );
    assert_eq!(
        redeem(&env, &code, "again", T0 + 5).err(),
        Some(ControlReason::PairingCodeUsed)
    );
    // 兑换失败的重放不会多出一台设备
    let conn = env.raw();
    let devices: i64 = conn
        .query_row("SELECT count(*) FROM pairing_devices", [], |r| r.get(0))
        .unwrap();
    assert_eq!(devices, 1);
    // 完全不认识的码：pairing_code_invalid，而且不写审计行
    let before = env.audit().len();
    assert_eq!(
        redeem(&env, &new_code("ghost"), "x", T0).err(),
        Some(ControlReason::PairingCodeInvalid)
    );
    assert_eq!(env.audit().len(), before);
    // 有效期的范围
    for (ttl, expected) in [
        (MIN_PAIRING_TTL_MS - 1, Some(ControlReason::InvalidRequest)),
        (MAX_PAIRING_TTL_MS + 1, Some(ControlReason::InvalidRequest)),
        (0, Some(ControlReason::InvalidRequest)),
        (-5, Some(ControlReason::InvalidRequest)),
        (MIN_PAIRING_TTL_MS, None),
        (MAX_PAIRING_TTL_MS, None),
    ] {
        let code = new_code("ttl");
        let rid = env.rid();
        let result = env.store.control_create_pairing_code(
            &service.who(),
            &env.meta(T0, &rid),
            &create_request(&code, "m-kid1", ttl),
        );
        assert_eq!(result.err().map(|f| f.reason), expected, "ttl {ttl}");
    }
}

#[test]
fn concurrent_redemptions_of_one_code_produce_exactly_one_device() {
    let (env, service) = setup();
    let code = Arc::new(new_code("race"));
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    let store = Arc::new(env.store);
    let mut handles = Vec::new();
    for i in 0..12 {
        let store = store.clone();
        let code = code.clone();
        handles.push(std::thread::spawn(move || {
            let key = MintedVirtualKey::mint();
            let request_id = format!("race-{i}");
            let outcome = store.control_redeem_pairing_code(
                &Meta {
                    now: ts(T0 + 1_000),
                    request_id: &request_id,
                },
                &RedeemPairingCode {
                    code_hash: code.hash,
                    key_id: key.key_id.clone(),
                    key_hash: key.hash,
                    label: format!("device-{i}"),
                },
            );
            (outcome.map(|_| key.key_id), i)
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winners: Vec<_> = results
        .iter()
        .filter_map(|(r, _)| r.as_ref().ok())
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one redemption may win: {results:?}"
    );
    for (outcome, _) in &results {
        if let Err(failure) = outcome {
            assert_eq!(failure.reason, ControlReason::PairingCodeUsed, "{failure}");
        }
    }
    let conn = rusqlite::Connection::open(&env.path).unwrap();
    let devices: i64 = conn
        .query_row("SELECT count(*) FROM pairing_devices", [], |r| r.get(0))
        .unwrap();
    let credentials: i64 = conn
        .query_row("SELECT count(*) FROM credentials WHERE binding_kind = 'member' AND membership_id = 'm-kid1'", [], |r| r.get(0))
        .unwrap();
    let keys: i64 = conn
        .query_row(
            "SELECT count(*) FROM virtual_keys WHERE owner LIKE 'device:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        (devices, credentials, keys),
        (1, 1, 1),
        "no extra credentials were left behind"
    );
}

#[test]
fn a_code_can_be_revoked_until_it_is_redeemed() {
    let (env, service) = setup();
    let code = new_code("rev");
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    let revoke = |who: &Who<'_>, id: &str, now: i64| {
        let rid = env.rid();
        env.store
            .control_revoke_pairing_code(
                who,
                &env.meta(now, &rid),
                &RevokePairingCode {
                    pairing_id: id.to_string(),
                },
            )
            .map(|w| w.result)
            .map_err(|f| f.reason)
    };
    // 别的组织撤销不了；不存在的码同一个答案
    assert_eq!(
        revoke(&other.who(), &code.pairing_id, T0 + 1),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        revoke(&service.who(), "pc_nope", T0 + 1),
        Err(ControlReason::OutOfScope)
    );
    assert_eq!(
        revoke(&service.who(), &code.pairing_id, T0 + 1),
        Ok(idoris_compute_contracts::control::WriteResult::Updated)
    );
    assert_eq!(
        revoke(&service.who(), &code.pairing_id, T0 + 2),
        Ok(idoris_compute_contracts::control::WriteResult::Unchanged)
    );
    assert_eq!(
        redeem(&env, &code, "x", T0 + 3).err(),
        Some(ControlReason::PairingCodeRevoked)
    );
    // 已经兑换的码撤销不了（要解绑设备，撤销它的凭证）
    let used = new_code("used");
    create_code(&env, &service.who(), &used, "m-kid1", T0);
    assert!(redeem(&env, &used, "d", T0 + 1).is_ok());
    assert_eq!(
        revoke(&service.who(), &used.pairing_id, T0 + 2),
        Err(ControlReason::PairingCodeUsed)
    );
}

#[test]
fn each_device_has_its_own_credential_and_can_be_unbound_independently() {
    let (env, service) = setup();
    let mut devices = Vec::new();
    for i in 0..3 {
        let code = new_code("multi");
        let code = Code {
            pairing_id: format!("{}-{i}", code.pairing_id),
            ..code
        };
        create_code(&env, &service.who(), &code, "m-kid1", T0);
        devices.push(redeem(&env, &code, &format!("device {i}"), T0 + 1).unwrap());
    }
    let ids: std::collections::BTreeSet<_> = devices.iter().map(|d| d.id().to_string()).collect();
    assert_eq!(ids.len(), 3, "one credential per device");
    for device in &devices {
        assert_eq!(gate(&env, device.id(), T0 + 2), GateVerdict::Allowed);
    }
    // 设备 0 自己解绑：只撤销它自己的凭证
    let rid = env.rid();
    let unpaired = env
        .store
        .control_unpair_device(&devices[0].who(), &env.meta(T0 + 3, &rid))
        .unwrap();
    assert_eq!(unpaired.value.status(), CredentialStatus::Revoked);
    assert_eq!(
        gate(&env, devices[0].id(), T0 + 3),
        GateVerdict::Denied(ReasonCode::CredentialRevoked)
    );
    assert!(
        env.keys
            .authenticate(&devices[0].key.secret, T0 + 3)
            .unwrap()
            .is_none(),
        "the verifier is revoked too"
    );
    assert_eq!(
        gate(&env, devices[1].id(), T0 + 3),
        GateVerdict::Allowed,
        "other devices are unaffected"
    );
    assert_eq!(gate(&env, devices[2].id(), T0 + 3), GateVerdict::Allowed);
    assert!(
        env.keys
            .authenticate(&devices[1].key.secret, T0 + 3)
            .unwrap()
            .is_some()
    );
    let last = env.last_audit();
    assert_eq!(
        (
            last.op.as_str(),
            last.actor_id.as_str(),
            last.outcome.as_str()
        ),
        ("unpair_device", devices[0].id(), "success")
    );
    assert_eq!(last.scope_account.as_deref(), Some("s1"));
    // 解绑之后旧凭证被拒：再用它做任何事都是 credential_revoked（并且留审计）
    let rid = env.rid();
    let again = env
        .store
        .control_unpair_device(&devices[0].who(), &env.meta(T0 + 4, &rid))
        .unwrap_err();
    assert_eq!(again.reason, ControlReason::CredentialRevoked);
    // 有权限的凭证（登录服务）从账号这一侧撤销设备 1：设备 2 仍然有效
    let rid = env.rid();
    let revoked = env
        .store
        .control_set_credential_status(
            &service.who(),
            &env.meta(T0 + 5, &rid),
            &super::SetCredentialStatus {
                credential: cid(devices[1].id()),
                status: CredentialStatus::Revoked,
            },
        )
        .unwrap();
    assert_eq!(revoked.value.status(), CredentialStatus::Revoked);
    assert_eq!(
        gate(&env, devices[1].id(), T0 + 5),
        GateVerdict::Denied(ReasonCode::CredentialRevoked)
    );
    assert_eq!(gate(&env, devices[2].id(), T0 + 5), GateVerdict::Allowed);
    // 账号的设备列表（只读，范围内）：三台都在，状态各自不同
    let view = env
        .store
        .control_read_credentials(&service.who(), ts(T0 + 6), &aid("s1"))
        .unwrap();
    let by_id: std::collections::BTreeMap<_, _> = view
        .credentials
        .iter()
        .filter(|c| c.device.is_some())
        .map(|c| (c.credential.id().to_string(), c.credential.status()))
        .collect();
    assert_eq!(by_id.len(), 3);
    assert_eq!(by_id[devices[0].id()], CredentialStatus::Revoked);
    assert_eq!(by_id[devices[1].id()], CredentialStatus::Revoked);
    assert_eq!(by_id[devices[2].id()], CredentialStatus::Active);
}

#[test]
fn only_paired_devices_can_unpair_themselves_and_only_themselves() {
    let (env, service) = setup();
    // 登录服务（Service 凭证）不是设备
    let rid = env.rid();
    assert_eq!(
        env.store
            .control_unpair_device(&service.who(), &env.meta(T0, &rid))
            .unwrap_err()
            .reason,
        ControlReason::NotADevice
    );
    // 运维签发的成员凭证也不是配对出来的设备
    let member = env.issue_as_operator(Issue::member("m-kid2"));
    let rid = env.rid();
    assert_eq!(
        env.store
            .control_unpair_device(&member.who(), &env.meta(T0, &rid))
            .unwrap_err()
            .reason,
        ControlReason::NotADevice
    );
    assert_eq!(
        gate(&env, member.id(), T0),
        GateVerdict::Allowed,
        "it was not touched"
    );
    // 运维没有「自己」可以解绑
    let rid = env.rid();
    assert_eq!(
        env.store
            .control_unpair_device(&Env::operator(), &env.meta(T0, &rid))
            .unwrap_err()
            .reason,
        ControlReason::NotADevice
    );
    // 设备的凭证不带管理权限：什么控制面操作都做不了
    let code = new_code("dev");
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    let device = redeem(&env, &code, "laptop", T0 + 1).unwrap();
    assert_eq!(
        super::control_tests::Action::CreateAccount {
            id: "x",
            parent: "s1"
        }
        .run(&env, &device.who(), T0 + 2),
        Err(ControlReason::OpNotGranted)
    );
    // 也不能给自己或别人创建配对码
    let rid = env.rid();
    let attempt = env.store.control_create_pairing_code(
        &device.who(),
        &env.meta(T0 + 2, &rid),
        &create_request(&new_code("mine"), "m-kid1", TTL),
    );
    assert_eq!(failure_reason(attempt), ControlReason::OpNotGranted);
}

#[test]
fn the_number_of_active_devices_per_account_is_bounded() {
    let (env, service) = setup();
    let limit = usize::try_from(MAX_ACTIVE_DEVICES_PER_MEMBERSHIP).unwrap();
    let mut devices = Vec::new();
    for i in 0..limit {
        let base = new_code("lim");
        let code = Code {
            pairing_id: format!("{}-{i}", base.pairing_id),
            ..base
        };
        create_code(&env, &service.who(), &code, "m-kid1", T0);
        devices.push(redeem(&env, &code, "d", T0 + 1).unwrap());
    }
    let extra = new_code("extra");
    create_code(&env, &service.who(), &extra, "m-kid1", T0);
    assert_eq!(
        redeem(&env, &extra, "one too many", T0 + 2).err(),
        Some(ControlReason::DeviceLimitReached)
    );
    // 解绑一台：腾出一个名额，那个码（没有被消耗）可以继续兑换
    let rid = env.rid();
    env.store
        .control_unpair_device(&devices[0].who(), &env.meta(T0 + 3, &rid))
        .unwrap();
    assert!(redeem(&env, &extra, "fits now", T0 + 4).is_ok());
    // 另一个账号不受影响
    let other = new_code("other");
    create_code(&env, &service.who(), &other, "m-kid2", T0);
    assert!(redeem(&env, &other, "separate account", T0 + 5).is_ok());
}

#[test]
fn a_code_cannot_be_redeemed_once_the_issuers_key_verifier_is_revoked() {
    // Codex r1 #2：`idoris key revoke <issuer>` 只撤销 virtual_keys 里的验证器，credentials 行仍是 active。
    // 旧代码兑换时只读 credentials，持有码的人仍然能兑出新的 active 设备密钥，绕过撤销。
    let (env, service) = setup();
    let code = new_code("vk");
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    assert!(env.keys.revoke(service.id(), T0 + 5).unwrap());
    assert_eq!(
        env.store
            .get_credential(&cid(service.id()))
            .unwrap()
            .unwrap()
            .status(),
        CredentialStatus::Active,
        "only the verifier was revoked"
    );
    assert_eq!(
        redeem(&env, &code, "x", T0 + 10).err(),
        Some(ControlReason::CredentialRevoked)
    );
    // 码没有被消耗，也没有设备凭证
    let conn = env.raw();
    let record = super::pairing::load_pairing_by_id(&conn, &code.pairing_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.status, PairingStatus::Pending);
    let devices: i64 = conn
        .query_row("SELECT count(*) FROM pairing_devices", [], |r| r.get(0))
        .unwrap();
    assert_eq!(devices, 0);
    assert_eq!(env.last_audit().reason_code, "credential_revoked");
}

#[test]
fn redemption_rechecks_the_issuers_current_data_plane_ceilings_and_expiry() {
    // 兑换重新走一遍完整的认证：验证器的有效期、数据面上限（隐私等级、角色、租户）都按签发者此刻的样子再核一次
    let (env, service) = setup();
    for (label, update, expected) in [
        (
            "verifier narrowed to no roles",
            "UPDATE virtual_keys SET allowed_roles = '[]' WHERE key_id = ?1",
            ControlReason::CeilingExceedsGrantor,
        ),
        (
            "verifier tenant changed",
            "UPDATE virtual_keys SET tenant_id = 'tenant-other' WHERE key_id = ?1",
            ControlReason::CeilingExceedsGrantor,
        ),
        (
            "verifier expired",
            "UPDATE virtual_keys SET expires_at_ms = 5 WHERE key_id = ?1",
            ControlReason::CredentialExpired,
        ),
    ] {
        let code = new_code("ceil2");
        create_code(&env, &service.who(), &code, "m-kid2", T0);
        env.raw().execute(update, [service.id()]).unwrap();
        assert_eq!(
            redeem(&env, &code, "x", T0 + 10).err(),
            Some(expected),
            "{label}"
        );
        // 还原，下一个用例从干净状态开始
        env.raw()
            .execute(
                "UPDATE virtual_keys SET allowed_roles = '[\"fast\"]', tenant_id = 'tenant-edu', expires_at_ms = NULL WHERE key_id = ?1",
                [service.id()],
            )
            .unwrap();
        assert!(
            redeem(&env, &code, "ok", T0 + 11).is_ok(),
            "{label}: back to normal"
        );
    }
}

/// 兑换 `n` 个设备（每个带 `device_ttl_ms`），码各不相同。
fn pair_devices(
    env: &Env,
    service: &Minted,
    n: usize,
    device_ttl_ms: Option<i64>,
    now: i64,
) -> Vec<Device> {
    (0..n)
        .map(|i| {
            let base = new_code("slots");
            let code = Code {
                pairing_id: format!("{}-{i}", base.pairing_id),
                ..base
            };
            let mut request = create_request(&code, "m-kid1", TTL);
            request.device_ttl_ms = device_ttl_ms;
            let rid = env.rid();
            env.store
                .control_create_pairing_code(&service.who(), &env.meta(now, &rid), &request)
                .unwrap();
            redeem(env, &code, "d", now + 1).unwrap()
        })
        .collect()
}

fn try_one_more(env: &Env, service: &Minted, now: i64) -> Result<Device, ControlReason> {
    let code = new_code("more");
    create_code(env, &service.who(), &code, "m-kid1", now);
    redeem(env, &code, "one more", now + 1)
}

#[test]
fn expired_devices_do_not_occupy_pairing_slots() {
    // Codex r1 #9 / PR-Daemon S2 F2：旧代码只排除 revoked，累计配对 10 张短期设备凭证、全部过期之后仍是
    // device_limit_reached；旧设备过期了又没法自己解绑，只能管理员逐张清理
    let (env, service) = setup();
    let limit = usize::try_from(MAX_ACTIVE_DEVICES_PER_MEMBERSHIP).unwrap();
    let devices = pair_devices(&env, &service, limit, Some(3_600_000), T0);
    assert_eq!(devices.len(), limit);
    assert_eq!(
        try_one_more(&env, &service, T0 + 10).err(),
        Some(ControlReason::DeviceLimitReached),
        "10 台都还有效"
    );
    // 一小时之后（设备寿命 1 小时）全部过期：名额自动空出来
    let later = T0 + 2 * 3_600_000;
    assert!(
        try_one_more(&env, &service, later).is_ok(),
        "expired devices free their slots"
    );
    for device in &devices {
        assert_eq!(
            gate(&env, device.id(), later),
            GateVerdict::Denied(ReasonCode::CredentialExpired)
        );
    }
}

#[test]
fn an_expired_credential_row_frees_its_slot_even_when_its_verifier_row_carries_no_expiry() {
    // 凭证行和验证器行的有效期是一起写的，但两边各自都是权威：数据面闸门看凭证行，虚拟 key 认证看验证器行。
    // 只要有一边已经永远不可用，名额就空出来；这里验证器行没有有效期（被改过），凭证行已经过期
    let (env, service) = setup();
    let limit = usize::try_from(MAX_ACTIVE_DEVICES_PER_MEMBERSHIP).unwrap();
    pair_devices(&env, &service, limit, Some(3_600_000), T0);
    env.raw()
        .execute(
            "UPDATE virtual_keys SET expires_at_ms = NULL \
              WHERE key_id IN (SELECT credential_id FROM pairing_devices)",
            [],
        )
        .unwrap();
    let later = T0 + 2 * 3_600_000;
    assert_eq!(
        try_one_more(&env, &service, T0 + 10).err(),
        Some(ControlReason::DeviceLimitReached),
        "before the credential rows expire the account is full"
    );
    assert!(try_one_more(&env, &service, later).is_ok());
}

#[test]
fn a_device_whose_key_verifier_was_revoked_or_that_was_unpaired_frees_its_slot_but_a_frozen_one_keeps_it()
 {
    let (env, service) = setup();
    let limit = usize::try_from(MAX_ACTIVE_DEVICES_PER_MEMBERSHIP).unwrap();
    let devices = pair_devices(&env, &service, limit, None, T0);
    assert_eq!(
        try_one_more(&env, &service, T0 + 10).err(),
        Some(ControlReason::DeviceLimitReached)
    );
    // 冻结的设备解冻后还能用：它的名额保留，不然解冻就会超限
    env.store
        .set_credential_status(&cid(devices[0].id()), CredentialStatus::Frozen, ts(T0 + 20))
        .unwrap();
    assert_eq!(
        try_one_more(&env, &service, T0 + 30).err(),
        Some(ControlReason::DeviceLimitReached),
        "a frozen device keeps its slot"
    );
    // 只撤销验证器（`idoris key revoke <设备>`）：这张设备凭证永远不可用了，名额空出来
    assert!(env.keys.revoke(devices[1].id(), T0 + 40).unwrap());
    assert!(try_one_more(&env, &service, T0 + 50).is_ok());
    assert_eq!(
        try_one_more(&env, &service, T0 + 60).err(),
        Some(ControlReason::DeviceLimitReached),
        "the freed slot is taken again"
    );
    // 解冻之后设备数仍不超限（它一直占着名额）
    env.store
        .set_credential_status(&cid(devices[0].id()), CredentialStatus::Active, ts(T0 + 70))
        .unwrap();
    assert_eq!(gate(&env, devices[0].id(), T0 + 71), GateVerdict::Allowed);
    // 设备自己解绑：照旧空出名额
    let rid = env.rid();
    env.store
        .control_unpair_device(&devices[2].who(), &env.meta(T0 + 80, &rid))
        .unwrap();
    assert!(try_one_more(&env, &service, T0 + 90).is_ok());
}

#[test]
fn issuing_member_credentials_directly_cannot_get_around_the_device_cap() {
    // PR-Daemon S2 低：上限只数配对出来的设备，直接给同一个成员关系签发成员凭证就绕过去了
    let (env, service) = setup();
    let limit = usize::try_from(MAX_ACTIVE_DEVICES_PER_MEMBERSHIP).unwrap();
    let issue = |now: i64| {
        let key = MintedVirtualKey::mint();
        let rid = env.rid();
        env.store
            .control_issue_credential(
                &service.who(),
                &env.meta(now, &rid),
                &Issue::member("m-kid1").request(&key),
            )
            .map(|w| w.value)
    };
    // 一半直接签发、一半配对：合起来才是这个成员关系的有效设备凭证
    let half = limit / 2;
    for i in 0..half {
        issue(T0 + i64::try_from(i).unwrap()).unwrap();
    }
    pair_devices(&env, &service, limit - half, None, T0 + 100);
    assert_eq!(
        issue(T0 + 200).unwrap_err().reason,
        ControlReason::DeviceLimitReached,
        "direct issuance counts toward the same cap"
    );
    assert_eq!(
        try_one_more(&env, &service, T0 + 210).err(),
        Some(ControlReason::DeviceLimitReached)
    );
    // 别的成员关系不受影响
    let key = MintedVirtualKey::mint();
    let rid = env.rid();
    assert!(
        env.store
            .control_issue_credential(
                &service.who(),
                &env.meta(T0 + 220, &rid),
                &Issue::member("m-kid2").request(&key)
            )
            .is_ok()
    );
}

#[test]
fn losing_authority_before_redemption_voids_the_code() {
    // 「签发者过期」不在这里：码不能比签发者活得久（创建时就拒绝，见另一个测试），所以签发者过期的时候码早已过期。
    let cases: Vec<(&str, ControlReason)> = vec![
        ("issuer revoked", ControlReason::CredentialRevoked),
        ("issuer frozen", ControlReason::CredentialFrozen),
        ("membership revoked", ControlReason::MembershipRevoked),
        ("principal revoked", ControlReason::PrincipalRevoked),
        ("member account frozen", ControlReason::AccountInactive),
        ("ancestor frozen", ControlReason::AccountInactive),
        ("issuer binding frozen", ControlReason::AccountInactive),
    ];
    for (what, expected) in cases {
        let env = Env::new();
        env.tree();
        let issuer = login_service(&env);
        let code = new_code("auth");
        create_code(&env, &issuer.who(), &code, "m-kid1", T0);
        let now = T0 + 1_000;
        match what {
            "issuer revoked" => env
                .store
                .set_credential_status(&cid(issuer.id()), CredentialStatus::Revoked, ts(T0))
                .unwrap(),
            "issuer frozen" => env
                .store
                .set_credential_status(&cid(issuer.id()), CredentialStatus::Frozen, ts(T0))
                .unwrap(),
            "membership revoked" => {
                env.store.revoke_membership(&mid("m-kid1"), ts(T0)).unwrap();
            }
            "principal revoked" => {
                env.store.revoke_principal(&pid("kid1"), ts(T0)).unwrap();
            }
            "member account frozen" => env
                .store
                .set_account_status(&aid("s1"), AccountStatus::Frozen)
                .unwrap(),
            "ancestor frozen" => env
                .store
                .set_account_status(&aid("pool"), AccountStatus::Frozen)
                .unwrap(),
            _ => env
                .store
                .set_account_status(&aid("eduai"), AccountStatus::Frozen)
                .unwrap(),
        }
        // 这些情形下码没有被消耗：数据库里仍是 pending，也没有设备凭证
        let result = redeem(&env, &code, "x", now);
        assert_eq!(result.err(), Some(expected), "{what}");
        let conn = env.raw();
        let record = super::pairing::load_pairing_by_id(&conn, &code.pairing_id)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, PairingStatus::Pending, "{what}");
        let devices: i64 = conn
            .query_row("SELECT count(*) FROM pairing_devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(devices, 0, "{what}");
        let last = env.last_audit();
        assert_eq!(last.actor_kind, "pairing_code", "{what}");
        assert_eq!(last.reason_code, expected.as_str(), "{what}");
    }
}

#[test]
fn redemption_rechecks_the_issuers_authority_even_for_a_code_row_written_behind_the_stores_back() {
    // 纵深防御：码是在创建时按签发者的权限校验过的，但兑换不信任「库里有这一行」本身——
    // 一行由别的代码（或手工 SQL）写进去的、签发者其实没有权限的码，兑换时同样被拒绝。
    let (env, _service) = setup();
    let reader = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::Read, "eduai", None)],
    ));
    let narrow = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::IssueCredential, "teacher", None)],
    ));
    for (label, issuer, membership, expected) in [
        (
            "an issuer with no issue_credential authority",
            reader.id(),
            "m-kid1",
            ControlReason::OpNotGranted,
        ),
        (
            "an issuer whose authority does not reach the member's account",
            narrow.id(),
            "m-kid2",
            ControlReason::OutOfScope,
        ),
    ] {
        let code = new_code("forged");
        env.raw()
            .execute(
                "INSERT INTO pairing_codes \
                    (pairing_id, code_hash, membership_id, issued_by, created_at_ms, expires_at_ms, status, \
                     device_privacy, device_roles) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', '[\"local_only\"]', '[\"fast\"]')",
                rusqlite::params![
                    code.pairing_id,
                    &code.hash[..],
                    membership,
                    issuer,
                    T0,
                    T0 + TTL
                ],
            )
            .unwrap();
        assert_eq!(
            redeem(&env, &code, "x", T0 + 1).err(),
            Some(expected),
            "{label}"
        );
    }
    let devices: i64 = env
        .raw()
        .query_row("SELECT count(*) FROM pairing_devices", [], |r| r.get(0))
        .unwrap();
    assert_eq!(devices, 0);
}

#[test]
fn a_device_credential_cannot_outlive_its_issuer_or_exceed_its_ceilings() {
    let env = Env::new();
    env.tree();
    let issuer = env.issue_as_operator(
        Issue::service("eduai", vec![sc(ControlOp::IssueCredential, "eduai", None)])
            .expiring(T0 + 3 * DAY_MS),
    );
    // 数据面上限：不超过签发者
    for (label, privacy, roles, tenant) in [
        (
            "wider privacy",
            vec![PrivacyClass::LocalOnly, PrivacyClass::Any],
            vec!["fast".to_string()],
            None,
        ),
        (
            "wider roles",
            vec![PrivacyClass::LocalOnly],
            vec!["fast".to_string(), "deep".to_string()],
            None,
        ),
        (
            "another tenant",
            vec![PrivacyClass::LocalOnly],
            vec!["fast".to_string()],
            Some("tenant-b".to_string()),
        ),
    ] {
        let code = new_code("ceil");
        let mut request = create_request(&code, "m-kid1", TTL);
        request.allowed_privacy = privacy;
        request.allowed_roles = roles;
        request.tenant_id = tenant;
        let rid = env.rid();
        let result =
            env.store
                .control_create_pairing_code(&issuer.who(), &env.meta(T0, &rid), &request);
        assert_eq!(
            failure_reason(result),
            ControlReason::CeilingExceedsGrantor,
            "{label}"
        );
        assert_eq!(env.last_audit().outcome, "denied");
    }
    // 至少要有一个隐私等级
    let code = new_code("empty");
    let mut request = create_request(&code, "m-kid1", TTL);
    request.allowed_privacy = vec![];
    let rid = env.rid();
    assert_eq!(
        failure_reason(env.store.control_create_pairing_code(
            &issuer.who(),
            &env.meta(T0, &rid),
            &request
        )),
        ControlReason::InvalidRequest
    );
    // 设备凭证的有效期 = min(设备寿命, 签发者的有效期)
    let code = new_code("life");
    let mut request = create_request(&code, "m-kid1", TTL);
    request.device_ttl_ms = Some(DAY_MS);
    let rid = env.rid();
    env.store
        .control_create_pairing_code(&issuer.who(), &env.meta(T0, &rid), &request)
        .unwrap();
    let device = redeem(&env, &code, "short", T0 + 1_000).unwrap();
    assert_eq!(
        env.store
            .get_credential(&cid(device.id()))
            .unwrap()
            .unwrap()
            .expires_at(),
        Some(ts(T0 + 1_000 + DAY_MS))
    );
    // 设备寿命比签发者剩下的还长：封顶在签发者的有效期
    let code = new_code("capped");
    let mut request = create_request(&code, "m-kid1", TTL);
    request.device_ttl_ms = Some(30 * DAY_MS);
    let rid = env.rid();
    env.store
        .control_create_pairing_code(&issuer.who(), &env.meta(T0, &rid), &request)
        .unwrap();
    let device = redeem(&env, &code, "capped", T0 + 1_000).unwrap();
    assert_eq!(
        env.store
            .get_credential(&cid(device.id()))
            .unwrap()
            .unwrap()
            .expires_at(),
        Some(ts(T0 + 3 * DAY_MS))
    );
    // 没有设备寿命、签发者有有效期：同样封顶
    let code = new_code("nottl");
    let rid = env.rid();
    env.store
        .control_create_pairing_code(
            &issuer.who(),
            &env.meta(T0, &rid),
            &create_request(&code, "m-kid1", TTL),
        )
        .unwrap();
    let device = redeem(&env, &code, "nottl", T0 + 1_000).unwrap();
    assert_eq!(
        env.store
            .get_credential(&cid(device.id()))
            .unwrap()
            .unwrap()
            .expires_at(),
        Some(ts(T0 + 3 * DAY_MS))
    );
    // 设备凭证过期之后：闸门拒绝、虚拟 key 认证也拒绝
    assert_eq!(
        gate(&env, device.id(), T0 + 3 * DAY_MS),
        GateVerdict::Denied(ReasonCode::CredentialExpired)
    );
    assert!(
        env.keys
            .authenticate(&device.key.secret, T0 + 3 * DAY_MS)
            .unwrap()
            .is_none()
    );
    // 码本身也不能比签发者活得久
    let code = new_code("late-code");
    let rid = env.rid();
    let result = env.store.control_create_pairing_code(
        &issuer.who(),
        &env.meta(T0 + 3 * DAY_MS - 1_000, &rid),
        &create_request(&code, "m-kid1", TTL),
    );
    assert_eq!(failure_reason(result), ControlReason::ExpiryExceedsGrantor);
}

#[test]
fn pairing_codes_follow_the_same_authorization_as_credential_issuance() {
    let (env, service) = setup();
    let create = |who: &Who<'_>, membership: &str| {
        let rid = env.rid();
        env.store
            .control_create_pairing_code(
                who,
                &env.meta(T0, &rid),
                &create_request(&new_code("auth"), membership, TTL),
            )
            .map_err(|f| f.reason)
    };
    // 别的组织：范围外；管理员关系（没有付款账户）：不能配对设备
    let other = env.issue_as_operator(Issue::service("school-b", all_ops("school-b", 1)));
    assert_eq!(
        create(&other.who(), "m-kid1").err(),
        Some(ControlReason::OutOfScope)
    );
    assert_eq!(
        create(&service.who(), "m-ghost").err(),
        Some(ControlReason::OutOfScope)
    );
    env.store.ensure_principal(&pid("boss"), ts(T0)).unwrap();
    env.store
        .create_membership(
            &idoris_compute_contracts::account::Membership {
                id: mid("m-boss"),
                principal: pid("boss"),
                org: aid("eduai"),
                role: idoris_compute_contracts::account::MembershipRole::Admin,
                account: None,
                status: MembershipStatus::Active,
            },
            ts(T0),
        )
        .unwrap();
    assert_eq!(
        create(&service.who(), "m-boss").err(),
        Some(ControlReason::OutOfScope)
    );
    // 没有 issue_credential 权限的凭证
    let reader = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::Read, "eduai", None)],
    ));
    assert_eq!(
        create(&reader.who(), "m-kid1").err(),
        Some(ControlReason::OpNotGranted)
    );
    // 权限只在另一棵子树（teacher）里：s2 在 teacher 之外
    let narrow = env.issue_as_operator(Issue::service(
        "eduai",
        vec![sc(ControlOp::IssueCredential, "teacher", None)],
    ));
    assert_eq!(
        create(&narrow.who(), "m-kid2").err(),
        Some(ControlReason::OutOfScope)
    );
    assert!(create(&narrow.who(), "m-kid1").is_ok());
    // 已撤销的成员关系 / 主体不能再配对
    env.store.revoke_membership(&mid("m-kid2"), ts(T0)).unwrap();
    assert_eq!(
        create(&service.who(), "m-kid2").err(),
        Some(ControlReason::MembershipRevoked)
    );
    env.store.revoke_principal(&pid("kid1"), ts(T0)).unwrap();
    assert_eq!(
        create(&service.who(), "m-kid1").err(),
        Some(ControlReason::PrincipalRevoked)
    );
    // 运维不能创建配对码（码要记下是哪张凭证签发的）
    assert_eq!(
        create(&Env::operator(), "m-kb1").err(),
        Some(ControlReason::OpNotGranted)
    );
    // 设备显示名不合法：在碰库之前就拒绝
    let code = new_code("label");
    create_code(&env, &service.who(), &code, "m-tch", T0);
    for label in ["", " leading", "trailing ", "line\nbreak", &"x".repeat(65)] {
        assert_eq!(
            redeem(&env, &code, label, T0 + 1).err(),
            Some(ControlReason::InvalidRequest),
            "{label:?}"
        );
    }
    assert!(
        redeem(&env, &code, &"x".repeat(64), T0 + 1).is_ok(),
        "64 characters is the limit"
    );
}

#[test]
fn pairing_tables_enforce_their_state_machine_in_the_database() {
    let (env, service) = setup();
    let code = new_code("db");
    create_code(&env, &service.who(), &code, "m-kid1", T0);
    let raw = env.raw();
    // 码的状态只能 pending -> redeemed / revoked；其余列不能改；不能删
    for (label, sql) in [
        (
            "extend the expiry",
            "UPDATE pairing_codes SET expires_at_ms = expires_at_ms + 1000000",
        ),
        (
            "swap the hash",
            "UPDATE pairing_codes SET code_hash = zeroblob(32)",
        ),
        (
            "widen the privacy ceiling",
            "UPDATE pairing_codes SET device_privacy = '[\"any\"]'",
        ),
        (
            "redeem without a credential",
            "UPDATE pairing_codes SET status = 'redeemed', redeemed_at_ms = 1",
        ),
        ("delete", "DELETE FROM pairing_codes"),
        (
            "replace",
            "INSERT OR REPLACE INTO pairing_codes (pairing_id, code_hash, membership_id, issued_by, created_at_ms, expires_at_ms, status, device_privacy, device_roles) SELECT pairing_id, code_hash, membership_id, issued_by, created_at_ms, expires_at_ms + 99999, status, device_privacy, device_roles FROM pairing_codes",
        ),
    ] {
        assert!(raw.execute(sql, []).is_err(), "{label} must be refused");
    }
    // 兑换之后是终态：不能退回 pending，也不能再撤销
    let device = redeem(&env, &code, "d", T0 + 1).unwrap();
    for sql in [
        "UPDATE pairing_codes SET status = 'pending', redeemed_at_ms = NULL, redeemed_credential_id = NULL",
        "UPDATE pairing_codes SET status = 'revoked', revoked_at_ms = 1, redeemed_at_ms = NULL, redeemed_credential_id = NULL",
    ] {
        assert!(raw.execute(sql, []).is_err(), "{sql}");
    }
    // 一个码最多一台设备
    assert!(raw
        .execute(
            "INSERT INTO pairing_devices (credential_id, membership_id, pairing_id, label, paired_at_ms) \
             SELECT ?1, 'm-kid1', pairing_id, 'second', 1 FROM pairing_codes",
            [device.id()],
        )
        .is_err());
    // 设备记录只追加
    assert!(
        raw.execute("UPDATE pairing_devices SET label = 'renamed'", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM pairing_devices", []).is_err());
    // 新的配对码必须是 pending，且不能给管理员关系
    assert!(raw
        .execute(
            "INSERT INTO pairing_codes (pairing_id, code_hash, membership_id, issued_by, created_at_ms, expires_at_ms, status, redeemed_at_ms, redeemed_credential_id, device_privacy, device_roles) \
             VALUES ('born-redeemed', zeroblob(32), 'm-kid1', ?1, 1, 2, 'redeemed', 1, ?2, '[]', '[]')",
            [service.id(), device.id()],
        )
        .is_err());
}

// ================================================================================================
// 数据面闸门：撤销即时生效
// ================================================================================================

struct Trio {
    member: Minted,
    node: Minted,
    service: Minted,
}

fn trio(env: &Env) -> Trio {
    Trio {
        member: env.issue_as_operator(Issue::member("m-kid1")),
        node: env.issue_as_operator(Issue {
            binding: CredentialBinding::Node {
                account: aid("eduai"),
            },
            ..Issue::service("eduai", vec![])
        }),
        service: env.issue_as_operator(Issue::service("eduai", vec![])),
    }
}

#[test]
fn the_gate_denies_the_very_next_request_after_every_kind_of_revocation() {
    let env = Env::new();
    env.tree();
    let t = trio(&env);
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    let now = T0 + 1_000;
    // 本地数据密钥（没有 credentials 行）：账户存储对它没有意见
    assert_eq!(
        gate(&env, "vk_00000000000000000000000000000000", now),
        GateVerdict::NotManaged
    );
    assert_eq!(
        gate(&env, "not a credential id!", now),
        GateVerdict::NotManaged
    );
    for credential in [&t.member, &t.node, &t.service] {
        assert_eq!(gate(&env, credential.id(), now), GateVerdict::Allowed);
    }

    // 1. 撤销凭证（走控制面）：下一次就被拒，没有任何缓存要失效
    let rid = env.rid();
    env.store
        .control_set_credential_status(
            &admin.who(),
            &env.meta(now, &rid),
            &super::SetCredentialStatus {
                credential: cid(t.member.id()),
                status: CredentialStatus::Revoked,
            },
        )
        .unwrap();
    assert_eq!(
        gate(&env, t.member.id(), now),
        GateVerdict::Denied(ReasonCode::CredentialRevoked)
    );
    assert!(
        env.keys
            .authenticate(&t.member.key.secret, now)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        gate(&env, t.node.id(), now),
        GateVerdict::Allowed,
        "other credentials are untouched"
    );

    // 2. 冻结 / 解冻
    for (status, expected) in [
        (
            CredentialStatus::Frozen,
            GateVerdict::Denied(ReasonCode::CredentialFrozen),
        ),
        (CredentialStatus::Active, GateVerdict::Allowed),
    ] {
        let rid = env.rid();
        env.store
            .control_set_credential_status(
                &admin.who(),
                &env.meta(now + 1, &rid),
                &super::SetCredentialStatus {
                    credential: cid(t.node.id()),
                    status,
                },
            )
            .unwrap();
        assert_eq!(gate(&env, t.node.id(), now + 1), expected);
    }

    // 3. 过期
    let short = env.issue_as_operator(Issue::member("m-kid2").expiring(T0 + 5_000));
    assert_eq!(gate(&env, short.id(), T0 + 4_999), GateVerdict::Allowed);
    assert_eq!(
        gate(&env, short.id(), T0 + 5_000),
        GateVerdict::Denied(ReasonCode::CredentialExpired)
    );

    // 4. 成员关系 / 主体：只影响绑定到这个成员关系的凭证；Node / Service 凭证不绑成员
    let fresh = env.issue_as_operator(Issue::member("m-kid1"));
    assert_eq!(gate(&env, fresh.id(), now), GateVerdict::Allowed);
    let rid = env.rid();
    env.store
        .control_revoke_membership(
            &admin.who(),
            &env.meta(now + 2, &rid),
            &super::RevokeMembership {
                membership: mid("m-kid1"),
            },
        )
        .unwrap();
    assert_eq!(
        gate(&env, fresh.id(), now + 2),
        GateVerdict::Denied(ReasonCode::MembershipRevoked)
    );
    assert_eq!(gate(&env, t.node.id(), now + 2), GateVerdict::Allowed);
    assert_eq!(gate(&env, t.service.id(), now + 2), GateVerdict::Allowed);
    let member2 = env.issue_as_operator(Issue::member("m-kid2"));
    assert_eq!(gate(&env, member2.id(), now + 2), GateVerdict::Allowed);
    let rid = env.rid();
    env.store
        .control_revoke_principal(
            &admin.who(),
            &env.meta(now + 3, &rid),
            &super::RevokePrincipal {
                principal: pid("kid2"),
                org: aid("eduai"),
            },
        )
        .unwrap();
    assert_eq!(
        gate(&env, member2.id(), now + 3),
        GateVerdict::Denied(ReasonCode::PrincipalRevoked)
    );
}

#[test]
fn freezing_or_closing_an_account_denies_everything_underneath_it_at_once() {
    for terminal in [false, true] {
        let env = Env::new();
        env.tree();
        let t = trio(&env);
        let teacher_member = env.issue_as_operator(Issue::member("m-kid1"));
        let now = T0 + 1;
        let set = |account: &str, status: AccountStatus| {
            env.store.set_account_status(&aid(account), status).unwrap();
        };
        let deny = if terminal {
            AccountStatus::Closed
        } else {
            AccountStatus::Frozen
        };
        // 成员账户自己
        set("s1", deny);
        assert_eq!(
            gate(&env, t.member.id(), now),
            GateVerdict::Denied(ReasonCode::AccountInactive)
        );
        assert_eq!(gate(&env, t.node.id(), now), GateVerdict::Allowed);
        if !terminal {
            set("s1", AccountStatus::Active);
            assert_eq!(gate(&env, t.member.id(), now), GateVerdict::Allowed);
        }
        // 祖先（担保人）：它下面的成员账户连带
        let under_teacher = teacher_member;
        set("teacher", deny);
        assert_eq!(
            gate(&env, under_teacher.id(), now),
            GateVerdict::Denied(ReasonCode::AccountInactive)
        );
        assert_eq!(
            gate(&env, t.node.id(), now),
            GateVerdict::Allowed,
            "the org above is fine"
        );
        if !terminal {
            set("teacher", AccountStatus::Active);
        }
        // 组织：它的成员、Node、Service 凭证全部连带
        set("eduai", deny);
        for credential in [&t.member, &t.node, &t.service] {
            assert_eq!(
                gate(&env, credential.id(), now),
                GateVerdict::Denied(ReasonCode::AccountInactive),
                "terminal={terminal}"
            );
        }
        if !terminal {
            set("eduai", AccountStatus::Active);
            for credential in [&t.member, &t.node, &t.service] {
                assert_eq!(gate(&env, credential.id(), now), GateVerdict::Allowed);
            }
            // 补贴池（祖先的祖先）
            set("pool", AccountStatus::Frozen);
            for credential in [&t.member, &t.node, &t.service] {
                assert_eq!(
                    gate(&env, credential.id(), now),
                    GateVerdict::Denied(ReasonCode::AccountInactive)
                );
            }
        }
    }
}

#[test]
fn a_service_credential_with_an_asserted_member_is_checked_on_every_payment_context_resolution() {
    // 数据面里「服务凭证 + 开通方提交的成员 id」是 S5 解析付款上下文时调用的：账户存储的 `payment_context` 每次都读库、
    // 不缓存，所以成员关系、主体被撤销，下一次解析就失败。这里钉住这条接缝。
    let env = Env::new();
    env.tree();
    let service = env.issue_as_operator(Issue::service("eduai", vec![]));
    let now = ts(T0 + 1);
    let resolve = |who: &str| {
        env.store
            .payment_context(&cid(service.id()), Some(&pid(who)), now)
    };
    assert_eq!(resolve("kid1").unwrap().payer().as_str(), "s1");
    assert_eq!(resolve("kid2").unwrap().payer().as_str(), "s2");
    // 别的组织的学生：不在这张凭证的范围里（不泄露是否存在）
    assert!(resolve("kb1").is_err());
    env.store.revoke_membership(&mid("m-kid1"), ts(T0)).unwrap();
    assert!(
        resolve("kid1").is_err(),
        "membership revoked: the very next resolution fails"
    );
    env.store.revoke_principal(&pid("kid2"), ts(T0)).unwrap();
    assert!(
        resolve("kid2").is_err(),
        "principal revoked: the very next resolution fails"
    );
    // 其余的学生不受影响
    env.store.ensure_principal(&pid("kid3"), ts(T0)).unwrap();
}

#[test]
fn a_credential_with_admin_scopes_is_also_a_data_plane_credential_and_its_scopes_never_change() {
    // 一张带管理权限的凭证，同时是数据面凭证（Service 凭证的设计就是这样）；闸门对它的态度和别的凭证一样
    let env = Env::new();
    env.tree();
    let admin = env.issue_as_operator(Issue::service("eduai", all_ops("eduai", 1)));
    assert_eq!(gate(&env, admin.id(), T0), GateVerdict::Allowed);
    let scopes: Vec<AdminScope> = env
        .store
        .get_credential(&cid(admin.id()))
        .unwrap()
        .unwrap()
        .admin_scopes()
        .to_vec();
    assert_eq!(scopes.len(), ControlOp::ALL.len());
    // 凭证的管理权限一经签发就不可改（数据库层），所以「撤销再重发」是收回权限的唯一方式
    let raw = env.raw();
    assert!(
        raw.execute("UPDATE credentials SET scope_count = scope_count + 1", [])
            .is_err()
    );
}

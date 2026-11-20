//! `register` 的纯函数与类型的单元测试（作为 `register` 的子模块编译，所以能碰到它的私有项）。
//! 进程与套接字层面的行为（握手、心跳、重连、孤儿超时、`set_admin_token`、fd 继承……）在 `tests/register_uds.rs`。

#![allow(clippy::unwrap_used)]

use super::*;
use serde_json::json;

// 规范 §2.10.5 的测试向量（两端实现都必须得到同样的值）。
const VECTOR_NONCE: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const CONN_ID: &str = "0123456789abcdef";
const CHALLENGE_NONCE: &str = "00112233445566778899aabbccddeeff";
const INSTANCE_ID: &str = "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11";
const SESSION_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
const REGISTER_PROOF: &str = "386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884";
const ACK_PROOF: &str = "099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967";

fn vector_identity() -> Identity {
    Identity {
        instance_id: INSTANCE_ID.to_string(),
        pid: 41234,
        version: "0.6.0".to_string(),
        contract_version: "1.3.0".to_string(),
        data_port: 51234,
        admin_port: 51235,
        started_at_ms: 1_763_625_330_123,
    }
}

fn vector_key() -> hmac::Key {
    InstanceNonce::from_hex(VECTOR_NONCE).unwrap().hmac_key()
}

#[test]
fn the_transcripts_are_exactly_the_specified_bytes() {
    let register = register_transcript(CONN_ID, CHALLENGE_NONCE, &vector_identity());
    assert_eq!(register.len(), 137);
    assert_eq!(
        register,
        b"idoris-register/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\n3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\n41234\n51234\n51235\n1763625330123"
    );
    let ack = ack_transcript(CONN_ID, CHALLENGE_NONCE, SESSION_ID);
    assert_eq!(ack.len(), 96);
    assert_eq!(
        ack,
        b"idoris-ack/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\na1b2c3d4e5f60718293a4b5c6d7e8f90"
    );
    // 末尾没有 LF，项与项之间只有一个 LF、没有空格。
    for transcript in [&register, &ack] {
        assert_ne!(transcript.last(), Some(&b'\n'));
        assert!(!transcript.contains(&b' '));
    }
}

#[test]
fn both_proofs_reproduce_the_specified_test_vectors() {
    let key = vector_key();
    assert_eq!(
        register_proof(&key, CONN_ID, CHALLENGE_NONCE, &vector_identity()),
        REGISTER_PROOF
    );
    assert_eq!(
        ack_proof(&key, CONN_ID, CHALLENGE_NONCE, SESSION_ID),
        ACK_PROOF
    );
    assert!(ack_proof_matches(
        &key,
        CONN_ID,
        CHALLENGE_NONCE,
        SESSION_ID,
        ACK_PROOF
    ));
}

/// `register.proof` 绑定 transcript 里的每一项：换掉任何一项都得到另一个值（端口与 pid 中途不能篡改，
/// `conn_id` 与挑战 nonce 每个连接都新所以不能重放）。`version`、`contract_version`、`phase` 不在 transcript 里
/// （规范只列了那几项），换掉它们 proof 不变。
#[test]
fn every_transcript_field_is_bound_into_the_register_proof() {
    let key = vector_key();
    let proof = |conn_id: &str, nonce: &str, identity: &Identity| {
        register_proof(&key, conn_id, nonce, identity)
    };
    let base = vector_identity();
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(proof(CONN_ID, CHALLENGE_NONCE, &base));
    seen.insert(proof("fedcba9876543210", CHALLENGE_NONCE, &base));
    seen.insert(proof(CONN_ID, "ffeeddccbbaa99887766554433221100", &base));
    for mutated in [
        Identity {
            instance_id: "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b12".to_string(),
            ..base.clone()
        },
        Identity {
            pid: 41235,
            ..base.clone()
        },
        Identity {
            data_port: 51236,
            ..base.clone()
        },
        Identity {
            admin_port: 51236,
            ..base.clone()
        },
        Identity {
            started_at_ms: 1_763_625_330_124,
            ..base.clone()
        },
    ] {
        seen.insert(proof(CONN_ID, CHALLENGE_NONCE, &mutated));
    }
    assert_eq!(seen.len(), 8, "each changed input must change the proof");
    let versions_changed = Identity {
        version: "9.9.9".to_string(),
        contract_version: "9.9.9".to_string(),
        ..base
    };
    assert_eq!(
        proof(CONN_ID, CHALLENGE_NONCE, &versions_changed),
        REGISTER_PROOF
    );
}

#[test]
fn ack_proof_is_bound_to_conn_id_nonce_session_and_key_and_must_be_lowercase_hex() {
    let key = vector_key();
    let other_key = InstanceNonce::from_hex(&"ab".repeat(32))
        .unwrap()
        .hmac_key();
    let matches = |key: &hmac::Key, conn: &str, nonce: &str, session: &str, claimed: &str| {
        ack_proof_matches(key, conn, nonce, session, claimed)
    };
    assert!(matches(
        &key,
        CONN_ID,
        CHALLENGE_NONCE,
        SESSION_ID,
        ACK_PROOF
    ));
    // 另一个连接的证明不能重放到这个连接上。
    assert!(!matches(
        &key,
        "fedcba9876543210",
        CHALLENGE_NONCE,
        SESSION_ID,
        ACK_PROOF
    ));
    assert!(!matches(
        &key,
        CONN_ID,
        "ffeeddccbbaa99887766554433221100",
        SESSION_ID,
        ACK_PROOF
    ));
    assert!(!matches(
        &key,
        CONN_ID,
        CHALLENGE_NONCE,
        "00000000000000000000000000000000",
        ACK_PROOF
    ));
    // 不知道 nonce 的冒充者算不出来。
    assert!(!matches(
        &other_key,
        CONN_ID,
        CHALLENGE_NONCE,
        SESSION_ID,
        ACK_PROOF
    ));
    // 形状不对一律不过：大写、长度不对、非 hex。
    for claimed in [
        ACK_PROOF.to_uppercase(),
        ACK_PROOF[..63].to_string(),
        format!("{ACK_PROOF}0"),
        format!("{}g", &ACK_PROOF[..63]),
        String::new(),
    ] {
        assert!(
            !matches(&key, CONN_ID, CHALLENGE_NONCE, SESSION_ID, &claimed),
            "{claimed:?}"
        );
    }
    // `register.proof` 与 `ack.proof` 的标签不同：一个不能冒充另一个。
    assert!(!matches(
        &key,
        CONN_ID,
        CHALLENGE_NONCE,
        SESSION_ID,
        REGISTER_PROOF
    ));
}

#[test]
fn outgoing_frames_are_the_specified_bytes() {
    let key = vector_key();
    let identity = vector_identity();
    let register = encode_frame(&RegisterFrame {
        v: FRAME_VERSION,
        op: "register",
        nonce: CHALLENGE_NONCE,
        instance_id: &identity.instance_id,
        pid: identity.pid,
        version: &identity.version,
        contract_version: &identity.contract_version,
        data_port: identity.data_port,
        admin_port: identity.admin_port,
        started_at_ms: identity.started_at_ms,
        phase: "ready",
        resume: false,
        proof: &register_proof(&key, CONN_ID, CHALLENGE_NONCE, &identity),
    })
    .unwrap();
    assert_eq!(
        String::from_utf8(register).unwrap(),
        "{\"v\":1,\"op\":\"register\",\"nonce\":\"00112233445566778899aabbccddeeff\",\"instance_id\":\"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\",\"pid\":41234,\"version\":\"0.6.0\",\"contract_version\":\"1.3.0\",\"data_port\":51234,\"admin_port\":51235,\"started_at_ms\":1763625330123,\"phase\":\"ready\",\"resume\":false,\"proof\":\"386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884\"}\n"
    );
    let heartbeat = encode_frame(&HeartbeatFrame {
        v: FRAME_VERSION,
        op: "heartbeat",
        seq: 1,
        phase: "ready",
        ready: true,
    })
    .unwrap();
    assert_eq!(
        heartbeat,
        b"{\"v\":1,\"op\":\"heartbeat\",\"seq\":1,\"phase\":\"ready\",\"ready\":true}\n"
    );
    let bye = encode_frame(&ByeFrame {
        v: FRAME_VERSION,
        op: "bye",
        reason: "shutdown",
    })
    .unwrap();
    assert_eq!(bye, b"{\"v\":1,\"op\":\"bye\",\"reason\":\"shutdown\"}\n");
    let error = error_frame("bad_frame", "invalid frame").unwrap();
    assert_eq!(
        error,
        b"{\"v\":1,\"op\":\"error\",\"code\":\"bad_frame\",\"message\":\"invalid frame\"}\n"
    );
    let token_ack = encode_frame(&AdminTokenAckFrame {
        v: FRAME_VERSION,
        op: "admin_token_ack",
    })
    .unwrap();
    assert_eq!(token_ack, b"{\"v\":1,\"op\":\"admin_token_ack\"}\n");
    // 超过 16 KiB 的帧不会被发出去。
    assert!(encode_frame(&json!({"pad": "x".repeat(MAX_FRAME_BYTES)})).is_none());
}

#[test]
fn the_default_timings_are_the_specified_numbers() {
    let t = Timings::default();
    let secs = Duration::from_secs;
    let ms = Duration::from_millis;
    assert_eq!(
        (t.connect, t.challenge_wait, t.register_within, t.ack_wait),
        (secs(2), secs(5), secs(1), secs(5))
    );
    assert_eq!(
        (t.heartbeat_default, t.heartbeat_min, t.heartbeat_max),
        (secs(10), secs(2), secs(60))
    );
    assert_eq!((t.heartbeat_ack_wait, t.heartbeat_misses), (secs(5), 3));
    assert_eq!(
        t.backoff_steps,
        [
            Duration::ZERO,
            ms(250),
            ms(500),
            secs(1),
            secs(2),
            secs(4),
            secs(8)
        ]
    );
    assert_eq!((t.backoff_cap, t.backoff_reset_after), (secs(10), secs(60)));
    assert!((t.heartbeat_jitter - 0.10).abs() < f64::EPSILON);
    assert!((t.backoff_jitter - 0.20).abs() < f64::EPSILON);
    assert_eq!(
        (t.wait_identity, t.wait_proof_invalid, t.wait_busy),
        (secs(60), secs(30), secs(5))
    );
    assert_eq!(
        (t.bye_budget, t.admin_token_interval, t.self_heal_interval),
        (ms(500), secs(10), secs(10))
    );
    assert_eq!(t.jitter, JitterSource::System);
}

#[test]
fn backoff_is_immediate_then_250ms_doubling_to_8s_then_a_fixed_10s() {
    let t = Timings {
        jitter: JitterSource::None,
        ..Timings::default()
    };
    let mut backoff = Backoff::new();
    let ms = Duration::from_millis;
    let secs = Duration::from_secs;
    let sequence: Vec<Duration> = (0..11).map(|_| backoff.next_delay(&t)).collect();
    assert_eq!(
        sequence,
        [
            Duration::ZERO,
            ms(250),
            ms(500),
            secs(1),
            secs(2),
            secs(4),
            secs(8),
            secs(10),
            secs(10),
            secs(10),
            secs(10)
        ]
    );
    assert_eq!(backoff.retries(), 11);
    // 清零之后从头开始：第一次又是立即。
    backoff.reset();
    assert_eq!(backoff.retries(), 0);
    assert_eq!(backoff.next_delay(&t), Duration::ZERO);
    assert_eq!(backoff.next_delay(&t), ms(250));
}

#[test]
fn backoff_jitter_stays_within_20_percent_and_never_makes_the_first_retry_wait() {
    let t = Timings::default();
    for step in 0..12 {
        let nominal = t.backoff_steps.get(step).copied().unwrap_or(t.backoff_cap);
        for _ in 0..200 {
            let mut backoff = Backoff { retries: step };
            let delay = backoff.next_delay(&t);
            if nominal.is_zero() {
                assert_eq!(delay, Duration::ZERO, "the first retry is immediate");
            } else {
                assert!(
                    delay >= nominal.mul_f64(0.8) && delay <= nominal.mul_f64(1.2),
                    "step {step}: {delay:?} not within 20% of {nominal:?}"
                );
            }
        }
    }
    // 抖动是真的在变，不是常数。
    let samples: std::collections::BTreeSet<Duration> = (0..50)
        .map(|_| Backoff { retries: 6 }.next_delay(&t))
        .collect();
    assert!(samples.len() > 10, "{}", samples.len());
}

#[test]
fn heartbeat_jitter_is_within_10_percent() {
    let t = Timings::default();
    let base = Duration::from_secs(10);
    let mut spread = std::collections::BTreeSet::new();
    for _ in 0..200 {
        let jittered = t.jitter.apply(base, t.heartbeat_jitter);
        assert!(
            jittered >= base.mul_f64(0.9) && jittered <= base.mul_f64(1.1),
            "{jittered:?}"
        );
        spread.insert(jittered);
    }
    assert!(spread.len() > 10);
    assert_eq!(JitterSource::None.apply(base, 0.1), base);
}

#[test]
fn the_error_code_table_maps_to_the_specified_waits() {
    let t = Timings::default();
    let secs = Duration::from_secs;
    let cases = [
        (
            "identity_rejected",
            ServerError::IdentityRejected,
            Some(secs(60)),
        ),
        (
            "unsupported_version",
            ServerError::UnsupportedVersion,
            Some(secs(60)),
        ),
        (
            "contract_incompatible",
            ServerError::ContractIncompatible,
            Some(secs(60)),
        ),
        ("proof_invalid", ServerError::ProofInvalid, Some(secs(30))),
        ("busy", ServerError::Busy, Some(secs(5))),
        // 其余按默认退避。
        ("bad_frame", ServerError::BadFrame, None),
        ("timeout", ServerError::Timeout, None),
        ("internal", ServerError::Internal, None),
        ("some_future_code", ServerError::Unknown, None),
        ("", ServerError::Unknown, None),
    ];
    for (code, expected, wait) in cases {
        let parsed = ServerError::parse(code);
        assert_eq!(parsed, expected, "{code}");
        assert_eq!(parsed.wait().fixed(&t), wait, "{code}");
        if expected != ServerError::Unknown {
            assert_eq!(parsed.code(), code);
        }
    }
    // `instance_conflict` 不重连（iDoris 退出）；这里只确认它能被识别。
    assert_eq!(
        ServerError::parse("instance_conflict"),
        ServerError::InstanceConflict
    );
    assert_eq!(ServerError::InstanceConflict.code(), "instance_conflict");
    // 大小写敏感：只认规范里的小写写法。
    assert_eq!(ServerError::parse("BUSY"), ServerError::Unknown);
}

#[test]
fn repeated_events_are_logged_only_for_the_first_and_every_tenth_occurrence() {
    let logged: Vec<u64> = (0..=35).filter(|n| should_log_repeat(*n)).collect();
    assert_eq!(logged, [1, 10, 20, 30]);
}

#[test]
fn stop_causes_map_to_exit_codes_and_stop_on_exit_never_only_counts_for_exit_0() {
    assert_eq!(StopCause::Signal.exit_code(), 0);
    assert_eq!(StopCause::InstanceConflict.exit_code(), 3);
    assert_eq!(StopCause::OrphanTimeout.exit_code(), 4);
    assert_eq!(EXIT_INSTANCE_CONFLICT, 3);
    assert_eq!(EXIT_ORPHAN_TIMEOUT, 4);
    // `bye` 只在信号停止时发：孤儿超时与 `instance_conflict` 都发生在没有已 ack 的会话时。
    assert!(StopCause::Signal.says_bye());
    assert!(!StopCause::InstanceConflict.says_bye());
    assert!(!StopCause::OrphanTimeout.says_bye());
    // 规范 §2.7.5：`never` 只对退出码 0 有效；退出码 4、因 `instance_conflict` 退出的 3 一律停自己拉起的引擎。
    assert!(StopCause::Signal.stops_owned_engines(StopOnExit::Owned));
    assert!(!StopCause::Signal.stops_owned_engines(StopOnExit::Never));
    for cause in [StopCause::InstanceConflict, StopCause::OrphanTimeout] {
        assert!(cause.stops_owned_engines(StopOnExit::Owned), "{cause:?}");
        assert!(cause.stops_owned_engines(StopOnExit::Never), "{cause:?}");
    }
}

fn parse(value: &serde_json::Value) -> Result<ServerFrame, Fault> {
    parse_server_frame(value.to_string().as_bytes())
}

#[test]
fn the_five_server_frames_parse_and_unknown_fields_are_ignored() {
    assert_eq!(
        parse(
            &json!({"v":1,"op":"challenge","conn_id":CONN_ID,"nonce":CHALLENGE_NONCE,"future":[1]})
        ),
        Ok(ServerFrame::Challenge {
            conn_id: CONN_ID.to_string(),
            nonce: CHALLENGE_NONCE.to_string()
        })
    );
    assert_eq!(
        parse(
            &json!({"v":1,"op":"ack","session_id":SESSION_ID,"heartbeat_ms":10000,"proof":ACK_PROOF})
        ),
        Ok(ServerFrame::Ack {
            session_id: SESSION_ID.to_string(),
            proof: ACK_PROOF.to_string(),
            heartbeat_ms: Some(10_000)
        })
    );
    // `heartbeat_ms` 可选。
    assert_eq!(
        parse(&json!({"v":1,"op":"ack","session_id":SESSION_ID,"proof":ACK_PROOF})),
        Ok(ServerFrame::Ack {
            session_id: SESSION_ID.to_string(),
            proof: ACK_PROOF.to_string(),
            heartbeat_ms: None
        })
    );
    assert_eq!(
        parse(&json!({"v":1,"op":"heartbeat_ack","seq":7,"extra":null})),
        Ok(ServerFrame::HeartbeatAck { seq: 7 })
    );
    assert_eq!(
        parse(&json!({"v":1,"op":"set_admin_token","token":"abc"})),
        Ok(ServerFrame::SetAdminToken {
            token: Some(Secret::new("abc"))
        })
    );
    // token 缺失或不是字符串：仍是一个 `set_admin_token`（随后校验失败），不是解析错误。
    for frame in [
        json!({"v":1,"op":"set_admin_token"}),
        json!({"v":1,"op":"set_admin_token","token":5}),
        json!({"v":1,"op":"set_admin_token","token":null}),
    ] {
        assert_eq!(
            parse(&frame),
            Ok(ServerFrame::SetAdminToken { token: None }),
            "{frame}"
        );
    }
    assert_eq!(
        parse(&json!({"v":1,"op":"error","code":"busy","message":"x"})),
        Ok(ServerFrame::Error {
            code: ServerError::Busy
        })
    );
    assert_eq!(
        parse(&json!({"v":1,"op":"error"})),
        Ok(ServerFrame::Error {
            code: ServerError::Unknown
        })
    );
    // 其他 `op`（含 iDoris 自己才会发的）一律忽略。
    for op in [
        "register",
        "heartbeat",
        "bye",
        "admin_token_ack",
        "future_op",
        "",
    ] {
        assert_eq!(
            parse(&json!({"v":1,"op":op,"whatever":true})),
            Ok(ServerFrame::Ignored),
            "{op:?}"
        );
    }
}

#[test]
fn malformed_frames_are_faults_and_required_fields_are_checked_strictly() {
    let bad = |line: &[u8]| parse_server_frame(line).unwrap_err();
    assert_eq!(bad(b""), Fault::Malformed);
    assert_eq!(bad(b"not json"), Fault::Malformed);
    assert_eq!(bad(b"[]"), Fault::Malformed);
    assert_eq!(bad(b"7"), Fault::Malformed);
    assert_eq!(bad(b"null"), Fault::Malformed);
    assert_eq!(bad(b"\"str\""), Fault::Malformed);
    assert_eq!(bad(&[0xff, 0xfe, b'{', b'}']), Fault::NotUtf8);
    // 不含 CR 与 BOM。
    assert_eq!(bad(b"{\"v\":1,\"op\":\"x\"}\r"), Fault::Malformed);
    assert_eq!(bad(b"\xEF\xBB\xBF{\"v\":1,\"op\":\"x\"}"), Fault::Malformed);
    // `v` 必须等于 1（缺失、别的数、字符串、浮点都不行）。
    for frame in [
        json!({"op":"heartbeat_ack","seq":1}),
        json!({"v":2,"op":"heartbeat_ack","seq":1}),
        json!({"v":0,"op":"heartbeat_ack","seq":1}),
        json!({"v":"1","op":"heartbeat_ack","seq":1}),
        json!({"v":1.5,"op":"heartbeat_ack","seq":1}),
        json!({"v":null,"op":"heartbeat_ack","seq":1}),
        json!({"v":-1,"op":"heartbeat_ack","seq":1}),
    ] {
        assert_eq!(parse(&frame), Err(Fault::BadVersion), "{frame}");
    }
    // 就算 `op` 是不认识的，`v` 不对也是坏帧。
    assert_eq!(parse(&json!({"v":2,"op":"future"})), Err(Fault::BadVersion));
    assert_eq!(parse(&json!({"v":1})), Err(Fault::BadField("op")));
    assert_eq!(parse(&json!({"v":1,"op":5})), Err(Fault::BadField("op")));

    let valid_ack = json!({"v":1,"op":"ack","session_id":SESSION_ID,"proof":ACK_PROOF});
    let with = |field: &str, value: serde_json::Value| {
        let mut frame = valid_ack.clone();
        frame[field] = value;
        frame
    };
    for (frame, field) in [
        (
            with("session_id", json!(SESSION_ID.to_uppercase())),
            "session_id",
        ),
        (with("session_id", json!(&SESSION_ID[..31])), "session_id"),
        (with("session_id", json!(7)), "session_id"),
        (with("proof", json!(ACK_PROOF.to_uppercase())), "proof"),
        (with("proof", json!(&ACK_PROOF[..63])), "proof"),
        (with("proof", json!(null)), "proof"),
        (with("heartbeat_ms", json!("10000")), "heartbeat_ms"),
        (with("heartbeat_ms", json!(-5)), "heartbeat_ms"),
        (with("heartbeat_ms", json!(1.5)), "heartbeat_ms"),
        (with("heartbeat_ms", json!(null)), "heartbeat_ms"),
    ] {
        assert_eq!(parse(&frame), Err(Fault::BadField(field)), "{frame}");
    }
    for field in ["session_id", "proof"] {
        let mut frame = valid_ack.clone();
        frame.as_object_mut().unwrap().remove(field);
        let expected = if field == "session_id" {
            "session_id"
        } else {
            "proof"
        };
        assert_eq!(parse(&frame), Err(Fault::BadField(expected)));
    }
    // challenge：`conn_id` 16 个、`nonce` 32 个小写 hex。
    for (conn_id, nonce, field) in [
        (
            CONN_ID.to_uppercase(),
            CHALLENGE_NONCE.to_string(),
            "conn_id",
        ),
        (
            CONN_ID[..15].to_string(),
            CHALLENGE_NONCE.to_string(),
            "conn_id",
        ),
        (CONN_ID.to_string(), CHALLENGE_NONCE.to_uppercase(), "nonce"),
        (
            CONN_ID.to_string(),
            CHALLENGE_NONCE[..31].to_string(),
            "nonce",
        ),
        (CONN_ID.to_string(), "g".repeat(32), "nonce"),
    ] {
        assert_eq!(
            parse(&json!({"v":1,"op":"challenge","conn_id":conn_id,"nonce":nonce})),
            Err(Fault::BadField(field))
        );
    }
    // heartbeat_ack：`seq` 必须是 u64。
    for seq in [json!("1"), json!(-1), json!(1.5), json!(null)] {
        assert_eq!(
            parse(&json!({"v":1,"op":"heartbeat_ack","seq":seq})),
            Err(Fault::BadField("seq"))
        );
    }
    assert_eq!(
        parse(&json!({"v":1,"op":"heartbeat_ack"})),
        Err(Fault::BadField("seq"))
    );
}

#[test]
fn retry_reason_codes_are_stable_and_carry_no_free_text() {
    let reasons = [
        (RetryReason::ConnectFailed, "connect_failed"),
        (RetryReason::ChallengeTimeout, "challenge_timeout"),
        (RetryReason::AckTimeout, "ack_timeout"),
        (RetryReason::WriteFailed, "write_failed"),
        (RetryReason::Disconnected, "disconnected"),
        (RetryReason::IoError, "io_error"),
        (RetryReason::BadFrame, "bad_frame"),
        (RetryReason::AckProofInvalid, "ack_proof_invalid"),
        (RetryReason::HeartbeatLost, "heartbeat_lost"),
        (RetryReason::AdminTokenRejected, "admin_token_rejected"),
        (
            RetryReason::Server(ServerError::IdentityRejected),
            "identity_rejected",
        ),
    ];
    for (reason, code) in reasons {
        assert_eq!(reason.code(), code);
        assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    }
    assert_eq!(TokenRejection::Invalid.code(), "invalid_token");
    assert_eq!(TokenRejection::RateLimited.code(), "rate_limited");
}

/// token 是秘密：解析出来的帧即使被 `{:?}` 打印（日志、断言信息、错误信息），也不带出 token；只有显式的 `expose` 看得到。
#[test]
fn a_parsed_set_admin_token_frame_never_renders_the_token() {
    let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let frame = parse(&json!({"v":1,"op":"set_admin_token","token":token})).unwrap();
    for rendered in [
        format!("{frame:?}"),
        format!("{:#?}", Some(&frame)),
        format!("{:?}", Secret::new(token)),
    ] {
        assert!(!rendered.contains(token), "{rendered}");
        assert!(!rendered.contains(&token[..16]), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
    }
    let ServerFrame::SetAdminToken {
        token: Some(secret),
    } = frame
    else {
        panic!("not a set_admin_token frame");
    };
    assert_eq!(secret.expose(), token);
}

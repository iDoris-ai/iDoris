//! 设备端的云账号配对：兑换配对码、解绑。**云端那一侧（S2）已经做完**（`control::handlers::redeem_pairing_code` /
//! `unpair_device`、`idoris-tenancy::accounts::pairing`），这里只是调用它的客户端。
//!
//! # 流程
//!
//! ```text
//!   云账号侧（登录服务 / 运维）                       本机
//!   POST /control/v1/pairing/codes  ──▶ 一次性配对码
//!                                              idoris cloud pair（配对码从 stdin 读，不进 argv、不进环境变量）
//!                                                 │ POST /control/v1/pairing/redeem {code, device_label}   ← 不带凭证，码本身是凭据
//!                                                 ▼
//!                                       一张绑定到该云账号付款上下文的设备凭证（Member 绑定，只有数据面权限）
//!                                                 │ 原子写入 0600 文件（credential::DeviceStore）
//!                                                 ▼
//!                                              idoris cloud status   只显示是否配对、账号绑定、凭证 id 前缀
//!                                              idoris cloud unpair   POST /control/v1/pairing/unpair（用这张凭证撤销它自己）
//! ```
//!
//! - **兑换前先预检**本机能不能写（目录权限、只读卷）：配对码是一次性的，兑换成功后才发现写不进去，
//!   云端那张凭证就成了没人持有的孤儿。万一兑换成功后写仍然失败，会立刻用手里的凭证向云端 `unpair` 把它撤销，再报错。
//! - **解绑永远先保证本机**：云端不可达、云端报错时也删除本机凭证，并明确提示云端那张凭证可能仍然有效、需要在控制面撤销
//!   （`DELETE` 不存在，撤销走 `POST /control/v1/credentials/{id}/status` 或云账号的管理员）。
//! - 配对码、凭证、设备名都不进日志、不进错误信息；错误文案固定，只带云端的**稳定原因码**。
//! - 请求不跟随重定向、不走代理（`idoris_upstream::http_client()`），响应体有大小上限。

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Url;
use serde_json::{Value, json};

use super::credential::CloudCredential;

/// 配对请求的总期限（含连接）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// 控制面响应体上限：它们都很小。
const MAX_BODY_BYTES: usize = 64 * 1024;
/// 设备显示名的最大长度（字符）；和云端 `MAX_DEVICE_LABEL_CHARS` 一致。
pub const MAX_DEVICE_LABEL_CHARS: usize = 64;
/// 不传 `--label` 时的设备名。不读主机名：设备名会出现在云端的设备列表里，不替用户泄露机器信息。
pub const DEFAULT_DEVICE_LABEL: &str = "iDoris device";

/// 配对 / 解绑的失败。文案固定；`code()` 是稳定字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    /// 连接不上云端（DNS、连接被拒、TLS）：请求没有到达，配对码没有被消耗。
    Unreachable,
    /// 等不到完整的答复，而请求可能已经到达：配对码**可能已经被兑换**。
    Ambiguous,
    /// 配对码不对（格式、查无此码）。
    CodeInvalid,
    CodeExpired,
    CodeUsed,
    CodeRevoked,
    /// 云端对兑换失败次数限速。
    RateLimited,
    /// 这个云账号已经有太多台设备。
    DeviceLimitReached,
    /// 签发者、成员关系、主体或账户不再有资格（带云端的稳定原因码）。
    NoLongerEligible(String),
    /// 云端不认这张凭证（已撤销、过期、不认识）。
    CredentialNotAccepted,
    /// 云端的其它错误。
    Cloud {
        status: u16,
        reason: Option<String>,
    },
    /// 云端的答复不是约定的形状。
    BadResponse,
}

impl PairingError {
    pub fn code(&self) -> String {
        match self {
            PairingError::Unreachable => "cloud_unreachable".into(),
            PairingError::Ambiguous => "response_lost".into(),
            PairingError::CodeInvalid => "pairing_code_invalid".into(),
            PairingError::CodeExpired => "pairing_code_expired".into(),
            PairingError::CodeUsed => "pairing_code_used".into(),
            PairingError::CodeRevoked => "pairing_code_revoked".into(),
            PairingError::RateLimited => "pairing_rate_limited".into(),
            PairingError::DeviceLimitReached => "device_limit_reached".into(),
            PairingError::NoLongerEligible(reason) => reason.clone(),
            PairingError::CredentialNotAccepted => "credential_not_accepted".into(),
            PairingError::Cloud { reason, .. } => {
                reason.clone().unwrap_or_else(|| "cloud_error".into())
            }
            PairingError::BadResponse => "bad_response".into(),
        }
    }
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::Unreachable => f.write_str("连接不上云端（请求没有到达，配对码没有被消耗）"),
            PairingError::Ambiguous => f.write_str(
                "请求已经发出，但没有收到完整的答复：配对码可能已经被兑换。请在云账号侧确认，必要时重新生成配对码",
            ),
            PairingError::CodeInvalid => f.write_str("配对码不对（检查是否输错）"),
            PairingError::CodeExpired => f.write_str("配对码已过期（有效期很短），请重新生成"),
            PairingError::CodeUsed => f.write_str("配对码已经被用过（一次性），请重新生成"),
            PairingError::CodeRevoked => f.write_str("配对码已被撤销，请重新生成"),
            PairingError::RateLimited => f.write_str("兑换失败次数过多，云端暂时拒绝兑换，请稍后再试"),
            PairingError::DeviceLimitReached => {
                f.write_str("这个云账号的设备数已达上限：请先在云账号侧解绑不用的设备")
            }
            PairingError::NoLongerEligible(reason) => write!(
                f,
                "云账号已经不再有资格配对新设备（云端原因：{reason}）"
            ),
            PairingError::CredentialNotAccepted => f.write_str("云端已经不认这张凭证（已撤销或已过期）"),
            PairingError::Cloud { status, reason } => match reason {
                Some(reason) => write!(f, "云端返回 HTTP {status}（{reason}）"),
                None => write!(f, "云端返回 HTTP {status}"),
            },
            PairingError::BadResponse => f.write_str("云端的答复不是约定的形状"),
        }
    }
}

impl std::error::Error for PairingError {}

/// 兑换成功的结果。
#[derive(Debug)]
pub struct Redeemed {
    pub secret: CloudCredential,
    pub credential_id: String,
    /// 云账号绑定：成员关系 id。
    pub membership: String,
    pub pairing_id: String,
}

/// 控制面客户端（只有配对要用的两个调用）。
pub struct PairingClient {
    http: reqwest::Client,
    control_url: Url,
}

impl PairingClient {
    /// `control_url` 是控制面基址（`…/control/v1`，不含结尾 `/`）。
    pub fn new(http: reqwest::Client, control_url: Url) -> Self {
        Self { http, control_url }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.control_url.as_str().trim_end_matches('/'))
    }

    /// 兑换配对码。**不带凭证**：码本身是凭据。
    pub async fn redeem(&self, code: &str, device_label: &str) -> Result<Redeemed, PairingError> {
        let response = self
            .http
            .post(self.endpoint("/pairing/redeem"))
            .timeout(REQUEST_TIMEOUT)
            .json(&json!({ "code": code, "device_label": device_label }))
            .send()
            .await
            .map_err(|error| classify_transport(&error, true))?;
        let (status, body) = read_capped(response).await?;
        if status == 200 || status == 201 {
            return parse_redeemed(&body);
        }
        Err(classify_status(status, &body))
    }

    /// 用设备凭证撤销它自己。成功（云端确认已撤销）返回 `Ok(())`；
    /// 云端说它已经不认这张凭证（401）返回 [`PairingError::CredentialNotAccepted`]：目的已经达到，但调用方要区分。
    pub async fn unpair(&self, credential: &CloudCredential) -> Result<(), PairingError> {
        let response = self
            .http
            .post(self.endpoint("/pairing/unpair"))
            .timeout(REQUEST_TIMEOUT)
            .header(reqwest::header::AUTHORIZATION, credential.authorization())
            .json(&json!({}))
            .send()
            .await
            .map_err(|error| classify_transport(&error, false))?;
        let (status, body) = read_capped(response).await?;
        match status {
            200 | 201 => {
                // 响应里应当有 credential.status == revoked；不是就当作形状不对，不假装成功
                let value: Value =
                    serde_json::from_slice(&body).map_err(|_| PairingError::BadResponse)?;
                match value
                    .pointer("/data/credential/status")
                    .and_then(Value::as_str)
                {
                    Some("revoked") => Ok(()),
                    _ => Err(PairingError::BadResponse),
                }
            }
            401 => Err(PairingError::CredentialNotAccepted),
            other => Err(classify_status(other, &body)),
        }
    }
}

/// 传输层错误：连接阶段失败（请求肯定没有到达）和「可能已经到达」要分开。
fn classify_transport(error: &reqwest::Error, redeem: bool) -> PairingError {
    if error.is_connect() {
        return PairingError::Unreachable;
    }
    if redeem {
        PairingError::Ambiguous
    } else {
        // 解绑没有「一次性」的问题：重试是安全的，不可达和超时对调用方是一回事。
        PairingError::Unreachable
    }
}

async fn read_capped(response: reqwest::Response) -> Result<(u16, Vec<u8>), PairingError> {
    let status = response.status().as_u16();
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    let read = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| PairingError::BadResponse)?;
            if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err(PairingError::BadResponse);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(())
    };
    tokio::time::timeout(REQUEST_TIMEOUT, read)
        .await
        .map_err(|_| PairingError::Ambiguous)??;
    Ok((status, body))
}

/// 云端错误体里的稳定原因码（`error.reason_code`）：必须是小写字母、数字、下划线，否则当作没有。
fn reason_of(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let reason = value.pointer("/error/reason_code")?.as_str()?;
    (!reason.is_empty()
        && reason.len() <= 64
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    .then(|| reason.to_string())
}

fn classify_status(status: u16, body: &[u8]) -> PairingError {
    let reason = reason_of(body);
    match reason.as_deref() {
        Some("pairing_code_invalid") => PairingError::CodeInvalid,
        Some("pairing_code_expired") => PairingError::CodeExpired,
        Some("pairing_code_used") => PairingError::CodeUsed,
        Some("pairing_code_revoked") => PairingError::CodeRevoked,
        Some("pairing_rate_limited") => PairingError::RateLimited,
        Some("device_limit_reached") => PairingError::DeviceLimitReached,
        Some(
            code @ ("membership_revoked"
            | "principal_revoked"
            | "grantor_has_no_authority"
            | "account_inactive"),
        ) => PairingError::NoLongerEligible(code.to_string()),
        _ if status == 429 => PairingError::RateLimited,
        _ => PairingError::Cloud { status, reason },
    }
}

fn valid_display_id(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn parse_redeemed(body: &[u8]) -> Result<Redeemed, PairingError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| PairingError::BadResponse)?;
    let text = |pointer: &str| -> Result<String, PairingError> {
        let text = value
            .pointer(pointer)
            .and_then(Value::as_str)
            .ok_or(PairingError::BadResponse)?;
        if valid_display_id(text) {
            Ok(text.to_string())
        } else {
            Err(PairingError::BadResponse)
        }
    };
    let secret = value
        .pointer("/data/secret")
        .and_then(Value::as_str)
        .ok_or(PairingError::BadResponse)?;
    Ok(Redeemed {
        secret: CloudCredential::parse(secret).map_err(|_| PairingError::BadResponse)?,
        credential_id: text("/data/credential/id")?,
        membership: text("/data/membership")?,
        pairing_id: text("/data/pairing_id")?,
    })
}

/// 校验设备名：非空、不超长、没有控制字符、没有首尾空白。
pub fn validate_device_label(label: &str) -> Result<(), String> {
    if label.is_empty()
        || label.chars().count() > MAX_DEVICE_LABEL_CHARS
        || label.trim() != label
        || label.chars().any(char::is_control)
    {
        return Err(format!(
            "设备名必须是 1 到 {MAX_DEVICE_LABEL_CHARS} 个字符，没有控制字符和首尾空白"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn secret(seed: char) -> String {
        format!("idk_{}", seed.to_string().repeat(64))
    }

    fn client(server: &MockServer) -> PairingClient {
        PairingClient::new(
            idoris_upstream::http_client().unwrap(),
            Url::parse(&format!("{}/control/v1", server.uri())).unwrap(),
        )
    }

    fn redeemed_body(seed: char) -> Value {
        json!({
            "result": "created", "reason_code": "ok", "request_id": "r",
            "data": {
                "credential": {"id": "vk_0123456789abcdef", "binding": {"kind": "member", "membership": "m-1"}, "status": "active"},
                "secret": secret(seed), "membership": "m-1", "pairing_id": "pc_0123456789abcdef"
            }
        })
    }

    fn error_body(reason: &str) -> Value {
        json!({"error": {"type": "x", "reason_code": reason, "message": "m", "request_id": "r"}})
    }

    #[tokio::test]
    async fn redeem_sends_only_the_code_and_the_label_and_no_credential() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/control/v1/pairing/redeem"))
            .and(body_json(
                json!({"code": "ABCD-EFGH-JKMN", "device_label": "my mac"}),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(redeemed_body('a')))
            .expect(1)
            .mount(&server)
            .await;
        let redeemed = client(&server)
            .redeem("ABCD-EFGH-JKMN", "my mac")
            .await
            .unwrap();
        assert_eq!(redeemed.secret.expose_secret(), secret('a'));
        assert_eq!(redeemed.credential_id, "vk_0123456789abcdef");
        assert_eq!(redeemed.membership, "m-1");
        let requests = server.received_requests().await.unwrap();
        assert!(
            !requests[0].headers.contains_key("authorization"),
            "the code itself is the credential; nothing else is sent"
        );
    }

    #[tokio::test]
    async fn every_cloud_refusal_maps_to_a_stable_error() {
        let server = MockServer::start().await;
        let cases: [(u16, &str, PairingError); 10] = [
            (400, "pairing_code_invalid", PairingError::CodeInvalid),
            (410, "pairing_code_expired", PairingError::CodeExpired),
            (409, "pairing_code_used", PairingError::CodeUsed),
            (410, "pairing_code_revoked", PairingError::CodeRevoked),
            (429, "pairing_rate_limited", PairingError::RateLimited),
            (
                409,
                "device_limit_reached",
                PairingError::DeviceLimitReached,
            ),
            (
                409,
                "membership_revoked",
                PairingError::NoLongerEligible("membership_revoked".into()),
            ),
            (
                409,
                "principal_revoked",
                PairingError::NoLongerEligible("principal_revoked".into()),
            ),
            (
                403,
                "account_inactive",
                PairingError::NoLongerEligible("account_inactive".into()),
            ),
            (
                503,
                "store_busy",
                PairingError::Cloud {
                    status: 503,
                    reason: Some("store_busy".into()),
                },
            ),
        ];
        for (status, reason, expected) in cases {
            server.reset().await;
            Mock::given(method("POST"))
                .and(path("/control/v1/pairing/redeem"))
                .respond_with(ResponseTemplate::new(status).set_body_json(error_body(reason)))
                .mount(&server)
                .await;
            let error = client(&server)
                .redeem("ABCD-EFGH-JKMN", "d")
                .await
                .unwrap_err();
            assert_eq!(error, expected, "{reason}");
            assert_eq!(error.code(), reason);
            assert!(!error.to_string().is_empty());
        }
    }

    #[tokio::test]
    async fn an_odd_error_body_is_not_trusted_for_the_reason() {
        let server = MockServer::start().await;
        for body in [
            "not json".to_string(),
            json!({"error": {"reason_code": "Has Space And CAPS"}}).to_string(),
            json!({"error": {"reason_code": "x".repeat(65)}}).to_string(),
            json!({"error": {}}).to_string(),
            String::new(),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(500).set_body_string(body.clone()))
                .mount(&server)
                .await;
            assert_eq!(
                client(&server).redeem("c", "d").await.unwrap_err(),
                PairingError::Cloud {
                    status: 500,
                    reason: None
                },
                "{body:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_malformed_success_body_is_a_bad_response_and_never_a_half_credential() {
        let server = MockServer::start().await;
        let mut bad_secret = redeemed_body('a');
        bad_secret["data"]["secret"] = json!("ab".repeat(32)); // 没有 idk_ 前缀（长得像 Admin token）
        let mut bad_id = redeemed_body('a');
        bad_id["data"]["membership"] = json!("has space");
        let mut missing = redeemed_body('a');
        missing["data"]
            .as_object_mut()
            .unwrap()
            .remove("pairing_id");
        for body in [bad_secret, bad_id, missing, json!({}), json!([])] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(201).set_body_json(body.clone()))
                .mount(&server)
                .await;
            assert_eq!(
                client(&server).redeem("c", "d").await.unwrap_err(),
                PairingError::BadResponse,
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_connection_means_the_code_was_not_consumed_but_a_lost_answer_is_ambiguous() {
        // 连接被拒：端口没人听
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dead = PairingClient::new(
            idoris_upstream::http_client().unwrap(),
            Url::parse(&format!("http://127.0.0.1:{port}/control/v1")).unwrap(),
        );
        assert_eq!(
            dead.redeem("c", "d").await.unwrap_err(),
            PairingError::Unreachable
        );
        assert_eq!(
            dead.unpair(&CloudCredential::parse(&secret('a')).unwrap())
                .await
                .unwrap_err(),
            PairingError::Unreachable
        );
        // 对方接受了连接，读了请求，然后什么都不回就断开：请求可能已经到达
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;
            drop(socket);
        });
        let flaky = PairingClient::new(
            idoris_upstream::http_client().unwrap(),
            Url::parse(&format!("http://127.0.0.1:{port}/control/v1")).unwrap(),
        );
        assert_eq!(
            flaky.redeem("c", "d").await.unwrap_err(),
            PairingError::Ambiguous
        );
    }

    #[tokio::test]
    async fn unpair_sends_the_credential_as_a_bearer_and_expects_a_revoked_credential_back() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/control/v1/pairing/unpair"))
            .and(header(
                "authorization",
                format!("Bearer {}", secret('a')).as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "updated", "reason_code": "ok", "request_id": "r",
                "data": {"credential": {"id": "vk_1", "status": "revoked"}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let credential = CloudCredential::parse(&secret('a')).unwrap();
        client(&server).unpair(&credential).await.unwrap();

        // 云端不认这张凭证（已撤销）：和成功分开报告
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(error_body("credential_revoked")),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).unpair(&credential).await.unwrap_err(),
            PairingError::CredentialNotAccepted
        );
        // 200 但状态不是 revoked：不假装成功
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"credential": {"status": "active"}}})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).unpair(&credential).await.unwrap_err(),
            PairingError::BadResponse
        );
        // 其它错误
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_json(error_body("not_a_device")))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).unpair(&credential).await.unwrap_err(),
            PairingError::Cloud {
                status: 403,
                reason: Some("not_a_device".into())
            }
        );
    }

    #[tokio::test]
    async fn a_redirect_is_never_followed_and_an_oversized_answer_is_refused() {
        let server = MockServer::start().await;
        let outside = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(redeemed_body('a')))
            .mount(&outside)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/capture", outside.uri())),
            )
            .mount(&server)
            .await;
        let error = client(&server).redeem("c", "d").await.unwrap_err();
        assert_eq!(
            error,
            PairingError::Cloud {
                status: 307,
                reason: None
            }
        );
        assert!(
            outside.received_requests().await.unwrap().is_empty(),
            "the redirect target is never contacted"
        );

        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_bytes(vec![b'x'; MAX_BODY_BYTES + 1]))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).redeem("c", "d").await.unwrap_err(),
            PairingError::BadResponse
        );
    }

    #[test]
    fn device_labels_are_validated_and_the_default_does_not_leak_the_machine() {
        assert!(validate_device_label(DEFAULT_DEVICE_LABEL).is_ok());
        assert!(validate_device_label("Jason's MacBook").is_ok());
        assert!(validate_device_label(&"é".repeat(64)).is_ok());
        for bad in ["", " lead", "trail ", "new\nline", "tab\t", &"x".repeat(65)] {
            assert!(validate_device_label(bad).is_err(), "{bad:?}");
        }
        let hostname = std::process::Command::new("hostname").output().ok();
        if let Some(output) = hostname {
            let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
            assert!(host.is_empty() || !DEFAULT_DEVICE_LABEL.contains(&host));
        }
    }

    #[test]
    fn error_texts_carry_no_secrets_and_every_error_has_a_code() {
        for error in [
            PairingError::Unreachable,
            PairingError::Ambiguous,
            PairingError::CodeInvalid,
            PairingError::CodeExpired,
            PairingError::CodeUsed,
            PairingError::CodeRevoked,
            PairingError::RateLimited,
            PairingError::DeviceLimitReached,
            PairingError::NoLongerEligible("membership_revoked".into()),
            PairingError::CredentialNotAccepted,
            PairingError::Cloud {
                status: 500,
                reason: None,
            },
            PairingError::BadResponse,
        ] {
            assert!(!error.code().is_empty());
            let text = format!("{error} {error:?}");
            assert!(!text.contains("idk_"));
        }
    }
}

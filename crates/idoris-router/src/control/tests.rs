//! 控制面的 HTTP 层测试：形态隔离、认证（来源不参与）、三类密钥互不通用、授权矩阵、越权签发、
//! 撤销即时生效（含代理响应缓存）、审计、配对、运维 bootstrap。
//!
//! 存储层的授权矩阵、审计触发器、配对状态机在 `idoris-tenancy::accounts` 的测试里，这里验证的是 HTTP 契约和接线。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::ids::TimestampMs;
use idoris_contracts::ComponentCard;
use idoris_contracts::DeployMode;
use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};

use super::bootstrap;
use super::{AccountGate, CONTROL_PREFIX, ControlClock, ControlPlane};
use crate::admin::{AdminSessionToken, build_admin_app};
use crate::auth::VirtualKeyAuthenticator;
use crate::{AppState, build_app};

const T0: i64 = 1_793_534_400_000; // 2026-11-01T12:00:00Z
const DAY_MS: i64 = 86_400_000;
const ROLES: [&str; 8] = [
    "fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto",
];

struct ManualClock(AtomicI64);

impl ManualClock {
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl ControlClock for ManualClock {
    fn now(&self) -> TimestampMs {
        TimestampMs(self.0.load(Ordering::SeqCst))
    }
}

/// 一次 HTTP 回复。
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
    raw: String,
}

impl Reply {
    fn reason(&self) -> &str {
        self.json["error"]["reason_code"]
            .as_str()
            .unwrap_or_else(|| panic!("not an error reply: {}", self.raw))
    }
}

/// 夹具里的凭证：id 和明文密钥。
#[derive(Clone)]
struct Cred {
    id: String,
    secret: String,
}

struct World {
    _dir: tempfile::TempDir,
    db: PathBuf,
    accounts: Arc<AccountStore>,
    keys: Arc<Mutex<VirtualKeyStore>>,
    clock: Arc<ManualClock>,
    app: Router,
    state: AppState,
    /// 运维 bootstrap 签发的凭证：root（eduai 全部操作）、teacher（teacher 子树）、reader（eduai 只读）、other（school-b 全部操作）。
    root: Cred,
    teacher: Cred,
    reader: Cred,
    other: Cred,
}

fn all_ops(subtree: &str, grant_cap: i64) -> Vec<Value> {
    [
        "create_account",
        "set_account_status",
        "manage_membership",
        "issue_credential",
        "revoke_credential",
        "freeze_credential",
        "grant_allocation",
        "create_principal",
        "revoke_principal",
        "read",
    ]
    .iter()
    .map(|op| {
        if *op == "grant_allocation" {
            json!({"op": op, "subtree": subtree, "max_amount": {"unit": "THB", "micros": grant_cap}})
        } else {
            json!({"op": op, "subtree": subtree})
        }
    })
    .collect()
}

fn fixture_spec() -> Value {
    json!({
        "accounts": [
            {"account_id": "platform", "kind": "platform", "unit": "THB"},
            {"account_id": "pool", "kind": "subsidy_pool", "parent": "platform", "unit": "THB"},
            {"account_id": "eduai", "kind": "org", "parent": "pool", "unit": "THB"},
            {"account_id": "teacher", "kind": "sponsor", "parent": "eduai", "unit": "THB"},
            {"account_id": "s1", "kind": "member", "parent": "teacher", "unit": "THB"},
            {"account_id": "s2", "kind": "member", "parent": "eduai", "unit": "THB"},
            {"account_id": "school-b", "kind": "org", "parent": "platform", "unit": "THB"},
            {"account_id": "b1", "kind": "member", "parent": "school-b", "unit": "THB"}
        ],
        "allocations": [
            alloc("a-pool", "platform", "pool", 10_000_000_000_i64),
            alloc("a-edu", "pool", "eduai", 1_000_000_000_i64),
            alloc("a-teacher", "eduai", "teacher", 100_000_000_i64),
            alloc("a-b", "platform", "school-b", 500_000_000_i64)
        ],
        "credentials": [
            {
                "binding": {"kind": "service", "account": "eduai"},
                "admin_scopes": all_ops("eduai", 100_000_000),
                "owner": "root", "tenant_id": "tenant-edu",
                "allowed_privacy": ["local_only", "any"], "allowed_roles": ROLES
            },
            {
                "binding": {"kind": "service", "account": "eduai"},
                "admin_scopes": all_ops("teacher", 5_000_000),
                "owner": "teacher", "tenant_id": "tenant-edu",
                "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]
            },
            {
                "binding": {"kind": "service", "account": "eduai"},
                "admin_scopes": [{"op": "read", "subtree": "eduai"}],
                "owner": "reader", "tenant_id": "tenant-edu",
                "allowed_privacy": ["local_only"], "allowed_roles": []
            },
            {
                "binding": {"kind": "service", "account": "school-b"},
                "admin_scopes": all_ops("school-b", 100_000_000),
                "owner": "other", "tenant_id": "tenant-b",
                "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]
            }
        ]
    })
}

fn alloc(id: &str, parent: &str, child: &str, micros: i64) -> Value {
    json!({
        "allocation_id": id, "parent": parent, "child": child,
        "amount": {"unit": "THB", "micros": micros}, "purpose": "prepaid",
        "valid_from_ms": T0, "valid_until_ms": T0 + 365 * DAY_MS
    })
}

impl World {
    fn new() -> Self {
        Self::with_spec(&fixture_spec())
    }

    fn with_spec(spec: &Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("control.sqlite3");
        let keys = Arc::new(Mutex::new(VirtualKeyStore::open(&db).unwrap()));
        let accounts = Arc::new(AccountStore::open(&db).unwrap());
        let clock = Arc::new(ManualClock(AtomicI64::new(T0)));
        // 运维 bootstrap：和 `idoris control bootstrap --spec-stdin` 同一个函数
        let mut output = Vec::new();
        bootstrap::run(
            &accounts,
            &mut spec.to_string().as_bytes(),
            &mut output,
            clock.as_ref(),
        )
        .unwrap();
        let credentials: Vec<Cred> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|line| line["kind"] == "credential")
            .map(|line| Cred {
                id: line["credential_id"].as_str().unwrap().to_string(),
                secret: line["secret"].as_str().unwrap().to_string(),
            })
            .collect();
        let plane = ControlPlane::new(accounts.clone(), DeploymentForm::Organization)
            .unwrap()
            .with_clock(clock.clone());
        let authenticator = VirtualKeyAuthenticator::new(keys.clone())
            .with_gate(Arc::new(AccountGate::new(accounts.clone())));
        let state = AppState {
            deploy_mode: DeployMode::Tenant,
            virtual_key_authenticator: Some(authenticator),
            control: Some(Arc::new(plane)),
            ..AppState::default()
        };
        let mut iter = credentials.into_iter();
        let (root, teacher, reader, other) = (
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
        );
        Self {
            _dir: dir,
            db,
            accounts,
            keys,
            clock,
            app: build_app(state.clone()),
            state,
            root,
            teacher,
            reader,
            other,
        }
    }

    async fn call(
        &self,
        verb: &str,
        uri: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        self.call_with(verb, uri, bearer, body, &[]).await
    }

    async fn call_with(
        &self,
        verb: &str,
        uri: &str,
        bearer: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Reply {
        send(&self.app, verb, uri, bearer, body, headers).await
    }

    /// 期望成功（2xx）的调用，返回 `data`。
    async fn ok(&self, uri: &str, bearer: &str, body: Value) -> Value {
        let reply = self.call("POST", uri, Some(bearer), Some(body)).await;
        assert!(
            reply.status.is_success(),
            "POST {uri} -> {} {}",
            reply.status,
            reply.raw
        );
        reply.json["data"].clone()
    }

    fn db(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// 创建夹具里的主体和成员关系（经 HTTP，用 root 凭证）：kid1 -> s1（teacher 下）、kid2 -> s2、tch -> teacher（担保人）。
    async fn people(&self) {
        let root = self.root.secret.clone();
        for (principal, membership, org, role, account) in [
            ("kid1", "m-kid1", "eduai", "member", "s1"),
            ("kid2", "m-kid2", "eduai", "member", "s2"),
            ("tch", "m-tch", "eduai", "sponsor", "teacher"),
        ] {
            self.ok(
                "/control/v1/principals",
                &root,
                json!({"principal_id": principal, "org": org}),
            )
            .await;
            self.ok(
                "/control/v1/memberships",
                &root,
                json!({"membership_id": membership, "principal_id": principal, "org": org,
                       "role": role, "account": account}),
            )
            .await;
        }
    }

    /// 签发一张成员凭证（经 HTTP，用 root 凭证），数据面上限 local_only + fast。
    async fn member_credential(&self, membership: &str) -> Cred {
        let data = self
            .ok(
                "/control/v1/credentials",
                &self.root.secret,
                json!({"binding": {"kind": "member", "membership": membership},
                       "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
            )
            .await;
        Cred {
            id: data["credential"]["id"].as_str().unwrap().to_string(),
            secret: data["secret"].as_str().unwrap().to_string(),
        }
    }

    /// 数据面上的一次认证请求（`GET /v1/models`：走完整的认证中间件，不碰上游）。
    async fn data_plane(&self, token: &str) -> StatusCode {
        send(&self.app, "GET", "/v1/models", Some(token), None, &[])
            .await
            .status
    }

    fn audit_rows(&self) -> Vec<(String, String, String, String, String)> {
        let conn = self.db();
        let mut stmt = conn
            .prepare(
                "SELECT actor_kind, actor_id, op, outcome, reason_code FROM control_audit ORDER BY audit_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }
}

async fn send(
    app: &Router,
    verb: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    send_from(app, None, verb, uri, bearer, body, headers).await
}

/// 和 [`send`] 一样，只是可以指定 TCP 对端地址：真实的监听器会给每个请求挂上对端地址（`ConnectInfo`），
/// 直接 `oneshot` 驱动路由的测试默认没有（来源未知）。
async fn send_from(
    app: &Router,
    peer: Option<&str>,
    verb: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut builder = Request::builder().method(verb).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    if let Some(peer) = peer {
        let peer = std::net::SocketAddr::new(peer.parse().unwrap(), 50_000);
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            crate::connection::ConnectionInfo::for_peer(peer),
        ));
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8_lossy(&bytes).to_string();
    Reply {
        status,
        headers,
        json: serde_json::from_str(&raw).unwrap_or(Value::Null),
        raw,
    }
}

/// 每条控制面路由（除了兑换，它不带凭证）的一个样例请求。
fn all_routes() -> Vec<(&'static str, &'static str, Option<Value>)> {
    let some = Some(json!({}));
    vec![
        ("GET", "/control/v1/whoami", None),
        ("POST", "/control/v1/principals", some.clone()),
        ("POST", "/control/v1/principals/kid1/revoke", some.clone()),
        ("POST", "/control/v1/accounts", some.clone()),
        ("POST", "/control/v1/accounts/s1/status", some.clone()),
        ("POST", "/control/v1/memberships", some.clone()),
        (
            "POST",
            "/control/v1/memberships/m-kid1/revoke",
            some.clone(),
        ),
        ("POST", "/control/v1/credentials", some.clone()),
        ("POST", "/control/v1/credentials/vk_x/status", some.clone()),
        ("POST", "/control/v1/allocations", some.clone()),
        (
            "POST",
            "/control/v1/allocations/a-teacher/revoke",
            some.clone(),
        ),
        ("POST", "/control/v1/pairing/codes", some.clone()),
        (
            "POST",
            "/control/v1/pairing/codes/pc_0123456789abcdef/revoke",
            some.clone(),
        ),
        ("POST", "/control/v1/pairing/unpair", some),
        ("GET", "/control/v1/accounts/eduai", None),
        ("GET", "/control/v1/accounts/eduai/subtree", None),
        ("GET", "/control/v1/accounts/eduai/allocations", None),
        ("GET", "/control/v1/accounts/eduai/credentials", None),
        ("GET", "/control/v1/accounts/eduai/memberships", None),
        ("GET", "/control/v1/principals/kid1", None),
        ("GET", "/control/v1/audit?account=eduai", None),
    ]
}

// ================================================================================================
// 形态隔离
// ================================================================================================

#[tokio::test]
async fn without_a_control_plane_none_of_the_routes_exist_and_the_data_plane_answers_exactly_as_before()
 {
    // 个人形态（默认）和存量 tenant 形态：`AppState::control` 是 None，控制面前缀下什么都没有
    for deploy_mode in [DeployMode::Personal, DeployMode::Tenant] {
        let state = AppState {
            deploy_mode,
            ..AppState::default()
        };
        assert!(state.control.is_none());
        let app = build_app(state);
        let mut paths: Vec<(&str, &str)> = all_routes().iter().map(|(v, p, _)| (*v, *p)).collect();
        paths.push(("POST", "/control/v1/pairing/redeem"));
        paths.push(("GET", "/control/v1"));
        paths.push(("POST", "/control/v1/anything/at/all"));
        for (verb, uri) in paths {
            for token in [None, Some("idk_".to_string() + &"0".repeat(64))] {
                let reply = send(&app, verb, uri, token.as_deref(), Some(json!({})), &[]).await;
                // 个人形态没有认证要求；tenant 形态里数据面的认证中间件先说话（401）。两种都不是控制面的回复。
                assert!(
                    matches!(
                        reply.status,
                        StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::BAD_REQUEST
                    ),
                    "{deploy_mode:?} {verb} {uri}: {}",
                    reply.status
                );
                assert!(
                    reply.json["error"]["reason_code"] != "unauthenticated"
                        && !reply.raw.contains("\"request_id\""),
                    "{deploy_mode:?} {verb} {uri}: must not be a control-plane reply: {}",
                    reply.raw
                );
            }
        }
    }
    // 个人形态里路径就是 404（没有别的东西挡在前面）
    let app = build_app(AppState::default());
    for (verb, uri, body) in all_routes() {
        let reply = send(&app, verb, uri, None, body, &[]).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{verb} {uri}");
        assert_eq!(
            reply.json,
            json!({"error": {"type": "not_found"}}),
            "{verb} {uri}"
        );
    }
    // 个人形态永远造不出控制面
    let accounts_dir = tempfile::tempdir().unwrap();
    let accounts = Arc::new(AccountStore::open(accounts_dir.path().join("a.sqlite3")).unwrap());
    let refused = ControlPlane::new(accounts, DeploymentForm::Personal).unwrap_err();
    assert!(refused.contains("personal"), "{refused}");
}

#[tokio::test]
async fn the_control_prefix_is_served_only_where_a_control_plane_is_mounted() {
    let world = World::new();
    // 挂了：未知路径和不对的方法都是控制面自己的 JSON 404，不是数据面的
    for (verb, uri) in [
        ("GET", "/control/v1"),
        ("GET", "/control/v1/nope"),
        ("POST", "/control/v1/nope/deeper"),
        ("GET", "/control/v1/principals"), // 这个路径只有 POST
        ("POST", "/control/v1/whoami"),    // 这个路径只有 GET
        ("DELETE", "/control/v1/accounts/s1"),
    ] {
        let reply = world
            .call(verb, uri, Some(&world.root.secret), Some(json!({})))
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{verb} {uri}");
        assert_eq!(reply.reason(), "not_found");
        assert!(reply.json["error"]["request_id"].is_string());
    }
    // 控制面之外的路径不受影响（仍然是数据面自己的行为）
    let health = world.call("GET", "/health", None, None).await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json["status"], "ok");
}

// ================================================================================================
// 认证：来源不参与；三类密钥互不通用
// ================================================================================================

#[tokio::test]
async fn every_route_demands_a_credential_whatever_the_origin_says() {
    let world = World::new();
    let origin_tricks: Vec<Vec<(&str, &str)>> = vec![
        vec![],
        vec![("x-forwarded-for", "127.0.0.1")],
        vec![
            ("x-forwarded-for", "127.0.0.1, 10.0.0.1"),
            ("x-real-ip", "127.0.0.1"),
        ],
        vec![("forwarded", "for=127.0.0.1;host=localhost;proto=https")],
        vec![
            ("host", "localhost"),
            ("origin", "http://localhost"),
            ("referer", "http://127.0.0.1/"),
        ],
        vec![
            ("x-idoris-tenant", "tenant-edu"),
            ("x-idoris-caller", "admin/self-asserted"),
        ],
    ];
    for (verb, uri, body) in all_routes() {
        assert!(uri.starts_with(CONTROL_PREFIX), "{uri}");
        let mut bodies: Vec<String> = Vec::new();
        for tricks in &origin_tricks {
            let reply = world.call_with(verb, uri, None, body.clone(), tricks).await;
            assert_eq!(
                reply.status,
                StatusCode::UNAUTHORIZED,
                "{verb} {uri} {tricks:?}"
            );
            assert_eq!(reply.reason(), "unauthenticated", "{verb} {uri} {tricks:?}");
            bodies.push(reply.json["error"]["message"].as_str().unwrap().to_string());
            assert_eq!(
                reply.headers["cache-control"], "no-store",
                "{verb} {uri}: responses are never cacheable"
            );
        }
        // 缺失、格式不对、不认识的密钥：同一个答案，调用方探测不出区别
        let stranger = format!("idk_{}", "ab".repeat(32));
        for token in [
            "not-a-key".to_string(),
            "idk_short".to_string(),
            stranger.clone(),
            format!("IDK_{}", "ab".repeat(32)),
        ] {
            let reply = world.call(verb, uri, Some(&token), body.clone()).await;
            assert_eq!(
                reply.status,
                StatusCode::UNAUTHORIZED,
                "{verb} {uri} {token}"
            );
            assert_eq!(reply.reason(), "unauthenticated");
            assert_eq!(reply.json["error"]["message"], bodies[0]);
        }
        // 另一种 scheme、重复的头
        let basic = send(
            &world.app,
            verb,
            uri,
            None,
            body.clone(),
            &[("authorization", &format!("Basic {}", world.root.secret))],
        )
        .await;
        assert_eq!(basic.status, StatusCode::UNAUTHORIZED);
    }
    // 没有任何一个这样的请求留下审计行（认不出的密钥不写审计）
    assert!(
        world.audit_rows().iter().all(|row| row.0 == "operator"),
        "unauthenticated requests are not audited"
    );
}

#[tokio::test]
async fn the_admin_token_the_local_data_key_and_the_control_credential_are_three_different_things()
{
    let world = World::new();
    let admin_token = AdminSessionToken::mint();
    let admin_app = build_admin_app(
        world.state.clone(),
        AdminSessionToken::from_launcher_secret(admin_token.expose_secret().as_bytes()).unwrap(),
    );

    // Admin token 不能当控制面凭证：没有 `idk_` 前缀，根本不是虚拟 key
    for (verb, uri, body) in all_routes() {
        let reply = world
            .call(verb, uri, Some(admin_token.expose_secret()), body)
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{verb} {uri}");
        assert_eq!(reply.reason(), "unauthenticated");
    }
    // 控制面凭证（以及本地数据密钥）也不能调用 Admin v0
    let local_key = MintedVirtualKey::mint();
    world
        .keys
        .lock()
        .unwrap()
        .insert_active(
            &local_key.key_id,
            local_key.hash,
            &VirtualKeyScope {
                owner: "local".into(),
                tenant_id: Some("tenant-edu".into()),
                allowed_privacy: vec![PrivacyClass::LocalOnly],
                allowed_roles: vec!["fast".into()],
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    for token in [world.root.secret.as_str(), local_key.secret.expose_secret()] {
        let reply = send(
            &admin_app,
            "GET",
            "/admin/api/v1/status",
            Some(token),
            None,
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    // 对照：Admin token 在 Admin 接口上是通的（说明上面的 401 来自密钥不对，不是接口坏了）
    let reply = send(
        &admin_app,
        "GET",
        "/admin/api/v1/status",
        Some(admin_token.expose_secret()),
        None,
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    // Admin 路由没有被挂到数据面，控制面路由也没有被挂到 Admin 上
    let reply = send(
        &world.app,
        "GET",
        "/admin/api/v1/status",
        Some(admin_token.expose_secret()),
        None,
        &[],
    )
    .await;
    assert_ne!(reply.status, StatusCode::OK);
    let reply = send(
        &admin_app,
        "GET",
        "/control/v1/whoami",
        Some(&world.root.secret),
        None,
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    // 本地数据密钥在控制面：认得出，但不是云凭证，没有任何控制面权限；留一行审计（actor 是它的 key id）
    let reply = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(local_key.secret.expose_secret()),
            Some(json!({"account_id": "x", "kind": "member", "parent": "eduai", "unit": "THB"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.reason(), "not_a_control_credential");
    let last = world.audit_rows().pop().unwrap();
    assert_eq!(last.1, local_key.key_id);
    assert_eq!(last.4, "not_a_control_credential");
    // 反过来，控制面凭证是数据面的合法凭证（Service 凭证本来就两用），数据面认证不受影响
    assert_eq!(world.data_plane(&world.root.secret).await, StatusCode::OK);
    assert_eq!(world.data_plane(&world.reader.secret).await, StatusCode::OK);
    assert_eq!(
        world.data_plane(admin_token.expose_secret()).await,
        StatusCode::UNAUTHORIZED
    );
}

/// 源码里（注释除外）读取请求来源的行：对端地址、转发头、回环判断……返回 `文件名:行号: 命中的词: 原行`。
///
/// `extractor` 给了的话，这份源码里**只有**那个提取器（`impl<S: Send + Sync> FromRequestParts<S> for <extractor>` 到它结尾的
/// `}`）允许读来源；`use` 行不算（导入本身读不了什么），`#[cfg(test)]` 之后的单元测试不算运行期代码。
fn origin_reads(name: &str, source: &str, extractor: Option<&str>) -> Vec<String> {
    const NEEDLES: [&str; 8] = [
        "connectinfo",
        "x-forwarded",
        "x-real-ip",
        "remote_addr",
        "peer_addr",
        "is_loopback",
        "forwarded",
        "origin",
    ];
    let allowed_impl = extractor
        .map(|extractor| format!("impl<S: Send + Sync> FromRequestParts<S> for {extractor}"));
    let mut inside_extractor = false;
    let mut found = Vec::new();
    for (number, line) in source.lines().enumerate() {
        let code = line.trim_start();
        if extractor.is_some() {
            if code.starts_with("#[cfg(test)]") {
                break;
            }
            if allowed_impl
                .as_deref()
                .is_some_and(|head| code.starts_with(head))
            {
                inside_extractor = true;
            }
            if inside_extractor {
                // 顶格的 `}` 结束这个 impl
                inside_extractor = line != "}";
                continue;
            }
            if code.starts_with("use ") {
                continue;
            }
        }
        if code.starts_with("//") {
            continue;
        }
        let lowered = code.to_ascii_lowercase();
        if let Some(needle) = NEEDLES.iter().find(|needle| lowered.contains(**needle)) {
            found.push(format!("{name}:{}: {needle}: {line}", number + 1));
        }
    }
    found
}

#[test]
fn the_control_plane_sources_never_look_at_where_a_request_came_from() {
    // 组织局域网也不默认互信：授权只由凭证决定。把「看来源」这件事在源码层面钉死：处理函数里没有任何
    // 读取对端地址或转发头的代码（注释除外）。
    for (name, source) in [
        ("handlers.rs", include_str!("handlers.rs")),
        ("mod.rs", include_str!("mod.rs")),
        ("wire.rs", include_str!("wire.rs")),
        ("bootstrap.rs", include_str!("bootstrap.rs")),
    ] {
        let found = origin_reads(name, source, None);
        assert!(
            found.is_empty(),
            "control-plane code must not consult the request origin: {found:#?}"
        );
    }
    // 配对兑换的失败限速器按来源分桶（#603 N3），读来源的代码在 `pairing_codes.rs` 里。PR-Daemon 在 #607 评审里指出：那个文件
    // 不在上面的扫描里。这里把它也扫一遍，只有 `RedeemSource` 提取器允许读来源；限速器自己（`acquire` 等）拿到的只是
    // 提取器算好的地址，不能再按「回环」「转发头」之类另做文章。
    let found = origin_reads(
        "pairing_codes.rs",
        include_str!("pairing_codes.rs"),
        Some("RedeemSource"),
    );
    assert!(
        found.is_empty(),
        "only the RedeemSource extractor may consult the request origin: {found:#?}"
    );
}

/// 上面那条扫描自己不能是摆设：提取器之外的读取、被误当成注释或 `use` 行遮住的读取都要被抓到，提取器之内的、注释里的、
/// 单元测试里的不报。
#[test]
fn the_origin_scan_flags_a_read_outside_the_one_allowed_extractor() {
    let sample = "\
use axum::extract::connect_info::ConnectInfo;
// X-Forwarded-For 只在注释里出现
impl FailureLimiter {
    fn acquire(&self, source: IpAddr) -> Option<Ticket> {
        if source.is_loopback() { return Some(Ticket(0)); }
        None
    }
}
impl<S: Send + Sync> FromRequestParts<S> for RedeemSource {
    async fn from_request_parts(parts: &mut Parts) -> Result<Self, Self::Rejection> {
        let peer = parts.extensions.get::<ConnectInfo<ConnectionInfo>>();
        Ok(Self(peer))
    }
}
fn later(headers: &HeaderMap) { let _ = headers.get(\"x-forwarded-for\"); }
#[cfg(test)]
mod tests {
    fn t(source: IpAddr) { source.is_loopback(); }
}
";
    let found = origin_reads("sample.rs", sample, Some("RedeemSource"));
    assert_eq!(found.len(), 2, "{found:#?}");
    assert!(
        found[0].starts_with("sample.rs:5: is_loopback:"),
        "{found:#?}"
    );
    assert!(
        found[1].starts_with("sample.rs:15: x-forwarded:"),
        "{found:#?}"
    );
    // 没有提取器的白名单时，同一份源码里 `use` 行、提取器里的读取也都算
    let strict = origin_reads("sample.rs", sample, None);
    assert!(strict.len() > found.len(), "{strict:#?}");
    // 提取器换了名字，它里面的读取就不再被放过
    let renamed = origin_reads("sample.rs", sample, Some("SomethingElse"));
    assert!(
        renamed
            .iter()
            .any(|line| line.contains(":11: connectinfo:")),
        "{renamed:#?}"
    );
}

// ================================================================================================
// 写操作的结果结构
// ================================================================================================

#[tokio::test]
async fn write_operations_return_one_stable_result_shape_and_a_correlation_id() {
    let world = World::new();
    let root = world.root.secret.clone();
    let reply = world
        .call(
            "POST",
            "/control/v1/principals",
            Some(&root),
            Some(json!({"principal_id": "kid1", "org": "eduai"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.json["result"], "created");
    assert_eq!(reply.json["reason_code"], "ok");
    assert_eq!(
        reply.json["data"]["principal"],
        json!({"principal_id": "kid1", "status": "active"})
    );
    let request_id = reply.json["request_id"].as_str().unwrap().to_string();
    assert_eq!(
        reply.headers["x-idoris-record-id"].to_str().unwrap(),
        request_id
    );
    assert_eq!(reply.headers["cache-control"], "no-store");
    assert_eq!(reply.headers["x-content-type-options"], "nosniff");
    // 同样的请求再发一次：200 replayed，仍然是 ok
    let again = world
        .call(
            "POST",
            "/control/v1/principals",
            Some(&root),
            Some(json!({"principal_id": "kid1", "org": "eduai"})),
        )
        .await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.json["result"], "replayed");
    assert_eq!(again.json["reason_code"], "ok");
    assert_ne!(again.json["request_id"], reply.json["request_id"]);
    // 审计行的 request_id 和响应头对得上
    let conn = world.db();
    let matched: i64 = conn
        .query_row(
            "SELECT count(*) FROM control_audit WHERE request_id = ?1 AND op = 'create_principal' AND outcome = 'success'",
            [&request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(matched, 1);

    // 账户
    let account = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(&root),
            Some(json!({"account_id": "s3", "kind": "member", "parent": "eduai", "unit": "THB"})),
        )
        .await;
    assert_eq!(account.status, StatusCode::CREATED);
    assert_eq!(
        account.json["data"]["account"],
        json!({"id": "s3", "kind": "member", "parent": "eduai", "unit": "THB", "status": "active"})
    );
    let status = world
        .call(
            "POST",
            "/control/v1/accounts/s3/status",
            Some(&root),
            Some(json!({"status": "frozen"})),
        )
        .await;
    assert_eq!(
        (status.status, status.json["result"].as_str()),
        (StatusCode::OK, Some("updated"))
    );
    assert_eq!(status.json["data"]["account"]["status"], "frozen");
    let status = world
        .call(
            "POST",
            "/control/v1/accounts/s3/status",
            Some(&root),
            Some(json!({"status": "frozen"})),
        )
        .await;
    assert_eq!(status.json["result"], "unchanged");

    // 稳定的失败结构：type / reason_code / message / request_id，没有任何别的租户的信息
    let denied = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(&root),
            Some(
                json!({"account_id": "x1", "kind": "member", "parent": "school-b", "unit": "THB"}),
            ),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.json["error"]["type"], "forbidden",);
    assert_eq!(denied.reason(), "out_of_scope");
    assert!(denied.json["error"]["request_id"].is_string());
    assert!(
        !denied.raw.contains("school-b"),
        "denials never echo the target: {}",
        denied.raw
    );
    // 同一个答案：目标不存在
    let ghost = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(&root),
            Some(json!({"account_id": "x2", "kind": "member", "parent": "ghost", "unit": "THB"})),
        )
        .await;
    assert_eq!(
        (ghost.status, ghost.reason()),
        (StatusCode::FORBIDDEN, "out_of_scope")
    );
    assert_eq!(
        ghost.json["error"]["message"],
        denied.json["error"]["message"]
    );
    // 冲突
    let conflict = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(&root),
            Some(json!({"account_id": "s3", "kind": "member", "parent": "teacher", "unit": "THB"})),
        )
        .await;
    assert_eq!(
        (conflict.status, conflict.reason()),
        (StatusCode::CONFLICT, "already_exists")
    );
    // S0 的账户树校验
    let invalid = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(&root),
            Some(json!({"account_id": "x3", "kind": "member", "parent": "s2", "unit": "THB"})),
        )
        .await;
    assert_eq!(
        (invalid.status, invalid.reason()),
        (StatusCode::BAD_REQUEST, "invalid_account")
    );
    // 「给自己所在的组织发额度」：接收方 eduai 在 root 的子树里，授权方补贴池不在，所以被拒
    let self_grant = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&root),
            Some(
                json!({"allocation_id": "self-1", "parent": "pool", "child": "eduai",
                        "amount": {"unit": "THB", "micros": 1}, "purpose": "prepaid",
                        "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS}),
            ),
        )
        .await;
    assert_eq!(
        (self_grant.status, self_grant.reason()),
        (StatusCode::FORBIDDEN, "out_of_scope")
    );
    // 额度：发放、重发、撤销
    let grant = json!({"allocation_id": "g1", "parent": "teacher", "child": "s1",
        "amount": {"unit": "THB", "micros": 2_000_000}, "purpose": "prepaid",
        "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS});
    let created = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&root),
            Some(grant.clone()),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(
        created.json["data"]["allocation"]["ceiling"]["amount"],
        json!({"unit": "THB", "micros": 2_000_000})
    );
    assert_eq!(
        created.json["data"]["allocation"]["ceiling"]["transferable"],
        false
    );
    assert!(created.json["data"]["revoked_at_ms"].is_null());
    let replay = world
        .call("POST", "/control/v1/allocations", Some(&root), Some(grant))
        .await;
    assert_eq!(replay.json["result"], "replayed");
    let revoked = world
        .call(
            "POST",
            "/control/v1/allocations/g1/revoke",
            Some(&root),
            None,
        )
        .await;
    assert_eq!(revoked.json["result"], "updated");
    assert_eq!(revoked.json["data"]["revoked_at_ms"], T0);
    let revoked_again = world
        .call(
            "POST",
            "/control/v1/allocations/g1/revoke",
            Some(&root),
            None,
        )
        .await;
    assert_eq!(revoked_again.json["result"], "unchanged");
}

#[tokio::test]
async fn a_guarantor_opens_and_closes_a_credit_line_within_its_own_authority() {
    // 授信（设计稿 §5「有担保的负值」）：担保人（teacher）给成员 s1 开一笔 purpose = credit_line 的额度，
    // 额度上限是担保人这条权限的单笔上限，关授信 = 撤销这笔额度。
    let world = World::new();
    world.people().await;
    let credit = |id: &str, micros: i64| {
        json!({"allocation_id": id, "parent": "teacher", "child": "s1",
               "amount": {"unit": "THB", "micros": micros}, "purpose": "credit_line",
               "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS})
    };
    let teacher = world.teacher.secret.clone();
    let opened = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&teacher),
            Some(credit("credit-1", 2_000_000)),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.raw);
    assert_eq!(
        opened.json["data"]["allocation"]["ceiling"]["purpose"],
        "credit_line"
    );
    // 超过担保人自己的单笔上限（5 THB）：拒绝
    let too_much = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&teacher),
            Some(credit("credit-2", 5_000_001)),
        )
        .await;
    assert_eq!(
        (too_much.status, too_much.reason()),
        (StatusCode::FORBIDDEN, "amount_exceeds_authority")
    );
    // 授信不能开在别的担保人名下
    let elsewhere = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&teacher),
            Some(
                json!({"allocation_id": "credit-3", "parent": "eduai", "child": "s2",
                        "amount": {"unit": "THB", "micros": 1}, "purpose": "credit_line",
                        "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS}),
            ),
        )
        .await;
    assert_eq!(
        (elsewhere.status, elsewhere.reason()),
        (StatusCode::FORBIDDEN, "out_of_scope")
    );
    let live = world
        .accounts
        .allocations_into(&idoris_compute_contracts::ids::AccountId::new("s1").unwrap())
        .unwrap();
    assert_eq!(live.len(), 1);
    // 关授信：撤销之后，读额度的接口把它标成已撤销，存储层的读函数不再把它当作有效额度
    let closed = world
        .call(
            "POST",
            "/control/v1/allocations/credit-1/revoke",
            Some(&teacher),
            None,
        )
        .await;
    assert_eq!(
        (closed.status, closed.json["result"].as_str()),
        (StatusCode::OK, Some("updated"))
    );
    assert_eq!(closed.json["data"]["revoked_at_ms"], T0);
    assert!(
        world
            .accounts
            .allocations_into(&idoris_compute_contracts::ids::AccountId::new("s1").unwrap())
            .unwrap()
            .is_empty()
    );
    let view = world
        .call(
            "GET",
            "/control/v1/accounts/s1/allocations",
            Some(&teacher),
            None,
        )
        .await;
    assert_eq!(view.json["data"]["inbound"][0]["revoked_at_ms"], T0);
    // 关掉的授信不能被原样重发「复活」
    let revived = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&teacher),
            Some(credit("credit-1", 2_000_000)),
        )
        .await;
    assert_eq!(
        (revived.status, revived.reason()),
        (StatusCode::CONFLICT, "allocation_revoked")
    );
}

#[tokio::test]
async fn malformed_requests_are_rejected_audited_for_known_callers_and_never_echo_a_secret() {
    let world = World::new();
    let root = world.root.secret.clone();
    let before = world.audit_rows().len();
    for (label, body) in [
        ("not json", "{not json".to_string()),
        (
            "unknown field",
            json!({"principal_id": "p", "org": "eduai", "extra": 1}).to_string(),
        ),
        ("missing field", json!({"principal_id": "p"}).to_string()),
        (
            "bad id",
            json!({"principal_id": "has space@x.com", "org": "eduai"}).to_string(),
        ),
        (
            "wrong type",
            json!({"principal_id": 5, "org": "eduai"}).to_string(),
        ),
        ("an array", "[]".to_string()),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/control/v1/principals")
            .header("authorization", format!("Bearer {root}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = world.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{label}");
        let raw = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(raw.contains("invalid_request"), "{label}: {raw}");
        assert!(!raw.contains(&root), "{label}");
    }
    let audit = world.audit_rows();
    assert_eq!(
        audit.len(),
        before + 6,
        "each rejected request is one audit row"
    );
    for row in &audit[before..] {
        assert_eq!(
            (
                row.0.as_str(),
                row.1.as_str(),
                row.2.as_str(),
                row.3.as_str(),
                row.4.as_str()
            ),
            (
                "credential",
                world.root.id.as_str(),
                "create_principal",
                "rejected",
                "invalid_request"
            )
        );
    }
    // 认不出的调用者发畸形请求：只是 401，没有审计
    let reply = world
        .call(
            "POST",
            "/control/v1/principals",
            Some(&format!("idk_{}", "cd".repeat(32))),
            Some(json!([1])),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(world.audit_rows().len(), before + 6);
    // 已撤销的调用者发畸形请求：审计的是它不可用的原因
    let revoker = world
        .call(
            "POST",
            &format!("/control/v1/credentials/{}/status", world.reader.id),
            Some(&root),
            Some(json!({"status": "revoked"})),
        )
        .await;
    assert_eq!(revoker.status, StatusCode::OK);
    let reply = world
        .call(
            "POST",
            "/control/v1/principals",
            Some(&world.reader.secret),
            Some(json!([1])),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::UNAUTHORIZED, "credential_revoked")
    );
    let last = world.audit_rows().pop().unwrap();
    assert_eq!(
        (last.1.as_str(), last.3.as_str(), last.4.as_str()),
        (world.reader.id.as_str(), "denied", "credential_revoked")
    );
    // 请求体过大（上限 64 KiB）
    let huge = json!({"principal_id": "p", "org": "eduai", "pad": "x".repeat(70_000)});
    let reply = world
        .call("POST", "/control/v1/principals", Some(&root), Some(huge))
        .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
}

// ================================================================================================
// 授权：HTTP 层的矩阵（存储层有完整的 10 个操作 x 5 种情形的矩阵，这里验证每条路由接对了）
// ================================================================================================

/// 一条写路由的「在 teacher 子树里」和「在子树外」两个请求。
struct RouteCase {
    name: &'static str,
    verb: &'static str,
    inside: (String, Option<Value>),
    outside: (String, Option<Value>),
}

async fn route_cases(world: &World) -> Vec<RouteCase> {
    world.people().await;
    // 撤销主体要对「登记它的组织」和它每个成员关系的账户都有权限（Codex r1 #1）：teacher 子树里「范围内放行」的
    // 那个主体，必须是在 teacher 名下登记的（kid1 是 eduai 登记的，teacher 子树的凭证撤销不了它）
    world
        .ok(
            "/control/v1/principals",
            &world.root.secret,
            json!({"principal_id": "p-teach", "org": "teacher"}),
        )
        .await;
    let member_in = world.member_credential("m-kid1").await; // s1，teacher 子树里
    let member_out = world.member_credential("m-kid2").await; // s2，teacher 子树外
    let account = |id: &str, parent: &str| json!({"account_id": id, "kind": "member", "parent": parent, "unit": "THB"});
    let grant = |id: &str, parent: &str, child: &str| {
        json!({"allocation_id": id, "parent": parent, "child": child,
               "amount": {"unit": "THB", "micros": 1_000_000}, "purpose": "prepaid",
               "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS})
    };
    vec![
        RouteCase {
            name: "create_principal",
            verb: "POST",
            inside: (
                "/control/v1/principals".into(),
                Some(json!({"principal_id": "p-in", "org": "teacher"})),
            ),
            outside: (
                "/control/v1/principals".into(),
                Some(json!({"principal_id": "p-out", "org": "eduai"})),
            ),
        },
        RouteCase {
            name: "revoke_principal",
            verb: "POST",
            inside: (
                "/control/v1/principals/p-teach/revoke".into(),
                Some(json!({"org": "teacher"})),
            ),
            outside: (
                "/control/v1/principals/kid2/revoke".into(),
                Some(json!({"org": "eduai"})),
            ),
        },
        RouteCase {
            name: "create_account",
            verb: "POST",
            inside: (
                "/control/v1/accounts".into(),
                Some(account("acct-in", "teacher")),
            ),
            outside: (
                "/control/v1/accounts".into(),
                Some(account("acct-out", "eduai")),
            ),
        },
        RouteCase {
            name: "set_account_status",
            verb: "POST",
            inside: (
                "/control/v1/accounts/s1/status".into(),
                Some(json!({"status": "frozen"})),
            ),
            outside: (
                "/control/v1/accounts/s2/status".into(),
                Some(json!({"status": "frozen"})),
            ),
        },
        RouteCase {
            name: "create_membership",
            verb: "POST",
            inside: (
                "/control/v1/memberships".into(),
                Some(
                    json!({"membership_id": "m-in", "principal_id": "kid1", "org": "eduai", "role": "member", "account": "m-acct-in"}),
                ),
            ),
            outside: (
                "/control/v1/memberships".into(),
                Some(
                    json!({"membership_id": "m-out", "principal_id": "kid1", "org": "eduai", "role": "member", "account": "m-acct-out"}),
                ),
            ),
        },
        RouteCase {
            name: "revoke_membership",
            verb: "POST",
            inside: ("/control/v1/memberships/m-kid1/revoke".into(), None),
            outside: ("/control/v1/memberships/m-kid2/revoke".into(), None),
        },
        RouteCase {
            name: "issue_credential",
            verb: "POST",
            inside: (
                "/control/v1/credentials".into(),
                Some(
                    json!({"binding": {"kind": "member", "membership": "m-kid1"}, "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
                ),
            ),
            outside: (
                "/control/v1/credentials".into(),
                Some(
                    json!({"binding": {"kind": "member", "membership": "m-kid2"}, "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
                ),
            ),
        },
        RouteCase {
            name: "freeze_credential",
            verb: "POST",
            inside: (
                format!("/control/v1/credentials/{}/status", member_in.id),
                Some(json!({"status": "frozen"})),
            ),
            outside: (
                format!("/control/v1/credentials/{}/status", member_out.id),
                Some(json!({"status": "frozen"})),
            ),
        },
        RouteCase {
            name: "revoke_credential",
            verb: "POST",
            inside: (
                format!("/control/v1/credentials/{}/status", member_in.id),
                Some(json!({"status": "revoked"})),
            ),
            outside: (
                format!("/control/v1/credentials/{}/status", member_out.id),
                Some(json!({"status": "revoked"})),
            ),
        },
        RouteCase {
            name: "grant_allocation",
            verb: "POST",
            inside: (
                "/control/v1/allocations".into(),
                Some(grant("g-in", "teacher", "s1")),
            ),
            outside: (
                "/control/v1/allocations".into(),
                Some(grant("g-out", "eduai", "s2")),
            ),
        },
        RouteCase {
            name: "revoke_allocation",
            verb: "POST",
            inside: ("/control/v1/allocations/g-pre/revoke".into(), None),
            outside: ("/control/v1/allocations/a-teacher/revoke".into(), None),
        },
        RouteCase {
            name: "create_pairing_code",
            verb: "POST",
            inside: (
                "/control/v1/pairing/codes".into(),
                Some(
                    json!({"membership": "m-kid1", "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
                ),
            ),
            outside: (
                "/control/v1/pairing/codes".into(),
                Some(
                    json!({"membership": "m-kid2", "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
                ),
            ),
        },
        RouteCase {
            name: "read",
            verb: "GET",
            inside: ("/control/v1/accounts/s1".into(), None),
            outside: ("/control/v1/accounts/s2".into(), None),
        },
    ]
}

#[tokio::test]
async fn http_authorization_matrix() {
    // 先铺好前置资源，再对每条写路由：teacher 子树权限的调用者「范围内放行」「子树外 403 out_of_scope」「没有这个操作的权限 403」，
    // 被冻结 / 撤销的调用者 401；都有对应的审计行。
    for case_index in 0..13 {
        let world = World::new();
        let root = world.root.secret.clone();
        for (id, kind, parent) in [
            ("m-acct-in", "member", "teacher"),
            ("m-acct-out", "member", "eduai"),
            ("s-pre", "member", "teacher"),
        ] {
            world
                .ok(
                    "/control/v1/accounts",
                    &root,
                    json!({"account_id": id, "kind": kind, "parent": parent, "unit": "THB"}),
                )
                .await;
        }
        world
            .ok(
                "/control/v1/allocations",
                &root,
                json!({"allocation_id": "g-pre", "parent": "teacher", "child": "s-pre",
                       "amount": {"unit": "THB", "micros": 1_000_000}, "purpose": "prepaid",
                       "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS}),
            )
            .await;
        let cases = route_cases(&world).await;
        let case = &cases[case_index];
        let teacher = world.teacher.secret.clone();
        let audited_before = world.audit_rows().len();

        // 子树外：403 out_of_scope（读也一样，只是不写审计）
        let outside = world
            .call(
                case.verb,
                &case.outside.0,
                Some(&teacher),
                case.outside.1.clone(),
            )
            .await;
        assert_eq!(
            outside.status,
            StatusCode::FORBIDDEN,
            "{}: {}",
            case.name,
            outside.raw
        );
        assert_eq!(outside.reason(), "out_of_scope", "{}", case.name);
        let rows = world.audit_rows();
        if case.verb == "POST" {
            assert_eq!(
                rows.len(),
                audited_before + 1,
                "{}: a denied write is audited",
                case.name
            );
            let last = rows.last().unwrap();
            assert_eq!(
                (
                    last.0.as_str(),
                    last.1.as_str(),
                    last.3.as_str(),
                    last.4.as_str()
                ),
                (
                    "credential",
                    world.teacher.id.as_str(),
                    "denied",
                    "out_of_scope"
                ),
                "{}",
                case.name
            );
        } else {
            assert_eq!(
                rows.len(),
                audited_before,
                "{}: reads are not audited",
                case.name
            );
        }

        // 没有这个操作的权限：一张只有 read 的凭证（reader）去做写操作；reader 自己去读则要换一张只有写权限的
        let wrong = if case.verb == "GET" {
            &world.other
        } else {
            &world.reader
        };
        let denied = world
            .call(
                case.verb,
                &case.inside.0,
                Some(&wrong.secret),
                case.inside.1.clone(),
            )
            .await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{}: {}",
            case.name,
            denied.raw
        );
        assert!(
            matches!(denied.reason(), "op_not_granted" | "out_of_scope"),
            "{}: {}",
            case.name,
            denied.reason()
        );
        if case.verb == "POST" && wrong.id == world.reader.id {
            assert_eq!(denied.reason(), "op_not_granted", "{}", case.name);
        }

        // 范围内放行
        let inside = world
            .call(
                case.verb,
                &case.inside.0,
                Some(&teacher),
                case.inside.1.clone(),
            )
            .await;
        assert!(
            inside.status.is_success(),
            "{}: in scope must be allowed: {} {}",
            case.name,
            inside.status,
            inside.raw
        );
        if case.verb == "POST" {
            let last = world.audit_rows().pop().unwrap();
            assert_eq!(
                (last.1.as_str(), last.3.as_str(), last.4.as_str()),
                (world.teacher.id.as_str(), "success", "ok"),
                "{}",
                case.name
            );
        }

        // 被冻结的调用者：401 credential_frozen；撤销之后：401 credential_revoked；都有审计
        let frozen = world
            .call(
                "POST",
                &format!("/control/v1/credentials/{}/status", world.teacher.id),
                Some(&root),
                Some(json!({"status": "frozen"})),
            )
            .await;
        assert_eq!(frozen.status, StatusCode::OK, "{}", case.name);
        let reply = world
            .call(
                case.verb,
                &case.inside.0,
                Some(&teacher),
                case.inside.1.clone(),
            )
            .await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::UNAUTHORIZED, "credential_frozen"),
            "{}",
            case.name
        );
        let revoked = world
            .call(
                "POST",
                &format!("/control/v1/credentials/{}/status", world.teacher.id),
                Some(&root),
                Some(json!({"status": "revoked"})),
            )
            .await;
        assert_eq!(revoked.status, StatusCode::OK);
        let reply = world
            .call(
                case.verb,
                &case.inside.0,
                Some(&teacher),
                case.inside.1.clone(),
            )
            .await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::UNAUTHORIZED, "credential_revoked"),
            "{}",
            case.name
        );
        if case.verb == "POST" {
            let last = world.audit_rows().pop().unwrap();
            assert_eq!(
                (last.1.as_str(), last.3.as_str(), last.4.as_str()),
                (world.teacher.id.as_str(), "denied", "credential_revoked"),
                "{}",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn a_credential_with_a_narrow_grant_cannot_escalate_over_http_and_every_attempt_is_audited() {
    let world = World::new();
    world.people().await;
    let root = world.root.secret.clone();
    // 「中间人」：能在 eduai 签发凭证；自己只能给 teacher 子树发额度（单笔 5 THB）、只读 teacher 子树
    let middle = world
        .ok(
            "/control/v1/credentials",
            &root,
            json!({
                "binding": {"kind": "service", "account": "eduai"},
                "admin_scopes": [
                    {"op": "issue_credential", "subtree": "eduai"},
                    {"op": "grant_allocation", "subtree": "teacher", "max_amount": {"unit": "THB", "micros": 5_000_000}},
                    {"op": "read", "subtree": "teacher"}
                ],
                "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]
            }),
        )
        .await;
    let middle = Cred {
        id: middle["credential"]["id"].as_str().unwrap().to_string(),
        secret: middle["secret"].as_str().unwrap().to_string(),
    };
    let rows_before = world.audit_rows().len();
    let attempt = |scopes: Value| {
        json!({"binding": {"kind": "service", "account": "eduai"}, "admin_scopes": scopes,
               "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]})
    };
    // 1. 签一张权限更大的凭证：更大的子树 / 更高的上限 / 没有上限 / 没有的操作
    let mut expected_denials = 0;
    for (label, scopes) in [
        (
            "wider subtree",
            json!([{"op": "grant_allocation", "subtree": "eduai", "max_amount": {"unit": "THB", "micros": 5_000_000}}]),
        ),
        (
            "higher cap",
            json!([{"op": "grant_allocation", "subtree": "teacher", "max_amount": {"unit": "THB", "micros": 5_000_001}}]),
        ),
        (
            "no cap",
            json!([{"op": "grant_allocation", "subtree": "teacher"}]),
        ),
        (
            "an operation it does not hold",
            json!([{"op": "revoke_credential", "subtree": "teacher"}]),
        ),
        (
            "read everything",
            json!([{"op": "read", "subtree": "eduai"}]),
        ),
    ] {
        let reply = world
            .call(
                "POST",
                "/control/v1/credentials",
                Some(&middle.secret),
                Some(attempt(scopes)),
            )
            .await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::FORBIDDEN, "scope_exceeds_grantor"),
            "{label}: {}",
            reply.raw
        );
        assert!(
            reply.json["data"].is_null() && !reply.raw.contains("idk_"),
            "no secret is ever returned for a refused issuance"
        );
        expected_denials += 1;
    }
    // 2. 给自己「加」权限：以自己的绑定账户签发一张超集凭证，同样被拒；已有凭证的权限行也没有任何办法被改
    let superset = attempt(json!([
        {"op": "issue_credential", "subtree": "eduai"},
        {"op": "grant_allocation", "subtree": "teacher", "max_amount": {"unit": "THB", "micros": 5_000_000}},
        {"op": "read", "subtree": "teacher"},
        {"op": "set_account_status", "subtree": "teacher"}
    ]));
    let reply = world
        .call(
            "POST",
            "/control/v1/credentials",
            Some(&middle.secret),
            Some(superset),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "scope_exceeds_grantor")
    );
    expected_denials += 1;
    // 3. 给子树外的账户发额度
    for (id, parent, child) in [
        ("g-out-1", "eduai", "s2"),
        ("g-out-2", "pool", "eduai"),
        ("g-out-3", "school-b", "b1"),
    ] {
        let reply = world
            .call(
                "POST",
                "/control/v1/allocations",
                Some(&middle.secret),
                Some(
                    json!({"allocation_id": id, "parent": parent, "child": child,
                            "amount": {"unit": "THB", "micros": 1}, "purpose": "prepaid",
                            "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS}),
                ),
            )
            .await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::FORBIDDEN, "out_of_scope"),
            "{id}"
        );
        expected_denials += 1;
    }
    // 4. 超过自己的单笔上限
    let reply = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&middle.secret),
            Some(
                json!({"allocation_id": "g-big", "parent": "teacher", "child": "s1",
                        "amount": {"unit": "THB", "micros": 5_000_001}, "purpose": "prepaid",
                        "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS}),
            ),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "amount_exceeds_authority")
    );
    expected_denials += 1;
    // 5. 签发到别的组织
    let reply = world
        .call(
            "POST",
            "/control/v1/credentials",
            Some(&middle.secret),
            Some(json!({"binding": {"kind": "service", "account": "school-b"}, "allowed_privacy": ["local_only"]})),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "out_of_scope")
    );
    expected_denials += 1;
    // 全部有审计，actor 是「中间人」的凭证 id，结果都是 denied
    let rows = world.audit_rows();
    let new_rows = &rows[rows_before..];
    assert_eq!(new_rows.len(), expected_denials);
    for row in new_rows {
        assert_eq!(
            (row.0.as_str(), row.1.as_str(), row.3.as_str()),
            ("credential", middle.id.as_str(), "denied"),
            "{row:?}"
        );
    }
    // 控制面里没有「改已有凭证权限」的路由：任何别的方法 / 路径都是 404
    for (verb, uri) in [
        ("PATCH", format!("/control/v1/credentials/{}", middle.id)),
        ("PUT", format!("/control/v1/credentials/{}", middle.id)),
        (
            "POST",
            format!("/control/v1/credentials/{}/scopes", middle.id),
        ),
    ] {
        let reply = world
            .call(
                verb,
                &uri,
                Some(&middle.secret),
                Some(json!({"admin_scopes": []})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{verb} {uri}");
    }
    // 并且这一切没有留下任何新凭证
    let conn = world.db();
    let credentials: i64 = conn
        .query_row("SELECT count(*) FROM credentials", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        credentials, 5,
        "four from bootstrap plus the one middle credential"
    );
    // 对照：没有越权的签发成功（说明上面的拒绝来自授权，不是路由坏了）
    let ok = world
        .call(
            "POST",
            "/control/v1/credentials",
            Some(&middle.secret),
            Some(attempt(json!([{"op": "read", "subtree": "teacher"}]))),
        )
        .await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.raw);
    assert!(
        ok.json["data"]["secret"]
            .as_str()
            .unwrap()
            .starts_with("idk_")
    );
}

#[tokio::test]
async fn data_plane_ceilings_and_expiry_cannot_exceed_the_issuer_over_http() {
    let world = World::new();
    world.people().await;
    let issuer = world
        .ok(
            "/control/v1/credentials",
            &world.root.secret,
            json!({
                "binding": {"kind": "service", "account": "eduai"},
                "admin_scopes": [{"op": "issue_credential", "subtree": "eduai"}],
                "expires_at_ms": T0 + 10 * DAY_MS,
                "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]
            }),
        )
        .await;
    let secret = issuer["secret"].as_str().unwrap().to_string();
    let base = json!({"binding": {"kind": "member", "membership": "m-kid1"},
                      "allowed_privacy": ["local_only"], "allowed_roles": ["fast"],
                      "expires_at_ms": T0 + 5 * DAY_MS});
    let with = |key: &str, value: Value| {
        let mut body = base.clone();
        body[key] = value;
        body
    };
    for (label, body, expected) in [
        (
            "privacy",
            with("allowed_privacy", json!(["local_only", "any"])),
            "ceiling_exceeds_grantor",
        ),
        (
            "roles",
            with("allowed_roles", json!(["fast", "deep"])),
            "ceiling_exceeds_grantor",
        ),
        (
            "tenant",
            with("tenant_id", json!("tenant-b")),
            "ceiling_exceeds_grantor",
        ),
        (
            "expiry after the issuer",
            with("expires_at_ms", json!(T0 + 10 * DAY_MS + 1)),
            "expiry_exceeds_grantor",
        ),
        (
            "no expiry",
            with("expires_at_ms", Value::Null),
            "expiry_exceeds_grantor",
        ),
    ] {
        let reply = world
            .call("POST", "/control/v1/credentials", Some(&secret), Some(body))
            .await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::FORBIDDEN, expected),
            "{label}: {}",
            reply.raw
        );
    }
    // 同一份请求，上限在授权者之内：成功；数据面上限和租户绑定是继承的
    let reply = world
        .call("POST", "/control/v1/credentials", Some(&secret), Some(base))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.raw);
    let token = reply.json["data"]["secret"].as_str().unwrap().to_string();
    let who = world
        .call("GET", "/control/v1/whoami", Some(&token), None)
        .await;
    // 成员凭证没有管理权限，但 whoami 只要求凭证此刻可用
    assert_eq!(who.status, StatusCode::OK);
    assert_eq!(who.json["data"]["data_plane"]["tenant_id"], "tenant-edu");
    assert_eq!(
        who.json["data"]["data_plane"]["allowed_roles"],
        json!(["fast"])
    );
    assert_eq!(who.json["data"]["credential"]["admin_scopes"], Value::Null);
    // 未知角色、重复的隐私等级：请求本身不合法
    let reply = world
        .call("POST", "/control/v1/credentials", Some(&secret), Some(json!({"binding": {"kind": "member", "membership": "m-kid1"}, "allowed_privacy": ["local_only"], "allowed_roles": ["root"], "expires_at_ms": T0 + DAY_MS})))
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    let reply = world
        .call("POST", "/control/v1/credentials", Some(&secret), Some(json!({"binding": {"kind": "member", "membership": "m-kid1"}, "allowed_privacy": ["local_only", "local_only"], "expires_at_ms": T0 + DAY_MS})))
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
}

// ================================================================================================
// 撤销即时生效
// ================================================================================================

#[tokio::test]
async fn revocation_takes_effect_on_the_very_next_data_plane_request() {
    // 每一种触发方式各用一张新的凭证，避免互相影响
    enum Trigger {
        RevokeCredential,
        FreezeCredential,
        RevokeMembership,
        RevokePrincipal,
        FreezeMemberAccount,
        FreezeAncestorAccount,
        CloseMemberAccount,
    }
    for (label, trigger) in [
        ("revoke the credential", Trigger::RevokeCredential),
        ("freeze the credential", Trigger::FreezeCredential),
        ("revoke the membership", Trigger::RevokeMembership),
        ("revoke the principal", Trigger::RevokePrincipal),
        ("freeze the member account", Trigger::FreezeMemberAccount),
        ("freeze an ancestor account", Trigger::FreezeAncestorAccount),
        ("close the member account", Trigger::CloseMemberAccount),
    ] {
        let world = World::new();
        world.people().await;
        let device = world.member_credential("m-kid1").await;
        let sibling = world.member_credential("m-kid2").await;
        let root = world.root.secret.clone();
        // 之前：通
        assert_eq!(
            world.data_plane(&device.secret).await,
            StatusCode::OK,
            "{label}"
        );
        assert_eq!(
            world.data_plane(&sibling.secret).await,
            StatusCode::OK,
            "{label}"
        );
        let reply = match trigger {
            Trigger::RevokeCredential => {
                world
                    .call(
                        "POST",
                        &format!("/control/v1/credentials/{}/status", device.id),
                        Some(&root),
                        Some(json!({"status": "revoked"})),
                    )
                    .await
            }
            Trigger::FreezeCredential => {
                world
                    .call(
                        "POST",
                        &format!("/control/v1/credentials/{}/status", device.id),
                        Some(&root),
                        Some(json!({"status": "frozen"})),
                    )
                    .await
            }
            Trigger::RevokeMembership => {
                world
                    .call(
                        "POST",
                        "/control/v1/memberships/m-kid1/revoke",
                        Some(&root),
                        None,
                    )
                    .await
            }
            Trigger::RevokePrincipal => {
                world
                    .call(
                        "POST",
                        "/control/v1/principals/kid1/revoke",
                        Some(&root),
                        Some(json!({"org": "eduai"})),
                    )
                    .await
            }
            Trigger::FreezeMemberAccount => {
                world
                    .call(
                        "POST",
                        "/control/v1/accounts/s1/status",
                        Some(&root),
                        Some(json!({"status": "frozen"})),
                    )
                    .await
            }
            Trigger::FreezeAncestorAccount => {
                world
                    .call(
                        "POST",
                        "/control/v1/accounts/teacher/status",
                        Some(&root),
                        Some(json!({"status": "frozen"})),
                    )
                    .await
            }
            Trigger::CloseMemberAccount => {
                world
                    .call(
                        "POST",
                        "/control/v1/accounts/s1/status",
                        Some(&root),
                        Some(json!({"status": "closed"})),
                    )
                    .await
            }
        };
        assert!(reply.status.is_success(), "{label}: {}", reply.raw);
        // 之后（紧接着的下一次请求，没有任何等待、没有任何重新加载）：被拒；别的凭证不受影响
        assert_eq!(
            world.data_plane(&device.secret).await,
            StatusCode::UNAUTHORIZED,
            "{label}"
        );
        assert_eq!(
            world.data_plane(&sibling.secret).await,
            StatusCode::OK,
            "{label}: other credentials are untouched"
        );
        // 可逆的触发方式（冻结）解除之后立即恢复；撤销是终态
        let undo = match trigger {
            Trigger::FreezeCredential => Some(
                world
                    .call(
                        "POST",
                        &format!("/control/v1/credentials/{}/status", device.id),
                        Some(&root),
                        Some(json!({"status": "active"})),
                    )
                    .await,
            ),
            Trigger::FreezeMemberAccount => Some(
                world
                    .call(
                        "POST",
                        "/control/v1/accounts/s1/status",
                        Some(&root),
                        Some(json!({"status": "active"})),
                    )
                    .await,
            ),
            Trigger::FreezeAncestorAccount => Some(
                world
                    .call(
                        "POST",
                        "/control/v1/accounts/teacher/status",
                        Some(&root),
                        Some(json!({"status": "active"})),
                    )
                    .await,
            ),
            _ => None,
        };
        match undo {
            Some(reply) => {
                assert!(reply.status.is_success(), "{label}: {}", reply.raw);
                assert_eq!(
                    world.data_plane(&device.secret).await,
                    StatusCode::OK,
                    "{label}: unfrozen"
                );
            }
            None => assert_eq!(
                world.data_plane(&device.secret).await,
                StatusCode::UNAUTHORIZED,
                "{label}: stays refused"
            ),
        }
    }

    // 组织被冻结：它的所有凭证（Service、Node、成员）立即被拒，包括控制面自己
    let world = World::new();
    world.people().await;
    let member = world.member_credential("m-kid1").await;
    let node = world
        .ok("/control/v1/credentials", &world.root.secret, json!({"binding": {"kind": "node", "account": "eduai"}, "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}))
        .await;
    let node = Cred {
        id: node["credential"]["id"].as_str().unwrap().to_string(),
        secret: node["secret"].as_str().unwrap().to_string(),
    };
    for token in [
        &world.root.secret,
        &world.reader.secret,
        &member.secret,
        &node.secret,
    ] {
        assert_eq!(world.data_plane(token).await, StatusCode::OK);
    }
    // 运维（拥有库文件的人）冻结组织：控制面凭证没有办法冻结自己的组织（would_disable_caller）
    let reply = world
        .call(
            "POST",
            "/control/v1/accounts/eduai/status",
            Some(&world.root.secret),
            Some(json!({"status": "frozen"})),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "would_disable_caller")
    );
    world
        .accounts
        .set_account_status(
            &idoris_compute_contracts::ids::AccountId::new("eduai").unwrap(),
            idoris_compute_contracts::account::AccountStatus::Frozen,
        )
        .unwrap();
    for token in [
        &world.root.secret,
        &world.reader.secret,
        &member.secret,
        &node.secret,
    ] {
        assert_eq!(world.data_plane(token).await, StatusCode::UNAUTHORIZED);
    }
    let reply = world
        .call("GET", "/control/v1/whoami", Some(&world.root.secret), None)
        .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "whoami only needs a usable credential"
    );
    let reply = world
        .call(
            "GET",
            "/control/v1/accounts/eduai",
            Some(&world.root.secret),
            None,
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "account_inactive")
    );
    // 本地数据密钥（没有凭证行）不归账户存储管：仍然通
    let local = MintedVirtualKey::mint();
    world
        .keys
        .lock()
        .unwrap()
        .insert_active(
            &local.key_id,
            local.hash,
            &VirtualKeyScope {
                owner: "local".into(),
                tenant_id: Some("tenant-edu".into()),
                allowed_privacy: vec![PrivacyClass::LocalOnly],
                allowed_roles: vec!["fast".into()],
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(
        world.data_plane(local.secret.expose_secret()).await,
        StatusCode::OK
    );
}

fn resident_card(endpoint: &str) -> ComponentCard {
    ComponentCard {
        provider: ProviderDescriptor {
            id: "control-cache".into(),
            family: Family::Local,
            tier: Tier::Local,
            capabilities: vec![Capability::Chat],
            privacy_class: PrivacyClass::LocalOnly,
            cost: Cost { input_per_m: 0.0, output_per_m: 0.0 },
            locality: Locality::Loopback,
            extensions: None,
        },
        form: Form::HttpService,
        endpoint: endpoint.to_string(),
        version_pin: "test".into(),
        privacy_class: PrivacyClass::LocalOnly,
        allowed_egress: vec![Egress::Loopback],
        fallback_policy: FallbackPolicy::FailClosed,
        fail_closed: true,
        load_policy: Some(LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        }),
        extensions: Some(serde_json::from_value(json!({"_degradation":"No aliases when removed", "idoris_role_models":{"fast":"cache-chat", "daily":"cache-chat"}})).unwrap()),
    }
}

#[tokio::test]
async fn a_revoked_credential_cannot_replay_a_warmed_proxy_response_cache() {
    // 代理响应缓存的键是（租户、URL、provider、X-iDoris-Request-Id），**不含 key id**（S1 的发现）。
    // 所以要证明的是：被撤销之后，同一个请求（同租户、同 request id、同请求体）不会从缓存里漏过去。
    // 缓存查找发生在认证之后，被撤销的凭证在认证这一步就被拒，根本走不到缓存。
    for kind in [
        "revoke the credential",
        "revoke the membership",
        "freeze the member account",
    ] {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let world = World::new();
        world.people().await;
        // 数据面带真实的本地模型卡：同一个应用上既有数据面又有控制面
        let mut state = world.state.clone();
        state.cards = vec![resident_card(&server.uri())];
        state.event_log = Some(Arc::new(
            idoris_tenancy::event_log::EventLogStore::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )
            .unwrap(),
        ));
        state.record_store = Some(Arc::new(Mutex::new(
            idoris_tenancy::store::TenantStore::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )
            .unwrap(),
        )));
        let app = build_app(state);
        let device = world.member_credential("m-kid1").await;
        let chat = |token: &str| {
            let request = Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .header("x-idoris-tenant", "tenant-edu")
                .header("x-idoris-request-id", "warmed-cache")
                .body(Body::from(json!({"model": "idoris/fast", "messages": [{"role": "user", "content": "hello"}]}).to_string()))
                .unwrap();
            let app = app.clone();
            async move {
                let response = app.oneshot(request).await.unwrap();
                let status = response.status();
                let cached = response.headers().contains_key("x-idoris-cached");
                (status, cached)
            }
        };
        let (first, first_cached) = chat(&device.secret).await;
        assert_eq!(
            (first, first_cached),
            (StatusCode::OK, false),
            "{kind}: the upstream is called once"
        );
        let (warm, warm_cached) = chat(&device.secret).await;
        assert_eq!(
            (warm, warm_cached),
            (StatusCode::OK, true),
            "{kind}: the second identical request is a cache hit"
        );
        // 撤销（经控制面）
        let root = world.root.secret.clone();
        let reply = match kind {
            "revoke the credential" => {
                world
                    .call(
                        "POST",
                        &format!("/control/v1/credentials/{}/status", device.id),
                        Some(&root),
                        Some(json!({"status": "revoked"})),
                    )
                    .await
            }
            "revoke the membership" => {
                world
                    .call(
                        "POST",
                        "/control/v1/memberships/m-kid1/revoke",
                        Some(&root),
                        None,
                    )
                    .await
            }
            _ => {
                world
                    .call(
                        "POST",
                        "/control/v1/accounts/s1/status",
                        Some(&root),
                        Some(json!({"status": "frozen"})),
                    )
                    .await
            }
        };
        assert!(reply.status.is_success(), "{kind}: {}", reply.raw);
        // 紧接着的下一次：被拒，不是缓存命中
        let (denied, denied_cached) = chat(&device.secret).await;
        assert_eq!(
            (denied, denied_cached),
            (StatusCode::UNAUTHORIZED, false),
            "{kind}: a revoked credential never reaches the cache"
        );
        server.verify().await;
    }
}

// ================================================================================================
// 只读
// ================================================================================================

#[tokio::test]
async fn reads_are_scoped_and_contain_no_secrets_or_hashes() {
    let world = World::new();
    world.people().await;
    let device = world.member_credential("m-kid1").await;
    let reader = world.reader.secret.clone();
    let reply = world
        .call("GET", "/control/v1/accounts/eduai", Some(&reader), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json["data"]["children"], json!(["s2", "teacher"]));
    assert_eq!(reply.headers["cache-control"], "no-store");
    let tree = world
        .call(
            "GET",
            "/control/v1/accounts/eduai/subtree",
            Some(&reader),
            None,
        )
        .await;
    assert_eq!(tree.json["data"]["accounts"].as_array().unwrap().len(), 4);
    let creds = world
        .call(
            "GET",
            "/control/v1/accounts/s1/credentials",
            Some(&reader),
            None,
        )
        .await;
    assert_eq!(creds.status, StatusCode::OK);
    let allocations = world
        .call(
            "GET",
            "/control/v1/accounts/teacher/allocations",
            Some(&reader),
            None,
        )
        .await;
    assert_eq!(
        allocations.json["data"]["inbound"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let memberships = world
        .call(
            "GET",
            "/control/v1/accounts/eduai/memberships",
            Some(&reader),
            None,
        )
        .await;
    assert_eq!(
        memberships.json["data"]["memberships"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let principal = world
        .call("GET", "/control/v1/principals/kid1", Some(&reader), None)
        .await;
    assert_eq!(principal.json["data"]["principal"]["status"], "active");
    // 范围外
    for uri in [
        "/control/v1/accounts/school-b",
        "/control/v1/accounts/b1/subtree",
        "/control/v1/accounts/platform/allocations",
        "/control/v1/accounts/pool/credentials",
        "/control/v1/accounts/ghost/memberships",
        "/control/v1/principals/ghost",
        "/control/v1/audit?account=school-b",
    ] {
        let reply = world.call("GET", uri, Some(&reader), None).await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::FORBIDDEN, "out_of_scope"),
            "{uri}"
        );
    }
    // 所有读取的响应里：没有任何密钥、验证器哈希
    let mut everything = String::new();
    for uri in [
        "/control/v1/whoami",
        "/control/v1/accounts/eduai",
        "/control/v1/accounts/eduai/subtree",
        "/control/v1/accounts/eduai/credentials",
        "/control/v1/accounts/s1/credentials",
        "/control/v1/accounts/eduai/allocations",
        "/control/v1/accounts/eduai/memberships",
        "/control/v1/principals/kid1",
        "/control/v1/audit?account=eduai&limit=500",
    ] {
        let reply = world.call("GET", uri, Some(&world.root.secret), None).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.raw);
        everything.push_str(&reply.raw);
    }
    for secret in [
        &world.root.secret,
        &world.teacher.secret,
        &world.reader.secret,
        &world.other.secret,
        &device.secret,
    ] {
        assert!(
            !everything.contains(secret.as_str()),
            "a read response contains a secret"
        );
    }
    assert!(
        !everything.contains("key_hash")
            && !everything.contains("code_hash")
            && !everything.contains("idk_")
    );
    // 查询参数不合法
    for uri in [
        "/control/v1/audit",
        "/control/v1/audit?account=eduai&extra=1",
        "/control/v1/audit?account=has%20space",
        "/control/v1/audit?account=eduai&limit=abc",
    ] {
        let reply = world.call("GET", uri, Some(&reader), None).await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::BAD_REQUEST, "invalid_request"),
            "{uri}"
        );
    }
    // whoami
    let who = world
        .call("GET", "/control/v1/whoami", Some(&reader), None)
        .await;
    assert_eq!(who.json["data"]["credential"]["id"], world.reader.id);
    assert_eq!(who.json["data"]["data_plane"]["allowed_roles"], json!([]));
}

#[tokio::test]
async fn the_audit_is_queryable_by_subtree_and_shows_who_what_target_result_reason_and_time() {
    let world = World::new();
    world.people().await;
    let denied = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(&world.teacher.secret),
            Some(json!({"account_id": "x", "kind": "member", "parent": "eduai", "unit": "THB"})),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    world.clock.advance(5_000);
    let reply = world
        .call(
            "GET",
            "/control/v1/audit?account=eduai&limit=500",
            Some(&world.root.secret),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let rows = reply.json["data"]["rows"].as_array().unwrap();
    let denied_row = rows
        .iter()
        .find(|row| row["op"] == "create_account" && row["outcome"] == "denied")
        .expect("the denied attempt is in the audit");
    assert_eq!(
        denied_row["actor"],
        json!({"kind": "credential", "id": world.teacher.id})
    );
    assert_eq!(denied_row["target"], json!({"kind": "account", "id": "x"}));
    assert_eq!(denied_row["scope_account"], "eduai");
    assert_eq!(denied_row["reason_code"], "out_of_scope");
    assert_eq!(denied_row["at_ms"], T0);
    assert!(denied_row["request_id"].is_string());
    assert!(rows.iter().any(|row| row["op"] == "create_membership"
        && row["outcome"] == "success"
        && row["reason_code"] == "ok"));
    // 别的组织看不到 eduai 的审计，也看不到 eduai 以外的（school-b 子树里只有它自己的）
    let theirs = world
        .call(
            "GET",
            "/control/v1/audit?account=school-b&limit=500",
            Some(&world.other.secret),
            None,
        )
        .await;
    assert_eq!(theirs.status, StatusCode::OK);
    assert!(
        theirs.json["data"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["scope_account"] != "eduai")
    );
    let blocked = world
        .call(
            "GET",
            "/control/v1/audit?account=eduai",
            Some(&world.other.secret),
            None,
        )
        .await;
    assert_eq!(
        (blocked.status, blocked.reason()),
        (StatusCode::FORBIDDEN, "out_of_scope")
    );
}

// ================================================================================================
// 配对
// ================================================================================================

async fn pairing_code(world: &World, membership: &str) -> (String, String) {
    let data = world
        .ok(
            "/control/v1/pairing/codes",
            &world.root.secret,
            json!({"membership": membership, "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
        )
        .await;
    (
        data["code"].as_str().unwrap().to_string(),
        data["pairing"]["pairing_id"].as_str().unwrap().to_string(),
    )
}

async fn redeem(world: &World, code: &str, label: &str) -> Reply {
    world
        .call(
            "POST",
            "/control/v1/pairing/redeem",
            None,
            Some(json!({"code": code, "device_label": label})),
        )
        .await
}

/// 从指定的 TCP 对端地址兑换（真实的监听器会给每个请求挂上它）。
async fn redeem_from(world: &World, peer: &str, code: &str, label: &str) -> Reply {
    send_from(
        &world.app,
        Some(peer),
        "POST",
        "/control/v1/pairing/redeem",
        None,
        Some(json!({"code": code, "device_label": label})),
        &[],
    )
    .await
}

/// 格式合法、查无此码的第 `n` 个猜测（12 位数字，每个不一样）。
fn wrong_code(n: u32) -> String {
    let digits = format!("{n:012}");
    format!("{}-{}-{}", &digits[0..4], &digits[4..8], &digits[8..12])
}

#[tokio::test]
async fn pairing_over_http_one_code_one_device_each_device_independently_revocable() {
    let world = World::new();
    world.people().await;
    let (code, pairing_id) = pairing_code(&world, "m-kid1").await;
    assert_eq!(code.len(), 14);
    // 创建响应里有一次性明文码；库里只有哈希；记录视图不带码
    let conn = world.db();
    let stored: String = conn
        .query_row("SELECT hex(code_hash) FROM pairing_codes", [], |r| r.get(0))
        .unwrap();
    assert!(!stored.contains(&code.replace('-', "")));
    // 兑换：不需要凭证；用户输入大小写和分组都无所谓
    let sloppy = code.to_ascii_lowercase().replace('-', " ");
    let reply = redeem(&world, &sloppy, "Jason's MacBook").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.raw);
    assert_eq!(reply.json["result"], "created");
    assert_eq!(reply.json["data"]["membership"], "m-kid1");
    assert_eq!(reply.json["data"]["pairing_id"], pairing_id);
    let device_a = Cred {
        id: reply.json["data"]["credential"]["id"]
            .as_str()
            .unwrap()
            .to_string(),
        secret: reply.json["data"]["secret"].as_str().unwrap().to_string(),
    };
    assert_eq!(
        reply.json["data"]["credential"]["binding"],
        json!({"kind": "member", "membership": "m-kid1"})
    );
    assert_eq!(reply.headers["cache-control"], "no-store");
    // 设备凭证走数据面，被限制在码上记的上限里；没有任何管理权限
    assert_eq!(world.data_plane(&device_a.secret).await, StatusCode::OK);
    let who = world
        .call("GET", "/control/v1/whoami", Some(&device_a.secret), None)
        .await;
    assert_eq!(
        who.json["data"]["data_plane"]["allowed_roles"],
        json!(["fast"])
    );
    assert_eq!(who.json["data"]["data_plane"]["tenant_id"], "tenant-edu");
    let denied = world
        .call(
            "POST",
            "/control/v1/accounts",
            Some(&device_a.secret),
            Some(json!({"account_id": "z", "kind": "member", "parent": "s1", "unit": "THB"})),
        )
        .await;
    assert_eq!(
        (denied.status, denied.reason()),
        (StatusCode::FORBIDDEN, "op_not_granted")
    );
    // 同一个码再兑换一次：已使用
    let again = redeem(&world, &code, "second try").await;
    assert_eq!(
        (again.status, again.reason()),
        (StatusCode::CONFLICT, "pairing_code_used")
    );
    // 同一个账号的第二台设备：新码，各自一张凭证
    let (code_b, _) = pairing_code(&world, "m-kid1").await;
    let reply = redeem(&world, &code_b, "Office iMac").await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let device_b = Cred {
        id: reply.json["data"]["credential"]["id"]
            .as_str()
            .unwrap()
            .to_string(),
        secret: reply.json["data"]["secret"].as_str().unwrap().to_string(),
    };
    assert_ne!(device_a.id, device_b.id);
    assert_ne!(device_a.secret, device_b.secret);
    assert_eq!(world.data_plane(&device_b.secret).await, StatusCode::OK);
    // 设备 A 自己解绑：A 立即被拒；B 不受影响
    let unpaired = world
        .call(
            "POST",
            "/control/v1/pairing/unpair",
            Some(&device_a.secret),
            None,
        )
        .await;
    assert_eq!(
        (unpaired.status, unpaired.json["result"].as_str()),
        (StatusCode::OK, Some("updated"))
    );
    assert_eq!(unpaired.json["data"]["credential"]["status"], "revoked");
    assert_eq!(
        world.data_plane(&device_a.secret).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(world.data_plane(&device_b.secret).await, StatusCode::OK);
    let again = world
        .call(
            "POST",
            "/control/v1/pairing/unpair",
            Some(&device_a.secret),
            None,
        )
        .await;
    assert_eq!(
        (again.status, again.reason()),
        (StatusCode::UNAUTHORIZED, "credential_revoked")
    );
    // 账号这一侧（有权限的凭证）撤销设备 B：B 被拒，A 本来就已解绑
    let revoked = world
        .call(
            "POST",
            &format!("/control/v1/credentials/{}/status", device_b.id),
            Some(&world.root.secret),
            Some(json!({"status": "revoked"})),
        )
        .await;
    assert_eq!(revoked.status, StatusCode::OK);
    assert_eq!(
        world.data_plane(&device_b.secret).await,
        StatusCode::UNAUTHORIZED
    );
    // 设备列表（范围内的只读）：两台设备都在，状态各自
    let list = world
        .call(
            "GET",
            "/control/v1/accounts/s1/credentials",
            Some(&world.root.secret),
            None,
        )
        .await;
    let devices: Vec<&Value> = list.json["data"]["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| !c["device"].is_null())
        .collect();
    assert_eq!(devices.len(), 2);
    assert!(
        devices
            .iter()
            .any(|d| d["device"]["label"] == "Jason's MacBook")
    );
    // 解绑 / 撤销都有审计
    let audit = world.audit_rows();
    assert!(
        audit
            .iter()
            .any(|r| r.2 == "unpair_device" && r.3 == "success" && r.1 == device_a.id)
    );
    assert!(audit.iter().any(|r| r.0 == "pairing_code"
        && r.1 == pairing_id
        && r.2 == "redeem_pairing_code"
        && r.3 == "success"));
    // 非设备凭证不能 unpair
    let reply = world
        .call(
            "POST",
            "/control/v1/pairing/unpair",
            Some(&world.root.secret),
            None,
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "not_a_device")
    );
}

#[tokio::test]
async fn pairing_codes_expire_cannot_be_replayed_and_can_be_revoked() {
    let world = World::new();
    world.people().await;
    // 过期
    let (code, _) = pairing_code(&world, "m-kid1").await;
    world.clock.advance(10 * 60 * 1000 - 1);
    // 差一毫秒还能用……
    let reply = redeem(&world, &code, "just in time").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.raw);
    // ……到点就过期（右端开）：第二个码在 T0 + 10 分钟创建？不：它在 T0 + 10 分钟 - 1ms 的时候创建，
    // 所以它的有效期到 T0 + 20 分钟 - 1ms。先看一个刚好到期的码：
    world.clock.set(T0);
    let (edge, _) = pairing_code(&world, "m-kid2").await;
    world.clock.set(T0 + 10 * 60 * 1000);
    let late = redeem(&world, &edge, "too late").await;
    assert_eq!(
        (late.status, late.reason()),
        (StatusCode::GONE, "pairing_code_expired")
    );
    // 撤销
    let (code, pairing_id) = pairing_code(&world, "m-kid1").await;
    let revoked = world
        .call(
            "POST",
            &format!("/control/v1/pairing/codes/{pairing_id}/revoke"),
            Some(&world.root.secret),
            None,
        )
        .await;
    assert_eq!(
        (revoked.status, revoked.json["result"].as_str()),
        (StatusCode::OK, Some("updated"))
    );
    let reply = redeem(&world, &code, "revoked").await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::GONE, "pairing_code_revoked")
    );
    // 别的组织撤销不了；格式不对的 id 是 400
    let reply = world
        .call(
            "POST",
            &format!("/control/v1/pairing/codes/{pairing_id}/revoke"),
            Some(&world.other.secret),
            None,
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "out_of_scope")
    );
    let reply = world
        .call(
            "POST",
            "/control/v1/pairing/codes/not-an-id/revoke",
            Some(&world.root.secret),
            None,
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    // 签发者的权力被收回之后，它发出去还没兑换的码作废
    let (code, _) = pairing_code(&world, "m-kid1").await;
    let service = world
        .ok(
            "/control/v1/credentials",
            &world.root.secret,
            json!({"binding": {"kind": "service", "account": "eduai"},
                   "admin_scopes": [{"op": "issue_credential", "subtree": "eduai"}],
                   "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
        )
        .await;
    let service = Cred {
        id: service["credential"]["id"].as_str().unwrap().to_string(),
        secret: service["secret"].as_str().unwrap().to_string(),
    };
    let created = world
        .call("POST", "/control/v1/pairing/codes", Some(&service.secret), Some(json!({"membership": "m-kid2", "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]})))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let orphan = created.json["data"]["code"].as_str().unwrap().to_string();
    let revoke = world
        .call(
            "POST",
            &format!("/control/v1/credentials/{}/status", service.id),
            Some(&world.root.secret),
            Some(json!({"status": "revoked"})),
        )
        .await;
    assert_eq!(revoke.status, StatusCode::OK);
    let reply = redeem(&world, &orphan, "orphan").await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::UNAUTHORIZED, "credential_revoked")
    );
    // 没受影响的码仍然可以兑换
    assert_eq!(
        redeem(&world, &code, "unaffected").await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn wrong_pairing_codes_are_rate_limited_globally_and_cannot_be_enumerated() {
    let world = World::new();
    world.people().await;
    let (good, _) = pairing_code(&world, "m-kid1").await;
    // 查无此码、格式不对：同一个答案（`pairing_code_invalid`），都计入失败限速
    for attempt in 0..20 {
        let wrong = if attempt % 2 == 0 {
            "ABCD-EFGH-JKMN".to_string()
        } else {
            "not a code".to_string()
        };
        let reply = redeem(&world, &wrong, "x").await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::BAD_REQUEST, "pairing_code_invalid"),
            "attempt {attempt}"
        );
    }
    // 第 21 次：限速，连正确的码也被拒绝（全局限速：暴力破解在有效期内不可行）
    let blocked = redeem(&world, &good, "x").await;
    assert_eq!(
        (blocked.status, blocked.reason()),
        (StatusCode::TOO_MANY_REQUESTS, "pairing_rate_limited")
    );
    assert!(blocked.headers.contains_key("retry-after"));
    // 窗口滑过去之后恢复（一分钟；配对码的有效期 10 分钟，所以这个码仍然有效）
    world.clock.advance(60_001);
    let reply = redeem(&world, &good, "now fine").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.raw);
    // 失败的兑换没有审计行（认不出的码没有 id）；成功的有
    let audit = world.audit_rows();
    assert_eq!(
        audit
            .iter()
            .filter(|r| r.2 == "redeem_pairing_code")
            .count(),
        1
    );
    // 畸形的请求体不是「猜码」：不计入限速
    for _ in 0..30 {
        let reply = world
            .call(
                "POST",
                "/control/v1/pairing/redeem",
                None,
                Some(json!({"nope": 1})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::BAD_REQUEST, "invalid_request")
        );
    }
    let (another, _) = pairing_code(&world, "m-kid2").await;
    assert_eq!(
        redeem(&world, &another, "still open").await.status,
        StatusCode::CREATED
    );
}

// ---- #603 N3：配对失败限速按来源分桶，全局总闸只是总速率的上界 ----

#[tokio::test]
async fn one_noisy_source_cannot_lock_every_other_source_out_of_pairing() {
    // 限速原来是全局的（一分钟 20 次、鉴权前）：一个来源每分钟约 20 个垃圾码，所有租户的合法配对都拿 429。
    // 现在每个来源有自己的份额，总闸调到远高于单来源上限，一个来源打不满它。
    let world = World::new();
    world.people().await;
    let (good_a, _) = pairing_code(&world, "m-kid1").await;
    let (good_b, _) = pairing_code(&world, "m-kid2").await;
    let (noisy, honest, other) = ("198.51.100.1", "198.51.100.2", "198.51.100.3");
    // 吵闹的来源：20 个猜错（查无此码、格式不对）用完它自己的份额
    for attempt in 0..20 {
        let wrong = if attempt % 2 == 0 {
            "ABCD-EFGH-JKMN"
        } else {
            "not a code"
        };
        let reply = redeem_from(&world, noisy, wrong, "x").await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::BAD_REQUEST, "pairing_code_invalid"),
            "attempt {attempt}"
        );
    }
    // 它自己的下一次（连正确的码也一样）被限速：这是它自己的份额
    let blocked = redeem_from(&world, noisy, &good_a, "noisy").await;
    assert_eq!(
        (blocked.status, blocked.reason()),
        (StatusCode::TOO_MANY_REQUESTS, "pairing_rate_limited")
    );
    assert!(blocked.headers.contains_key("retry-after"));
    // 别的来源的合法码照常兑换：不受吵闹来源的失败影响
    for (peer, code) in [(honest, &good_a), (other, &good_b)] {
        let reply = redeem_from(&world, peer, code, "legitimate").await;
        assert_eq!(reply.status, StatusCode::CREATED, "{peer}: {}", reply.raw);
    }
    // 吵闹的来源仍然被限速；窗口滑过去之后恢复（恢复成「有机会再猜」，不是「放行」）
    assert_eq!(
        redeem_from(&world, noisy, "ABCD-EFGH-JKMN", "x")
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    world.clock.advance(60_001);
    assert_eq!(
        redeem_from(&world, noisy, "ABCD-EFGH-JKMN", "x")
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn many_sources_together_are_still_capped_by_the_global_gate() {
    // 总闸：所有来源合起来一分钟最多 600 次「猜错」（暴力破解的总速率上界，和有多少个来源无关）。
    // 30 个来源各用满自己的 20 个；第 31 个来源自己一次都没猜错，也要等窗口滑过去。
    let world = World::new();
    world.people().await;
    let (good, _) = pairing_code(&world, "m-kid1").await;
    let mut counter = 0_u32;
    for source in 1..=30 {
        for attempt in 0..20 {
            counter += 1;
            let reply = redeem_from(
                &world,
                &format!("198.51.100.{source}"),
                &wrong_code(counter),
                "x",
            )
            .await;
            assert_eq!(
                (reply.status, reply.reason()),
                (StatusCode::BAD_REQUEST, "pairing_code_invalid"),
                "source {source}, attempt {attempt}"
            );
        }
    }
    assert_eq!(counter, 600);
    let blocked = redeem_from(&world, "198.51.100.31", &good, "x").await;
    assert_eq!(
        (blocked.status, blocked.reason()),
        (StatusCode::TOO_MANY_REQUESTS, "pairing_rate_limited"),
        "the global gate is full: nobody else may start a lookup"
    );
    world.clock.advance(60_001);
    assert_eq!(
        redeem_from(&world, "198.51.100.31", &good, "now")
            .await
            .status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn a_forged_forwarding_header_never_buys_another_failure_budget() {
    // 组织形态没有「受信代理」的配置：头里写的地址一律不可信，来源只有 TCP 对端
    let world = World::new();
    world.people().await;
    let (good, _) = pairing_code(&world, "m-kid1").await;
    for round in 0..30 {
        let forged = format!("198.51.100.{}", round + 1);
        let reply = send_from(
            &world.app,
            Some("203.0.113.9"),
            "POST",
            "/control/v1/pairing/redeem",
            None,
            Some(json!({"code": "ABCD-EFGH-JKMN", "device_label": "x"})),
            &[
                ("x-forwarded-for", forged.as_str()),
                ("x-real-ip", "198.51.100.200"),
                ("forwarded", "for=198.51.100.201"),
            ],
        )
        .await;
        let expected = if round < 20 {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(reply.status, expected, "round {round}");
    }
    // 真正的另一个客户端（另一个 TCP 对端）不受影响，头里写没写它的地址都一样
    assert_eq!(
        redeem_from(&world, "203.0.113.10", &good, "a real second client")
            .await
            .status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn requests_without_a_known_source_share_one_bucket_apart_from_every_real_source() {
    // 直接驱动路由、没有对端地址的请求（只有测试会这样）：全部归在同一个「来源未知」的桶里，不占任何真实来源的份额
    let world = World::new();
    world.people().await;
    let (good, _) = pairing_code(&world, "m-kid1").await;
    for n in 0..20 {
        let reply = redeem(&world, &wrong_code(n), "x").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "attempt {n}");
    }
    assert_eq!(
        redeem(&world, &wrong_code(99), "x").await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    let reply = redeem_from(&world, "198.51.100.1", &good, "real source").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.raw);
}

#[tokio::test]
async fn authorization_outcomes_never_depend_on_the_tcp_peer_even_though_the_redeem_limiter_reads_it()
 {
    // 兑换的失败限速按来源分桶，是控制面里唯一读来源的地方。授权不能因此和来源有关：同一个请求从任何来源发来
    // （没有对端、loopback、局域网、公网、IPv6），状态码和原因码都一样
    let world = World::new();
    let root = world.root.secret.clone();
    let reader = world.reader.secret.clone();
    let account = json!({"account_id": "z", "kind": "member", "parent": "s1", "unit": "THB"});
    let cases: Vec<(&str, &str, Option<&str>, Option<Value>)> = vec![
        ("GET", "/control/v1/whoami", Some(root.as_str()), None),
        (
            "GET",
            "/control/v1/accounts/eduai",
            Some(reader.as_str()),
            None,
        ),
        // 超出范围、没有这个操作的权限、没有凭证：照样是各自固定的拒绝
        (
            "GET",
            "/control/v1/accounts/school-b",
            Some(reader.as_str()),
            None,
        ),
        (
            "POST",
            "/control/v1/accounts",
            Some(reader.as_str()),
            Some(account),
        ),
        ("GET", "/control/v1/whoami", None, None),
    ];
    for (verb, uri, bearer, body) in cases {
        let mut outcomes = std::collections::BTreeSet::new();
        for peer in [
            None,
            Some("127.0.0.1"),
            Some("10.0.0.5"),
            Some("203.0.113.9"),
            Some("::1"),
            Some("2001:db8::7"),
        ] {
            let reply = send_from(&world.app, peer, verb, uri, bearer, body.clone(), &[]).await;
            let reason = reply.json["error"]["reason_code"]
                .as_str()
                .unwrap_or("ok")
                .to_string();
            outcomes.insert((reply.status.as_u16(), reason));
        }
        assert_eq!(
            outcomes.len(),
            1,
            "{verb} {uri}: the answer depends on where the request came from: {outcomes:?}"
        );
    }
}

#[tokio::test]
async fn the_real_listener_gives_every_redeem_request_its_tcp_peer_as_the_source() {
    // 上面的测试是把对端地址手工挂到请求上；这一条走真的监听器（组织形态：控制面合并在数据面之后，不经过任何边缘中间件），
    // 证明生产里每个兑换请求的来源就是 TCP 对端，而不是「来源未知」的那个共用桶。
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let world = World::new();
    world.people().await;
    let (good, _) = pairing_code(&world, "m-kid1").await;
    // 先把「来源未知」的桶用满（直接驱动路由、没有对端地址）
    for n in 0..20 {
        let reply = redeem(&world, &wrong_code(n), "x").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "attempt {n}");
    }
    assert_eq!(
        redeem(&world, &wrong_code(99), "x").await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let app = world.app.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            crate::connection::ConnectionListener::new(listener, shutdown).unwrap(),
            app.into_make_service_with_connect_info::<crate::connection::ConnectionInfo>(),
        )
        .await
        .unwrap();
    });
    let raw = |code: String, extra_header: &'static str| async move {
        let body = json!({"code": code, "device_label": "x"}).to_string();
        let request = format!(
            "POST /control/v1/pairing/redeem HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{extra_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).to_string()
    };
    // 真的 TCP 连接有自己的来源（127.0.0.1），不受「来源未知」那个桶已满的影响：前 20 次猜错各得到查库的答案，
    // 请求头里写的地址（每次不同）不换桶；第 21 次被它自己的份额挡住
    for n in 0..21 {
        let reply = raw(wrong_code(n), "X-Forwarded-For: 198.51.100.77\r\n").await;
        let expected = if n < 20 { " 400 " } else { " 429 " };
        assert!(
            reply.starts_with("HTTP/1.1") && reply.lines().next().unwrap().contains(expected),
            "attempt {n}: {reply}"
        );
    }
    server.abort();
    // 对照：这个码没有被动过（猜码都是别的码），从别的来源照常兑换
    let ok = redeem_from(&world, "198.51.100.9", &good, "elsewhere").await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.raw);
}

#[tokio::test]
async fn ipv6_sources_share_their_64_bit_prefix_and_ipv4_mapped_addresses_count_as_ipv4() {
    // 来源取法和云入口的来源软配额一致（`abuse::source_key`）：IPv6 取 /64，IPv4 映射地址还原成 IPv4
    let world = World::new();
    world.people().await;
    let (good, _) = pairing_code(&world, "m-kid1").await;
    for n in 0..10 {
        let reply = redeem_from(&world, "2001:db8:1:2:aaaa::1", &wrong_code(n), "x").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "attempt {n}");
    }
    for n in 10..20 {
        // 同一个 /64 里的另一个地址：同一个桶
        let reply = redeem_from(&world, "2001:db8:1:2:bbbb::7", &wrong_code(n), "x").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "attempt {n}");
    }
    assert_eq!(
        redeem_from(&world, "2001:db8:1:2:cccc::3", &wrong_code(99), "x")
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS,
        "the whole /64 is one source"
    );
    // 另一个 /64 是另一个来源
    let reply = redeem_from(&world, "2001:db8:1:3::1", &good, "another /64").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.raw);
    // 映射地址和它的 IPv4 地址是同一个来源：交替用两种写法，一共 20 次猜错，第 21 次被限速
    for n in 20..40 {
        let peer = if n % 2 == 0 {
            "203.0.113.7"
        } else {
            "::ffff:203.0.113.7"
        };
        let reply = redeem_from(&world, peer, &wrong_code(n), "x").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "attempt {n}");
    }
    assert_eq!(
        redeem_from(&world, "::ffff:203.0.113.7", &wrong_code(99), "x")
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        redeem_from(&world, "203.0.113.7", &wrong_code(98), "x")
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn free_tier_over_http_is_bounded_by_the_pool_cap_and_its_window_must_be_the_month() {
    // Codex r1 #7 / #8 经 HTTP：补贴池 12 月总上限 100，两个主体各发 60，第二个 409；月份和窗口对不上 400
    let world = World::new();
    world.people().await;
    world
        .accounts
        .set_pool_cap(
            &idoris_compute_contracts::ids::AccountId::new("pool").unwrap(),
            "2026-12".parse().unwrap(),
            &idoris_compute_contracts::money::Money::new(
                idoris_compute_contracts::money::Unit::Thb,
                100,
            ),
            TimestampMs(T0),
        )
        .unwrap();
    // 2026-12 在账本时区（默认 UTC）里的窗口；窗口在授权方（teacher / eduai）的额度窗口之内
    let dec_start = T0 + 30 * DAY_MS - 12 * 3_600_000;
    let jan_start = dec_start + 31 * DAY_MS;
    let free = |id: &str, parent: &str, child: &str, micros: i64, from: i64, until: i64| {
        json!({"allocation_id": id, "parent": parent, "child": child,
               "amount": {"unit": "THB", "micros": micros}, "purpose": "free_tier",
               "valid_from_ms": from, "valid_until_ms": until, "month": "2026-12"})
    };
    let root = world.root.secret.clone();
    let first = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&root),
            Some(free("f-1", "teacher", "s1", 60, dec_start, jan_start)),
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.raw);
    let second = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&root),
            Some(free("f-2", "eduai", "s2", 60, dec_start, jan_start)),
        )
        .await;
    assert_eq!(
        (second.status, second.reason()),
        (StatusCode::CONFLICT, "allocation_exceeds_grantor"),
        "{}",
        second.raw
    );
    assert_eq!(
        second.json["error"]["message"],
        "the subsidy pool's cap for this month cannot cover this free-tier allocation",
        "the pool's remaining cap is shared across organizations and is not disclosed"
    );
    let mislabelled = world
        .call(
            "POST",
            "/control/v1/allocations",
            Some(&root),
            Some(free(
                "f-3",
                "eduai",
                "s2",
                10,
                dec_start - DAY_MS,
                jan_start,
            )),
        )
        .await;
    assert_eq!(
        (mislabelled.status, mislabelled.reason()),
        (StatusCode::BAD_REQUEST, "invalid_allocation"),
        "{}",
        mislabelled.raw
    );
    let conn = world.db();
    let (grants, allocations): (i64, i64) = (
        conn.query_row("SELECT count(*) FROM ledger_subsidy_grants", [], |r| {
            r.get(0)
        })
        .unwrap(),
        conn.query_row(
            "SELECT count(*) FROM allocations WHERE purpose = 'free_tier'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    );
    assert_eq!((grants, allocations), (1, 1), "only the first grant exists");
}

#[tokio::test]
async fn registering_another_organizations_principal_over_http_is_a_plain_conflict() {
    // PR-Daemon S2 F3 补充：重复登记别的组织的主体，得到固定的 409，没有主体、状态、登记人
    let world = World::new();
    world.people().await;
    let other = world.other.secret.clone();
    let reply = world
        .call(
            "POST",
            "/control/v1/principals",
            Some(&other),
            Some(json!({"principal_id": "kid1", "org": "school-b"})),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::CONFLICT, "already_exists"),
        "{}",
        reply.raw
    );
    assert!(reply.json.get("data").is_none(), "{}", reply.raw);
    assert!(
        !reply.raw.contains("active") && !reply.raw.contains("revoked"),
        "no status: {}",
        reply.raw
    );
    // 登记它的组织自己重发：照旧是 replayed，带状态
    let again = world
        .call(
            "POST",
            "/control/v1/principals",
            Some(&world.root.secret),
            Some(json!({"principal_id": "kid1", "org": "eduai"})),
        )
        .await;
    assert_eq!(again.json["result"], "replayed", "{}", again.raw);
    assert_eq!(again.json["data"]["principal"]["status"], "active");
}

/// 响应的状态、头和正文，去掉每次请求都不一样的关联 id（头 `X-iDoris-Record-Id` 和正文里的 `request_id`）。
fn answer_without_the_correlation_id(reply: &Reply) -> (StatusCode, Vec<(String, String)>, Value) {
    let mut headers: Vec<(String, String)> = reply
        .headers
        .iter()
        .filter(|(name, _)| name.as_str() != "x-idoris-record-id")
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
        .collect();
    headers.sort();
    let mut body = reply.json.clone();
    body["error"]["request_id"] = Value::Null;
    (reply.status, headers, body)
}

#[tokio::test]
async fn an_outsider_gets_the_same_http_answer_for_an_active_a_revoked_and_a_missing_principal() {
    // PR #600 第四轮复审新问题 2：外部组织（school-b）建成员关系时，
    // （a）伪造 `org` 字段（org = eduai，付款账户是 school-b 自己的账户），
    // （b）只有 `manage_membership`、没有 `revoke_principal` 的凭证提交真实的 `org`，
    // 对「eduai 登记的活跃主体」「eduai 登记后撤销的主体」「根本不存在的主体」必须得到逐字节相同的回答
    // （状态码、头、正文，只有每次请求都不一样的关联 id 除外），不能靠原因码或文案区分。
    let world = World::new();
    world.people().await;
    let (root, other) = (world.root.secret.clone(), world.other.secret.clone());
    world
        .ok(
            "/control/v1/principals",
            &root,
            json!({"principal_id": "dead", "org": "eduai"}),
        )
        .await;
    world
        .ok(
            "/control/v1/principals/dead/revoke",
            &root,
            json!({"org": "eduai"}),
        )
        .await;
    world
        .ok(
            "/control/v1/accounts",
            &other,
            json!({"account_id": "acct-b", "kind": "member", "parent": "school-b", "unit": "THB"}),
        )
        .await;
    // 只有 manage_membership 的凭证（school-b 子树）
    let narrow = world
        .ok(
            "/control/v1/credentials",
            &other,
            json!({
                "binding": {"kind": "service", "account": "school-b"},
                "admin_scopes": [{"op": "manage_membership", "subtree": "school-b"}],
                "owner": "narrow", "tenant_id": "tenant-b",
                "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]
            }),
        )
        .await["secret"]
        .as_str()
        .unwrap()
        .to_string();

    for (label, bearer, org) in [
        ("forged org", other.as_str(), "eduai"),
        ("only manage_membership", narrow.as_str(), "school-b"),
    ] {
        let mut answers = Vec::new();
        for (principal, id) in [
            ("kid1", "m-probe-active"),
            ("dead", "m-probe-revoked"),
            ("ghost", "m-probe-missing"),
        ] {
            let reply = world
                .call(
                    "POST",
                    "/control/v1/memberships",
                    Some(bearer),
                    Some(
                        json!({"membership_id": id, "principal_id": principal, "org": org,
                                "role": "member", "account": "acct-b"}),
                    ),
                )
                .await;
            assert!(
                !reply.status.is_success(),
                "{label} {principal}: {}",
                reply.raw
            );
            for name in ["kid1", "dead", "ghost"] {
                assert!(
                    !reply.raw.contains(name),
                    "{label} {principal}: the answer must not echo a principal id: {}",
                    reply.raw
                );
            }
            answers.push(answer_without_the_correlation_id(&reply));
        }
        assert!(
            answers.iter().all(|answer| *answer == answers[0]),
            "{label}: the three principal states must be indistinguishable: {answers:?}"
        );
    }
    // 对照：归属方自己仍然看得到具体原因（已撤销的主体）
    world
        .ok(
            "/control/v1/accounts",
            &root,
            json!({"account_id": "acct-dead", "kind": "member", "parent": "eduai", "unit": "THB"}),
        )
        .await;
    let owner = world
        .call(
            "POST",
            "/control/v1/memberships",
            Some(&root),
            Some(
                json!({"membership_id": "m-owner-dead", "principal_id": "dead", "org": "eduai",
                        "role": "member", "account": "acct-dead"}),
            ),
        )
        .await;
    assert_eq!(owner.reason(), "principal_revoked", "{}", owner.raw);
}

#[tokio::test]
async fn parallel_wrong_pairing_codes_cannot_all_get_to_the_database_lookup() {
    // PR-Daemon S2 F1：限速器「先检查、后记录」——失败要等数据库查完才记，所以并发的猜码全都先过了检查。
    // 200 个并发的错码，旧代码全部走到了查询（200 个 pairing_code_invalid）。现在先占名额再查：窗口里最多 20 次
    // 尝试能走到查询，其余在查询之前就是 429。
    let world = Arc::new(World::new());
    world.people().await;
    let (good, _) = pairing_code(&world, "m-kid1").await;
    let mut handles = Vec::new();
    for i in 0..200_u32 {
        let world = world.clone();
        handles.push(tokio::spawn(async move {
            // 格式合法、查无此码的 12 位码
            let digits = format!("{i:012}");
            let wrong = format!("{}-{}-{}", &digits[0..4], &digits[4..8], &digits[8..12]);
            let reply = redeem(&world, &wrong, "x").await;
            (reply.status, reply.reason().to_string())
        }));
    }
    let (mut reached_lookup, mut limited) = (0, 0);
    for handle in handles {
        match handle.await.unwrap() {
            (StatusCode::BAD_REQUEST, reason) if reason == "pairing_code_invalid" => {
                reached_lookup += 1;
            }
            (StatusCode::TOO_MANY_REQUESTS, reason) if reason == "pairing_rate_limited" => {
                limited += 1;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(reached_lookup, 20, "at most the limit reaches the lookup");
    assert_eq!(limited, 180);
    // 连正确的码也被限速（名额被猜码占满了），窗口滑过去之后恢复
    let blocked = redeem(&world, &good, "x").await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    world.clock.advance(60_001);
    assert_eq!(
        redeem(&world, &good, "now fine").await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn a_successful_or_recognized_redemption_gives_its_slot_back() {
    // 名额是先占的，所以只有「猜错」（查无此码、格式不对）才一直占着；兑换成功、码已用 / 过期 / 被撤销 / 设备满
    // （码是认得出的，不是在猜）把名额还回去，正常使用不会攒出限速。
    // 30 次成功（三个成员关系各 10 台设备，超过限速的 20）再加 25 次「设备满」，一次猜错都没有：一个名额都不能占。
    let world = World::new();
    world.people().await;
    for round in 0..30 {
        let membership = ["m-kid1", "m-kid2", "m-tch"][round / 10];
        let (code, _) = pairing_code(&world, membership).await;
        let reply = redeem(&world, &code, &format!("device {round}")).await;
        assert_eq!(
            reply.status,
            StatusCode::CREATED,
            "round {round}: {}",
            reply.raw
        );
    }
    for round in 0..25 {
        let (code, _) = pairing_code(&world, "m-kid2").await;
        let reply = redeem(&world, &code, &format!("one too many {round}")).await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::CONFLICT, "device_limit_reached"),
            "round {round}: {}",
            reply.raw
        );
    }
    // 这 55 次里没有一次猜错：全局限速一个名额都没占
    let wrong = redeem(&world, "ABCD-EFGH-JKMN", "x").await;
    assert_eq!(
        (wrong.status, wrong.reason()),
        (StatusCode::BAD_REQUEST, "pairing_code_invalid")
    );
}

#[tokio::test]
async fn concurrent_http_redemptions_of_one_code_yield_exactly_one_device() {
    let world = Arc::new(World::new());
    world.people().await;
    let (code, _) = pairing_code(&world, "m-kid1").await;
    let mut handles = Vec::new();
    for i in 0..10 {
        let world = world.clone();
        let code = code.clone();
        handles.push(tokio::spawn(async move {
            let reply = redeem(&world, &code, &format!("device {i}")).await;
            (
                reply.status,
                reply.json["error"]["reason_code"]
                    .as_str()
                    .map(str::to_string),
            )
        }));
    }
    let mut created = 0;
    for handle in handles {
        let (status, reason) = handle.await.unwrap();
        match status {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => assert_eq!(reason.as_deref(), Some("pairing_code_used")),
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(created, 1, "exactly one concurrent redemption may win");
    let conn = world.db();
    let devices: i64 = conn
        .query_row("SELECT count(*) FROM pairing_devices", [], |r| r.get(0))
        .unwrap();
    assert_eq!(devices, 1);
}

#[tokio::test]
async fn pairing_codes_require_a_live_member_and_respect_the_account_limits() {
    let world = World::new();
    world.people().await;
    let body = |extra: Value| {
        let mut body = json!({"membership": "m-kid1", "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]});
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    };
    // 有效期的范围
    for (ttl, expected) in [
        (29, StatusCode::BAD_REQUEST),
        (901, StatusCode::BAD_REQUEST),
        (30, StatusCode::CREATED),
        (900, StatusCode::CREATED),
    ] {
        let reply = world
            .call(
                "POST",
                "/control/v1/pairing/codes",
                Some(&world.root.secret),
                Some(body(json!({"ttl_seconds": ttl}))),
            )
            .await;
        assert_eq!(reply.status, expected, "ttl {ttl}: {}", reply.raw);
    }
    // 数据面上限不能超过签发者（root 有全部角色；teacher 只有 fast）
    let reply = world
        .call(
            "POST",
            "/control/v1/pairing/codes",
            Some(&world.teacher.secret),
            Some(body(json!({"allowed_roles": ["fast", "deep"]}))),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::FORBIDDEN, "ceiling_exceeds_grantor")
    );
    // 设备数上限：同一个账号最多 10 台有效设备
    for i in 0..10 {
        let (code, _) = pairing_code(&world, "m-kid1").await;
        let reply = redeem(&world, &code, &format!("d{i}")).await;
        assert_eq!(
            reply.status,
            StatusCode::CREATED,
            "device {i}: {}",
            reply.raw
        );
    }
    let (code, _) = pairing_code(&world, "m-kid1").await;
    let reply = redeem(&world, &code, "eleventh").await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::CONFLICT, "device_limit_reached")
    );
    // 成员关系被撤销之后：不能再创建、也不能兑换
    let reply = world
        .call(
            "POST",
            "/control/v1/memberships/m-kid1/revoke",
            Some(&world.root.secret),
            None,
        )
        .await;
    assert!(reply.status.is_success());
    let reply = world
        .call(
            "POST",
            "/control/v1/pairing/codes",
            Some(&world.root.secret),
            Some(body(json!({}))),
        )
        .await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::CONFLICT, "membership_revoked")
    );
    let reply = redeem(&world, &code, "after revocation").await;
    assert_eq!(
        (reply.status, reply.reason()),
        (StatusCode::CONFLICT, "membership_revoked")
    );
    // 设备显示名不合法
    let (code, _) = pairing_code(&world, "m-kid2").await;
    for label in ["", " leading", "tab\there"] {
        let reply = redeem(&world, &code, label).await;
        assert_eq!(
            (reply.status, reply.reason()),
            (StatusCode::BAD_REQUEST, "invalid_request"),
            "{label:?}"
        );
    }
    assert_eq!(
        redeem(&world, &code, "ok").await.status,
        StatusCode::CREATED
    );
}

// ================================================================================================
// 运维 bootstrap
// ================================================================================================

#[test]
fn bootstrap_is_audited_as_an_operator_is_idempotent_for_accounts_and_never_logs_secrets() {
    let world = World::new();
    // bootstrap 写的审计行：actor 是 operator / cli，每个账户、额度、凭证一行
    let rows = world.audit_rows();
    assert!(
        rows.iter()
            .all(|r| r.0 == "operator" && r.1 == "cli" && r.3 == "success"),
        "{rows:?}"
    );
    assert_eq!(rows.iter().filter(|r| r.2 == "create_account").count(), 8);
    assert_eq!(rows.iter().filter(|r| r.2 == "grant_allocation").count(), 4);
    assert_eq!(rows.iter().filter(|r| r.2 == "issue_credential").count(), 4);
    // 重跑：账户和额度是 replayed（幂等），凭证再签发一批新的（不幂等，文档写明）
    let mut output = Vec::new();
    bootstrap::run(
        &world.accounts,
        &mut fixture_spec().to_string().as_bytes(),
        &mut output,
        world.clock.as_ref(),
    )
    .unwrap();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        lines
            .iter()
            .filter(|l| l["kind"] == "account" || l["kind"] == "allocation")
            .all(|l| l["result"] == "replayed")
    );
    let secrets: Vec<&str> = lines
        .iter()
        .filter(|l| l["kind"] == "credential")
        .map(|l| l["secret"].as_str().unwrap())
        .collect();
    assert_eq!(secrets.len(), 4);
    for secret in secrets
        .iter()
        .chain([&world.root.secret.as_str(), &world.teacher.secret.as_str()])
    {
        let hits: i64 = world
            .db()
            .query_row("SELECT count(*) FROM control_audit WHERE op LIKE ?1 OR actor_id LIKE ?1 OR related_id LIKE ?1 OR target_id LIKE ?1", [format!("%{secret}%")], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 0);
    }
}

#[tokio::test]
async fn platform_level_emergency_actions_belong_to_the_operator_only() {
    // 顶层组织（eduai 的父账户是补贴池、school-b 的父账户是平台）没有任何凭证能管：它们的管理凭证只能管自己的子树。
    let world = World::new();
    world.people().await;
    let member = world.member_credential("m-kid1").await;
    for token in [
        &world.root.secret,
        &world.other.secret,
        &world.teacher.secret,
    ] {
        for (uri, body) in [
            (
                "/control/v1/accounts/eduai/status",
                json!({"status": "frozen"}),
            ),
            (
                "/control/v1/accounts/pool/status",
                json!({"status": "frozen"}),
            ),
            (
                "/control/v1/accounts/platform/status",
                json!({"status": "closed"}),
            ),
        ] {
            let reply = world.call("POST", uri, Some(token), Some(body)).await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}: {}", reply.raw);
            assert!(
                matches!(reply.reason(), "would_disable_caller" | "out_of_scope"),
                "{uri}: {}",
                reply.reason()
            );
        }
    }
    assert_eq!(world.data_plane(&member.secret).await, StatusCode::OK);
    // 运维用 `idoris control bootstrap`（同一个函数）冻结 eduai：整个子树的凭证在数据面立即被拒；再解冻立即恢复
    let run = |spec: Value| {
        let mut output = Vec::new();
        bootstrap::run(
            &world.accounts,
            &mut spec.to_string().as_bytes(),
            &mut output,
            world.clock.as_ref(),
        )
        .map(|()| String::from_utf8(output).unwrap())
    };
    let output =
        run(json!({"account_statuses": [{"account_id": "eduai", "status": "frozen"}]})).unwrap();
    assert!(output.contains("\"result\":\"updated\""), "{output}");
    for token in [&world.root.secret, &member.secret, &world.reader.secret] {
        assert_eq!(world.data_plane(token).await, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        world.data_plane(&world.other.secret).await,
        StatusCode::OK,
        "school-b is a different organization"
    );
    run(json!({"account_statuses": [{"account_id": "eduai", "status": "active"}]})).unwrap();
    assert_eq!(world.data_plane(&member.secret).await, StatusCode::OK);
    // 运维撤销一张凭证
    let output =
        run(json!({"credential_statuses": [{"credential_id": member.id, "status": "revoked"}]}))
            .unwrap();
    assert!(output.contains("\"kind\":\"credential_status\""));
    assert_eq!(
        world.data_plane(&member.secret).await,
        StatusCode::UNAUTHORIZED
    );
    // 都留了审计，actor 是 operator
    let rows = world.audit_rows();
    assert!(
        rows.iter()
            .filter(|r| r.0 == "operator" && r.2 == "set_account_status")
            .count()
            >= 2
    );
    assert!(
        rows.iter()
            .any(|r| r.0 == "operator" && r.2 == "revoke_credential")
    );
    // 存储层的状态机照旧：关闭是终态，运维也不能重开
    run(json!({"account_statuses": [{"account_id": "b1", "status": "closed"}]})).unwrap();
    let error =
        run(json!({"account_statuses": [{"account_id": "b1", "status": "active"}]})).unwrap_err();
    assert!(error.contains("illegal_transition"), "{error}");
    // 反复冻结同一个账户是 unchanged，不是错误
    let output = run(json!({"account_statuses": [{"account_id": "s2", "status": "frozen"}, {"account_id": "s2", "status": "frozen"}]})).unwrap();
    assert!(output.contains("\"result\":\"unchanged\""));
}

#[test]
fn bootstrap_rejects_bad_specs_before_touching_the_database_and_stops_at_the_first_failure() {
    let world = World::new();
    let run = |spec: &str| -> Result<String, String> {
        let mut output = Vec::new();
        bootstrap::run(
            &world.accounts,
            &mut spec.as_bytes(),
            &mut output,
            world.clock.as_ref(),
        )
        .map(|()| String::from_utf8(output).unwrap())
    };
    let before = world.audit_rows().len();
    for (label, spec) in [
        ("empty", String::new()),
        ("not json", "{".to_string()),
        ("unknown field", json!({"accounts": [], "extra": 1}).to_string()),
        ("unknown account field", json!({"accounts": [{"account_id": "x", "kind": "member", "unit": "THB", "colour": "red"}]}).to_string()),
        ("bad id", json!({"accounts": [{"account_id": "a b", "kind": "member", "parent": "eduai", "unit": "THB"}]}).to_string()),
        ("no privacy class", json!({"credentials": [{"binding": {"kind": "service", "account": "eduai"}, "allowed_privacy": []}]}).to_string()),
        ("unknown role", json!({"credentials": [{"binding": {"kind": "service", "account": "eduai"}, "allowed_privacy": ["local_only"], "allowed_roles": ["root"]}]}).to_string()),
        ("bad owner", json!({"credentials": [{"binding": {"kind": "service", "account": "eduai"}, "allowed_privacy": ["local_only"], "owner": " padded "}]}).to_string()),
        ("too large", format!("{{\"pad\": \"{}\"}}", "x".repeat(70_000))),
    ] {
        assert!(run(&spec).is_err(), "{label}");
    }
    assert_eq!(
        world.audit_rows().len(),
        before,
        "a rejected spec writes nothing, not even an audit row"
    );
    // 第二项失败：第一项已经落库并输出，之后的不执行
    let spec = json!({"accounts": [
        {"account_id": "extra-1", "kind": "member", "parent": "eduai", "unit": "THB"},
        {"account_id": "extra-2", "kind": "member", "parent": "ghost", "unit": "THB"},
        {"account_id": "extra-3", "kind": "member", "parent": "eduai", "unit": "THB"}
    ]})
    .to_string();
    let mut output = Vec::new();
    let error = bootstrap::run(
        &world.accounts,
        &mut spec.as_bytes(),
        &mut output,
        world.clock.as_ref(),
    )
    .unwrap_err();
    assert!(error.contains("accounts[1]"), "{error}");
    let printed = String::from_utf8(output).unwrap();
    assert!(printed.contains("extra-1") && !printed.contains("extra-3"));
    assert!(
        world
            .accounts
            .get_account(&idoris_compute_contracts::ids::AccountId::new("extra-1").unwrap())
            .unwrap()
            .is_some()
    );
    assert!(
        world
            .accounts
            .get_account(&idoris_compute_contracts::ids::AccountId::new("extra-3").unwrap())
            .unwrap()
            .is_none()
    );
    // 失败也留审计（运维的失败同样是一条记录）
    let last = world.audit_rows().pop().unwrap();
    assert_eq!(
        (last.0.as_str(), last.2.as_str(), last.3.as_str()),
        ("operator", "create_account", "rejected")
    );
    // 凭证的签发规则对运维同样适用（S1 的绑定约束）：Service 凭证只能绑 Org
    let error = run(&json!({"credentials": [{"binding": {"kind": "service", "account": "teacher"}, "allowed_privacy": ["local_only"]}]}).to_string()).unwrap_err();
    assert!(error.contains("credentials[0]"), "{error}");
    // 没有任何错误信息带着密钥
    assert!(!error.contains("idk_"));
}

#[test]
fn the_operator_command_exists_only_in_the_organization_and_cloud_forms() {
    use crate::cli::{Command, ControlCommand, parse_args};
    use crate::startup_form::StartupForm;
    use std::ffi::OsString;
    assert_eq!(
        parse_args(["control", "bootstrap", "--spec-stdin"].map(OsString::from)).unwrap(),
        Command::Control(ControlCommand::Bootstrap)
    );
    for bad in [
        vec!["control"],
        vec!["control", "bootstrap"],
        vec!["control", "bootstrap", "--spec"],
        vec!["control", "bootstrap", "--spec-stdin", "extra"],
        vec!["control", "unknown", "--spec-stdin"],
    ] {
        assert!(parse_args(bad.into_iter().map(OsString::from)).is_err());
    }
    // 个人形态和没声明形态的存量 tenant 部署：拒绝，而且不碰数据库
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("never-created.sqlite3");
    // `process_db_path` 读环境变量；这里只验证形态判定（先于任何数据库访问）
    for startup in [
        StartupForm::legacy(DeployMode::Personal),
        StartupForm::legacy(DeployMode::Tenant),
        StartupForm::from_env_value(DeployMode::Personal, Some("personal")).unwrap(),
    ] {
        assert!(!startup.opens_account_store());
        let error = crate::storage::open_account_store_process(startup)
            .err()
            .unwrap();
        assert!(error.contains("IDORIS_FORM"), "{error}");
    }
    assert!(!db.exists());
    for text in ["organization", "cloud"] {
        assert!(
            StartupForm::from_env_value(DeployMode::Tenant, Some(text))
                .unwrap()
                .opens_account_store()
        );
    }
}

#[tokio::test]
async fn a_caller_without_the_operation_gets_the_same_http_answer_for_an_existing_and_a_missing_id()
{
    // issue #603 N7：撤销成员关系 / 撤销额度 / 改凭证状态是按 id 查的写操作。没有这个操作权限的调用者（reader 只有 read），
    // 对「自己组织的」「别的组织的」「不存在的」id 必须得到逐字节相同的回答（状态码、头、正文，只有每次请求都不一样的
    // 关联 id 除外），不能靠状态码或文案探测别的组织有哪些 id。
    let world = World::new();
    world.people().await;
    let (root, reader) = (world.root.secret.clone(), world.reader.secret.clone());
    let grant = json!({"allocation_id": "g1", "parent": "teacher", "child": "s1",
        "amount": {"unit": "THB", "micros": 2_000_000}, "purpose": "prepaid",
        "valid_from_ms": T0, "valid_until_ms": T0 + DAY_MS});
    world.ok("/control/v1/allocations", &root, grant).await;

    type Call = (String, Option<Value>);
    let probes: Vec<(&str, Vec<Call>)> = vec![
        (
            "revoke membership",
            ["m-kid1", "m-nope-1", "m-nope-2"]
                .iter()
                .map(|id| (format!("/control/v1/memberships/{id}/revoke"), None))
                .collect(),
        ),
        (
            "revoke allocation",
            ["g1", "g-nope-1", "g-nope-2"]
                .iter()
                .map(|id| (format!("/control/v1/allocations/{id}/revoke"), None))
                .collect(),
        ),
        (
            "credential status",
            [world.root.id.as_str(), "k-nope-1", "k-nope-2"]
                .iter()
                .map(|id| {
                    (
                        format!("/control/v1/credentials/{id}/status"),
                        Some(json!({"status": "revoked"})),
                    )
                })
                .collect(),
        ),
    ];
    for (label, calls) in probes {
        let mut answers = Vec::new();
        for (path, body) in calls {
            let reply = world.call("POST", &path, Some(&reader), body).await;
            assert!(!reply.status.is_success(), "{label} {path}: {}", reply.raw);
            answers.push(answer_without_the_correlation_id(&reply));
        }
        assert!(
            answers.iter().all(|answer| *answer == answers[0]),
            "{label}: an existing and a missing id must be indistinguishable: {answers:?}"
        );
    }
}

#[tokio::test]
async fn member_credentials_and_pairing_codes_give_a_caller_without_the_operation_the_same_http_answer_for_any_id()
 {
    // issue #603 N7 同类（PR-Daemon 在 #608 评审里指出）：签发成员凭证、创建配对码按成员关系 id 去查，撤销配对码按配对码 id 去查，
    // 旧代码都是先查、后授权。没有 `issue_credential` 权限的调用者（reader 只有 read），对「自己组织的」「别的组织的」
    // 「不存在的」id 必须得到逐字节相同的回答（状态码、头、正文，只有每次请求都不一样的关联 id 除外）。
    let world = World::new();
    world.people().await;
    let (reader, other) = (world.reader.secret.clone(), world.other.secret.clone());
    // school-b 自己的主体、成员关系（成员账户 b1）和一个配对码
    world
        .ok(
            "/control/v1/principals",
            &other,
            json!({"principal_id": "kb1", "org": "school-b"}),
        )
        .await;
    world
        .ok(
            "/control/v1/memberships",
            &other,
            json!({"membership_id": "m-kb1", "principal_id": "kb1", "org": "school-b",
                   "role": "member", "account": "b1"}),
        )
        .await;
    let (_, own_code) = pairing_code(&world, "m-kid1").await;
    let foreign_code = world
        .ok(
            "/control/v1/pairing/codes",
            &other,
            json!({"membership": "m-kb1", "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
        )
        .await["pairing"]["pairing_id"]
        .as_str()
        .unwrap()
        .to_string();

    type Call = (String, Option<Value>);
    let probes: Vec<(&str, Vec<Call>)> = vec![
        (
            "issue member credential",
            ["m-kid1", "m-kb1", "m-ghost"]
                .iter()
                .map(|id| {
                    (
                        "/control/v1/credentials".to_string(),
                        Some(json!({"binding": {"kind": "member", "membership": id},
                                    "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]})),
                    )
                })
                .collect(),
        ),
        (
            "create pairing code",
            ["m-kid1", "m-kb1", "m-ghost"]
                .iter()
                .map(|id| {
                    (
                        "/control/v1/pairing/codes".to_string(),
                        Some(json!({"membership": id, "allowed_privacy": ["local_only"],
                                    "allowed_roles": ["fast"]})),
                    )
                })
                .collect(),
        ),
        (
            "revoke pairing code",
            [
                own_code.as_str(),
                foreign_code.as_str(),
                "pc_0123456789abcdef",
            ]
            .iter()
            .map(|id| (format!("/control/v1/pairing/codes/{id}/revoke"), None))
            .collect(),
        ),
    ];
    for (label, calls) in probes {
        let mut answers = Vec::new();
        for (path, body) in calls {
            let reply = world.call("POST", &path, Some(&reader), body).await;
            assert!(!reply.status.is_success(), "{label} {path}: {}", reply.raw);
            answers.push(answer_without_the_correlation_id(&reply));
        }
        assert!(
            answers.iter().all(|answer| *answer == answers[0]),
            "{label}: an existing and a missing id must be indistinguishable: {answers:?}"
        );
    }
}

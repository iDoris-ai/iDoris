//! 云入口（计算服务 S5）端到端测试的共用夹具：真实的账户存储 / 虚拟 key 存储 / 账本 / 尝试记录（同一个临时 SQLite 文件），
//! 真实的运维 bootstrap 签发凭证，真实的云入口路由（`build_app` 在 `AppState::cloud_ingress` 上交给 `build_cloud_app`），
//! 假上游（`127.0.0.1`）。**不连接任何真实的推理供应商，也不启动任何推理进程。**
//!
//! 账户树（单位全是 THB，每个付款账户都充了值）：
//!
//! ```text
//! platform
//! ├── eduai (Org)                  租户 eduai：child_edu、补贴
//! │   ├── school-a (Org) ── s1 (Member, kid-1) ── s2 (Member, kid-2) ── sh-a (Member, shared，学校 A 的账户)
//! │   └── school-b (Org) ── s3 (Member, kid-3) ── sh-b (Member, shared，学校 B 的账户)
//! ├── agent24 (Org)                租户 agent24：adult、付费
//! │   └── a1 (Member, agent-1) ── a2 (Member, agent-2)
//! ├── rival (Org) ── r1 (Member, rival-1)     租户 rival：adult、付费（另一个客户，用来证明冒用被拒）
//! └── stray (Org) ── st1 (Member, stray-1)    账户树里有、配置里没有声明的租户
//! ```
#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]

#[path = "cloud_harness.rs"]
pub mod base;
#[path = "../../../idoris-upstream/tests/support/fake_upstream.rs"]
pub mod fake_upstream;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use idoris_compute_contracts::account::{Membership, MembershipRole, MembershipStatus};
use idoris_compute_contracts::ids::{
    AccountId, IdempotencyKey, MembershipId, PrincipalId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_router::auth::VirtualKeyAuthenticator;
use idoris_router::cloud::ingress::IngressConfig;
use idoris_router::cloud::{CloudIngress, CloudUpstream, LedgerAdmission, ManualClock};
use idoris_router::connection::ConnectionInfo;
use idoris_router::control::{AccountGate, ControlClock, ControlPlane, bootstrap};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::attempts::AttemptStore;
use idoris_tenancy::virtual_key::store::VirtualKeyStore;
use serde_json::{Value, json};
use tower::ServiceExt;

pub use base::{DAY_MS, Endpoint, KEY, NOW_MS, Options as CloudOptions, simple_body};
pub use fake_upstream::{Behavior, FakeUpstream, SseStep, completion, content_event, usage};

pub const ROLES_FAST: &str = "fast";

/// 12 入 30 出的实际价（夹具价目：对 THB 用户每百万 token 输入 10 / 输出 40）。
pub const ACTUAL_MICROS: i64 = 1_320;
/// 每个付款账户的初始充值（10 泰铢）。
pub const FUNDS_MICROS: i64 = 10_000_000;

pub fn aid(text: &str) -> AccountId {
    AccountId::new(text).unwrap()
}

pub fn pid(text: &str) -> PrincipalId {
    PrincipalId::new(text).unwrap()
}

pub fn thb(micros: i64) -> Money {
    Money::new(Unit::Thb, micros)
}

pub fn ts() -> TimestampMs {
    TimestampMs(NOW_MS)
}

/// 夹具里的凭证：id 和明文密钥。
#[derive(Debug, Clone)]
pub struct Cred {
    pub id: String,
    pub secret: String,
}

struct ClockAdapter(Arc<ManualClock>);

impl ControlClock for ClockAdapter {
    fn now(&self) -> TimestampMs {
        idoris_router::cloud::Clock::now(self.0.as_ref())
    }
}

/// 一个 HTTP 请求的描述。
#[derive(Debug, Clone)]
pub struct Req {
    pub method: &'static str,
    pub uri: String,
    pub bearer: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
    pub raw_body: Option<Vec<u8>>,
    /// 请求体只来一小段，然后永远不再来（慢请求）。
    pub stalled_body: bool,
    pub peer: Option<SocketAddr>,
}

impl Req {
    pub fn chat(cred: &Cred) -> Self {
        Self {
            method: "POST",
            uri: "/api/v1/chat/completions".to_string(),
            bearer: Some(cred.secret.clone()),
            headers: Vec::new(),
            body: Some(simple_body(100)),
            raw_body: None,
            stalled_body: false,
            peer: Some(default_peer()),
        }
    }

    /// 请求体发了一小段就停住（慢请求 / slowloris）。
    pub fn stalled(mut self) -> Self {
        self.stalled_body = true;
        self
    }

    pub fn models(cred: &Cred) -> Self {
        Self {
            method: "GET",
            uri: "/api/v1/models".to_string(),
            body: None,
            ..Self::chat(cred)
        }
    }

    pub fn uri(mut self, uri: &str) -> Self {
        self.uri = uri.to_string();
        self
    }

    pub fn method(mut self, method: &'static str) -> Self {
        self.method = method;
        self
    }

    pub fn member(self, member: &str) -> Self {
        self.header("x-idoris-member", member)
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn bearer_raw(mut self, bearer: Option<&str>) -> Self {
        self.bearer = bearer.map(str::to_string);
        self
    }

    pub fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    pub fn raw_body(mut self, bytes: Vec<u8>) -> Self {
        self.raw_body = Some(bytes);
        self.body = None;
        self
    }

    pub fn peer(mut self, ip: &str) -> Self {
        self.peer = Some(SocketAddr::new(ip.parse().unwrap(), 50_000));
        self
    }

    pub fn no_peer(mut self) -> Self {
        self.peer = None;
        self
    }

    pub fn build(&self) -> Request<Body> {
        let mut builder = Request::builder().method(self.method).uri(&self.uri);
        if let Some(bearer) = &self.bearer {
            builder = builder.header("authorization", format!("Bearer {bearer}"));
        }
        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body = match (&self.raw_body, &self.body) {
            _ if self.stalled_body => {
                use futures_util::StreamExt;
                let first = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"{\"model\":"))
                });
                Body::from_stream(first.chain(futures_util::stream::pending()))
            }
            (Some(raw), _) => Body::from(raw.clone()),
            (None, Some(json)) => {
                builder = builder.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            (None, None) => Body::empty(),
        };
        let mut request = builder.body(body).unwrap();
        if let Some(peer) = self.peer {
            request
                .extensions_mut()
                .insert(ConnectInfo(ConnectionInfo::for_peer(peer)));
        }
        request
    }
}

/// 一个没有任何特殊地位的公网地址（不是受信代理）。
pub fn default_peer() -> SocketAddr {
    SocketAddr::new("203.0.113.50".parse().unwrap(), 50_000)
}

/// 一次 HTTP 回复。
#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// 响应体里的 `error.reason_code`。
    pub fn code(&self) -> String {
        self.json()["error"]["reason_code"]
            .as_str()
            .unwrap_or_else(|| panic!("not an error reply ({}): {}", self.status, self.text()))
            .to_string()
    }

    /// 响应头 `X-iDoris-Reason`。
    pub fn reason(&self) -> Option<&str> {
        self.header("x-idoris-reason")
    }

    pub fn status_u16(&self) -> u16 {
        self.status.as_u16()
    }
}

pub async fn collect(response: axum::response::Response) -> Reply {
    let (parts, body) = response.into_parts();
    let body = body.collect().await.map(|c| c.to_bytes().to_vec());
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// 云入口配置
// ---------------------------------------------------------------------------

/// 云入口配置的各段；每段默认是「宽松到不会碰到」的值，测试只改自己要测的那一段。
#[derive(Debug, Clone)]
pub struct IngressYaml {
    pub public: String,
    pub control_public: bool,
    pub cors: String,
    pub abuse: String,
    pub limits: String,
    pub tenants: String,
}

impl Default for IngressYaml {
    fn default() -> Self {
        Self {
            public: String::new(),
            control_public: true,
            cors: String::new(),
            abuse: String::new(),
            limits: "limits:
  platform: {requests_per_minute: 600000, burst: 100000, max_concurrent: 10000}
  tenant: {requests_per_minute: 600000, burst: 100000, max_concurrent: 10000}
  member: {requests_per_minute: 600000, burst: 100000, max_concurrent: 10000}
"
            .to_string(),
            tenants: "tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    billing: subsidized
  - account: agent24
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
  - account: rival
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
"
            .to_string(),
        }
    }
}

impl IngressYaml {
    pub fn render(&self) -> String {
        format!(
            "schema_version: 1\n{}control_plane:\n  public: {}\n{}{}{}{}",
            self.public, self.control_public, self.cors, self.abuse, self.limits, self.tenants
        )
    }

    pub fn trusted(mut self, proxies: &[&str]) -> Self {
        let list = proxies
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        self.public = format!("public:\n  trusted_proxies: [{list}]\n");
        self
    }

    pub fn limits(mut self, platform: &str, tenant: &str, member: &str) -> Self {
        self.limits =
            format!("limits:\n  platform: {platform}\n  tenant: {tenant}\n  member: {member}\n");
        self
    }

    pub fn abuse(mut self, body: &str) -> Self {
        self.abuse = format!("abuse:\n{body}");
        self
    }

    pub fn cors(mut self, origins: &[&str]) -> Self {
        let list = origins
            .iter()
            .map(|o| format!("\"{o}\""))
            .collect::<Vec<_>>()
            .join(", ");
        self.cors = format!("cors:\n  allowed_origins: [{list}]\n  max_age_secs: 300\n");
        self
    }

    pub fn control(mut self, public: bool) -> Self {
        self.control_public = public;
        self
    }

    pub fn tenants(mut self, yaml: &str) -> Self {
        self.tenants = yaml.to_string();
        self
    }
}

// ---------------------------------------------------------------------------
// 世界
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct WorldOptions {
    pub script: Vec<Behavior>,
    pub ingress: IngressYaml,
    /// 云上游配置的 `limits:` 段（YAML 片段，缩进两格）；`None` = 夹具默认。
    pub cloud_limits: Option<String>,
}

impl WorldOptions {
    pub fn with_script(script: Vec<Behavior>) -> Self {
        Self {
            script,
            ..Self::default()
        }
    }

    /// 足够多的成功回复（每个 12 入 30 出）。
    pub fn plenty() -> Self {
        Self::with_script(ok_script(400))
    }

    pub fn ingress(mut self, ingress: IngressYaml) -> Self {
        self.ingress = ingress;
        self
    }
}

pub fn ok_behavior() -> Behavior {
    Behavior::json(200, completion("ok", "hello", Some(usage(12, 30))))
}

pub fn ok_script(count: usize) -> Vec<Behavior> {
    vec![ok_behavior(); count]
}

pub struct World {
    pub dir: tempfile::TempDir,
    pub db_path: PathBuf,
    pub accounts: Arc<AccountStore>,
    pub keys: Arc<Mutex<VirtualKeyStore>>,
    pub clock: Arc<ManualClock>,
    pub attempts: Arc<AttemptStore>,
    pub cloud: Arc<CloudUpstream>,
    pub ingress: Arc<CloudIngress>,
    pub control: Arc<ControlPlane>,
    pub app: Router,
    pub upstream: FakeUpstream,
    pub creds: BTreeMap<String, Cred>,
}

fn account_spec(id: &str, kind: &str, parent: Option<&str>) -> Value {
    match parent {
        Some(parent) => json!({"account_id": id, "kind": kind, "parent": parent, "unit": "THB"}),
        None => json!({"account_id": id, "kind": kind, "unit": "THB"}),
    }
}

fn allocation(id: &str, parent: &str, child: &str, micros: i64) -> Value {
    json!({
        "allocation_id": id, "parent": parent, "child": child,
        "amount": {"unit": "THB", "micros": micros}, "purpose": "prepaid",
        "valid_from_ms": NOW_MS - DAY_MS, "valid_until_ms": NOW_MS + 365 * DAY_MS
    })
}

fn all_ops(subtree: &str) -> Vec<Value> {
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
            json!({"op": op, "subtree": subtree, "max_amount": {"unit": "THB", "micros": 5_000_000}})
        } else {
            json!({"op": op, "subtree": subtree})
        }
    })
    .collect()
}

impl World {
    pub async fn new(options: WorldOptions) -> World {
        let upstream = FakeUpstream::start(options.script).await;
        let endpoints = vec![Endpoint::child_edu_compliant(
            "example-alpha",
            upstream.url(),
        )];
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("upstream.key");
        std::fs::write(&key_file, format!("{KEY}\n")).unwrap();
        std::fs::write(
            dir.path().join("permits.yaml"),
            base::permit_table_yaml(&endpoints),
        )
        .unwrap();
        let mut cloud_options = CloudOptions {
            callers: Vec::new(),
            ..CloudOptions::default()
        };
        if let Some(limits) = options.cloud_limits {
            cloud_options.limits = limits;
        }
        let yaml = base::config_yaml(&endpoints, key_file.to_str().unwrap(), &cloud_options);
        let parts = idoris_router::cloud::config::parse(&yaml, dir.path(), ts())
            .unwrap_or_else(|e| panic!("fixture config must load: {e}\n{yaml}"));

        let db_path = dir.path().join("idoris.sqlite3");
        let keys = Arc::new(Mutex::new(VirtualKeyStore::open(&db_path).unwrap()));
        let accounts = Arc::new(AccountStore::open(&db_path).unwrap());
        let attempts = Arc::new(AttemptStore::open(&db_path).unwrap());
        attempts
            .install_ledger_hook(accounts.attempt_ledger_hook())
            .unwrap();
        let clock = Arc::new(ManualClock::new(ts()));

        let mut world_creds = BTreeMap::new();
        Self::seed(&accounts, &clock, &mut world_creds);

        let cloud = Arc::new(CloudUpstream::new(
            parts,
            attempts.clone(),
            clock.clone(),
            Arc::new(LedgerAdmission::new(accounts.clone())),
        ));
        let authenticator = VirtualKeyAuthenticator::new(keys.clone())
            .with_gate(Arc::new(AccountGate::new(accounts.clone())));
        let config = IngressConfig::parse(&options.ingress.render())
            .unwrap_or_else(|e| panic!("{e}\n{}", options.ingress.render()));
        let ingress = Arc::new(
            CloudIngress::new(
                config,
                accounts.clone(),
                authenticator.clone(),
                clock.clone(),
                &cloud,
            )
            .unwrap(),
        );
        let control = Arc::new(
            ControlPlane::new(
                accounts.clone(),
                idoris_compute_contracts::form::DeploymentForm::Cloud,
            )
            .unwrap()
            .with_clock(Arc::new(ClockAdapter(clock.clone()))),
        );
        let state = idoris_router::AppState {
            deploy_mode: idoris_contracts::DeployMode::Tenant,
            virtual_key_authenticator: Some(authenticator),
            cloud: Some(cloud.clone()),
            control: Some(control.clone()),
            cloud_ingress: Some(ingress.clone()),
            ..idoris_router::AppState::default()
        };
        World {
            dir,
            db_path,
            accounts,
            keys,
            clock,
            attempts,
            cloud,
            ingress,
            control,
            app: idoris_router::build_app(state),
            upstream,
            creds: world_creds,
        }
    }

    /// 账户树、主体、成员关系、充值、凭证。凭证由真实的运维 bootstrap 签发（和 `idoris control bootstrap` 同一个函数）。
    fn seed(
        accounts: &Arc<AccountStore>,
        clock: &Arc<ManualClock>,
        creds: &mut BTreeMap<String, Cred>,
    ) {
        let adapter = ClockAdapter(clock.clone());
        let run = |spec: Value| -> Vec<Value> {
            let mut out = Vec::new();
            bootstrap::run(
                accounts,
                &mut spec.to_string().as_bytes(),
                &mut out,
                &adapter,
            )
            .unwrap();
            String::from_utf8(out)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        };
        run(json!({"accounts": [
            account_spec("platform", "platform", None),
            account_spec("eduai", "org", Some("platform")),
            account_spec("school-a", "org", Some("eduai")),
            account_spec("s1", "member", Some("school-a")),
            account_spec("s2", "member", Some("school-a")),
            account_spec("sh-a", "member", Some("school-a")),
            account_spec("school-b", "org", Some("eduai")),
            account_spec("s3", "member", Some("school-b")),
            account_spec("sh-b", "member", Some("school-b")),
            account_spec("agent24", "org", Some("platform")),
            account_spec("a1", "member", Some("agent24")),
            account_spec("a2", "member", Some("agent24")),
            account_spec("rival", "org", Some("platform")),
            account_spec("r1", "member", Some("rival")),
            account_spec("stray", "org", Some("platform")),
            account_spec("st1", "member", Some("stray")),
        ],
        // 额度沿树边往下发（授权上限，不是转移余额）：学校要有自己的额度，才能再给成员发额度
        "allocations": [
            allocation("a-eduai", "platform", "eduai", 1_000_000_000),
            allocation("a-school-a", "eduai", "school-a", 500_000_000),
            allocation("a-school-b", "eduai", "school-b", 500_000_000),
        ]}));
        for (principal, membership, org, account) in [
            ("kid-1", "m-kid-1", "school-a", "s1"),
            ("kid-2", "m-kid-2", "school-a", "s2"),
            ("kid-3", "m-kid-3", "school-b", "s3"),
            ("shared", "m-shared-a", "school-a", "sh-a"),
            ("shared", "m-shared-b", "school-b", "sh-b"),
            ("agent-1", "m-agent-1", "agent24", "a1"),
            ("agent-2", "m-agent-2", "agent24", "a2"),
            ("rival-1", "m-rival-1", "rival", "r1"),
            ("stray-1", "m-stray-1", "stray", "st1"),
        ] {
            accounts.ensure_principal(&pid(principal), ts()).unwrap();
            accounts
                .create_membership(
                    &Membership {
                        id: MembershipId::new(membership).unwrap(),
                        principal: pid(principal),
                        org: aid(org),
                        role: MembershipRole::Member,
                        account: Some(aid(account)),
                        status: MembershipStatus::Active,
                    },
                    ts(),
                )
                .unwrap();
        }
        // 每个付款账户（含它的所有祖先，除了平台根）都有自己的资金：准入沿整条祖先链检查，每个账户都要过
        for account in [
            "eduai", "school-a", "school-b", "s1", "s2", "s3", "sh-a", "sh-b", "agent24", "a1",
            "a2", "rival", "r1", "stray", "st1",
        ] {
            accounts
                .fund_account(
                    &aid(account),
                    &thb(FUNDS_MICROS),
                    &IdempotencyKey::new(format!("fund-{account}")).unwrap(),
                    ts(),
                )
                .unwrap();
        }
        let credential = |name: &str, binding: Value, admin: Vec<Value>| -> Value {
            let _ = name;
            json!({
                "binding": binding,
                "admin_scopes": admin,
                "owner": name,
                "tenant_id": name,
                "allowed_privacy": ["local_only"],
                "allowed_roles": [ROLES_FAST],
            })
        };
        let service = |account: &str| json!({"kind": "service", "account": account});
        let member = |membership: &str| json!({"kind": "member", "membership": membership});
        let specs = [
            ("svc_eduai", service("eduai"), vec![]),
            ("svc_school_a", service("school-a"), vec![]),
            ("svc_school_b", service("school-b"), vec![]),
            ("svc_agent24", service("agent24"), vec![]),
            ("svc_rival", service("rival"), vec![]),
            ("svc_stray", service("stray"), vec![]),
            ("svc_eduai_admin", service("eduai"), all_ops("eduai")),
            (
                "node_agent24",
                json!({"kind": "node", "account": "agent24"}),
                vec![],
            ),
            ("mem_kid1", member("m-kid-1"), vec![]),
            ("mem_kid2", member("m-kid-2"), vec![]),
            ("mem_agent1", member("m-agent-1"), vec![]),
            ("mem_agent2", member("m-agent-2"), vec![]),
        ];
        let outputs = run(json!({
            "credentials": specs
                .iter()
                .map(|(name, binding, admin)| credential(name, binding.clone(), admin.clone()))
                .collect::<Vec<_>>()
        }));
        let issued: Vec<Cred> = outputs
            .into_iter()
            .filter(|line| line["kind"] == "credential")
            .map(|line| Cred {
                id: line["credential_id"].as_str().unwrap().to_string(),
                secret: line["secret"].as_str().unwrap().to_string(),
            })
            .collect();
        assert_eq!(issued.len(), specs.len());
        for ((name, _, _), cred) in specs.iter().zip(issued) {
            creds.insert((*name).to_string(), cred);
        }
    }

    pub fn cred(&self, name: &str) -> &Cred {
        self.creds
            .get(name)
            .unwrap_or_else(|| panic!("no credential {name}"))
    }

    pub async fn send(&self, req: &Req) -> Reply {
        collect(self.app.clone().oneshot(req.build()).await.unwrap()).await
    }

    /// 在后台发一个请求（并发测试用）。
    pub fn spawn(&self, req: Req) -> tokio::task::JoinHandle<Reply> {
        let app = self.app.clone();
        tokio::spawn(async move { collect(app.oneshot(req.build()).await.unwrap()).await })
    }

    /// 一个账户此刻的余额（微泰铢）。
    pub fn balance(&self, account: &str) -> i64 {
        self.accounts
            .account_view(&aid(account), ts())
            .unwrap()
            .balance
            .unwrap()
            .micros
    }

    /// 账本里每个付款账户花掉的钱：`FUNDS - balance`。
    pub fn spent(&self, account: &str) -> i64 {
        FUNDS_MICROS - self.balance(account)
    }

    /// `compute_requests` 里的行：(付款账户, 凭证 id, 数据类别, 计费性质)，按创建顺序。
    pub fn request_rows(&self) -> Vec<(String, String, String, String)> {
        let conn = rusqlite::Connection::open(&self.db_path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT payer_account, credential_id, data_classes, billing \
                 FROM compute_requests ORDER BY created_at_ms, rowid",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    pub fn request_count(&self) -> usize {
        self.request_rows().len()
    }
}

/// `Authorization` 里一个格式正确、但库里没有的凭证。
pub fn unknown_secret(seed: u32) -> String {
    format!(
        "idk_{seed:08x}{}",
        "0123456789abcdef".repeat(3) + "01234567"
    )
}

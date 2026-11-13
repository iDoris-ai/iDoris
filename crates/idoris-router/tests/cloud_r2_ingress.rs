//! 计算服务 PR #600 的 Codex 审查 r2 与 PR-Daemon 评审修复的云入口回归（公网入口：认证之前、隐私、限流、请求体、儿童租户）。
//!
//! 整条路径是真的：HTTP -> 云入口（认证、成员 id、付款上下文、租户、限流）-> 处理函数 -> 执行器 -> 账本 -> 假上游。
//! 每个场景都是「在修复之前的代码上会失败」的复现。全部对本地假上游（`127.0.0.1`）进行，不连接任何真实的推理或安全服务。
//!
//! | 编号 | 场景 |
//! |---|---|
//! | R2 High#1 | 显式 `local_only`（头或请求体）在任何上游 / 安全检查调用之前被拒，不论租户是不是儿童租户 |
//! | R2 High#4 | 随机假凭证打不满认证存储：认证之前有界（在途个数 + 查询预算），在用的凭证不被饿死 |
//! | R2 Medium#5 | 认证排了很久队才完成的请求，按完成那一刻的时间限流 |
//! | R2 Medium#7 | 入口配置允许的请求体，处理函数的提取器不再用 axum 默认的 2 MiB 上限重复拒绝 |
//! | PD B3 | 付款链上任何一个 `child_edu` 声明都算；声明写在非顶层账户上启动失败 |
//! | PD Low | `X-iDoris-Tenant` 在最外层被删，后面的层看不到它 |
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ingress_harness.rs"]
mod h;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use h::*;
use idoris_compute_contracts::account::{Account, AccountKind, AccountStatus};
use idoris_router::cloud::safety::{
    BoxFuture, CheckContext, CheckReport, CheckedInput, CheckedOutput, ExtraCostEstimate,
    RuleChecker,
};
use idoris_router::cloud::{SafetyChecker, SafetyGate, SafetyLimits, SimpleExtraCost};
use serde_json::{Value, json};

const ROOMY: &str = "{requests_per_minute: 600000, burst: 100000, max_concurrent: 10000}";

/// 每个付款账户都不该被记账的清单。
const ALL_PAYERS: [&str; 15] = [
    "eduai", "school-a", "school-b", "s1", "s2", "s3", "sh-a", "sh-b", "agent24", "a1", "a2",
    "rival", "r1", "stray", "st1",
];

// ---------------------------------------------------------------------------
// 夹具：计数的安全检查器、占着认证存储的锁
// ---------------------------------------------------------------------------

/// 规则检查器加计数：证明一条路径确实走了儿童安全管线（或者确实没碰它）。
#[derive(Debug)]
struct Counting {
    inner: RuleChecker,
    input_calls: AtomicUsize,
    output_calls: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RuleChecker::new(),
            input_calls: AtomicUsize::new(0),
            output_calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> (usize, usize) {
        (
            self.input_calls.load(Ordering::SeqCst),
            self.output_calls.load(Ordering::SeqCst),
        )
    }
}

impl SafetyChecker for Counting {
    fn check_input<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
    ) -> BoxFuture<'a, CheckReport> {
        self.input_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.check_input(ctx, input)
    }

    fn check_output<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
        output: &'a CheckedOutput,
    ) -> BoxFuture<'a, CheckReport> {
        self.output_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.check_output(ctx, input, output)
    }

    fn estimate(
        &self,
        _stage: idoris_compute_contracts::safety::SafetyStage,
        _bytes: usize,
    ) -> Option<ExtraCostEstimate> {
        None
    }
}

fn counting_gate(checker: Arc<Counting>) -> SafetyGate {
    SafetyGate::new(
        checker,
        SafetyLimits::default(),
        Arc::new(SimpleExtraCost::new()),
    )
}

/// 在另一个线程里占着虚拟 key 存储的锁：认证查询会卡在 `spawn_blocking` 里等这把锁。
/// 用它证明「被拒绝的请求没有碰库」（碰了就会卡住）、也用它把一个认证拖慢（模拟排队很久）。
struct StoreLock {
    release: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl StoreLock {
    fn hold(w: &World) -> Self {
        let keys = w.keys.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _guard = keys.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        Self { release, thread }
    }

    fn release(self) {
        let _ = self.release.send(());
        self.thread.join().unwrap();
    }
}

fn random_credential_request(w: &World, seed: u32) -> Req {
    Req::chat(w.cred("svc_eduai"))
        .bearer_raw(Some(&unknown_secret(seed)))
        .member("kid-1")
}

/// 一个请求在期限内必须有答案（认证查询被拒绝时不碰库，所以即使库被占着也立刻返回）。
async fn answered_quickly(w: &World, req: &Req) -> Reply {
    tokio::time::timeout(Duration::from_millis(1_500), w.send(req))
        .await
        .expect("the request must be answered without touching the (locked) authentication store")
}

// ---------------------------------------------------------------------------
// R2 High#1：显式 local_only 永远不出网
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_local_only_request_never_reaches_the_upstream_or_the_safety_checker() {
    let counting = Counting::new();
    let w = World::new(WorldOptions {
        script: ok_script(10),
        safety_gate: Some(counting_gate(counting.clone())),
        ..WorldOptions::default()
    })
    .await;
    // eduai 的 Service 凭证（儿童租户，走安全管线）、agent24 的节点凭证（成人）、孩子自己的成员凭证
    let bases = [
        Req::chat(w.cred("svc_eduai")).member("kid-1"),
        Req::chat(w.cred("node_agent24")),
        Req::chat(w.cred("mem_kid1")),
    ];
    let mut refused = 0;
    for base in &bases {
        let mut in_body = simple_body(100);
        in_body["privacy"] = json!("local_only");
        let mut in_metadata = simple_body(100);
        in_metadata["metadata"] = json!({"privacy": "local_only"});
        for req in [
            base.clone().header("x-idoris-privacy", "local_only"),
            base.clone().header("X-iDoris-Privacy", " LOCAL_ONLY "),
            // 请求体里的等价声明（头说 any 也一样：取最严的）
            base.clone().body(in_body),
            base.clone()
                .header("x-idoris-privacy", "any")
                .body(in_metadata),
        ] {
            let reply = w.send(&req).await;
            assert_eq!(reply.status, 403, "{}", reply.text());
            assert_eq!(reply.code(), "local_only_refused");
            assert_eq!(reply.reason(), Some("local_only_refused"));
            assert_eq!(reply.json()["error"]["type"], "local_only_refused");
            refused += 1;
        }
    }
    // 云上游一个请求都没收到；安全检查器（对儿童租户，那同样是把正文送给检查模型）一次都没被调用；谁都没被记账
    assert_eq!(w.upstream.request_count(), 0);
    assert_eq!(w.upstream.connection_count(), 0);
    assert_eq!(counting.calls(), (0, 0));
    for account in ALL_PAYERS {
        assert_eq!(w.spent(account), 0, "{account}");
    }
    // 每次拒绝留一行已拒绝的审计记录（没有任何内容、没有尝试）
    assert_eq!(w.request_count(), refused);

    // 对照：没声明、显式 any 都照常服务；儿童租户这时才走安全管线（一次输入检查 + 一次输出检查）
    let child = Req::chat(w.cred("svc_eduai")).member("kid-1");
    assert_eq!(
        w.send(&child).await.status,
        200,
        "undeclared privacy is served"
    );
    assert_eq!(counting.calls(), (1, 1));
    let reply = w
        .send(&child.clone().header("x-idoris-privacy", "any"))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(counting.calls(), (2, 2));
    assert_eq!(w.upstream.request_count(), 2);
    let adult = Req::chat(w.cred("node_agent24")).header("x-idoris-privacy", "any");
    assert_eq!(w.send(&adult).await.status, 200);
}

#[tokio::test]
async fn a_malformed_privacy_declaration_is_refused_not_treated_as_undeclared() {
    let w = World::new(WorldOptions::plenty()).await;
    let base = Req::chat(w.cred("node_agent24"));
    for req in [
        base.clone().header("x-idoris-privacy", "bogus"),
        base.clone().header("x-idoris-privacy", ""),
        base.clone().header("x-idoris-privacy", "Any"),
        // 两个值（其中一个是 any）：不能取其中一个
        base.clone()
            .header("x-idoris-privacy", "any")
            .header("x-idoris-privacy", "local_only"),
        base.clone()
            .header("x-idoris-privacy", "any")
            .header("x-idoris-privacy", "any"),
    ] {
        let reply = w.send(&req).await;
        assert_eq!(reply.status, 400, "{}", reply.text());
        assert_eq!(reply.code(), "invalid_privacy");
    }
    assert_eq!(w.upstream.request_count(), 0);
    assert_eq!(w.spent("agent24"), 0);
}

// ---------------------------------------------------------------------------
// R2 High#4：认证之前的有界工作
// ---------------------------------------------------------------------------

/// 让匿名限流器不碍事，这样 429 一定是认证前的有界工作给的。
const ROOMY_ANONYMOUS: &str = "  anonymous: {requests_per_minute: 600000, burst: 100000}\n  per_source_anonymous: {requests_per_minute: 600000, burst: 100000}\n";

#[tokio::test]
async fn a_flood_of_random_credentials_cannot_keep_hitting_the_authentication_store() {
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().abuse(&format!(
            "{ROOMY_ANONYMOUS}  auth_failures: {{max_failures: 1000, window_secs: 60}}\n  auth_unknown: {{requests_per_minute: 60, burst: 3}}\n"
        ))),
    )
    .await;
    // 预算里有 3 次「没成功过的前缀」的查询：前缀各不相同的随机凭证，每个都查一次库，401
    for seed in 0..3 {
        let reply = w.send(&random_credential_request(&w, seed)).await;
        assert_eq!(reply.status, 401, "{seed}: {}", reply.text());
    }
    // 之后：预算用完，直接 429，**不碰库**——库被占着也立刻返回（碰了就会卡住）
    let lock = StoreLock::hold(&w);
    for seed in 100..140 {
        let reply = answered_quickly(&w, &random_credential_request(&w, seed)).await;
        assert_eq!(reply.status, 429, "{seed}");
        assert_eq!(reply.code(), "auth_capacity_limited");
        assert_eq!(reply.reason(), Some("auth_capacity_limited"));
        assert!(
            reply.header("retry-after").is_some(),
            "a throttle tells the client when to come back"
        );
    }
    lock.release();
    assert_eq!(w.upstream.request_count(), 0);
}

#[tokio::test]
async fn the_number_of_authentications_in_flight_is_bounded_and_the_overflow_is_refused_at_once() {
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().abuse(&format!(
            "{ROOMY_ANONYMOUS}  auth_failures: {{max_failures: 1000, window_secs: 60}}\n  auth_max_concurrent: 2\n  auth_unknown: {{requests_per_minute: 600000, burst: 100000}}\n"
        ))),
    )
    .await;
    assert_eq!(w.ingress.auth_budget().available_slots(), 2);
    let lock = StoreLock::hold(&w);
    // 两个认证查询进来，卡在被占着的库上，各占一个名额
    let first = w.spawn(random_credential_request(&w, 1));
    let second = w.spawn(random_credential_request(&w, 2));
    for _ in 0..200 {
        if w.ingress.auth_budget().available_slots() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(w.ingress.auth_budget().available_slots(), 0);
    // 第三个：名额满了，直接拒绝，不排队、不碰库
    let overflow = answered_quickly(&w, &random_credential_request(&w, 3)).await;
    assert_eq!(overflow.status, 429);
    assert_eq!(overflow.code(), "auth_capacity_limited");
    lock.release();
    for pending in [first, second] {
        assert_eq!(pending.await.unwrap().status, 401);
    }
    // 名额都还回来了
    assert_eq!(w.ingress.auth_budget().available_slots(), 2);
}

#[tokio::test]
async fn a_credential_that_recently_authenticated_is_not_starved_by_a_flood_of_unknown_ones() {
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().abuse(&format!(
            "{ROOMY_ANONYMOUS}  auth_failures: {{max_failures: 1000, window_secs: 60}}\n  auth_unknown: {{requests_per_minute: 60, burst: 2}}\n"
        ))),
    )
    .await;
    // eduai 后端的服务凭证第一次出现：消耗预算里的一个（它还没有成功过）；成功之后它的前缀就是「在用的」
    let legit = Req::chat(w.cred("svc_eduai")).member("kid-1");
    assert_eq!(w.send(&legit).await.status, 200);
    // 攻击者的随机凭证把剩下的预算耗光
    assert_eq!(w.send(&random_credential_request(&w, 1)).await.status, 401);
    let refused = w.send(&random_credential_request(&w, 2)).await;
    assert_eq!(refused.status, 429);
    assert_eq!(refused.code(), "auth_capacity_limited");
    // 在用的凭证照样能认证，一次又一次（学校共享出口下，别人发垃圾请求不能让这个孩子的请求被拒绝）
    for _ in 0..5 {
        let reply = w.send(&legit).await;
        assert_eq!(reply.status, 200, "{}", reply.text());
    }
    // 另一个从没出现过的前缀（假的）仍然被拦
    assert_eq!(w.send(&random_credential_request(&w, 3)).await.status, 429);
}

// ---------------------------------------------------------------------------
// R2 Medium#5：认证排了很久队，按完成那一刻的时间限流
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_authentication_that_finishes_late_is_rate_limited_at_its_completion_time() {
    // 成员级：每分钟 20 个（每 3 秒一个）、突发 1
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().limits(
            ROOMY,
            ROOMY,
            "{requests_per_minute: 20, burst: 1, max_concurrent: 10}",
        )),
    )
    .await;
    let kid = Req::chat(w.cred("mem_kid1"));
    assert_eq!(w.send(&kid).await.status, 200, "the member's only token");
    let again = w.send(&kid).await;
    assert_eq!(
        (again.status_u16(), again.header("x-idoris-limit-scope")),
        (429, Some("member"))
    );

    // 第二个请求的认证被拖住（认证存储被占着）；这期间时间过了 3 秒（令牌回来了一个）。
    // 修复之前：限流用的是请求**进来那一刻**读的时间，这个请求带着 3 秒前的时间读数去限流，被拒绝；
    // 现在：认证完成之后重新读时间，按它完成的这一刻限流。
    let lock = StoreLock::hold(&w);
    let late = w.spawn(kid.clone());
    for _ in 0..200 {
        if w.ingress.auth_budget().available_slots() < 64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        w.ingress.auth_budget().available_slots() < 64,
        "the request is waiting inside the authentication"
    );
    w.clock.advance_ms(3_000);
    lock.release();
    let reply = late.await.unwrap();
    assert_eq!(reply.status, 200, "{}", reply.text());
}

// ---------------------------------------------------------------------------
// R2 Medium#7：配置允许的请求体，不被提取器的默认上限重复拒绝
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_the_ingress_accepts_is_not_refused_again_by_the_default_2_mib_extractor_limit() {
    // 入口配置的上限是 4 MiB；请求体 2.5 MiB（超过 axum 提取器默认的 2 MiB）。agent24 是成人租户（不过儿童安全的大小上限）
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().abuse("  body_max_bytes: 4194304\n")),
    )
    .await;
    let big = "x".repeat(2_500_000);
    let body = json!({
        "model": "idoris/agent",
        "messages": [{"role": "user", "content": big}],
        "max_tokens": 100,
    });
    let reply = w
        .send(&Req::chat(w.cred("node_agent24")).body(body.clone()))
        .await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        &reply.text()[..reply.text().len().min(200)]
    );
    assert_eq!(w.upstream.request_count(), 1);

    // 入口自己的上限照旧：超过配置的 4 MiB 是 413
    let huge = json!({
        "model": "idoris/agent",
        "messages": [{"role": "user", "content": "x".repeat(4_300_000)}],
        "max_tokens": 100,
    });
    let reply = w.send(&Req::chat(w.cred("node_agent24")).body(huge)).await;
    assert_eq!(reply.status, 413);
    assert_eq!(reply.code(), "request_body_too_large");
}

// ---------------------------------------------------------------------------
// PD B3：儿童声明写在非顶层账户上
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_child_declaration_on_an_account_below_the_top_is_a_startup_error_not_a_warning() {
    let w = World::new(WorldOptions::plenty()).await;
    // 顶层客户 eduai 声明成人，它下面的学校 school-a 声明儿童：顶层客户的声明会胜出，学校 A 的孩子就按成人策略被服务
    let misdeclared = idoris_router::cloud::ingress::IngressConfig::parse(
        "schema_version: 1
tenants:
  - account: eduai
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
  - account: school-a
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    billing: subsidized
",
    )
    .unwrap();
    let authenticator = idoris_router::auth::VirtualKeyAuthenticator::new(w.keys.clone());
    let error = idoris_router::cloud::CloudIngress::new(
        misdeclared,
        w.accounts.clone(),
        authenticator.clone(),
        w.clock.clone(),
        &w.cloud,
    )
    .unwrap_err();
    assert!(
        error.contains("school-a") && error.contains("顶层客户") && error.contains("child_edu"),
        "{error}"
    );
    // 对照：声明在顶层客户上没有问题；下级账户上的**成人**声明不是儿童声明，仍然只是启动警告（不阻止启动）
    let fine = idoris_router::cloud::ingress::IngressConfig::parse(
        "schema_version: 1
tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    billing: subsidized
  - account: school-a
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
",
    )
    .unwrap();
    idoris_router::cloud::CloudIngress::new(
        fine,
        w.accounts.clone(),
        authenticator,
        w.clock.clone(),
        &w.cloud,
    )
    .unwrap();
}

/// 启动之后才建的账户：声明了 child_edu 的账户在启动时还不存在（先写配置、后建账户），所以启动检查放行；
/// 它建出来之后，它下面的孩子的请求必须在准入时被判定为儿童（走儿童安全管线），不能因为顶层客户声明成人就按成人处理。
#[tokio::test]
async fn a_child_under_an_account_created_after_start_up_is_served_as_a_child() {
    let counting = Counting::new();
    let w = World::new(WorldOptions {
        script: ok_script(10),
        safety_gate: Some(counting_gate(counting.clone())),
        ingress: IngressYaml::default().tenants(
            "tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    billing: subsidized
  - account: agent24
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
  - account: school-late
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    billing: subsidized
",
        ),
        ..WorldOptions::default()
    })
    .await;
    // 启动之后在 agent24（成人顶层客户）下面建一个学校和一个孩子的账户
    let now = ts();
    for (id, kind, parent) in [
        ("school-late", AccountKind::Org, "agent24"),
        ("late-kid", AccountKind::Member, "school-late"),
    ] {
        w.accounts
            .create_account(
                &Account {
                    id: aid(id),
                    kind,
                    parent: Some(aid(parent)),
                    unit: idoris_compute_contracts::money::Unit::Thb,
                    status: AccountStatus::Active,
                },
                now,
            )
            .unwrap();
        w.accounts
            .fund_account(
                &aid(id),
                &thb(FUNDS_MICROS),
                &idoris_compute_contracts::ids::IdempotencyKey::new(format!("fund-{id}")).unwrap(),
                now,
            )
            .unwrap();
    }
    // 绑在 school-late 的 Service 凭证（经真实的运维 bootstrap 签发）+ 孩子的主体和成员关系
    w.accounts.ensure_principal(&pid("late-1"), now).unwrap();
    w.accounts
        .create_membership(
            &idoris_compute_contracts::account::Membership {
                id: idoris_compute_contracts::ids::MembershipId::new("m-late-1").unwrap(),
                principal: pid("late-1"),
                org: aid("school-late"),
                role: idoris_compute_contracts::account::MembershipRole::Member,
                account: Some(aid("late-kid")),
                status: idoris_compute_contracts::account::MembershipStatus::Active,
            },
            now,
        )
        .unwrap();
    let secret = issue_service_credential(&w, "school-late");

    let reply = w
        .send(
            &Req::chat(&Cred {
                id: String::new(),
                secret,
            })
            .member("late-1"),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    // 儿童安全管线被走到（输入检查 + 输出检查）：顶层客户 agent24 声明成人，但付款链上的 school-late 声明了 child_edu
    assert_eq!(
        counting.calls(),
        (1, 1),
        "the request was served as a child's"
    );
    let rows = w.request_rows();
    let last = rows.last().unwrap();
    assert!(last.2.contains("child_edu"), "{last:?}");
}

/// 用真实的运维 bootstrap 给一个账户签发一张 Service 凭证，返回明文密钥。
fn issue_service_credential(w: &World, account: &str) -> String {
    struct Adapter(Arc<idoris_router::cloud::ManualClock>);
    impl idoris_router::control::ControlClock for Adapter {
        fn now(&self) -> idoris_compute_contracts::ids::TimestampMs {
            idoris_router::cloud::Clock::now(self.0.as_ref())
        }
    }
    let spec = json!({"credentials": [{
        "binding": {"kind": "service", "account": account},
        "admin_scopes": [],
        "owner": account,
        "tenant_id": account,
        "allowed_privacy": ["local_only"],
        "allowed_roles": ["fast"],
    }]});
    let mut out = Vec::new();
    idoris_router::control::bootstrap::run(
        &w.accounts,
        &mut spec.to_string().as_bytes(),
        &mut out,
        &Adapter(w.clock.clone()),
    )
    .unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["kind"] == "credential")
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// PD Low：X-iDoris-Tenant 在最外层被删
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tenant_header_is_dropped_at_the_outermost_layer_and_never_changes_who_is_served() {
    let w = World::new(WorldOptions::plenty()).await;
    // 冒用另一个租户（rival）的头：被忽略，请求按凭证推导出来的租户（agent24）处理
    for name in ["x-idoris-tenant", "X-iDoris-Tenant"] {
        let reply = w
            .send(&Req::chat(w.cred("node_agent24")).header(name, "rival"))
            .await;
        assert_eq!(reply.status, 200, "{}", reply.text());
    }
    // 重复值、空值、非法值同样被忽略（本地租户模式里它们是 400；云入口根本不读这个头）
    for value in ["", "a b", "rival, eduai"] {
        let reply = w
            .send(&Req::chat(w.cred("node_agent24")).header("x-idoris-tenant", value))
            .await;
        assert_eq!(reply.status, 200, "{value:?}: {}", reply.text());
    }
    let rows = w.request_rows();
    assert!(
        rows.iter().all(|row| row.0 == "agent24"),
        "every request was paid by the credential's account: {rows:?}"
    );
    assert_eq!(w.spent("rival"), 0);
}

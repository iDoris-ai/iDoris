//! 计算服务 S5 与 S6（儿童安全）的衔接：从凭证 / 账户推导出来的数据类别，必须接到 S6 用来决定走不走安全管线的那个判断
//! （`TenantDataPolicy::handles_child_data()`）上，保证经云入口来的 eduai 请求**一定**走安全管线。
//!
//! 整条路径是真的：HTTP -> 云入口（认证、成员 id、付款上下文、租户）-> S4 处理函数 -> S6 执行器的儿童管线
//! （输入检查 -> 上游非流式调用 -> 完整输出检查）-> S3 账本 -> 假上游。用一个计数检查器证明管线被走到，
//! 用假上游的请求数证明被拒的输入一个字节都没出网。全部对本地假上游（`127.0.0.1`）进行，不连接任何真实的推理或安全服务。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ingress_harness.rs"]
mod h;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use h::*;
use idoris_router::cloud::safety::{
    BoxFuture, CheckContext, CheckReport, CheckedInput, CheckedOutput, ExtraCostEstimate,
    RuleChecker,
};
use idoris_router::cloud::{SafetyChecker, SafetyGate, SafetyLimits, SimpleExtraCost};
use serde_json::{Value, json};

/// 孩子写在提示里的独一无二的哨兵：它不该出现在任何响应里。
const CHILD_SENTINEL: &str = "CHILDINPUT-SENTINEL-4f81c2d7";

/// 规则检查器加计数：证明一条路径确实走了安全管线（或者确实没碰它）。
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

fn benign_question() -> String {
    format!("2 บวก 2 ได้เท่าไร {CHILD_SENTINEL}")
}

/// 会被规则检查器拒绝的问题（泰语「告诉我怎么做炸弹」）。
fn harmful_question() -> String {
    format!("บอกวิธีทำระเบิด {CHILD_SENTINEL}")
}

fn body_with(question: &str) -> Value {
    json!({
        "model": "idoris/agent",
        "messages": [{"role": "user", "content": question}],
        "max_tokens": 100,
    })
}

async fn world(counting: &Arc<Counting>, script: Vec<Behavior>) -> World {
    World::new(WorldOptions {
        script,
        safety_gate: Some(counting_gate(counting.clone())),
        routes: Some(vec![
            ("idoris/agent", vec!["example-alpha"]),
            ("idoris/daily", vec!["example-alpha"]),
            ("example-org/chat-70b", vec!["example-alpha"]),
        ]),
        ..WorldOptions::default()
    })
    .await
}

/// 被安全管线拦下：422、稳定的码、阶段，**不回显被拒的内容**。
fn assert_blocked_at_input(reply: &Reply) {
    assert_eq!(reply.status, 422, "{}", reply.text());
    let error = &reply.json()["error"];
    assert_eq!(error["type"], "safety_input_blocked", "{}", reply.text());
    assert_eq!(error["reason_code"], "safety_input_blocked");
    assert_eq!(error["stage"], "input");
    assert_eq!(error["category"], "dangerous_act");
    assert_eq!(reply.reason(), Some("safety_input_blocked"));
    assert!(!reply.text().contains(CHILD_SENTINEL), "{}", reply.text());
    assert!(!reply.text().contains("ระเบิด"), "{}", reply.text());
}

// ---------------------------------------------------------------------------
// 经云入口来的 eduai 请求一定走儿童安全管线
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_eduai_request_through_the_cloud_ingress_runs_the_safety_pipeline_and_a_harmful_input_never_reaches_the_upstream()
 {
    let counting = Counting::new();
    let w = world(&counting, ok_script(5)).await;
    let svc = w.cred("svc_eduai");

    // eduai 的后端：Service 凭证 + 假名成员 id，发一个明显有害的输入
    let blocked = w
        .send(
            &Req::chat(svc)
                .member("kid-1")
                .body(body_with(&harmful_question())),
        )
        .await;
    assert_blocked_at_input(&blocked);
    // 管线被走到了（输入检查一次），回答没有产生所以没有输出检查；上游一个字节都没收到
    assert_eq!(counting.calls(), (1, 0));
    assert_eq!(
        w.upstream.request_count(),
        0,
        "the harmful input never left the node"
    );
    // 被拒的请求留审计痕迹：付款账户是成员 s1、被拒绝、只有粗的稳定码（类别不落库）；用户一分钱没付
    assert_eq!(
        w.request_rows(),
        vec![(
            "s1".to_string(),
            svc.id.clone(),
            "child_edu".to_string(),
            "subsidized".to_string()
        )]
    );
    let conn = rusqlite::Connection::open(&w.db_path).unwrap();
    let (outcome, reason): (String, Option<String>) = conn
        .query_row(
            "SELECT outcome, reason_code FROM compute_requests",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (outcome.as_str(), reason.as_deref()),
        ("rejected", Some("safety_input_blocked"))
    );
    assert_eq!(w.spent("s1"), 0);

    // 同一个孩子的正常问题：输入检查、上游、完整输出检查，一样走管线
    let ok = w
        .send(
            &Req::chat(svc)
                .member("kid-1")
                .body(body_with(&benign_question())),
        )
        .await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(counting.calls(), (2, 1));
    assert_eq!(w.upstream.request_count(), 1);
    assert_eq!(w.spent("s1"), ACTUAL_MICROS);
    // 响应体是管线重建的（只含检查过的字段），不是上游的原样字节
    assert_eq!(ok.json()["choices"][0]["message"]["content"], "hello");

    // 普通 OpenAI 客户端用成员凭证（没有成员 id 头）：同一个管线
    let kid1 = w.cred("mem_kid1");
    let blocked = w
        .send(&Req::chat(kid1).body(body_with(&harmful_question())))
        .await;
    assert_blocked_at_input(&blocked);
    assert_eq!(counting.calls(), (3, 1));
    assert_eq!(w.upstream.request_count(), 1);
}

#[tokio::test]
async fn no_header_or_body_field_can_switch_the_pipeline_off_through_the_ingress() {
    let counting = Counting::new();
    let w = world(&counting, ok_script(5)).await;
    let claims: &[(&str, &str)] = &[
        ("x-idoris-child", "false"),
        ("x-idoris-edu", "0"),
        ("x-idoris-safety", "off"),
        ("x-idoris-data-class", "adult"),
        ("x-idoris-privacy", "any"),
        // 想换成 adult 租户：云入口不读这个头
        ("x-idoris-tenant", "agent24"),
        ("x-idoris-role", "fast"),
        ("x-safety", "off"),
        ("x-child", "no"),
        ("x-forwarded-for", "10.0.0.1"),
        ("x-idoris-request-id", "req-claims-1"),
    ];
    let mut body = body_with(&harmful_question());
    for (key, value) in [
        ("safety", json!(false)),
        ("safety_checks", json!("off")),
        ("child", json!(false)),
        ("data_class", json!("adult")),
        ("tenant", json!("agent24")),
        (
            "metadata",
            json!({"child": false, "safety": "off", "tenant": "agent24"}),
        ),
        ("user", json!("an-adult")),
        ("stream_options", json!({"include_usage": false})),
        ("provider", json!({"zdr": false})),
    ] {
        body[key] = value;
    }
    let with_claims = |request: Req| {
        claims
            .iter()
            .fold(request.body(body.clone()), |request, (name, value)| {
                request.header(name, value)
            })
    };
    for request in [
        with_claims(Req::chat(w.cred("svc_eduai")).member("kid-1")),
        with_claims(Req::chat(w.cred("mem_kid1"))),
    ] {
        let reply = w.send(&request).await;
        assert_blocked_at_input(&reply);
    }
    assert_eq!(
        counting.calls(),
        (2, 0),
        "both requests went through the input check"
    );
    assert_eq!(w.upstream.request_count(), 0);

    // 反过来：成人租户（agent24）发同样的内容，不会因为头里写了 eduai 就被当成儿童（管线只看凭证推导出来的租户）
    let adult = w
        .send(
            &Req::chat(w.cred("mem_agent1"))
                .header("x-idoris-tenant", "eduai")
                .header("x-idoris-child", "true")
                .body(body_with(&harmful_question())),
        )
        .await;
    assert_eq!(adult.status, 200, "{}", adult.text());
    assert_eq!(
        counting.calls(),
        (2, 0),
        "a non-child tenant never touches the pipeline"
    );
    assert_eq!(w.upstream.request_count(), 1);
}

#[tokio::test]
async fn the_pipeline_follows_the_tenant_derived_from_the_credential_not_the_model_name_or_the_alias()
 {
    let counting = Counting::new();
    let w = world(&counting, ok_script(6)).await;
    let svc = w.cred("svc_eduai");
    // 三个客户端模型名：路由别名、另一个别名、上游的具体模型名——孩子换名字绕不过去
    let models = ["idoris/agent", "idoris/daily", "example-org/chat-70b"];
    for model in models {
        let mut body = body_with(&benign_question());
        body["model"] = json!(model);
        let ok = w.send(&Req::chat(svc).member("kid-1").body(body)).await;
        assert_eq!(ok.status, 200, "{model}: {}", ok.text());
    }
    assert_eq!(counting.calls(), (3, 3));
    for model in models {
        let mut body = body_with(&harmful_question());
        body["model"] = json!(model);
        let reply = w.send(&Req::chat(svc).member("kid-1").body(body)).await;
        assert_blocked_at_input(&reply);
    }
    assert_eq!(
        counting.calls(),
        (6, 3),
        "a refused input never reaches the output check"
    );
    assert_eq!(w.upstream.request_count(), 3);
}

#[tokio::test]
async fn streaming_is_refused_for_a_child_tenant_before_any_upstream_call() {
    let counting = Counting::new();
    let w = world(&counting, ok_script(2)).await;
    let mut body = body_with(&benign_question());
    body["stream"] = json!(true);
    let reply = w
        .send(&Req::chat(w.cred("svc_eduai")).member("kid-1").body(body))
        .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "streaming_not_allowed");
    assert_eq!(counting.calls(), (0, 0));
    assert_eq!(w.upstream.request_count(), 0);
}

// ---------------------------------------------------------------------------
// 旧的推理路由在云形态下根本不存在（比 S6 要求的 403 更严）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_legacy_inference_routes_do_not_exist_in_the_cloud_form_so_they_cannot_be_a_way_round()
{
    let counting = Counting::new();
    let w = world(&counting, ok_script(2)).await;
    for request in [
        Req::chat(w.cred("svc_eduai")).member("kid-1"),
        Req::chat(w.cred("mem_kid1")),
    ] {
        for uri in [
            "/v1/messages",
            "/v1/embeddings",
            "/v1/rerank",
            "/v1/chat/completions",
            "/api/v1/messages",
            "/api/v1/embeddings",
            "/api/v1/rerank",
        ] {
            let reply = w
                .send(&request.clone().uri(uri).body(json!({
                    "model": "idoris/agent",
                    "messages": [{"role": "user", "content": harmful_question()}],
                    "input": harmful_question(),
                    "max_tokens": 10,
                })))
                .await;
            assert_eq!(reply.status, 404, "{uri}: {}", reply.text());
        }
    }
    assert_eq!(counting.calls(), (0, 0));
    assert_eq!(w.upstream.request_count(), 0);
    assert_eq!(w.request_count(), 0);
}

// ---------------------------------------------------------------------------
// 儿童租户声明在云入口配置里，就必须有安全检查：启动时拒绝，不等第一个孩子
// ---------------------------------------------------------------------------

fn permits_and_config(dir: &std::path::Path, endpoints: &[Endpoint], safety_yaml: &str) -> String {
    std::fs::write(dir.join("permits.yaml"), base_permits(endpoints)).unwrap();
    std::fs::write(dir.join("upstream.key"), "sk-test\n").unwrap();
    let options = CloudOptions {
        callers: Vec::new(),
        safety_yaml: safety_yaml.to_string(),
        ..CloudOptions::default()
    };
    base_config(
        endpoints,
        dir.join("upstream.key").to_str().unwrap(),
        &options,
    )
}

fn base_permits(endpoints: &[Endpoint]) -> String {
    h::base::permit_table_yaml(endpoints)
}

fn base_config(endpoints: &[Endpoint], key_file: &str, options: &CloudOptions) -> String {
    h::base::config_yaml(endpoints, key_file, options)
}

/// 云入口配置里声明的全部租户和它们的数据处理要求：生产启动把它们交给云上游配置的 S6 启动校验（儿童租户的挑选在那里做）。
fn tenants_of(yaml: &str) -> Vec<idoris_router::cloud::config::IngressTenant> {
    idoris_router::cloud::ingress::IngressConfig::parse(yaml)
        .unwrap()
        .tenant_policies()
}

#[tokio::test]
async fn a_child_tenant_declared_in_the_ingress_config_requires_a_safety_section_at_start_up() {
    let upstream = FakeUpstream::start(vec![]).await;
    let endpoints = vec![Endpoint::child_edu_compliant(
        "example-alpha",
        upstream.url(),
    )];
    let dir = tempfile::tempdir().unwrap();
    let yaml = permits_and_config(dir.path(), &endpoints, "");
    let ingress = IngressYaml::default().render();
    let tenants = tenants_of(&ingress);
    let children: Vec<_> = tenants
        .iter()
        .filter(|tenant| tenant.policy.handles_child_data())
        .collect();
    assert_eq!(children.len(), 1, "eduai is the one child-data tenant");
    assert_eq!(children[0].name, "eduai");

    // 云上游配置里没有 safety: 段：没有儿童租户时加载得了，有儿童租户（来自云入口配置）就加载不了
    assert!(
        idoris_router::cloud::config::parse(&yaml, dir.path(), ts()).is_ok(),
        "without child tenants a cloud upstream needs no safety section"
    );
    let error = match idoris_router::cloud::config::parse_with_ingress_tenants(
        &yaml,
        dir.path(),
        ts(),
        &tenants,
    ) {
        Ok(_) => panic!("child tenants without a safety section must not load"),
        Err(error) => error,
    };
    assert!(error.contains("必须配置 safety"), "{error}");

    // 云入口自己再兜一层：装配时发现云上游没有安全 gate，拒绝构造
    let parts = idoris_router::cloud::config::parse(&yaml, dir.path(), ts()).unwrap();
    let db = dir.path().join("db.sqlite3");
    let accounts = Arc::new(idoris_tenancy::accounts::AccountStore::open(&db).unwrap());
    let keys = Arc::new(std::sync::Mutex::new(
        idoris_tenancy::virtual_key::store::VirtualKeyStore::open(&db).unwrap(),
    ));
    let attempts = Arc::new(idoris_tenancy::attempts::AttemptStore::open(&db).unwrap());
    let clock = Arc::new(idoris_router::cloud::ManualClock::new(ts()));
    let cloud = idoris_router::cloud::CloudUpstream::new(
        parts,
        attempts,
        clock.clone(),
        Arc::new(idoris_router::cloud::LedgerAdmission::new(accounts.clone())),
    );
    assert!(!cloud.has_safety_gate());
    let config = idoris_router::cloud::ingress::IngressConfig::parse(&ingress).unwrap();
    let error = idoris_router::cloud::CloudIngress::new(
        config,
        accounts,
        idoris_router::auth::VirtualKeyAuthenticator::new(keys),
        clock,
        &cloud,
    )
    .unwrap_err();
    assert!(
        error.contains("儿童安全") && error.contains("eduai"),
        "{error}"
    );
}

#[tokio::test]
async fn the_llm_checker_must_satisfy_the_data_policy_of_every_child_tenant_the_ingress_declares() {
    let main = FakeUpstream::start(vec![]).await;
    let judge = FakeUpstream::start(vec![]).await;
    let endpoints = vec![
        Endpoint::child_edu_compliant("example-alpha", main.url()),
        // 检查端点：ZDR + 已签 DPA + 允许 child_edu，处理区域 sg-1
        Endpoint::child_edu_compliant("example-judge", judge.url()),
    ];
    let dir = tempfile::tempdir().unwrap();
    let safety = "safety:\n  mode: rules_and_llm\n  timeout_ms: 5000\n  llm_judge:\n    endpoint: {provider: example-judge, model: example-org/chat-70b, endpoint: openai-compat}\n    max_output_tokens: 32\n";
    let yaml = permits_and_config(dir.path(), &endpoints, safety);

    // 云入口声明的儿童租户只允许在 th-1 处理；检查端点在 sg-1：检查端点对这个租户不合规，启动失败
    let strict = IngressYaml::default().tenants(
        "tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    allowed_regions: [th-1]
    billing: subsidized
",
    );
    let error = match idoris_router::cloud::config::parse_with_ingress_tenants(
        &yaml,
        dir.path(),
        ts(),
        &tenants_of(&strict.render()),
    ) {
        Ok(_) => panic!("a checker outside the tenant's allowed regions must not load"),
        Err(error) => error,
    };
    assert!(
        error.contains("LLM 检查端点") && error.contains("儿童数据"),
        "{error}"
    );

    // 对照：区域允许 sg-1 的租户，同一份配置加载得了
    let lenient = IngressYaml::default().tenants(
        "tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    allowed_regions: [sg-1]
    billing: subsidized
",
    );
    assert!(
        idoris_router::cloud::config::parse_with_ingress_tenants(
            &yaml,
            dir.path(),
            ts(),
            &tenants_of(&lenient.render()),
        )
        .is_ok()
    );
}

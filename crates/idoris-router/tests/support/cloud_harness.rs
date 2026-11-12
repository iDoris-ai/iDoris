//! 云上游（计算服务 S4）端到端测试的共用夹具：假上游（`127.0.0.1`）、端点许可表、配置、临时 SQLite、
//! 可拨的时钟、记录调用的准入替身。**不连接任何真实的推理供应商。**
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::BodyExt;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::ids::{RequestId, TimestampMs};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_router::cloud::config::{self};
use idoris_router::cloud::{
    AdmissionError, AdmissionRequest, CandidateAdmission, CloudUpstream, ExtraCostMeter,
    ManualClock, SafetyGate, SimpleAdmission,
};
use idoris_router::{AppState, build_app};
use idoris_tenancy::attempts::AttemptStore;
use serde_json::{Value, json};
use tower::ServiceExt;

/// 假的上游密钥（文件里的内容）。任何响应、错误、日志、数据库里都不该出现它。
pub const KEY: &str = "sk-test-SECRET-9f3a77c1";

/// 2026-11-12T00:00:00Z：夹具里端点的核验时间。
pub const VERIFIED_MS: i64 = 1_794_441_600_000;
pub const DAY_MS: i64 = 86_400_000;
/// 测试用的「现在」：核验后半天。
pub const NOW_MS: i64 = VERIFIED_MS + DAY_MS / 2;

/// 一个夹具端点（许可表里的一条，加上配置里的地址与价目）。
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub provider: String,
    /// 指向某个 FakeUpstream 的基址；`None` = 许可表里有、配置里没有（比如 subscription）。
    pub base_url: Option<String>,
    pub access: &'static str,
    pub retention: &'static str,
    pub dpa: &'static str,
    pub classes: Vec<&'static str>,
    pub regions: Vec<&'static str>,
    pub stream_usage: bool,
    pub tool_calling: bool,
    pub structured_output: bool,
    /// 许可表里的「取消」能力：断开连接后上游是否停止生成与计费。
    pub cancellation: bool,
    pub verified_at: &'static str,
    pub extra_body: Option<Value>,
}

impl Endpoint {
    /// 成人 / 公开数据的 API 端点，支持流式 usage、工具调用、结构化输出。
    pub fn adult(provider: &str, base_url: String) -> Self {
        Self {
            provider: provider.to_string(),
            base_url: Some(base_url),
            access: "api",
            retention: "with_exceptions",
            dpa: "unsigned",
            classes: vec!["adult", "public"],
            regions: vec!["sg-1"],
            stream_usage: true,
            tool_calling: true,
            structured_output: true,
            cancellation: true,
            verified_at: "2026-11-12",
            extra_body: None,
        }
    }

    /// 上游断开连接后不一定停止计费。
    pub fn no_cancellation(mut self) -> Self {
        self.cancellation = false;
        self
    }

    /// 允许 `child_edu` 的端点：ZDR + 已签 DPA。**今天没有任何真实端点是这样的**（S8 的核验结论），
    /// 这里的假端点只用来证明「有合规端点时」路径能走通。
    pub fn child_edu_compliant(provider: &str, base_url: String) -> Self {
        Self {
            retention: "zdr",
            dpa: "signed",
            classes: vec!["child_edu", "adult", "public"],
            ..Self::adult(provider, base_url)
        }
    }

    pub fn no_stream_usage(mut self) -> Self {
        self.stream_usage = false;
        self
    }

    pub fn subscription(provider: &str, base_url: Option<String>) -> Self {
        let mut endpoint = Self::adult(provider, String::new());
        endpoint.access = "subscription";
        endpoint.base_url = base_url;
        endpoint
    }

    pub fn verified(mut self, at: &'static str) -> Self {
        self.verified_at = at;
        self
    }

    pub fn extra_body(mut self, extra: Value) -> Self {
        self.extra_body = Some(extra);
        self
    }
}

/// 调用方（静态付款上下文）。
#[derive(Debug, Clone)]
pub struct Caller {
    pub tenant_id: Option<&'static str>,
    pub payer: &'static str,
    pub unit: &'static str,
    pub billing: &'static str,
    pub classes: Vec<&'static str>,
    pub max_retention: &'static str,
    pub require_dpa: bool,
    pub allowed_regions: Option<Vec<&'static str>>,
}

impl Caller {
    pub fn adult_default() -> Self {
        Self {
            tenant_id: None,
            payer: "acct-default",
            unit: "THB",
            billing: "paid",
            classes: vec!["adult"],
            max_retention: "with_exceptions",
            require_dpa: false,
            allowed_regions: None,
        }
    }

    pub fn child_edu() -> Self {
        Self {
            classes: vec!["child_edu"],
            max_retention: "zdr",
            require_dpa: true,
            ..Self::adult_default()
        }
    }
}

/// 儿童安全（S6）怎么装：默认读配置里的 `safety:` 段（规则检查器），S4 的 edu 测试因此照常验证它们原本要验证的东西。
pub enum SafetyOption {
    /// 配置里的 `safety:` 段（见 [`Options::safety_yaml`]）。
    FromConfig,
    /// 配置里有 `safety:` 段，但运行时**不装** gate：证明「没有配置检查器的 edu 请求一律被拒」。
    NoGate,
    /// 换成给定的 gate。
    Gate(SafetyGate),
}

pub struct Options {
    pub callers: Vec<Caller>,
    /// 配置里 `safety:` 段的 YAML（整段，含 `safety:` 这个键）。
    pub safety_yaml: String,
    pub safety: SafetyOption,
    /// 换掉安全检查成本的计量器（S3 的接缝）：测试用它观察预留与结算，或模拟额度被拒。
    pub extra_cost: Option<Arc<dyn ExtraCostMeter>>,
    pub admission: Option<Arc<dyn CandidateAdmission>>,
    /// `limits:` 段（YAML 片段，缩进两格）。
    pub limits: String,
    /// 路由：客户端模型名 -> 候选的 provider 列表；默认 `idoris/agent` -> 全部已配置的端点（按给定顺序）。
    pub routes: Option<Vec<(&'static str, Vec<&'static str>)>>,
    pub state: Option<Box<dyn FnOnce(AppState) -> AppState>>,
    /// 提前造好的时钟（测试要在准入钩子里拨它）；默认新建一个停在 [`NOW_MS`] 的。
    pub clock: Option<Arc<ManualClock>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            callers: vec![Caller::adult_default()],
            safety_yaml: "safety:\n  mode: rules_only\n".to_string(),
            safety: SafetyOption::FromConfig,
            extra_cost: None,
            admission: None,
            limits: "  total_deadline_ms: 6000\n  connect_timeout_ms: 500\n  response_head_timeout_ms: 800\n  body_timeout_ms: 2000\n  stream_idle_timeout_ms: 600\n  stream_total_timeout_ms: 10000\n  max_attempts: 3\n".to_string(),
            routes: None,
            state: None,
            clock: None,
        }
    }
}

pub struct Harness {
    pub app: Router,
    /// 直接驱动执行器（绕过 HTTP）用。
    pub cloud: Arc<CloudUpstream>,
    pub store: Arc<AttemptStore>,
    pub clock: Arc<ManualClock>,
    pub db_path: PathBuf,
    pub dir: tempfile::TempDir,
}

fn yaml_list(items: &[&str]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// 许可表 YAML。
pub fn permit_table_yaml(endpoints: &[Endpoint]) -> String {
    let mut out = String::from("schema_version: 1\nverification_max_age_days: 90\nendpoints:\n");
    for e in endpoints {
        out.push_str(&format!(
            "  - provider: {}\n    model: example-org/chat-70b\n    endpoint: openai-compat\n    access: {}\n    processing_regions: {}\n    retention: {}\n    dpa: {}\n    allowed_data_classes: {}\n    capabilities:\n      tool_calling: {}\n      stream_usage: {}\n      cache_pricing: false\n      structured_output: {}\n      cancellation: {}\n    verified_at: \"{}\"\n",
            e.provider,
            e.access,
            yaml_list(&e.regions),
            e.retention,
            e.dpa,
            yaml_list(&e.classes),
            e.tool_calling,
            e.stream_usage,
            e.structured_output,
            e.cancellation,
            e.verified_at,
        ));
    }
    out
}

/// 云上游配置 YAML。价目：成本 USD 0.26 / 0.38 每百万 token；对 THB 用户的价 10 / 40 每百万 token。
pub fn config_yaml(endpoints: &[Endpoint], key_file: &str, options: &Options) -> String {
    let mut out = format!(
        "schema_version: 1\npermit_table: permits.yaml\nlimits:\n{}reserve:\n  max_tokens_hard_cap: 1000\n  min_useful_max_tokens: 10\nendpoints:\n",
        options.limits
    );
    for e in endpoints {
        let Some(base_url) = &e.base_url else {
            continue;
        };
        out.push_str(&format!(
            "  - provider: {p}\n    model: example-org/chat-70b\n    endpoint: openai-compat\n    base_url: {base_url}\n    api_key_file: {key_file}\n    price_version: pv-{p}\n    cost: {{unit: USD, input_per_mtok: \"0.26\", output_per_mtok: \"0.38\"}}\n    prices:\n      - {{unit: THB, input_per_mtok: \"10\", output_per_mtok: \"40\"}}\n      - {{unit: USD, input_per_mtok: \"0.3\", output_per_mtok: \"0.5\"}}\n",
            p = e.provider
        ));
        if let Some(extra) = &e.extra_body {
            out.push_str(&format!("    extra_body: {extra}\n"));
        }
    }
    out.push_str("routes:\n");
    let default_route = vec![(
        "idoris/agent",
        endpoints
            .iter()
            .map(|e| e.provider.as_str())
            .collect::<Vec<_>>(),
    )];
    let routes: Vec<(&str, Vec<&str>)> = match &options.routes {
        Some(routes) => routes
            .iter()
            .map(|(model, providers)| (*model, providers.clone()))
            .collect(),
        None => default_route,
    };
    for (model, providers) in routes {
        out.push_str(&format!("  - model: {model}\n    candidates:\n"));
        for provider in providers {
            out.push_str(&format!(
                "      - {{provider: {provider}, model: example-org/chat-70b, endpoint: openai-compat}}\n"
            ));
        }
    }
    out.push_str(if options.callers.is_empty() {
        "callers: []\n"
    } else {
        "callers:\n"
    });
    for c in &options.callers {
        out.push_str(&format!(
            "  - {}credential_id: cred-{}\n    payer_account: {}\n    unit: {}\n    billing: {}\n    data_classes: {}\n    max_retention: {}\n    require_dpa: {}\n",
            c.tenant_id
                .map(|t| format!("tenant_id: {t}\n    "))
                .unwrap_or_default(),
            c.payer,
            c.payer,
            c.unit,
            c.billing,
            yaml_list(&c.classes),
            c.max_retention,
            c.require_dpa,
        ));
        if let Some(regions) = &c.allowed_regions {
            out.push_str(&format!("    allowed_regions: {}\n", yaml_list(regions)));
        }
    }
    out.push_str(&options.safety_yaml);
    out
}

pub fn build(endpoints: Vec<Endpoint>, options: Options) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("upstream.key");
    std::fs::write(&key_file, format!("{KEY}\n")).unwrap();
    std::fs::write(
        dir.path().join("permits.yaml"),
        permit_table_yaml(&endpoints),
    )
    .unwrap();
    let yaml = config_yaml(&endpoints, key_file.to_str().unwrap(), &options);
    let clock = options
        .clock
        .clone()
        .unwrap_or_else(|| Arc::new(ManualClock::new(TimestampMs(NOW_MS))));
    let mut parts = config::parse(&yaml, dir.path(), TimestampMs(NOW_MS))
        .unwrap_or_else(|e| panic!("fixture config must load: {e}\n{yaml}"));
    if matches!(options.safety, SafetyOption::NoGate) {
        parts.safety = None;
    }
    let db_path = dir.path().join("idoris.sqlite3");
    let store = Arc::new(AttemptStore::open(&db_path).unwrap());
    let admission = options
        .admission
        .clone()
        .unwrap_or_else(|| Arc::new(SimpleAdmission));
    let mut cloud = CloudUpstream::new(parts, store.clone(), clock.clone(), admission);
    if let SafetyOption::Gate(gate) = options.safety {
        cloud = cloud.with_safety(gate);
    }
    if let Some(meter) = options.extra_cost {
        cloud = cloud.with_extra_cost_meter(meter);
    }
    let cloud = Arc::new(cloud);
    let mut state = AppState {
        cloud: Some(cloud.clone()),
        ..AppState::default()
    };
    if let Some(tweak) = options.state {
        state = tweak(state);
    }
    Harness {
        app: build_app(state),
        cloud,
        store,
        clock,
        db_path,
        dir,
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

pub fn chat_request(body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 最小的合法请求：32 字节的 messages JSON -> 输入估算 11 token。
pub fn simple_body(max_tokens: u64) -> Value {
    json!({
        "model": "idoris/agent",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": max_tokens,
    })
}

pub async fn send(app: &Router, request: Request<Body>) -> Response<Body> {
    app.clone().oneshot(request).await.unwrap()
}

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

    pub fn request_id(&self) -> RequestId {
        RequestId::new(self.header("x-idoris-record-id").unwrap()).unwrap()
    }
}

pub async fn collect(response: Response<Body>) -> Reply {
    let (parts, body) = response.into_parts();
    let body = body.collect().await.map(|c| c.to_bytes().to_vec());
    Reply {
        status: parts.status,
        headers: parts.headers,
        // 流在中途被中止时，已经收到的字节仍然要给测试看
        body: body.unwrap_or_default(),
    }
}

pub async fn post(app: &Router, body: &Value) -> Reply {
    collect(send(app, chat_request(body)).await).await
}

/// SSE 响应体里的 data 事件（字符串）。
pub fn sse_data(text: &str) -> Vec<String> {
    text.split("\n\n")
        .filter_map(|event| {
            event
                .lines()
                .find_map(|l| l.strip_prefix("data: ").map(str::to_string))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 准入替身
// ---------------------------------------------------------------------------

/// 准入时执行的钩子（参数是 provider）。
pub type AdmitHook = Box<dyn Fn(&str) + Send>;

/// 记录每次准入调用（provider、序号），可以让指定 provider 被拒绝，也可以在某次调用时执行钩子（比如拨时钟）。
#[derive(Default)]
pub struct RecordingAdmission {
    pub calls: Mutex<Vec<(String, usize)>>,
    pub reject: Mutex<BTreeSet<String>>,
    pub on_admit: Mutex<Option<AdmitHook>>,
}

impl RecordingAdmission {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn rejecting(providers: &[&str]) -> Arc<Self> {
        let me = Self::default();
        me.reject
            .lock()
            .unwrap()
            .extend(providers.iter().map(|p| (*p).to_string()));
        Arc::new(me)
    }

    pub fn calls(&self) -> Vec<(String, usize)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn providers(&self) -> Vec<String> {
        self.calls().into_iter().map(|(p, _)| p).collect()
    }
}

impl CandidateAdmission for RecordingAdmission {
    fn admit(&self, request: &AdmissionRequest<'_>) -> Result<AdmissionDecision, AdmissionError> {
        let provider = request.endpoint.provider.to_string();
        self.calls
            .lock()
            .unwrap()
            .push((provider.clone(), request.candidate_ordinal));
        if let Some(hook) = self.on_admit.lock().unwrap().as_ref() {
            hook(&provider);
        }
        if self.reject.lock().unwrap().contains(&provider) {
            return AdmissionDecision::reject(ReasonCode::BudgetExhausted)
                .map_err(|e| AdmissionError::Reserve(e.to_string()));
        }
        SimpleAdmission.admit(request)
    }
}

// ---------------------------------------------------------------------------
// 读库（断言用）
// ---------------------------------------------------------------------------

pub fn rows(h: &Harness, id: &RequestId) -> Vec<idoris_tenancy::attempts::AttemptRow> {
    h.store.attempt_rows(id).unwrap()
}

/// (尝试序号, 类别, 金额, 单位) 的分录摘要，按写入顺序。
pub fn postings(h: &Harness, id: &RequestId) -> Vec<(u8, String, i64, String)> {
    h.store
        .postings(id)
        .unwrap()
        .into_iter()
        .map(|p| {
            (
                p.attempt.index().0,
                p.kind,
                p.money.micros,
                p.money.unit.to_string(),
            )
        })
        .collect()
}

pub fn p(attempt: u8, kind: &str, micros: i64, unit: &str) -> (u8, String, i64, String) {
    (attempt, kind.to_string(), micros, unit.to_string())
}

/// 库里的请求行数（用原生 SQL 数，不依赖被测的 API）。
pub fn request_count(h: &Harness) -> i64 {
    let conn = rusqlite::Connection::open(&h.db_path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM compute_requests", [], |row| {
        row.get(0)
    })
    .unwrap()
}

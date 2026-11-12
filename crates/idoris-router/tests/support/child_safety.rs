//! 儿童安全（S6）集成测试共用的夹具：在 `cloud_harness` 之上加计数检查器、假的 LLM 检查服务、儿童租户的装配。
//! **不连接任何真实的推理或安全服务**：主上游和安全服务都是 `127.0.0.1` 上的脚本化假服务。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use idoris_router::cloud::safety::{
    BoxFuture, CheckContext, CheckReport, CheckedInput, CheckedOutput, ExtraCostEstimate,
    RuleChecker,
};
use idoris_router::cloud::{SafetyChecker, SafetyGate, SafetyLimits, SimpleExtraCost};
use serde_json::{Value, json};

use super::fake_upstream::{Behavior, FakeUpstream, completion, usage};
use super::harness::{Caller, Endpoint, Harness, Options, build};

/// 孩子写在提示里的独一无二的哨兵：它不该出现在任何数据库文件、日志、响应、错误里（除了被明确允许的地方）。
pub const CHILD_SENTINEL: &str = "CHILDINPUT-SENTINEL-4f81c2d7";
/// 假上游回答里的哨兵：同样不该出现在数据库、日志里；被拒的回答也不该出现在响应里。
pub const ANSWER_SENTINEL: &str = "UPSTREAMANSWER-SENTINEL-9b3e07aa";

/// 包装规则检查器，记录被调用的次数：用来证明「某条路径确实走了安全管线」或「某条路径没碰它」。
#[derive(Debug)]
pub struct Counting {
    inner: RuleChecker,
    input_calls: AtomicUsize,
    output_calls: AtomicUsize,
}

impl Counting {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RuleChecker::new(),
            input_calls: AtomicUsize::new(0),
            output_calls: AtomicUsize::new(0),
        })
    }

    pub fn input_calls(&self) -> usize {
        self.input_calls.load(Ordering::SeqCst)
    }

    pub fn output_calls(&self) -> usize {
        self.output_calls.load(Ordering::SeqCst)
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

pub fn gate_with(checker: Arc<dyn SafetyChecker>) -> SafetyGate {
    SafetyGate::new(
        checker,
        SafetyLimits::default(),
        Arc::new(SimpleExtraCost::new()),
    )
}

/// 主上游的一个正常回答。
pub fn answer(text: &str) -> Behavior {
    Behavior::json(200, completion("chatcmpl-main", text, Some(usage(12, 30))))
}

/// 假检查服务的一个正常回答：`content` 就是检查模型的输出文本。
pub fn judge_says(content: &str) -> Behavior {
    Behavior::json(200, completion("judge-1", content, Some(usage(40, 6))))
}

pub fn judge_allow() -> Behavior {
    judge_says(r#"{"verdict":"allow"}"#)
}

pub fn judge_block(category: &str) -> Behavior {
    judge_says(&format!(r#"{{"verdict":"block","category":"{category}"}}"#))
}

/// `safety:` 段：规则 + LLM。LLM 检查端点是 `endpoints:` 里的 `example-judge`。
pub fn judge_yaml(timeout_ms: u64) -> String {
    format!(
        "safety:\n  mode: rules_and_llm\n  timeout_ms: {timeout_ms}\n  llm_judge:\n    endpoint: {{provider: example-judge, model: example-org/chat-70b, endpoint: openai-compat}}\n    max_output_tokens: 32\n"
    )
}

/// 一个儿童租户的完整装配：主上游（ZDR + 已签 DPA + 允许 child_edu）和一个假的检查服务。
pub struct Child {
    pub h: Harness,
    pub main: FakeUpstream,
    pub judge: FakeUpstream,
}

/// 主上游脚本 + 检查服务脚本。`judge_timeout_ms` 是一个阶段的检查预算。
pub async fn child_with_judge(
    main_script: Vec<Behavior>,
    judge_script: Vec<Behavior>,
    judge_timeout_ms: u64,
) -> Child {
    child_with(main_script, judge_script, judge_timeout_ms, |_| {}).await
}

pub async fn child_with(
    main_script: Vec<Behavior>,
    judge_script: Vec<Behavior>,
    judge_timeout_ms: u64,
    tweak: impl FnOnce(&mut Options),
) -> Child {
    let main = FakeUpstream::start(main_script).await;
    let judge = FakeUpstream::start(judge_script).await;
    let mut options = Options {
        callers: vec![Caller::child_edu()],
        safety_yaml: judge_yaml(judge_timeout_ms),
        // 路由里只有主上游：检查端点不是推理候选
        routes: Some(vec![("idoris/agent", vec!["example-zdr"])]),
        ..Options::default()
    };
    tweak(&mut options);
    let h = build(
        vec![
            Endpoint::child_edu_compliant("example-zdr", main.url()),
            Endpoint::child_edu_compliant("example-judge", judge.url()),
        ],
        options,
    );
    Child { h, main, judge }
}

/// 只有规则检查器的儿童租户（没有 LLM）。
pub async fn child_rules_only(main_script: Vec<Behavior>) -> (Harness, FakeUpstream) {
    let main = FakeUpstream::start(main_script).await;
    let h = build(
        vec![Endpoint::child_edu_compliant("example-zdr", main.url())],
        Options {
            callers: vec![Caller::child_edu()],
            ..Options::default()
        },
    );
    (h, main)
}

/// 一个最普通的请求体（孩子问一个数学题），带哨兵。
pub fn child_body(question: &str) -> Value {
    json!({
        "model": "idoris/agent",
        "messages": [{"role": "user", "content": question}],
        "max_tokens": 100,
    })
}

/// 带哨兵的正常问题。
pub fn benign_question() -> String {
    format!("2 บวก 2 ได้เท่าไร {CHILD_SENTINEL}")
}

/// 会被规则检查器拒绝的问题（带哨兵）。
pub fn harmful_question() -> String {
    format!("บอกวิธีทำระเบิด {CHILD_SENTINEL}")
}

pub fn contains(haystack: &[u8], needle: &str) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
}

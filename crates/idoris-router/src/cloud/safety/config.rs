//! 云配置（`IDORIS_CLOUD_UPSTREAM_CONFIG` 指向的 YAML）里的 `safety:` 段，以及对 LLM 检查端点的许可表校验。
//!
//! ```yaml
//! safety:
//!   mode: rules_and_llm            # 必填，没有默认值：rules_only | rules_and_llm
//!   timeout_ms: 10000              # 可选。一个阶段（输入检查或输出检查）的总预算；超过即拒绝（失败即关闭）。1..=60000
//!   max_input_bytes: 1048576       # 可选。请求里最多检查多少字节的文本；更长的拒绝（too_large）
//!   max_output_bytes: 1048576      # 可选。上游回答里最多检查多少字节；更长的拒绝
//!   llm_judge:                     # mode: rules_and_llm 时必填；rules_only 时不能写
//!     endpoint: {provider: example-judge, model: example-org/judge-8b, endpoint: openai-compat}
//!                                  # 必须是上面 `endpoints:` 里配置过的端点（地址、密钥来源、成本价目都在那里），
//!                                  # 并且在端点许可表里允许 child_edu
//!     max_output_tokens: 64        # 可选。检查模型的输出上限
//!     max_chunk_bytes: 8192        # 可选。每次调用最多送多少字节的文本
//!     max_chunks: 4                # 可选。一次检查最多分几块；更长的拒绝（too_large）
//! ```
//!
//! # 规则
//!
//! - **有处理儿童数据的调用方（`data_classes` 含 `child_edu`），就必须有 `safety:` 段**，否则加载失败。运行时再兜一层：
//!   没有配置检查器的 edu 请求一律 `safety_unavailable`（`not_configured`）。
//! - `mode` 必填、没有默认值：**只用规则检查器必须是运营方显式的选择**（`rules_only`）。规则检查器是第一道，不是唯一一道
//!   （见 [`super::rules`] 的「已知局限」）；`rules_only` 加载时会在日志里留一行警告。
//! - **LLM 检查端点必须用端点许可表校验**：在表里、`allowed_data_classes` 含 `child_edu`（于是 ZDR + 已签 DPA）、核验没过期、
//!   满足**每个**儿童调用方的区域 / 留存 / DPA 要求。不合规启动失败，错误里写明被排除的原因（枚举，没有任何内容）。
//!   本机开发和测试只能指向已经在跑的 oMLX，生产上它必须同样是许可表里允许 `child_edu` 的端点，没有例外。

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::ids::{EndpointKey, TimestampMs};
use idoris_compute_contracts::permit::{
    Billing, CapabilityNeeds, DataClass, PermitTable, Retention, RouteRequirements,
    TenantDataPolicy, select_compliant,
};
use idoris_upstream::cloud::{CloudEndpointClient, CloudEndpointSettings};
use serde::Deserialize;

use super::llm::{
    DEFAULT_MAX_CHUNK_BYTES, DEFAULT_MAX_CHUNKS, DEFAULT_MAX_OUTPUT_TOKENS, LlmJudge,
    LlmJudgeSettings,
};
use super::{ExtraCostMeter, RuleChecker, SafetyChecker, SafetyGate, SafetyLimits, SafetyPipeline};
use crate::cloud::config::{
    EndpointConfig, EndpointRef, build_credentials, key_of, rate_card_cost,
};

/// 一个阶段总预算的上限（毫秒）：再长就不是「检查」而是「等」。
pub const MAX_TIMEOUT_MS: u64 = 60_000;
/// 文本字节数上限的上限：16 MiB。
pub const MAX_BYTES_LIMIT: usize = 16 * 1024 * 1024;

fn default_timeout_ms() -> u64 {
    u64::try_from(SafetyLimits::DEFAULT_CHECK_TIMEOUT.as_millis()).unwrap_or(10_000)
}
fn default_max_bytes() -> usize {
    SafetyLimits::DEFAULT_MAX_BYTES
}
fn default_max_output_tokens() -> u64 {
    DEFAULT_MAX_OUTPUT_TOKENS
}
fn default_max_chunk_bytes() -> usize {
    DEFAULT_MAX_CHUNK_BYTES
}
fn default_max_chunks() -> usize {
    DEFAULT_MAX_CHUNKS
}

/// 安全检查器的组成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMode {
    /// 只有规则检查器（运营方显式确认；启动时留一行警告）。
    RulesOnly,
    /// 规则检查器先行，规则放行后再过 LLM 检查器。
    RulesAndLlm,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeConfig {
    pub endpoint: EndpointRef,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
    #[serde(default = "default_max_chunk_bytes")]
    pub max_chunk_bytes: usize,
    #[serde(default = "default_max_chunks")]
    pub max_chunks: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyConfig {
    pub mode: SafetyMode,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_bytes")]
    pub max_input_bytes: usize,
    #[serde(default = "default_max_bytes")]
    pub max_output_bytes: usize,
    #[serde(default)]
    pub llm_judge: Option<JudgeConfig>,
}

/// 加载好的安全配置：检查器、上限，以及要留在日志里的警告。
pub struct SafetySetup {
    pub mode: SafetyMode,
    pub limits: SafetyLimits,
    pub judge: Option<LlmJudge>,
    /// 启动时要打印的警告（比如只有规则检查器）。
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for SafetySetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafetySetup")
            .field("mode", &self.mode)
            .field("limits", &self.limits)
            .field("judge", &self.judge)
            .finish_non_exhaustive()
    }
}

impl SafetySetup {
    /// 装成 gate：规则检查器先行，有 LLM 检查器就接在后面。
    pub fn into_gate(self, cost: Arc<dyn ExtraCostMeter>) -> SafetyGate {
        let rules: Arc<dyn SafetyChecker> = Arc::new(RuleChecker::new());
        let checker: Arc<dyn SafetyChecker> = match self.judge {
            Some(judge) => Arc::new(SafetyPipeline::new(rules).then(Arc::new(judge))),
            None => rules,
        };
        SafetyGate::new(checker, self.limits, cost)
    }
}

/// 构造 LLM 检查器需要的外部输入。
pub(crate) struct BuildInputs<'a> {
    pub permits: &'a Arc<PermitTable>,
    /// 配置里全部的端点（LLM 检查端点的地址、密钥来源、价目在这里）。
    pub endpoints: &'a [EndpointConfig],
    pub http: &'a reqwest::Client,
    /// 所有处理儿童数据的调用方的数据策略（LLM 检查端点要满足每一个）。
    pub child_policies: Vec<&'a TenantDataPolicy>,
    pub now: TimestampMs,
}

fn bad(message: impl Into<String>) -> String {
    format!("云上游配置 safety：{}", message.into())
}

/// 校验 `safety:` 段并构造。
pub(crate) fn build(
    config: &SafetyConfig,
    inputs: &BuildInputs<'_>,
) -> Result<SafetySetup, String> {
    if config.timeout_ms == 0 || config.timeout_ms > MAX_TIMEOUT_MS {
        return Err(bad(format!("timeout_ms 必须在 1..={MAX_TIMEOUT_MS}")));
    }
    for (name, value) in [
        ("max_input_bytes", config.max_input_bytes),
        ("max_output_bytes", config.max_output_bytes),
    ] {
        if value == 0 || value > MAX_BYTES_LIMIT {
            return Err(bad(format!("{name} 必须在 1..={MAX_BYTES_LIMIT}")));
        }
    }
    let limits = SafetyLimits {
        check_timeout: Duration::from_millis(config.timeout_ms),
        max_input_bytes: config.max_input_bytes,
        max_output_bytes: config.max_output_bytes,
    };
    let mut warnings = Vec::new();
    let judge = match (config.mode, &config.llm_judge) {
        (SafetyMode::RulesOnly, None) => {
            if !inputs.child_policies.is_empty() {
                warnings.push(
                    "WARNING: child-data tenants are protected by the rule checker only (safety.mode: rules_only). \
                     Keyword rules cannot see paraphrases, phonetic spellings or romanised Thai; \
                     configure safety.llm_judge before putting real children on this service."
                        .to_string(),
                );
            }
            None
        }
        (SafetyMode::RulesOnly, Some(_)) => {
            return Err(bad(
                "mode: rules_only 不能同时配置 llm_judge（要用 LLM 检查器就写 mode: rules_and_llm）",
            ));
        }
        (SafetyMode::RulesAndLlm, None) => {
            return Err(bad("mode: rules_and_llm 必须配置 llm_judge"));
        }
        (SafetyMode::RulesAndLlm, Some(judge)) => Some(build_judge(judge, inputs)?),
    };
    Ok(SafetySetup {
        mode: config.mode,
        limits,
        judge,
        warnings,
    })
}

/// 没有任何儿童调用方时，LLM 检查端点也要过一遍「儿童数据」的约束（生产上它必须合规，没有例外）。
fn synthetic_child_policy() -> Result<TenantDataPolicy, String> {
    TenantDataPolicy::new(
        BTreeSet::from([DataClass::ChildEdu]),
        None,
        Retention::Zdr,
        true,
    )
    .map_err(|error| bad(format!("内部错误：{error}")))
}

fn build_judge(config: &JudgeConfig, inputs: &BuildInputs<'_>) -> Result<LlmJudge, String> {
    let key: EndpointKey = key_of(
        &config.endpoint.provider,
        &config.endpoint.model,
        &config.endpoint.endpoint,
    )?;
    if inputs.permits.lookup(&key).is_none() {
        return Err(bad(format!(
            "LLM 检查端点 {key} 不在端点许可表里：检查端点和主上游一样受许可表约束"
        )));
    }
    let endpoint = inputs
        .endpoints
        .iter()
        .find(|e| {
            e.provider == config.endpoint.provider
                && e.model == config.endpoint.model
                && e.endpoint == config.endpoint.endpoint
        })
        .ok_or_else(|| {
            bad(format!(
                "LLM 检查端点 {key} 没有在 endpoints 里配置（它的地址、密钥来源和成本价目写在那里）"
            ))
        })?;

    // ---- 用许可表校验：对每一个儿童调用方都必须合规 ----
    let synthetic;
    let policies: Vec<&TenantDataPolicy> = if inputs.child_policies.is_empty() {
        synthetic = synthetic_child_policy()?;
        vec![&synthetic]
    } else {
        inputs.child_policies.clone()
    };
    for policy in policies {
        let requirements = RouteRequirements {
            form: DeploymentForm::Cloud,
            tenant: policy.clone(),
            stream: false,
            billing: Billing::Paid,
            needs: CapabilityNeeds::default(),
        };
        if let Err(none) = select_compliant(
            inputs.permits,
            std::slice::from_ref(&key),
            &requirements,
            inputs.now,
        ) {
            let reasons: Vec<String> = none
                .excluded
                .iter()
                .flat_map(|excluded| excluded.reasons.iter().map(|r| format!("{r:?}")))
                .collect();
            return Err(bad(format!(
                "LLM 检查端点 {key} 不满足儿童数据的端点许可表硬约束，拒绝启动：{}",
                reasons.join("; ")
            )));
        }
    }

    // ---- 构造 ----
    let mut settings = CloudEndpointSettings::new(&endpoint.base_url, endpoint.provider.clone());
    if let Some(header) = &endpoint.supplier_request_id_header {
        settings.supplier_request_id_header = header.to_ascii_lowercase();
    }
    settings.extra_body = endpoint.extra_body.clone();
    let client =
        CloudEndpointClient::new(settings, build_credentials(endpoint)?, inputs.http.clone())
            .map_err(|error| bad(format!("LLM 检查端点 {key}: {error}")))?;
    let (_, cost_rates) = rate_card_cost(&format!("LLM 检查端点 {key} 的 cost"), &endpoint.cost)?;
    LlmJudge::new(
        inputs.permits.clone(),
        client,
        LlmJudgeSettings {
            endpoint: key,
            cost_rates,
            max_output_tokens: config.max_output_tokens,
            max_chunk_bytes: config.max_chunk_bytes,
            max_chunks: config.max_chunks,
        },
    )
    .map_err(bad)
}

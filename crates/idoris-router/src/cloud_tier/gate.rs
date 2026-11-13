//! 云端档的出网闸门：**唯一**能让请求离开本机去云端的地方。
//!
//! 这里每一条都是「数据第一次离开用户的机器」的前提，全部满足才放行，任何一条不满足就**一字不改地**交还本机已经算出来的
//! 响应（调用方看到的和没有云端档时逐字节相同）。闸门分两层，**两层都要过**：
//!
//! 1. **纯检查**（[`check`]）：请求自己的事实，不依赖任何配置——有效隐私类别必须是 `any`；没有被内容检查钉成本地；
//!    远程熔断没开；不是免 key 开发模式；调用方显式同意（[`HEADER_CONSENT`]）；不是流式；本机的失败属于「本机服务不了」
//!    （[`LocalFailure::is_unavailability`]）；这个请求有对应的云端模型名。
//! 2. **既有的路由策略和隐私闸门**（`CloudTier::select`）：把云端档合成的组件卡（`tier: remote`、`privacy_class: any`、
//!    `locality: remote`）交给**现有的**路由策略（`dispatch::policy_cards`：`tiers` 里必须有 `remote`、`fail_closed`
//!    必须不是真）和决策管道（`select_with_gate_snapshot_and_completion`：`local_only` 只收紧不放宽，只有可信本地候选
//!    过得了），不另造一套。第 1 层的隐私检查和这一层是两道独立的门，对 `local_only` 各自单独就能挡住。
//!
//! 两层都通过才会铸出 [`EgressPermit`]；它的字段是私有的，只有这个模块里的 `grant` 能造它，而真正发请求的
//! `forward::send` 要求按值传入一张。`grant` 和 `send` 都是 `pub(super)`：只有 `cloud_tier` 模块树里的代码能调，
//! crate 外的代码既造不出凭条、也调不到 `send`（它们文档里的 `compile_fail` 文档测试钉住这一点：谁把可见性改回 `pub`，
//! 测试就失败）。
//!
//! **这条保证有边界，别说过头**：类型系统保证的是「出网必须经过 `grant`」，**不**保证「交给 `grant` 的事实是真的」——
//! `grant` 的输入（有效隐私类别、同意、熔断……）由调用方填。crate 内非测试的调用点目前只有 `cloud_tier::fallback` 一处，
//! 它从请求里取真实的事实；这一处的正确性靠 `tests/cloud_tier_privacy.rs` 的集成测试（`local_only` 永不出网、没有同意不出网、
//! 远程熔断……）兜底，不是类型层面的保证。以前这里写的是「类型层面就没有绕过闸门去发请求的路」，而 `grant` 当时是 `pub`——
//! 说过头了（PR-Daemon 的评审指出过）。
//!
//! # 「本机服务不了」是什么（[`LocalFailure::is_unavailability`]）
//!
//! | 本机的结果 | 触发云端档？ | 为什么 |
//! |---|---|---|
//! | 路由策略过滤之后没有任何候选 | 是 | 本机没有能接这个请求的组件 |
//! | `NoEligibleCandidate`（角色 / 能力没有候选、全部被熔断排除） | 是 | 本机没有能接的模型 |
//! | `NoEligibleCandidate`（价格未知） | 否 | 配置错误，该报给运维，不是容量问题 |
//! | `BudgetExceeded`（402） | **否** | 预算是商业约束，不是技术性失败；换去云端花的是另一份钱，替用户做这个决定不对 |
//! | `UnknownRole`（400） | 否 | 请求本身有问题 |
//! | Supervisor：`SupervisorUnavailable`、`Upstream`、`ModelUnavailable`、`ProbeTimedOut`、`AdapterTimedOut`、`AdapterPanicked`、`LoadUnconfirmed`、`LoadPostconditionFailed` | 是 | 本机运行时不在线或起不来 |
//! | Supervisor：`EvictionImpossible`、`Oom` | 是 | 本机放不下（内存额度、驱逐后也不够） |
//! | Supervisor：`HeadroomConfirmationRequired` | **否** | 本机**放得下**，只是要问用户是否动用保留余量（规范 §2.8）；替用户选「出网」就绕过了这次询问 |
//! | Supervisor：`Busy` | **否** | 瞬时忙，规范要求有限退避后用同一个请求头重试，不要询问用户，更不要出网 |
//! | Supervisor：`ModelLoading`、`EvictionFailed` | 否 | 瞬时状态，重试可能就好 |
//! | Supervisor：`ModelNotFound` | 否 | 调用方点名要一个本机没有的模型；换成云端模型是换了他要的东西 |
//! | Supervisor：`Cancelled`、`InvalidRequest` | 否 | 调用方自己的事 |
//! | Supervisor：`InvariantViolation`、`LockPoisoned`、`Internal` | 否 | 本机自己的 bug，要让它显形，不能用云端盖住 |
//!
//! `BackendError` 的匹配没有通配分支：以后加新的变体，这里编译不过，逼着有人决定它属于哪一类。

use axum::http::HeaderMap;
use idoris_backend::BackendError;
use idoris_contracts::common::PrivacyClass;
use idoris_policy::{Rejection, Stage};

use crate::dispatch::Selected;

/// 调用方显式同意这次请求在本机服务不了时去云端的请求头（只认这一个值，ASCII 不分大小写，且只能出现一次）。
///
/// 不是 `X-iDoris-Privacy: any`（那只说「这份数据允许离开本机」，是策略层的前提，不是对「这次要不要去云端」的选择），
/// 也不是 `X-iDoris-Fallback: next_in_chain`（那是预算 / 角色降级的声明），更不是 `X-iDoris-Headroom-Breach: confirmed`
/// （那是「同意动用保留的内存余量」，和出网毫无关系）。数据第一次离开用户的机器，必须有它自己的、显式的同意。
pub const HEADER_CONSENT: &str = "x-idoris-cloud-fallback";

/// 同意头的取值。
pub const CONSENT_VALUE: &str = "allow";

/// 调用方有没有给出同意。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    Given,
    Absent,
}

/// 读同意头：恰好一个值、恰好是 `allow`（ASCII 不分大小写）才算同意。重复的头有歧义，不算同意；空值、别的值、非 UTF-8 都不算。
/// 同意只对这一个请求有效，不会被记住。
pub fn consent(headers: &HeaderMap) -> Consent {
    let mut values = headers.get_all(HEADER_CONSENT).iter();
    match (values.next(), values.next()) {
        (Some(value), None)
            if value
                .as_bytes()
                .eq_ignore_ascii_case(CONSENT_VALUE.as_bytes()) =>
        {
            Consent::Given
        }
        _ => Consent::Absent,
    }
}

/// 内容检查（闸一）和远程熔断之后的隐私事实。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivacyFacts {
    /// 内容检查写回之后的有效隐私类别（`parsed.task.privacy`）。`None` = 没声明 = `local_only`（保守默认）。
    pub effective: Option<PrivacyClass>,
    /// 内容检查把这个请求钉成了本地（声明 `any` 被收紧、扫描失败、不可检查的内容）。
    pub pinned_local_by_content: bool,
    /// 远程熔断开着：所有请求按 `local_only` 处理。
    pub kill_switch: bool,
}

/// 本机已经算出来的失败。借用，不拥有。
#[derive(Debug, Clone, Copy)]
pub enum LocalFailure<'a> {
    /// 路由策略过滤之后没有任何候选（`policy_cards` 为空）。
    NoCandidate,
    /// 选路阶段被拒。
    Rejection(&'a Rejection),
    /// 选定了本机运行时，但执行失败。
    Backend(&'a BackendError),
}

impl LocalFailure<'_> {
    /// 这个失败是不是「本机服务不了」（见模块文档的表）。**没有通配分支**。
    pub fn is_unavailability(&self) -> bool {
        match self {
            LocalFailure::NoCandidate => true,
            LocalFailure::Rejection(rejection) => match rejection {
                Rejection::LocalOnlyUnavailable => true,
                Rejection::NoEligibleCandidate { stage } => match stage {
                    Stage::RoleCapability | Stage::Admission => true,
                    Stage::Pricing => false,
                },
                Rejection::BudgetExceeded { .. } | Rejection::UnknownRole { .. } => false,
            },
            LocalFailure::Backend(error) => match error {
                BackendError::SupervisorUnavailable
                | BackendError::Upstream { .. }
                | BackendError::ModelUnavailable { .. }
                | BackendError::ProbeTimedOut { .. }
                | BackendError::AdapterTimedOut { .. }
                | BackendError::AdapterPanicked { .. }
                | BackendError::LoadUnconfirmed { .. }
                | BackendError::LoadPostconditionFailed { .. }
                | BackendError::EvictionImpossible { .. }
                | BackendError::Oom { .. } => true,
                BackendError::HeadroomConfirmationRequired { .. }
                | BackendError::Busy { .. }
                | BackendError::ModelLoading { .. }
                | BackendError::EvictionFailed { .. }
                | BackendError::ModelNotFound { .. }
                | BackendError::Cancelled
                | BackendError::InvalidRequest { .. }
                | BackendError::InvariantViolation { .. }
                | BackendError::LockPoisoned { .. }
                | BackendError::Internal { .. } => false,
            },
        }
    }
}

/// 闸门的纯检查需要的全部事实。
#[derive(Debug, Clone, Copy)]
pub struct GateInput<'a> {
    pub privacy: PrivacyFacts,
    /// 这是一个没带 key 的免密钥开发请求（只允许免费 loopback `local_only`）。
    pub dev_no_key: bool,
    pub consent: Consent,
    pub stream: bool,
    pub failure: LocalFailure<'a>,
    /// 这个请求要发给云端的模型名（按角色或默认映射；没有映射就是 `None`）。
    pub cloud_model: Option<&'a str>,
}

/// 闸门拒绝的原因。只给日志和测试看，**不会**出现在任何响应里（拒绝 = 原样交还本机的响应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// 有效隐私不是 `any`（`local_only` 或没声明）。
    PrivacyNotAny,
    /// 内容检查把这个请求钉成了本地。
    PinnedByContent,
    /// 远程熔断开着。
    KillSwitch,
    /// 免 key 开发模式。
    DevNoKey,
    /// 调用方没有显式同意。
    NoConsent,
    /// 流式请求不走云端档。
    Streaming,
    /// 本机的失败不属于「本机服务不了」。
    LocalFailureNotEligible,
    /// 这个请求没有对应的云端模型名。
    NoCloudModel,
    /// 路由策略不允许这个请求用 `remote`。
    PolicyForbidsRemote,
    /// 路由策略命中的动作是 `fail_closed`。
    PolicyFailClosed,
    /// 既有的决策管道拒绝了云端档这张卡。
    PipelineRefused,
}

impl Denial {
    pub fn as_str(self) -> &'static str {
        match self {
            Denial::PrivacyNotAny => "privacy_not_any",
            Denial::PinnedByContent => "pinned_by_content",
            Denial::KillSwitch => "kill_switch",
            Denial::DevNoKey => "dev_no_key",
            Denial::NoConsent => "no_consent",
            Denial::Streaming => "streaming",
            Denial::LocalFailureNotEligible => "local_failure_not_eligible",
            Denial::NoCloudModel => "no_cloud_model",
            Denial::PolicyForbidsRemote => "policy_forbids_remote",
            Denial::PolicyFailClosed => "policy_fail_closed",
            Denial::PipelineRefused => "pipeline_refused",
        }
    }
}

/// 第 1 层：纯检查。顺序是「最根本的在前」：隐私先于一切。
pub fn check(input: &GateInput<'_>) -> Result<(), Denial> {
    // 没声明 = local_only；只有明确是 any 才继续。
    if input.privacy.effective != Some(PrivacyClass::Any) {
        return Err(Denial::PrivacyNotAny);
    }
    if input.privacy.pinned_local_by_content {
        return Err(Denial::PinnedByContent);
    }
    if input.privacy.kill_switch {
        return Err(Denial::KillSwitch);
    }
    if input.dev_no_key {
        return Err(Denial::DevNoKey);
    }
    if input.consent != Consent::Given {
        return Err(Denial::NoConsent);
    }
    if input.stream {
        return Err(Denial::Streaming);
    }
    if !input.failure.is_unavailability() {
        return Err(Denial::LocalFailureNotEligible);
    }
    if input.cloud_model.is_none() {
        return Err(Denial::NoCloudModel);
    }
    Ok(())
}

/// 通过了闸门的凭条。字段私有：只有 `grant` 能造；`forward::send` 要求按值传入一张（用一次就没了）。
#[derive(Debug)]
pub struct EgressPermit {
    _private: (),
}

/// 两层闸门：先纯检查，再跑既有的路由策略 + 决策管道（`select`，第 2 层）。两层都通过才铸出凭条，
/// 并带着第 2 层选出来的 [`Selected`]（原因码、落点）。
///
/// # 可见性是这条保证的一部分
///
/// 只有 `cloud_tier` 模块树里的代码能调它（`pub(super)`）：crate 外的代码既造不出凭条，也调不到
/// [`forward::send`](super::forward)。下面的文档测试钉住这一点——谁把它改回 `pub`，`compile_fail` 那一条就会失败：
///
/// ```compile_fail
/// use idoris_router::cloud_tier::gate::{self, GateInput};
/// fn mint(input: &GateInput<'_>) {
///     let _ = gate::grant(input, || unreachable!());
/// }
/// ```
///
/// 对照：同一条 `use`、同一个类型，只要不去碰 `grant`，就能编译（所以上面那条是因为 `grant` 不可见才编译失败，不是路径写错了）：
///
/// ```
/// use idoris_router::cloud_tier::gate::{self, GateInput};
/// fn only_the_pure_check_is_public(input: &GateInput<'_>) -> bool {
///     gate::check(input).is_ok()
/// }
/// ```
pub(super) fn grant<'a>(
    input: &GateInput<'a>,
    select: impl FnOnce() -> Result<Selected, Denial>,
) -> Result<(EgressPermit, Selected), Denial> {
    check(input)?;
    let selected = select()?;
    Ok((EgressPermit { _private: () }, selected))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use axum::http::HeaderValue;
    use idoris_contracts::provider::Locality;

    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(HEADER_CONSENT, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    // ---------------------------------------------------------------------------------------
    // 同意头
    // ---------------------------------------------------------------------------------------

    #[test]
    fn only_a_single_allow_is_consent() {
        assert_eq!(consent(&HeaderMap::new()), Consent::Absent);
        for yes in ["allow", "ALLOW", "Allow", "aLlOw"] {
            assert_eq!(consent(&headers(&[yes])), Consent::Given, "{yes}");
        }
        for no in [
            "",
            "allowed",
            "true",
            "1",
            "yes",
            "on",
            "confirmed",
            "any",
            "next_in_chain",
            "allow,allow",
            "allow ",
            " allow",
            "deny",
        ] {
            assert_eq!(consent(&headers(&[no])), Consent::Absent, "{no:?}");
        }
        // 重复的头有歧义：不算同意，即使两个都是 allow
        assert_eq!(consent(&headers(&["allow", "allow"])), Consent::Absent);
        assert_eq!(consent(&headers(&["allow", "deny"])), Consent::Absent);
        // 非 UTF-8 的值
        let mut raw = HeaderMap::new();
        raw.append(
            HEADER_CONSENT,
            HeaderValue::from_bytes(b"allow\xff").unwrap(),
        );
        assert_eq!(consent(&raw), Consent::Absent);
    }

    #[test]
    fn the_other_headers_that_look_related_are_not_consent() {
        // 余量确认、降级声明、隐私声明都不是出网同意
        for (name, value) in [
            ("x-idoris-headroom-breach", "confirmed"),
            ("x-idoris-fallback", "next_in_chain"),
            ("x-idoris-privacy", "any"),
            ("x-idoris-cloud-tier", "allow"),
        ] {
            let mut map = HeaderMap::new();
            map.append(name, HeaderValue::from_static(value));
            assert_eq!(consent(&map), Consent::Absent, "{name}");
        }
    }

    // ---------------------------------------------------------------------------------------
    // 本机失败的分类：逐个变体钉死
    // ---------------------------------------------------------------------------------------

    fn backend_table() -> Vec<(BackendError, bool)> {
        vec![
            (BackendError::SupervisorUnavailable, true),
            (
                BackendError::Upstream {
                    message: "m".into(),
                },
                true,
            ),
            (BackendError::model_unavailable("m"), true),
            (BackendError::probe_timed_out("m"), true),
            (BackendError::adapter_timed_out("m"), true),
            (BackendError::adapter_panicked("m", "p"), true),
            (BackendError::load_unconfirmed("m", "x"), true),
            (BackendError::load_postcondition_failed("m", "x"), true),
            (BackendError::eviction_impossible("m"), true),
            (BackendError::oom("m"), true),
            // 要问用户：不是「服务不了」
            (BackendError::headroom_confirmation_required("m"), false),
            // 瞬时忙：规范要求退避重试
            (
                BackendError::Busy {
                    reason: "r".into(),
                    active_id: None,
                    retry_after_ms: None,
                },
                false,
            ),
            (BackendError::model_loading("m"), false),
            (BackendError::eviction_failed("m"), false),
            (BackendError::model_not_found("m"), false),
            (BackendError::Cancelled, false),
            (
                BackendError::InvalidRequest {
                    message: "m".into(),
                },
                false,
            ),
            (
                BackendError::InvariantViolation {
                    message: "m".into(),
                },
                false,
            ),
            (
                BackendError::LockPoisoned {
                    message: "m".into(),
                },
                false,
            ),
            (
                BackendError::Internal {
                    message: "m".into(),
                },
                false,
            ),
        ]
    }

    #[test]
    fn every_backend_error_is_classified_and_the_ones_that_ask_the_user_or_retry_never_leave() {
        for (error, expected) in backend_table() {
            assert_eq!(
                LocalFailure::Backend(&error).is_unavailability(),
                expected,
                "{}",
                error.reason_code()
            );
        }
        // 表要覆盖全部 20 个变体（BackendError 的 reason_code 各不相同）
        let codes: std::collections::BTreeSet<&str> = backend_table()
            .iter()
            .map(|(error, _)| error.reason_code())
            .collect();
        assert_eq!(codes.len(), 20, "{codes:?}");
    }

    #[test]
    fn selection_rejections_are_classified() {
        let budget = Rejection::BudgetExceeded {
            balance_minor: 0,
            estimated_cost_minor: 5,
            topup_hint: "x".into(),
        };
        let cases: Vec<(Rejection, bool)> = vec![
            (Rejection::LocalOnlyUnavailable, true),
            (
                Rejection::NoEligibleCandidate {
                    stage: Stage::RoleCapability,
                },
                true,
            ),
            (
                Rejection::NoEligibleCandidate {
                    stage: Stage::Admission,
                },
                true,
            ),
            (
                Rejection::NoEligibleCandidate {
                    stage: Stage::Pricing,
                },
                false,
            ),
            // 预算：商业约束，不降级
            (budget, false),
            (
                Rejection::UnknownRole {
                    requested: "x".into(),
                },
                false,
            ),
        ];
        for (rejection, expected) in cases {
            assert_eq!(
                LocalFailure::Rejection(&rejection).is_unavailability(),
                expected,
                "{rejection:?}"
            );
        }
        assert!(LocalFailure::NoCandidate.is_unavailability());
    }

    // ---------------------------------------------------------------------------------------
    // 闸门：只有「全部满足」的那一行放行，任何一项单独翻转都拒绝
    // ---------------------------------------------------------------------------------------

    fn allowed_input<'a>(failure: LocalFailure<'a>) -> GateInput<'a> {
        GateInput {
            privacy: PrivacyFacts {
                effective: Some(PrivacyClass::Any),
                pinned_local_by_content: false,
                kill_switch: false,
            },
            dev_no_key: false,
            consent: Consent::Given,
            stream: false,
            failure,
            cloud_model: Some("idoris/agent"),
        }
    }

    #[test]
    fn the_gate_passes_only_when_every_condition_holds_and_each_single_flip_denies() {
        let error = BackendError::SupervisorUnavailable;
        let base = allowed_input(LocalFailure::Backend(&error));
        assert_eq!(check(&base), Ok(()));

        type Flip = (&'static str, Box<dyn Fn(&mut GateInput<'_>)>, Denial);
        let flips: Vec<Flip> = vec![
            (
                "declared local_only",
                Box::new(|i| i.privacy.effective = Some(PrivacyClass::LocalOnly)),
                Denial::PrivacyNotAny,
            ),
            (
                "undeclared (defaults to local_only)",
                Box::new(|i| i.privacy.effective = None),
                Denial::PrivacyNotAny,
            ),
            (
                "content gate pinned it local",
                Box::new(|i| i.privacy.pinned_local_by_content = true),
                Denial::PinnedByContent,
            ),
            (
                "kill switch",
                Box::new(|i| i.privacy.kill_switch = true),
                Denial::KillSwitch,
            ),
            (
                "dev no-key",
                Box::new(|i| i.dev_no_key = true),
                Denial::DevNoKey,
            ),
            (
                "no consent",
                Box::new(|i| i.consent = Consent::Absent),
                Denial::NoConsent,
            ),
            (
                "streaming",
                Box::new(|i| i.stream = true),
                Denial::Streaming,
            ),
            (
                "no cloud model",
                Box::new(|i| i.cloud_model = None),
                Denial::NoCloudModel,
            ),
        ];
        for (name, flip, expected) in flips {
            let mut input = base;
            flip(&mut input);
            assert_eq!(check(&input), Err(expected), "{name}");
        }
        // 本机的失败不属于「服务不了」
        for (error, eligible) in backend_table() {
            let input = allowed_input(LocalFailure::Backend(&error));
            assert_eq!(
                check(&input),
                if eligible {
                    Ok(())
                } else {
                    Err(Denial::LocalFailureNotEligible)
                },
                "{}",
                error.reason_code()
            );
        }
    }

    #[test]
    fn local_only_is_denied_by_the_privacy_check_whatever_else_is_true() {
        // 对 local_only（以及没声明）：不管同意、不管本机怎么失败、不管有没有云端模型，一律拒绝。
        for effective in [Some(PrivacyClass::LocalOnly), None] {
            for consent_value in [Consent::Given, Consent::Absent] {
                for (error, _) in backend_table() {
                    for stream in [false, true] {
                        for dev_no_key in [false, true] {
                            for (pinned, kill) in
                                [(false, false), (true, false), (false, true), (true, true)]
                            {
                                let input = GateInput {
                                    privacy: PrivacyFacts {
                                        effective,
                                        pinned_local_by_content: pinned,
                                        kill_switch: kill,
                                    },
                                    dev_no_key,
                                    consent: consent_value,
                                    stream,
                                    failure: LocalFailure::Backend(&error),
                                    cloud_model: Some("idoris/agent"),
                                };
                                assert_eq!(check(&input), Err(Denial::PrivacyNotAny));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_permit_needs_both_layers_and_the_policy_layer_is_not_consulted_when_the_first_denies() {
        let error = BackendError::SupervisorUnavailable;
        let input = allowed_input(LocalFailure::Backend(&error));
        let mut consulted = false;
        let mut denied = input;
        denied.consent = Consent::Absent;
        let result = grant(&denied, || {
            consulted = true;
            Err(Denial::PipelineRefused)
        });
        assert_eq!(result.unwrap_err(), Denial::NoConsent);
        assert!(!consulted);
        // 第 1 层通过、第 2 层拒绝：仍然没有凭条
        assert_eq!(
            grant(&input, || Err(Denial::PolicyForbidsRemote)).unwrap_err(),
            Denial::PolicyForbidsRemote
        );
        // 两层都通过：有凭条，带着第 2 层的选择结果
        let selected = Selected {
            decision: idoris_policy::Decision {
                chosen_id: "idoris-cloud".into(),
                reason_codes: Vec::new(),
                degradations: Vec::new(),
            },
            served_locality: Locality::Remote,
            card: serde_yaml::from_str(include_str!(
                "../../../../config/components/subscription.yaml"
            ))
            .unwrap(),
            load_policy: idoris_contracts::load_policy::LoadPolicy {
                mode: idoris_contracts::load_policy::LoadMode::Resident,
                keepalive: idoris_contracts::load_policy::Keepalive::Pinned { pinned: true },
                admission: idoris_contracts::load_policy::Admission::Coexist,
            },
            estimated_cost_minor: 0,
        };
        let (_permit, chosen) = grant(&input, || Ok(selected)).unwrap();
        assert_eq!(chosen.decision.chosen_id, "idoris-cloud");
    }

    #[test]
    fn denial_strings_are_distinct_and_stable() {
        let all = [
            (Denial::PrivacyNotAny, "privacy_not_any"),
            (Denial::PinnedByContent, "pinned_by_content"),
            (Denial::KillSwitch, "kill_switch"),
            (Denial::DevNoKey, "dev_no_key"),
            (Denial::NoConsent, "no_consent"),
            (Denial::Streaming, "streaming"),
            (
                Denial::LocalFailureNotEligible,
                "local_failure_not_eligible",
            ),
            (Denial::NoCloudModel, "no_cloud_model"),
            (Denial::PolicyForbidsRemote, "policy_forbids_remote"),
            (Denial::PolicyFailClosed, "policy_fail_closed"),
            (Denial::PipelineRefused, "pipeline_refused"),
        ];
        let distinct: std::collections::BTreeSet<&str> =
            all.iter().map(|(_, text)| *text).collect();
        assert_eq!(distinct.len(), all.len());
        for (denial, text) in all {
            assert_eq!(denial.as_str(), text);
        }
    }
}

//! 尝试（attempt）状态机（设计稿 §6，评审 B3）。
//!
//! ```text
//! admitted ─▶ attempt[i]: sending ─▶ maybe_executed ─▶ executed(usage 已知 | usage 未知) ─▶ settled
//!                              └─▶ not_sent（可以安全地换下一个候选）
//! ```
//!
//! 一次用户请求可以有多次**尝试**，每次尝试是一台独立的状态机，各自记录供应商、模型、价格版本、
//! usage、执行确定性、预留（见 [`crate::request::Attempt`]）。本模块只管**单次尝试**的合法转移；
//! 跨尝试的规则（重新准入、首字节后禁止切换、用户只付一次）在 [`crate::request`]。
//!
//! # 状态
//!
//! | 状态 | 含义 | 终态？ |
//! |---|---|---|
//! | `admitted` | 这个候选已经准入（预留已占），还没开始发 | 否 |
//! | `sending` | 正在交给上游客户端，**还没有任何字节离开本机**（此时失败是安全的） | 否 |
//! | `maybe_executed` | 请求字节已经开始发出，上游**可能**已经执行；成本按预留额**保留待核销**，不能释放 | 否 |
//! | `executed_usage_known` | 确认上游执行了，usage 已知 | 否 |
//! | `executed_usage_unknown` | 确认上游执行了，usage 未知（比如流中途断了）→ 按预留结算 | 否 |
//! | `settled` | 这次尝试的账已经落定 | 是 |
//! | `not_sent` | 证明上游没有执行（可以安全换下一个候选）；预留释放，没有成本 | 是 |
//!
//! # 事件与转移表（每个格子都有测试，见 `tests`）
//!
//! | 事件 | 合法的来源状态 → 目标 |
//! |---|---|
//! | `dispatch` | `admitted` → `sending` |
//! | `never_left_host` | `admitted` / `sending` → `not_sent`（连接失败、断路器打开、客户端取消等，没有字节离开本机） |
//! | `wire_write_started` | `sending` → `maybe_executed`（**先记这个状态再写字节**，崩溃后账上永远是保守的） |
//! | `upstream_refused{http_status}` | `sending` / `maybe_executed` → `not_sent`，**仅当** `http_status` 在 400..=499 |
//! | `usage_reported{usage}` | `maybe_executed` / `executed_usage_unknown` → `executed_usage_known` |
//! | `execution_observed_usage_unknown` | `maybe_executed` → `executed_usage_unknown` |
//! | `settle` | `executed_usage_known` → `settled`（按实际用量）；`executed_usage_unknown` → `settled`（按预留额） |
//!
//! 其它所有（状态，事件）组合都是非法转移，返回 [`TransitionError`]。终态不接受任何事件。
//!
//! # 关于 `maybe_executed` 的保留规则
//!
//! `maybe_executed` 的尝试**不会**因为超时、取消、换了下一个候选而变化：它们不是事件。能让它离开
//! `maybe_executed` 的只有两种证据：(a) 上游明确拒绝（`upstream_refused`，只认 4xx；5xx/超时/连接中断
//! 都**不能**证明没执行），(b) 执行被确认（`usage_reported` / `execution_observed_usage_unknown`）。
//! 供应商账单核对出「其实没执行」，用 `usage_reported` 上报全零用量 [`Usage::ZERO`] 即可：它走
//! `executed_usage_known → settled`，账上是 0。
//!
//! # 流式的 usage
//!
//! 流式时 `maybe_executed` 一直持续到流结束：流里带了最终 usage 就 `usage_reported`；流中途断了、或者上游
//! 不支持流式 usage，就 `execution_observed_usage_unknown`，之后 `settle` 按预留额结算
//! （设计稿 §6：流式 usage 未知时按预留结算）。不支持流式 usage 的端点根本不能跑付费或补贴的流式请求，
//! 由许可表过滤保证（[`crate::permit`]）。

use serde::{Deserialize, Serialize};

use crate::pricing::Usage;

/// 一次尝试的状态。
///
/// 反序列化走私有的镜像类型 `AttemptStateWire`：serde 对「内部标签枚举的单元变体」不检查未知字段
/// （`{"state":"sending","x":1}` 会被悄悄接受），而本仓库的约定是未知字段一律拒绝，
/// 所以线上形式用「空结构变体」的镜像类型来解析，再转成这个更好用的类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", from = "AttemptStateWire")]
pub enum AttemptState {
    Admitted,
    Sending,
    NotSent,
    MaybeExecuted,
    ExecutedUsageKnown { usage: Usage },
    ExecutedUsageUnknown,
    Settled { basis: SettleBasis },
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum AttemptStateWire {
    Admitted {},
    Sending {},
    NotSent {},
    MaybeExecuted {},
    ExecutedUsageKnown { usage: Usage },
    ExecutedUsageUnknown {},
    Settled { basis: SettleBasis },
}

impl From<AttemptStateWire> for AttemptState {
    fn from(wire: AttemptStateWire) -> Self {
        match wire {
            AttemptStateWire::Admitted {} => AttemptState::Admitted,
            AttemptStateWire::Sending {} => AttemptState::Sending,
            AttemptStateWire::NotSent {} => AttemptState::NotSent,
            AttemptStateWire::MaybeExecuted {} => AttemptState::MaybeExecuted,
            AttemptStateWire::ExecutedUsageKnown { usage } => {
                AttemptState::ExecutedUsageKnown { usage }
            }
            AttemptStateWire::ExecutedUsageUnknown {} => AttemptState::ExecutedUsageUnknown,
            AttemptStateWire::Settled { basis } => AttemptState::Settled { basis },
        }
    }
}

/// 一次尝试怎么落的账。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleBasis {
    /// 按实际 usage。
    ActualUsage(Usage),
    /// usage 未知，按预留额。之后供应商账单核对出真实用量时，差额作为单独的调整分录入账，
    /// 不回头改这个状态。
    ReservedAmount,
}

/// 不带数据的状态种类，用于转移表和错误信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStateKind {
    Admitted,
    Sending,
    NotSent,
    MaybeExecuted,
    ExecutedUsageKnown,
    ExecutedUsageUnknown,
    Settled,
}

impl AttemptStateKind {
    pub const ALL: [AttemptStateKind; 7] = [
        AttemptStateKind::Admitted,
        AttemptStateKind::Sending,
        AttemptStateKind::NotSent,
        AttemptStateKind::MaybeExecuted,
        AttemptStateKind::ExecutedUsageKnown,
        AttemptStateKind::ExecutedUsageUnknown,
        AttemptStateKind::Settled,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(self, AttemptStateKind::NotSent | AttemptStateKind::Settled)
    }

    /// 线上字符串（与 `serde` 形式一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            AttemptStateKind::Admitted => "admitted",
            AttemptStateKind::Sending => "sending",
            AttemptStateKind::NotSent => "not_sent",
            AttemptStateKind::MaybeExecuted => "maybe_executed",
            AttemptStateKind::ExecutedUsageKnown => "executed_usage_known",
            AttemptStateKind::ExecutedUsageUnknown => "executed_usage_unknown",
            AttemptStateKind::Settled => "settled",
        }
    }
}

/// 执行确定性（设计稿 §6：每次尝试记录「执行确定性」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Certainty {
    /// 证明没有执行（没发出去，或上游明确拒绝）。
    NotExecuted,
    /// 可能执行了，不确定。成本保留待核销。
    Maybe,
    /// 确认执行了。
    Executed,
}

impl AttemptState {
    pub fn kind(&self) -> AttemptStateKind {
        match self {
            AttemptState::Admitted => AttemptStateKind::Admitted,
            AttemptState::Sending => AttemptStateKind::Sending,
            AttemptState::NotSent => AttemptStateKind::NotSent,
            AttemptState::MaybeExecuted => AttemptStateKind::MaybeExecuted,
            AttemptState::ExecutedUsageKnown { .. } => AttemptStateKind::ExecutedUsageKnown,
            AttemptState::ExecutedUsageUnknown => AttemptStateKind::ExecutedUsageUnknown,
            AttemptState::Settled { .. } => AttemptStateKind::Settled,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.kind().is_terminal()
    }

    /// 这次尝试正在进行（还没有离开「准备发 / 正在发」），下一个候选必须等它结束。
    pub fn is_in_flight(&self) -> bool {
        matches!(self, AttemptState::Admitted | AttemptState::Sending)
    }

    pub fn certainty(&self) -> Certainty {
        match self {
            AttemptState::Admitted | AttemptState::Sending | AttemptState::NotSent => {
                Certainty::NotExecuted
            }
            AttemptState::MaybeExecuted => Certainty::Maybe,
            AttemptState::ExecutedUsageKnown { .. }
            | AttemptState::ExecutedUsageUnknown
            | AttemptState::Settled { .. } => Certainty::Executed,
        }
    }
}

/// 驱动状态机的事件。反序列化同样走镜像类型，原因见 [`AttemptState`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", from = "AttemptEventWire")]
pub enum AttemptEvent {
    Dispatch,
    NeverLeftHost,
    WireWriteStarted,
    UpstreamRefused { http_status: u16 },
    UsageReported { usage: Usage },
    ExecutionObservedUsageUnknown,
    Settle,
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum AttemptEventWire {
    Dispatch {},
    NeverLeftHost {},
    WireWriteStarted {},
    UpstreamRefused { http_status: u16 },
    UsageReported { usage: Usage },
    ExecutionObservedUsageUnknown {},
    Settle {},
}

impl From<AttemptEventWire> for AttemptEvent {
    fn from(wire: AttemptEventWire) -> Self {
        match wire {
            AttemptEventWire::Dispatch {} => AttemptEvent::Dispatch,
            AttemptEventWire::NeverLeftHost {} => AttemptEvent::NeverLeftHost,
            AttemptEventWire::WireWriteStarted {} => AttemptEvent::WireWriteStarted,
            AttemptEventWire::UpstreamRefused { http_status } => {
                AttemptEvent::UpstreamRefused { http_status }
            }
            AttemptEventWire::UsageReported { usage } => AttemptEvent::UsageReported { usage },
            AttemptEventWire::ExecutionObservedUsageUnknown {} => {
                AttemptEvent::ExecutionObservedUsageUnknown
            }
            AttemptEventWire::Settle {} => AttemptEvent::Settle,
        }
    }
}

/// 不带数据的事件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptEventKind {
    Dispatch,
    NeverLeftHost,
    WireWriteStarted,
    UpstreamRefused,
    UsageReported,
    ExecutionObservedUsageUnknown,
    Settle,
}

impl AttemptEventKind {
    pub const ALL: [AttemptEventKind; 7] = [
        AttemptEventKind::Dispatch,
        AttemptEventKind::NeverLeftHost,
        AttemptEventKind::WireWriteStarted,
        AttemptEventKind::UpstreamRefused,
        AttemptEventKind::UsageReported,
        AttemptEventKind::ExecutionObservedUsageUnknown,
        AttemptEventKind::Settle,
    ];
}

impl AttemptEvent {
    pub fn kind(&self) -> AttemptEventKind {
        match self {
            AttemptEvent::Dispatch => AttemptEventKind::Dispatch,
            AttemptEvent::NeverLeftHost => AttemptEventKind::NeverLeftHost,
            AttemptEvent::WireWriteStarted => AttemptEventKind::WireWriteStarted,
            AttemptEvent::UpstreamRefused { .. } => AttemptEventKind::UpstreamRefused,
            AttemptEvent::UsageReported { .. } => AttemptEventKind::UsageReported,
            AttemptEvent::ExecutionObservedUsageUnknown => {
                AttemptEventKind::ExecutionObservedUsageUnknown
            }
            AttemptEvent::Settle => AttemptEventKind::Settle,
        }
    }
}

/// 转移失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("illegal transition: event {event:?} is not allowed in state {from:?}")]
    IllegalTransition {
        from: AttemptStateKind,
        event: AttemptEventKind,
    },
    #[error("event {event:?} carries invalid evidence: {reason}")]
    InvalidEvidence {
        event: AttemptEventKind,
        reason: &'static str,
    },
}

/// 上游明确拒绝（还没执行）的 HTTP 状态：只认 4xx。5xx、超时、连接中断都不能证明没执行。
pub fn is_refusal_status(http_status: u16) -> bool {
    (400..=499).contains(&http_status)
}

/// 单次尝试的转移函数。纯函数：不改入参，成功返回新状态，非法返回错误。
pub fn transition(
    state: &AttemptState,
    event: &AttemptEvent,
) -> Result<AttemptState, TransitionError> {
    use AttemptEvent as E;
    use AttemptState as S;
    let illegal = || TransitionError::IllegalTransition {
        from: state.kind(),
        event: event.kind(),
    };
    match (state, event) {
        (S::Admitted, E::Dispatch) => Ok(S::Sending),
        (S::Admitted | S::Sending, E::NeverLeftHost) => Ok(S::NotSent),
        (S::Sending, E::WireWriteStarted) => Ok(S::MaybeExecuted),
        (S::Sending | S::MaybeExecuted, E::UpstreamRefused { http_status }) => {
            if is_refusal_status(*http_status) {
                Ok(S::NotSent)
            } else {
                Err(TransitionError::InvalidEvidence {
                    event: event.kind(),
                    reason: "only 4xx statuses prove the upstream did not execute",
                })
            }
        }
        (S::MaybeExecuted | S::ExecutedUsageUnknown, E::UsageReported { usage }) => {
            Ok(S::ExecutedUsageKnown { usage: *usage })
        }
        (S::MaybeExecuted, E::ExecutionObservedUsageUnknown) => Ok(S::ExecutedUsageUnknown),
        (S::ExecutedUsageKnown { usage }, E::Settle) => Ok(S::Settled {
            basis: SettleBasis::ActualUsage(*usage),
        }),
        (S::ExecutedUsageUnknown, E::Settle) => Ok(S::Settled {
            basis: SettleBasis::ReservedAmount,
        }),
        _ => Err(illegal()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use AttemptEventKind as EK;
    use AttemptStateKind as SK;

    fn usage() -> Usage {
        Usage::new(100, 50, 10).unwrap()
    }

    /// 每个状态种类的一个代表实例。
    fn state_of(kind: SK) -> AttemptState {
        match kind {
            SK::Admitted => AttemptState::Admitted,
            SK::Sending => AttemptState::Sending,
            SK::NotSent => AttemptState::NotSent,
            SK::MaybeExecuted => AttemptState::MaybeExecuted,
            SK::ExecutedUsageKnown => AttemptState::ExecutedUsageKnown { usage: usage() },
            SK::ExecutedUsageUnknown => AttemptState::ExecutedUsageUnknown,
            SK::Settled => AttemptState::Settled {
                basis: SettleBasis::ReservedAmount,
            },
        }
    }

    /// 每个事件种类的一个代表实例（合法的证据）。
    fn event_of(kind: EK) -> AttemptEvent {
        match kind {
            EK::Dispatch => AttemptEvent::Dispatch,
            EK::NeverLeftHost => AttemptEvent::NeverLeftHost,
            EK::WireWriteStarted => AttemptEvent::WireWriteStarted,
            EK::UpstreamRefused => AttemptEvent::UpstreamRefused { http_status: 429 },
            EK::UsageReported => AttemptEvent::UsageReported {
                usage: Usage::new(7, 3, 0).unwrap(),
            },
            EK::ExecutionObservedUsageUnknown => AttemptEvent::ExecutionObservedUsageUnknown,
            EK::Settle => AttemptEvent::Settle,
        }
    }

    /// **独立写下的、期望的转移表**（与实现里的 match 互不依赖）：
    /// 每一行是 (来源状态, 事件) → 目标状态种类。不在表里的组合必须非法。
    const EXPECTED_LEGAL: &[(SK, EK, SK)] = &[
        (SK::Admitted, EK::Dispatch, SK::Sending),
        (SK::Admitted, EK::NeverLeftHost, SK::NotSent),
        (SK::Sending, EK::NeverLeftHost, SK::NotSent),
        (SK::Sending, EK::WireWriteStarted, SK::MaybeExecuted),
        (SK::Sending, EK::UpstreamRefused, SK::NotSent),
        (SK::MaybeExecuted, EK::UpstreamRefused, SK::NotSent),
        (SK::MaybeExecuted, EK::UsageReported, SK::ExecutedUsageKnown),
        (
            SK::ExecutedUsageUnknown,
            EK::UsageReported,
            SK::ExecutedUsageKnown,
        ),
        (
            SK::MaybeExecuted,
            EK::ExecutionObservedUsageUnknown,
            SK::ExecutedUsageUnknown,
        ),
        (SK::ExecutedUsageKnown, EK::Settle, SK::Settled),
        (SK::ExecutedUsageUnknown, EK::Settle, SK::Settled),
    ];

    #[test]
    fn exhaustive_state_event_table_every_cell_is_asserted() {
        let mut legal_seen = 0;
        for from in SK::ALL {
            for event in EK::ALL {
                let result = transition(&state_of(from), &event_of(event));
                let expected = EXPECTED_LEGAL
                    .iter()
                    .find(|(f, e, _)| *f == from && *e == event)
                    .map(|(_, _, to)| *to);
                match (result, expected) {
                    (Ok(next), Some(to)) => {
                        assert_eq!(next.kind(), to, "{from:?} --{event:?}-->");
                        legal_seen += 1;
                    }
                    (Err(TransitionError::IllegalTransition { from: f, event: e }), None) => {
                        assert_eq!((f, e), (from, event));
                    }
                    (got, want) => panic!("{from:?} --{event:?}--> got {got:?}, want {want:?}"),
                }
            }
        }
        assert_eq!(legal_seen, EXPECTED_LEGAL.len());
        assert_eq!(SK::ALL.len() * EK::ALL.len(), 49);
    }

    #[test]
    fn terminal_states_accept_nothing() {
        for kind in SK::ALL {
            let terminal = kind.is_terminal();
            let any_legal = EK::ALL
                .iter()
                .any(|e| transition(&state_of(kind), &event_of(*e)).is_ok());
            assert_eq!(terminal, !any_legal, "{kind:?}");
        }
        assert!(SK::NotSent.is_terminal() && SK::Settled.is_terminal());
        assert!(!SK::MaybeExecuted.is_terminal());
    }

    #[test]
    fn the_happy_path_and_the_streaming_path() {
        let mut s = AttemptState::Admitted;
        for ev in [
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::UsageReported { usage: usage() },
            AttemptEvent::Settle,
        ] {
            s = transition(&s, &ev).unwrap();
        }
        assert_eq!(
            s,
            AttemptState::Settled {
                basis: SettleBasis::ActualUsage(usage())
            }
        );
        // 流中途断了：usage 未知，按预留额结算
        let mut s = AttemptState::Admitted;
        for ev in [
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::ExecutionObservedUsageUnknown,
            AttemptEvent::Settle,
        ] {
            s = transition(&s, &ev).unwrap();
        }
        assert_eq!(
            s,
            AttemptState::Settled {
                basis: SettleBasis::ReservedAmount
            }
        );
        // usage 未知之后，在结算前账单回来了：升级为已知
        let s = transition(
            &AttemptState::ExecutedUsageUnknown,
            &AttemptEvent::UsageReported { usage: usage() },
        )
        .unwrap();
        assert_eq!(s, AttemptState::ExecutedUsageKnown { usage: usage() });
    }

    #[test]
    fn upstream_refusal_proves_non_execution_only_for_4xx() {
        for status in [400u16, 401, 403, 404, 408, 422, 429, 451, 499] {
            for from in [AttemptState::Sending, AttemptState::MaybeExecuted] {
                assert_eq!(
                    transition(
                        &from,
                        &AttemptEvent::UpstreamRefused {
                            http_status: status
                        }
                    ),
                    Ok(AttemptState::NotSent),
                    "{status}"
                );
            }
        }
        // 2xx/3xx/5xx、0、超出范围：都不能证明没执行，状态不变（调用方拿到错误后仍应保持 maybe_executed）
        for status in [
            0u16, 100, 200, 204, 301, 399, 500, 502, 503, 504, 599, 600, 999,
        ] {
            for from in [AttemptState::Sending, AttemptState::MaybeExecuted] {
                assert_eq!(
                    transition(
                        &from,
                        &AttemptEvent::UpstreamRefused {
                            http_status: status
                        }
                    ),
                    Err(TransitionError::InvalidEvidence {
                        event: EK::UpstreamRefused,
                        reason: "only 4xx statuses prove the upstream did not execute"
                    }),
                    "{status}"
                );
            }
            assert!(!is_refusal_status(status));
        }
        // 其它来源状态：非法转移，不是证据问题
        assert!(matches!(
            transition(
                &AttemptState::Admitted,
                &AttemptEvent::UpstreamRefused { http_status: 429 }
            ),
            Err(TransitionError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn maybe_executed_has_no_way_out_without_evidence() {
        // 能让 maybe_executed 离开的事件，只有「4xx 拒绝」「用量上报」「确认执行但用量未知」三种。
        let leaving: Vec<EK> = EK::ALL
            .iter()
            .copied()
            .filter(|e| transition(&AttemptState::MaybeExecuted, &event_of(*e)).is_ok())
            .collect();
        assert_eq!(
            leaving,
            vec![
                EK::UpstreamRefused,
                EK::UsageReported,
                EK::ExecutionObservedUsageUnknown
            ]
        );
        // 连「正在发 → 没发出去」这种本地结论都不能用在 maybe_executed 上
        assert!(transition(&AttemptState::MaybeExecuted, &AttemptEvent::NeverLeftHost).is_err());
        // maybe_executed 不能直接结算（成本保留待核销，必须先确认执行）
        assert!(transition(&AttemptState::MaybeExecuted, &AttemptEvent::Settle).is_err());
    }

    #[test]
    fn certainty_matches_the_state() {
        for (kind, certainty) in [
            (SK::Admitted, Certainty::NotExecuted),
            (SK::Sending, Certainty::NotExecuted),
            (SK::NotSent, Certainty::NotExecuted),
            (SK::MaybeExecuted, Certainty::Maybe),
            (SK::ExecutedUsageKnown, Certainty::Executed),
            (SK::ExecutedUsageUnknown, Certainty::Executed),
            (SK::Settled, Certainty::Executed),
        ] {
            assert_eq!(state_of(kind).certainty(), certainty, "{kind:?}");
        }
        for kind in SK::ALL {
            assert_eq!(state_of(kind).kind(), kind);
            assert_eq!(
                state_of(kind).is_in_flight(),
                matches!(kind, SK::Admitted | SK::Sending)
            );
        }
    }

    #[test]
    fn wire_shapes_are_pinned() {
        let cases: Vec<(AttemptState, &str)> = vec![
            (AttemptState::Admitted, r#"{"state":"admitted"}"#),
            (AttemptState::Sending, r#"{"state":"sending"}"#),
            (AttemptState::NotSent, r#"{"state":"not_sent"}"#),
            (AttemptState::MaybeExecuted, r#"{"state":"maybe_executed"}"#),
            (
                AttemptState::ExecutedUsageKnown { usage: usage() },
                r#"{"state":"executed_usage_known","usage":{"prompt_tokens":100,"completion_tokens":50,"cached_prompt_tokens":10}}"#,
            ),
            (
                AttemptState::ExecutedUsageUnknown,
                r#"{"state":"executed_usage_unknown"}"#,
            ),
            (
                AttemptState::Settled {
                    basis: SettleBasis::ReservedAmount,
                },
                r#"{"state":"settled","basis":"reserved_amount"}"#,
            ),
            (
                AttemptState::Settled {
                    basis: SettleBasis::ActualUsage(usage()),
                },
                r#"{"state":"settled","basis":{"actual_usage":{"prompt_tokens":100,"completion_tokens":50,"cached_prompt_tokens":10}}}"#,
            ),
        ];
        for (state, json) in cases {
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(serde_json::from_str::<AttemptState>(json).unwrap(), state);
            assert_eq!(
                serde_json::to_value(&state).unwrap()["state"],
                state.kind().as_str()
            );
        }
        let events: Vec<(AttemptEvent, &str)> = vec![
            (AttemptEvent::Dispatch, r#"{"event":"dispatch"}"#),
            (
                AttemptEvent::NeverLeftHost,
                r#"{"event":"never_left_host"}"#,
            ),
            (
                AttemptEvent::WireWriteStarted,
                r#"{"event":"wire_write_started"}"#,
            ),
            (
                AttemptEvent::UpstreamRefused { http_status: 429 },
                r#"{"event":"upstream_refused","http_status":429}"#,
            ),
            (
                AttemptEvent::UsageReported { usage: Usage::ZERO },
                r#"{"event":"usage_reported","usage":{"prompt_tokens":0,"completion_tokens":0,"cached_prompt_tokens":0}}"#,
            ),
            (
                AttemptEvent::ExecutionObservedUsageUnknown,
                r#"{"event":"execution_observed_usage_unknown"}"#,
            ),
            (AttemptEvent::Settle, r#"{"event":"settle"}"#),
        ];
        for (event, json) in events {
            assert_eq!(serde_json::to_string(&event).unwrap(), json);
            assert_eq!(serde_json::from_str::<AttemptEvent>(json).unwrap(), event);
        }
    }

    #[test]
    fn unknown_states_events_and_fields_are_rejected() {
        assert!(serde_json::from_str::<AttemptState>(r#"{"state":"retrying"}"#).is_err());
        assert!(serde_json::from_str::<AttemptState>(r#"{"state":"sending","x":1}"#).is_err());
        assert!(
            serde_json::from_str::<AttemptState>(r#"{"state":"executed_usage_known"}"#).is_err()
        );
        assert!(serde_json::from_str::<AttemptEvent>(r#"{"event":"retry"}"#).is_err());
        assert!(serde_json::from_str::<AttemptEvent>(r#"{"event":"settle","x":1}"#).is_err());
        assert!(serde_json::from_str::<AttemptEvent>(r#"{"event":"upstream_refused"}"#).is_err());
        assert!(
            serde_json::from_str::<AttemptEvent>(
                r#"{"event":"upstream_refused","http_status":-1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<AttemptEvent>(
                r#"{"event":"upstream_refused","http_status":70000}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<AttemptStateKind>("\"retrying\"").is_err());
    }

    #[test]
    fn transition_never_mutates_its_inputs_and_is_deterministic() {
        for from in SK::ALL {
            for event in EK::ALL {
                let s = state_of(from);
                let e = event_of(event);
                let (s2, e2) = (s.clone(), e.clone());
                let first = transition(&s, &e);
                let second = transition(&s, &e);
                assert_eq!(first, second);
                assert_eq!((s, e), (s2, e2));
            }
        }
    }
}

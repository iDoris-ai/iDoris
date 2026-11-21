//! `GET /admin/api/v1/upstreams` 与受限写接口 `POST /admin/api/v1/upstreams/{id}/start|stop|restart`
//! （接口规范 §2.11.4 与勘误七；IDORIS-CORE S6）。
//!
//! # 读
//!
//! 元素是 `/health.upstreams[]` 的六个字段（`id`、`engine_kind`、`state`、`reason`、`managed`、`owned`，同源同口径：托管上游的
//! 值来自引擎管理器发布状态时同时写 `/health` 的那一次，见 [`crate::lifecycle::EngineManager::views`]）加 Admin 专属的只读细节：
//! `port`（卡的 endpoint 里的 loopback 端口）、`pid`（只有 `owned` 才有）、`restart_required`、托管上游的
//! `failure_count` 与 `last_failure_at_ms`。**没有任何 key、token、nonce 的位置**：元素的类型（`deny_unknown_fields`）与
//! JSON Schema（`additionalProperties: false`）都不接受多余的字段。
//!
//! # 写
//!
//! 方法 POST，不需要请求体（有也不读），鉴权同 Admin 的其余路由。成功是 **202** 加该上游此刻的元素（字段同 GET）：命令被
//! 受理就回答，动作可能还在进行（`stop` 要等引擎停下，至多一个 `stop_grace`），调用方轮询 `GET` 或 `/health` 到终态。
//! 幂等：目标已达成时重复请求仍是 202 加当前状态。
//!
//! 错误（体是统一的 `{error:{type,rule_id,reason_code,evidence,remediation}}`）：
//!
//! | 状态码 | `type` | `reason_code` | 何时 |
//! |---|---|---|---|
//! | 404 | `upstream_not_found` | `upstream_not_found` | 没有这个 id |
//! | 409 | `upstream_not_owned` | `not_managed` | 卡没有声明托管（resident、`setup upstream` 的卡、伴生卡），三个动作都是 |
//! | 409 | `upstream_not_owned` | `not_owned` | 托管卡，但引擎不是 iDoris 拉起的（外部引擎、没有引擎）：`stop`、`restart` |
//! | 409 | `restart_required` | `restart_required` | 门控放行之后引擎不能换代：`restart`，以及终局的实例的 `start`（见 [`crate::lifecycle`]） |
//! | 503 | `idoris_stopping` | `idoris_stopping` | 引擎管理器已经在停止 |
//!
//! 判定的先后：先看 id，再看 `managed`，再看归属（`stop`、`restart` 要 `owned`；已经 `stopped` 的 `stop` 是幂等，不报错），
//! 最后交给管理器（它回答 `restart_required`）。所以 `owned` 为 false 的上游，`restart` 一律是 `upstream_not_owned`
//! （规范 §2.11.4「一律」），`restart_required` 只出现在 `owned` 为 true 的 `restart` 与终局的 `start` 上。

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use idoris_contracts::admin_v0::{
    AdminUpstream, AdminUpstreamReason, AdminUpstreamState, AdminUpstreamsResponse,
};

use super::AdminHttpState;
use crate::AppState;
use crate::lifecycle::{CommandError, EngineManager, ManagedView};
use crate::readiness::{UpstreamReason, UpstreamState, UpstreamStatus};

/// Admin 的三个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Start,
    Stop,
    Restart,
}

/// 此刻每个上游的元素，顺序同 `/health.upstreams`。`engines` 为 `None`（没有托管卡）时每个元素都只有 `/health` 里有的那些
/// 事实加 `port`，`restart_required` 恒为 false。
pub fn upstreams(state: &AppState, engines: Option<&EngineManager>) -> Vec<AdminUpstream> {
    let managed = engines.map(EngineManager::views).unwrap_or_default();
    state
        .health_state
        .view()
        .upstreams
        .into_iter()
        .map(|health| {
            let view = managed.get(&health.id);
            element(state, health, view)
        })
        .collect()
}

/// 一个元素：托管上游的 `state`、`reason`、`owned` 取管理器的快照（`view`，只有托管上游才有；它与 `/health` 同源，且与
/// `pid`、`restart_required` 是同一把锁里读的，彼此一致），别的取 `/health` 的快照。
fn element(state: &AppState, health: UpstreamStatus, view: Option<&ManagedView>) -> AdminUpstream {
    let (upstream_state, reason, owned) = match view {
        Some(view) => (view.state, view.reason, view.owned),
        None => (health.state, health.reason, health.owned),
    };
    let port = match view {
        Some(view) => Some(view.port),
        None => state
            .cards
            .iter()
            .find(|card| card.provider.id == health.id)
            .and_then(|card| port_of(&card.endpoint)),
    };
    AdminUpstream {
        id: health.id,
        engine_kind: health.engine_kind,
        state: state_wire(upstream_state),
        reason: reason.map(reason_wire),
        managed: health.managed,
        owned,
        port,
        pid: view.and_then(|view| view.pid),
        restart_required: view.is_some_and(|view| view.restart_required),
        failure_count: view.map(|view| view.failure_count),
        last_failure_at_ms: view.and_then(|view| view.last_failure_at_ms),
    }
}

/// 卡的 endpoint 里的端口（http 与 https 没写端口时是协议的缺省端口）。只取端口：endpoint 里的凭据、路径不会出现在响应里。
fn port_of(endpoint: &str) -> Option<u16> {
    reqwest::Url::parse(endpoint).ok()?.port_or_known_default()
}

/// 路由器的状态到契约的状态：穷尽的 `match`，路由器里加了新状态而契约没跟上，这里编译不过。
fn state_wire(state: UpstreamState) -> AdminUpstreamState {
    match state {
        UpstreamState::Ready => AdminUpstreamState::Ready,
        UpstreamState::Starting => AdminUpstreamState::Starting,
        UpstreamState::Unavailable => AdminUpstreamState::Unavailable,
        UpstreamState::Restarting => AdminUpstreamState::Restarting,
        UpstreamState::Failed => AdminUpstreamState::Failed,
        UpstreamState::Stopped => AdminUpstreamState::Stopped,
        UpstreamState::Absent => AdminUpstreamState::Absent,
    }
}

/// 同上，原因码。
fn reason_wire(reason: UpstreamReason) -> AdminUpstreamReason {
    match reason {
        UpstreamReason::EngineNotFound => AdminUpstreamReason::EngineNotFound,
        UpstreamReason::PortInUse => AdminUpstreamReason::PortInUse,
        UpstreamReason::AuthRequired => AdminUpstreamReason::AuthRequired,
        UpstreamReason::AuthFailed => AdminUpstreamReason::AuthFailed,
        UpstreamReason::StartupTimeout => AdminUpstreamReason::StartupTimeout,
        UpstreamReason::CrashLoop => AdminUpstreamReason::CrashLoop,
        UpstreamReason::FencePending => AdminUpstreamReason::FencePending,
        UpstreamReason::BudgetZero => AdminUpstreamReason::BudgetZero,
        UpstreamReason::BaseUnavailable => AdminUpstreamReason::BaseUnavailable,
        UpstreamReason::StartupLatched => AdminUpstreamReason::StartupLatched,
    }
}

// ---------------------------------------------------------------------------------------------
// 处理函数
// ---------------------------------------------------------------------------------------------

pub(super) async fn list(State(state): State<AdminHttpState>) -> Json<AdminUpstreamsResponse> {
    Json(AdminUpstreamsResponse {
        upstreams: upstreams(&state.app, state.engines.as_deref()),
    })
}

pub(super) async fn start(state: State<AdminHttpState>, id: Path<String>) -> Response {
    act(state.0, id.0, Action::Start).await
}

pub(super) async fn stop(state: State<AdminHttpState>, id: Path<String>) -> Response {
    act(state.0, id.0, Action::Stop).await
}

pub(super) async fn restart(state: State<AdminHttpState>, id: Path<String>) -> Response {
    act(state.0, id.0, Action::Restart).await
}

/// 为什么一个上游不归 iDoris 管。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// 卡没有声明托管。
    NotManaged,
    /// 托管卡，但引擎不是 iDoris 拉起的。
    NotOwned,
}

/// 管理器之前的判定（只看 `/health` 同源的快照）。`None` = 放行，交给管理器。
fn precheck(action: Action, upstream: &AdminUpstream) -> Option<Ownership> {
    if !upstream.managed {
        return Some(Ownership::NotManaged);
    }
    let needs_ownership = matches!(action, Action::Stop | Action::Restart);
    // 已经停下的引擎再 `stop` 是幂等：目标状态已经达成，仍是 202（规范 §2.11.4 通则）。
    let already_stopped = action == Action::Stop && upstream.state == AdminUpstreamState::Stopped;
    (needs_ownership && !upstream.owned && !already_stopped).then_some(Ownership::NotOwned)
}

async fn act(state: AdminHttpState, id: String, action: Action) -> Response {
    let current = upstreams(&state.app, state.engines.as_deref());
    let Some(upstream) = current.iter().find(|upstream| upstream.id == id) else {
        return upstream_not_found(&id);
    };
    if let Some(why) = precheck(action, upstream) {
        return upstream_not_owned(&upstream.id, why);
    }
    let Some(engines) = state.engines.as_deref() else {
        // 声明了托管却没有引擎管理器：不会发生（有托管卡才会创建它）；当作管理器已经不在。
        return idoris_stopping();
    };
    let outcome = match action {
        Action::Start => engines.start_upstream(&id).await,
        Action::Stop => engines.stop_upstream(&id).await,
        Action::Restart => engines.restart_upstream(&id).await,
    };
    match outcome {
        Ok(()) => accepted(&state, &id),
        Err(CommandError::NotFound) => upstream_not_found(&id),
        Err(CommandError::NotOwned) => upstream_not_owned(&id, Ownership::NotOwned),
        Err(CommandError::RestartRequired) => restart_required(&id, action),
        Err(CommandError::Closed) => idoris_stopping(),
    }
}

/// 202 加该上游此刻的元素（命令受理之后重新读：幂等的情形里它就是目标状态，进行中的情形里是当前状态）。
fn accepted(state: &AdminHttpState, id: &str) -> Response {
    match upstreams(&state.app, state.engines.as_deref())
        .into_iter()
        .find(|upstream| upstream.id == id)
    {
        Some(upstream) => (StatusCode::ACCEPTED, Json(upstream)).into_response(),
        None => upstream_not_found(id),
    }
}

// ---------------------------------------------------------------------------------------------
// 错误体
// ---------------------------------------------------------------------------------------------

/// 统一的错误体（接口规范 §2.5；与数据面的 `error_envelope_with_reason` 同形：`type` 是错误的类，`reason_code` 是更细的码）。
fn error_response(
    status: StatusCode,
    error_type: &'static str,
    reason_code: &'static str,
    remediation: String,
) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "type": error_type,
                "rule_id": null,
                "reason_code": reason_code,
                "evidence": null,
                "remediation": remediation,
            }
        })),
    )
        .into_response()
}

/// `id` 来自调用方：这里只在响应里回显它（JSON 转义），不进日志。
fn upstream_not_found(id: &str) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "upstream_not_found",
        "upstream_not_found",
        format!("no upstream with id {id:?}; GET /admin/api/v1/upstreams lists them"),
    )
}

fn upstream_not_owned(id: &str, why: Ownership) -> Response {
    let (reason_code, text) = match why {
        Ownership::NotManaged => (
            "not_managed",
            "its card does not declare extensions.idoris_managed, so iDoris never starts, stops or restarts its engine",
        ),
        Ownership::NotOwned => (
            "not_owned",
            "the engine behind it is not a process iDoris launched (an external engine, or none), and iDoris never signals processes it does not own",
        ),
    };
    error_response(
        StatusCode::CONFLICT,
        "upstream_not_owned",
        reason_code,
        format!("upstream {id:?}: {text}"),
    )
}

fn restart_required(id: &str, action: Action) -> Response {
    // 只有 `restart` 与 `start` 会得到它（`stop` 在放行之后照常可用）。
    let what = if action == Action::Restart {
        "restarting it would replace its engine"
    } else {
        "it is out of service and cannot be started again"
    };
    error_response(
        StatusCode::CONFLICT,
        "restart_required",
        "restart_required",
        format!(
            "upstream {id:?}: after startup the engine cannot be replaced while iDoris runs ({what}); restart iDoris to bring it back"
        ),
    )
}

fn idoris_stopping() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "idoris_stopping",
        "idoris_stopping",
        "iDoris is shutting down its engines; the command was not accepted".to_owned(),
    )
}

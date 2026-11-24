//! `GET /admin/api/v1/upstreams` 与受限写接口 `POST /admin/api/v1/upstreams/{id}/start|stop|restart`
//! （接口规范 §2.11.4 与勘误十；IDORIS-CORE S6）。
//!
//! # 读
//!
//! 元素是 `/health.upstreams[]` 的六个字段（`id`、`engine_kind`、`state`、`reason`、`managed`、`owned`，同源同口径：托管上游的
//! 值来自引擎管理器发布状态时同时写 `/health` 的那一次，见 [`crate::lifecycle::EngineManager::views`]）加 Admin 专属的只读细节：
//! `port`（卡的 endpoint 里的 loopback 端口）、`pid`（只有 `owned` 才有）、`restart_required`（放行时没就绪而冻结的上游：
//! 要重启 iDoris 才会回来）、托管上游的 `failure_count` 与 `last_failure_at_ms`。**没有任何 key、token、nonce 的位置**：元素的类型（`deny_unknown_fields`）与
//! JSON Schema（`additionalProperties: false`）都不接受多余的字段。
//!
//! # 写
//!
//! 方法 POST，不需要请求体（有也不读），鉴权同 Admin 的其余路由。成功是 **202** 加该上游**被受理那一刻**的元素：命令被受理就回答，
//! 动作可能还在进行（`stop` 要等引擎停下，至多一个 `stop_grace`），调用方轮询 `GET` 或 `/health` 到终态。
//!
//! **202 体是实例任务自己在受理那一刻的快照**（Codex 预审 L4 的 M1），不是回答之后再读一遍的共享状态：受理之后实例任务还在往前走（排空、
//! 停进程、换新引擎），也还会收下排在后面的命令，事后去读，`restart` 的 202 可能说 `stopped`，或者快到新引擎已经就绪、`restarting`
//! 被盖掉。幂等的情形里它就是当前状态。唯一的例外：实例任务已经**认领**了命令（受理了、会执行），但第一步（关 Supervisor 的门）等得比
//! 命令的截止时间（`command_timeout`）还久、没来得及回答——命令是受理的，不能回 503「没有受理」，也没有受理那一刻的快照可给：回 202，
//! 体是回答那一刻读到的元素，可能还是命令之前的状态，调用方照常轮询 `GET`（202 本来就不是完成）。**生效的时刻**是实例任务拿到决定锁的那一刻：截止时间的检查与决定
//! 在同一把锁里做（[`crate::lifecycle`] 里的 `Expiry`），认领成功当且仅当它读到的时钟还没到截止时间，所以到点时还没被认领的命令绝不会被受理。
//!
//! 幂等：目标已达成时重复请求仍是 202 加当前状态。**`restart` 的幂等只在第一次操作进行期间成立**：并发的、或在第一次操作员发起的、真的在换自有引擎的
//! `restart` 进行期间到达的重复 `restart` 并入它（只换一次引擎）；第一次已经做完（新引擎就绪）之后到达的重传是一次**新的** `restart`，再换一次引擎。
//! 丢了第一个 202 的调用方要确认的话，去读 `GET`（`pid` 变了、状态回到 `ready`），不要重发。
//!
//! 错误（体是统一的 `{error:{type,rule_id,reason_code,evidence,remediation}}`）：
//!
//! | 状态码 | `type` | `reason_code` | 何时 |
//! |---|---|---|---|
//! | 400 | `invalid_upstream_id` | `invalid_upstream_id` | 路径里的 `{id}` 解码之后不是 UTF-8（体里不回显它） |
//! | 404 | `upstream_not_found` | `upstream_not_found` | 没有这个 id |
//! | 409 | `upstream_not_owned` | `not_managed` | 卡没有声明托管（resident、`setup upstream` 的卡、伴生卡），三个动作都是 |
//! | 409 | `upstream_not_owned` | `not_owned` | 托管卡，但引擎不是 iDoris 拉起的（外部引擎、没有引擎）：`stop`、`restart` |
//! | 409 | `restart_required` | `restart_required` | 放行时没就绪的上游（降级放行，完整的 Router 里没有它的 Supervisor）的 `start`（见 [`crate::lifecycle`]） |
//! | 503 | `idoris_stopping` | `idoris_stopping` | 引擎管理器已经在停止，或命令没有在截止时间之前被实例任务认领（实例任务忙在排空、复位里；命令没有受理，事后也不会执行，可以重发） |
//!
//! 判定的先后：先看 id，再看 `managed`，再看归属（`stop`、`restart` 要 `owned`；已经 `stopped` 的 `stop` 是幂等，不报错），
//! 最后交给管理器（它回答 `restart_required`）。所以 `owned` 为 false 的上游，`restart` 一律是 `upstream_not_owned`
//! （规范 §2.11.4「一律」；冻结的上游没有进程，`owned` 恒为 false），`restart_required` 只出现在冻结上游的 `start` 上；冻结的上游要不要
//! 重启 iDoris，读 `GET` 元素的 `restart_required` 字段。归属在这里只是**预检**（读的是 `/health` 同源的快照，读完到命令被收下之间它可以变）：
//! 状态是 `starting`、`restarting` 的上游，`restart` 放行到实例任务，由它把「归属」与「并入进行中的操作」**一起**决定（Codex 预审 L4 的
//! M2、复审 M1'）——并入的只有操作员发起的、真的在换 iDoris 自己引擎的 `restart`（进行中的，或刚做完而且换了自有引擎的）；接管外部引擎的 `start`、
//! 外部引擎换了进程时的自动重新同步（也发布 `restarting`）都没有可并入的结果，那里的 `restart` 是 409 `upstream_not_owned`，不是一个没有任何东西跟在后面的 202。
//!
//! # `stop` 什么时候算做完
//!
//! 202 之后轮询 `GET`：**`state` 是 `stopped` 并且 `owned` 是 false** 才是做完了（进程确认消失、记录删了、端点封上）。`owned` 量的是 iDoris
//! 此刻**握不握着**一个它拉起的（或认回的）引擎进程，不是「进程一定不在了」：没能确认进程已经消失（读不出它的身份、不能给它发信号）时，
//! 句柄已经交出去收尾、不再握着，`owned` 也是 false，但收尾的状态是 `unavailable` + `port_in_use`——这是**未确认**，进程可能还占着端口，
//! 所有权记录与围栏原样保留、端点保持封上。只看 `owned: false` 就收工会把它当成停好了。未确认之后：`stop` 是 409 `not_owned`（没有句柄可
//! 停），`start` 重新走启动决策、先重读所有权记录：身份此刻读得出，就是**那个之前没能确认停掉的进程**——它被判了「停」，不能因为 `start` 又成了服务中的
//! 引擎，所以先把它停掉、确认消失，再拉起一个新的（第 3 层的 `condemned`）；读不出仍是 `unavailable` + `port_in_use`。

use axum::{
    Json,
    extract::{Path, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use idoris_contracts::admin_v0::{
    AdminUpstream, AdminUpstreamReason, AdminUpstreamState, AdminUpstreamsResponse,
};

use super::AdminHttpState;
use crate::AppState;
use crate::lifecycle::{CommandAck, CommandError, EngineManager, ManagedView};
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
/// 端口 0 不是一个端口（契约的下限是 1）：`http://127.0.0.1:0` 这样的非托管卡不输出 `port`（IDORIS-CORE S6，Codex 终审 L2）。
fn port_of(endpoint: &str) -> Option<u16> {
    reqwest::Url::parse(endpoint)
        .ok()?
        .port_or_known_default()
        .filter(|port| *port != 0)
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

/// 三个写接口共用：`{id}` 取不出来（路径里的百分号编码解码之后不是 UTF-8）时，Axum 自己的提取器会回一个纯文本的 400，绕过统一的错误体，
/// 按文档解析错误体的调用方在报告输入错误之前就解析失败了（Codex 预审 L4 的 L1）。这里把拒绝接住，回统一的 JSON 400，体里不回显被拒绝的值。
/// 合法的 id 照旧精确匹配解码之后的值。
async fn act_on(
    state: AdminHttpState,
    id: Result<Path<String>, PathRejection>,
    action: Action,
) -> Response {
    match id {
        Ok(Path(id)) => act(state, id, action).await,
        Err(_rejection) => invalid_upstream_id(),
    }
}

pub(super) async fn start(
    state: State<AdminHttpState>,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    act_on(state.0, id, Action::Start).await
}

pub(super) async fn stop(
    state: State<AdminHttpState>,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    act_on(state.0, id, Action::Stop).await
}

pub(super) async fn restart(
    state: State<AdminHttpState>,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    act_on(state.0, id, Action::Restart).await
}

/// 为什么一个上游不归 iDoris 管。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// 卡没有声明托管。
    NotManaged,
    /// 托管卡，但引擎不是 iDoris 拉起的。
    NotOwned,
}

/// 管理器之前的判定（只看 `/health` 同源的快照）。`None` = 放行，交给管理器。**只是预检**：读完到命令被实例任务收下之间快照可以变，
/// 所以凡是「也许能并入一次进行中的操作」的（`restart` 遇到 `starting`、`restarting`），这里一律放行，由实例任务把归属与并入一起决定
/// （Codex 预审 L4 的 M2：`restarting` 也可能是外部引擎自动重新同步，那里没有可并入的操作，实例任务回 `NotOwned`）。
fn precheck(action: Action, upstream: &AdminUpstream) -> Option<Ownership> {
    if !upstream.managed {
        return Some(Ownership::NotManaged);
    }
    let needs_ownership = matches!(action, Action::Stop | Action::Restart);
    // 目标状态已经达成（或正在达成）时重复的请求是幂等的，仍是 202（规范 §2.11.4 通则）：已经停下的引擎再 `stop`；已经在
    // （重新）启动的再 `restart`（此刻引擎还没起来，`owned` 是 false；并发的两个 `restart` 只换一次引擎）。
    let already_stopped = action == Action::Stop && upstream.state == AdminUpstreamState::Stopped;
    let already_restarting = action == Action::Restart
        && matches!(
            upstream.state,
            AdminUpstreamState::Starting | AdminUpstreamState::Restarting
        );
    (needs_ownership && !upstream.owned && !already_stopped && !already_restarting)
        .then_some(Ownership::NotOwned)
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
        Ok(ack) => accepted(&state, &id, &ack),
        Err(CommandError::NotFound) => upstream_not_found(&id),
        Err(CommandError::NotOwned) => upstream_not_owned(&id, Ownership::NotOwned),
        Err(CommandError::RestartRequired) => restart_required(&id, action),
        Err(CommandError::Closed) => idoris_stopping(),
    }
}

/// 202 加该上游**被受理那一刻**的元素：实例任务回答时带的它自己的快照（[`CommandAck::Snapshot`]；Codex 预审 L4 的 M1），静态的事实
/// （`engine_kind`、`managed`）来自 `/health`。不重新读共享的状态表：受理之后实例任务还在往前走，也还会收下别的命令。实例任务已经认领了
/// 命令、却没来得及在截止时间之前回答（[`CommandAck::Accepted`]）时没有这样的快照，就读此刻的元素（可能还是命令之前的状态，见模块文档）。
fn accepted(state: &AdminHttpState, id: &str, ack: &CommandAck) -> Response {
    let rendered = match ack {
        CommandAck::Snapshot(view) => state
            .app
            .health_state
            .view()
            .upstreams
            .into_iter()
            .find(|health| health.id == id)
            .map(|health| element(&state.app, health, Some(view))),
        CommandAck::Accepted => upstreams(&state.app, state.engines.as_deref())
            .into_iter()
            .find(|upstream| upstream.id == id),
    };
    match rendered {
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

/// 路径里的 `{id}` 解码之后不是 UTF-8：体里不回显它（它不是一个合法的字符串，也没有安全的写法）。
fn invalid_upstream_id() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_upstream_id",
        "invalid_upstream_id",
        "the upstream id in the path is not valid UTF-8 once percent-decoded; GET /admin/api/v1/upstreams lists the ids"
            .to_owned(),
    )
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
    // HTTP 上只有 `start` 会得到它（冻结的上游没有进程，`owned` 恒为 false，`restart` 在预检就是 `upstream_not_owned`）；管理器的接口对冻结的
    // 上游的 `restart` 也回 `RestartRequired`，所以这里仍按动作写。`stop` 不会。
    let what = if action == Action::Restart {
        "restart"
    } else {
        "start"
    };
    error_response(
        StatusCode::CONFLICT,
        "restart_required",
        "restart_required",
        format!(
            "upstream {id:?}: it was out of service when iDoris finished starting, so no Supervisor was built for it and it cannot be brought back while iDoris runs (cannot {what}); restart iDoris"
        ),
    )
}

fn idoris_stopping() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "idoris_stopping",
        "idoris_stopping",
        "the command was not accepted and nothing was done (it will not run later): either iDoris is shutting down its engines, or this engine's instance task did not take the command before its deadline because it was busy (draining in-flight work, rebuilding the ledger); GET /admin/api/v1/upstreams shows when the upstream has settled, then send the command again".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeSet;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, header::AUTHORIZATION};
    use http_body_util::BodyExt;
    use idoris_contracts::ComponentCard;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::admin::{
        AdminSessionToken, SharedAdminToken, build_admin_app, build_admin_app_with_token,
        build_admin_starting_app,
    };
    use crate::readiness::{HealthState, Observation};

    const ACTIONS: [&str; 3] = ["start", "stop", "restart"];

    fn health_entry(
        id: &str,
        engine_kind: &str,
        managed: bool,
        state: UpstreamState,
        reason: Option<UpstreamReason>,
        owned: bool,
    ) -> UpstreamStatus {
        UpstreamStatus {
            id: id.to_owned(),
            engine_kind: engine_kind.to_owned(),
            state,
            reason,
            managed,
            owned,
        }
    }

    /// 一个托管且就绪的 oMLX、一张 resident 的自定义上游卡、一个没装引擎的托管上游。
    fn health() -> HealthState {
        HealthState::with_upstreams(
            1_763_625_330_123,
            vec![
                health_entry("omlx", "omlx", true, UpstreamState::Ready, None, true),
                health_entry(
                    "lmstudio",
                    "openai_compat",
                    false,
                    UpstreamState::Ready,
                    None,
                    false,
                ),
                health_entry(
                    "omlx-b",
                    "omlx",
                    true,
                    UpstreamState::Absent,
                    Some(UpstreamReason::EngineNotFound),
                    false,
                ),
            ],
        )
    }

    fn card(id: &str, endpoint: &str) -> ComponentCard {
        let mut card: ComponentCard =
            serde_yaml::from_str(include_str!("../../../../config/components/omlx.yaml")).unwrap();
        card.provider.id = id.to_owned();
        card.endpoint = endpoint.to_owned();
        card
    }

    fn state() -> AppState {
        AppState {
            cards: vec![
                card("lmstudio", "http://127.0.0.1:1234"),
                card("omlx", "http://127.0.0.1:19731"),
            ],
            health_state: health(),
            ..AppState::default()
        }
    }

    fn app() -> (Router, String) {
        let token = AdminSessionToken::mint();
        let secret = token.expose_secret().to_owned();
        (build_admin_app(state(), token), secret)
    }

    fn request(method: &str, path: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value, String) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, text)
    }

    fn element_by_id<'a>(body: &'a Value, id: &str) -> &'a Value {
        body["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["id"] == id)
            .unwrap_or_else(|| panic!("no upstream {id} in {body}"))
    }

    // -----------------------------------------------------------------------------------------
    // 读
    // -----------------------------------------------------------------------------------------

    /// Codex 终审 L2：端口 0 不是一个端口（契约的下限是 1）。一张非托管的卡写成 `http://127.0.0.1:0`，`port_or_known_default()` 给出 `Some(0)`，
    /// `GET` 之前会输出 `port: 0`，违反契约；现在不输出 `port`，响应照样通过契约校验。真实的 `GET`，不是只测 `port_of`。
    #[tokio::test]
    async fn a_card_on_port_zero_outputs_no_port_and_the_response_still_validates() {
        let token = AdminSessionToken::mint();
        let secret = token.expose_secret().to_owned();
        let mut state = state();
        state.cards.push(card("zero-port", "http://127.0.0.1:0"));
        state.health_state = HealthState::with_upstreams(
            1_763_625_330_123,
            vec![
                health_entry(
                    "lmstudio",
                    "openai_compat",
                    false,
                    UpstreamState::Ready,
                    None,
                    false,
                ),
                health_entry(
                    "zero-port",
                    "openai_compat",
                    false,
                    UpstreamState::Ready,
                    None,
                    false,
                ),
            ],
        );
        let app = build_admin_app(state, token);
        let (status, body, _) = send(
            &app,
            request("GET", "/admin/api/v1/upstreams", Some(&secret)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            idoris_contracts::parse::<AdminUpstreamsResponse>(&body).is_ok(),
            "{body}"
        );
        assert_eq!(element_by_id(&body, "lmstudio")["port"], 1234);
        assert!(
            element_by_id(&body, "zero-port").get("port").is_none(),
            "{body}"
        );
        assert_eq!(port_of("http://127.0.0.1:0"), None);
        assert_eq!(port_of("https://example.com"), Some(443));
    }

    #[tokio::test]
    async fn get_is_the_health_snapshot_field_for_field_in_the_same_order() {
        let (app, secret) = app();
        let (status, body, _) = send(
            &app,
            request("GET", "/admin/api/v1/upstreams", Some(&secret)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // 契约（Rust 一侧；TS 一侧由 `parity.test.ts` 钉住）。
        assert!(idoris_contracts::parse::<AdminUpstreamsResponse>(&body).is_ok());

        let health_json = serde_json::to_value(state().health_state.view().upstreams).unwrap();
        let health_ids: Vec<&Value> = health_json
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| &entry["id"])
            .collect();
        let admin_ids: Vec<&Value> = body["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|element| &element["id"])
            .collect();
        assert_eq!(admin_ids, health_ids, "same upstreams, same order");
        for (element, entry) in body["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .zip(health_json.as_array().unwrap())
        {
            for key in ["id", "engine_kind", "state", "reason", "managed", "owned"] {
                assert_eq!(
                    element.get(key),
                    entry.get(key),
                    "{key} of {} must read the same as /health.upstreams",
                    entry["id"]
                );
            }
        }
    }

    #[tokio::test]
    async fn without_an_engine_manager_an_element_has_only_the_health_facts_and_the_port() {
        let (app, secret) = app();
        let (_, body, _) = send(
            &app,
            request("GET", "/admin/api/v1/upstreams", Some(&secret)),
        )
        .await;
        assert_eq!(
            element_by_id(&body, "lmstudio"),
            &json!({
                "id": "lmstudio", "engine_kind": "openai_compat", "state": "ready",
                "managed": false, "owned": false, "port": 1234, "restart_required": false
            })
        );
        // 托管卡而没有管理器（只在测试里会有）：只有 `/health` 里有的事实；端口来自卡。
        assert_eq!(
            element_by_id(&body, "omlx"),
            &json!({
                "id": "omlx", "engine_kind": "omlx", "state": "ready", "managed": true,
                "owned": true, "port": 19731, "restart_required": false
            })
        );
        // 没有对应的卡：没有端口，也不编一个。
        assert_eq!(
            element_by_id(&body, "omlx-b"),
            &json!({
                "id": "omlx-b", "engine_kind": "omlx", "state": "absent",
                "reason": "engine_not_found", "managed": true, "owned": false,
                "restart_required": false
            })
        );
    }

    #[tokio::test]
    async fn an_element_never_carries_a_secret_and_the_response_never_echoes_the_token() {
        let (app, secret) = app();
        let (_, body, text) = send(
            &app,
            request("GET", "/admin/api/v1/upstreams", Some(&secret)),
        )
        .await;
        let allowed: BTreeSet<&str> = [
            "id",
            "engine_kind",
            "state",
            "reason",
            "managed",
            "owned",
            "port",
            "pid",
            "restart_required",
            "failure_count",
            "last_failure_at_ms",
        ]
        .into_iter()
        .collect();
        for element in body["upstreams"].as_array().unwrap() {
            for key in element.as_object().unwrap().keys() {
                assert!(allowed.contains(key.as_str()), "unexpected field {key}");
            }
        }
        assert!(!text.contains(&secret));
        for needle in ["key", "token", "nonce", "secret", "Bearer"] {
            assert!(!text.contains(needle), "{needle} in {text}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // 鉴权
    // -----------------------------------------------------------------------------------------

    fn family() -> Vec<(&'static str, String)> {
        let mut requests = vec![("GET", "/admin/api/v1/upstreams".to_owned())];
        for action in ACTIONS {
            requests.push(("POST", format!("/admin/api/v1/upstreams/omlx/{action}")));
        }
        requests
    }

    #[tokio::test]
    async fn every_route_of_the_family_needs_the_admin_token() {
        let (app, secret) = app();
        let wrong = AdminSessionToken::mint().expose_secret().to_owned();
        for (method, path) in &family() {
            for token in [None, Some(wrong.as_str())] {
                let (status, _, text) = send(&app, request(method, path, token)).await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} {token:?}"
                );
                assert!(!text.contains(&secret));
            }
            // 有 token 才进得去（这些请求的结果见别的测试）。
            let (status, _, _) = send(&app, request(method, path, Some(&secret))).await;
            assert_ne!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn the_starting_body_refuses_the_whole_family_after_authentication() {
        let token = SharedAdminToken::new(AdminSessionToken::mint());
        let secret = token.current().expose_secret().to_owned();
        let app = build_admin_starting_app(state(), token);
        for (method, path) in &family() {
            let (status, body, _) = send(&app, request(method, path, Some(&secret))).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {path}");
            assert_eq!(body["error"]["type"], "idoris_starting");
            assert_eq!(body["error"]["reason_code"], "idoris_starting");
            let (status, _, _) = send(&app, request(method, path, None)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn the_data_plane_does_not_serve_the_family() {
        for (method, path) in family() {
            let response = crate::build_app(state())
                .oneshot(request(method, &path, None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // 写：判定的先后与错误体
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn an_unknown_id_is_404_upstream_not_found_for_every_action() {
        let (app, secret) = app();
        for action in ACTIONS {
            let (status, body, _) = send(
                &app,
                request(
                    "POST",
                    &format!("/admin/api/v1/upstreams/nope/{action}"),
                    Some(&secret),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{action}");
            assert_eq!(body["error"]["type"], "upstream_not_found");
            assert_eq!(body["error"]["reason_code"], "upstream_not_found");
        }
    }

    #[tokio::test]
    async fn a_card_that_is_not_managed_is_409_upstream_not_owned_not_managed_for_every_action() {
        let (app, secret) = app();
        for action in ACTIONS {
            let (status, body, _) = send(
                &app,
                request(
                    "POST",
                    &format!("/admin/api/v1/upstreams/lmstudio/{action}"),
                    Some(&secret),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{action}");
            assert_eq!(body["error"]["type"], "upstream_not_owned");
            assert_eq!(body["error"]["reason_code"], "not_managed");
        }
    }

    #[tokio::test]
    async fn a_managed_upstream_whose_engine_is_not_ours_refuses_stop_and_restart_before_the_manager()
     {
        // `omlx-b` 是托管卡，没有自己拥有的引擎（`absent`）：`stop`、`restart` 是 `not_owned`（没有管理器也一样：
        // 判定在管理器之前）。
        let (app, secret) = app();
        for action in ["stop", "restart"] {
            let (status, body, _) = send(
                &app,
                request(
                    "POST",
                    &format!("/admin/api/v1/upstreams/omlx-b/{action}"),
                    Some(&secret),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{action}");
            assert_eq!(body["error"]["type"], "upstream_not_owned");
            assert_eq!(body["error"]["reason_code"], "not_owned");
        }
    }

    #[tokio::test]
    async fn a_command_for_a_managed_upstream_with_no_engine_manager_is_a_503_idoris_stopping() {
        // 声明了托管却没有管理器：不会发生（有托管卡才有管理器）。不能 panic，也不能谎称受理。
        let (app, secret) = app();
        for action in ["start", "stop"] {
            let (status, body, _) = send(
                &app,
                request(
                    "POST",
                    &format!("/admin/api/v1/upstreams/omlx/{action}"),
                    Some(&secret),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{action}");
            assert_eq!(body["error"]["type"], "idoris_stopping");
            assert_eq!(body["error"]["reason_code"], "idoris_stopping");
        }
    }

    #[tokio::test]
    async fn the_error_body_is_the_unified_envelope_and_a_hostile_id_is_only_ever_escaped() {
        let (app, secret) = app();
        let hostile = "x%22%2C%22evil%22%3A1%7D%0A";
        let (status, body, text) = send(
            &app,
            request(
                "POST",
                &format!("/admin/api/v1/upstreams/{hostile}/stop"),
                Some(&secret),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let error = body["error"].as_object().unwrap();
        assert_eq!(
            error.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(["type", "rule_id", "reason_code", "evidence", "remediation"])
        );
        assert!(error["rule_id"].is_null() && error["evidence"].is_null());
        // 解析之后 `error` 里没有多出一个 `evil` 字段：id 只是被转义地回显在说明里。
        assert!(!error.contains_key("evil"), "{text}");
        assert!(error["remediation"].as_str().unwrap().contains("evil"));
    }

    fn plain_upstream(managed: bool, owned: bool, state: AdminUpstreamState) -> AdminUpstream {
        AdminUpstream {
            id: "omlx".into(),
            engine_kind: "omlx".into(),
            state,
            reason: None,
            managed,
            owned,
            port: None,
            pid: None,
            restart_required: false,
            failure_count: None,
            last_failure_at_ms: None,
        }
    }

    #[test]
    fn the_precheck_asks_for_management_then_ownership_and_lets_an_already_stopped_stop_through() {
        use AdminUpstreamState::{Ready, Stopped};
        for action in [Action::Start, Action::Stop, Action::Restart] {
            // 没声明托管：三个动作都是 `NotManaged`，不管归属与状态。
            for owned in [false, true] {
                assert_eq!(
                    precheck(action, &plain_upstream(false, owned, Ready)),
                    Some(Ownership::NotManaged)
                );
            }
            // 托管、归 iDoris：谁也不拦。
            assert_eq!(precheck(action, &plain_upstream(true, true, Ready)), None);
        }
        // 托管、引擎不是自己拉起的：`start` 不要求归属；`stop`、`restart` 是 `NotOwned`。
        assert_eq!(
            precheck(Action::Start, &plain_upstream(true, false, Ready)),
            None
        );
        for action in [Action::Stop, Action::Restart] {
            assert_eq!(
                precheck(action, &plain_upstream(true, false, Ready)),
                Some(Ownership::NotOwned),
                "{action:?}"
            );
        }
        // 已经 `stopped` 的 `stop` 是幂等，放行；同样的状态下 `restart` 照旧拒绝（规范：`owned` 为 false 的 `restart` 一律拒绝）。
        assert_eq!(
            precheck(Action::Stop, &plain_upstream(true, false, Stopped)),
            None
        );
        assert_eq!(
            precheck(Action::Restart, &plain_upstream(true, false, Stopped)),
            Some(Ownership::NotOwned)
        );
        // 已经在（重新）启动的 `restart` 是幂等（并发的两个只换一次引擎）；`stop` 在此时仍要求归属。
        for state in [AdminUpstreamState::Starting, AdminUpstreamState::Restarting] {
            assert_eq!(
                precheck(Action::Restart, &plain_upstream(true, false, state)),
                None
            );
            assert_eq!(
                precheck(Action::Stop, &plain_upstream(true, false, state)),
                Some(Ownership::NotOwned)
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // 路由表
    // -----------------------------------------------------------------------------------------

    // -----------------------------------------------------------------------------------------
    // Codex 预审 L4：202 体是受理那一刻的快照（M1）、路径里的 id 不是 UTF-8（L1）
    // -----------------------------------------------------------------------------------------

    fn admin_state() -> AdminHttpState {
        AdminHttpState {
            app: std::sync::Arc::new(state()),
            token: SharedAdminToken::new(AdminSessionToken::mint()),
            engines: None,
        }
    }

    async fn json_of(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// M1：共享的状态（`/health` 的快照）此刻说 `ready`、自有、没有 pid；实例任务回答时说的是它受理那一刻的样子——重启受理了，旧引擎还握在手里
    /// （`restarting`，旧的 pid）。202 体是后者：之前它在回答之后再读一遍共享状态，受理之后实例任务还在往前走（也还会收下别的命令），读到的
    /// 可能是另一个命令、或这一次操作更晚的状态。
    #[tokio::test]
    async fn the_202_body_is_the_snapshot_the_instance_answered_with_not_a_reread_of_the_shared_state()
     {
        let state = admin_state();
        let answered = ManagedView {
            state: UpstreamState::Restarting,
            reason: None,
            owned: true,
            port: 19731,
            pid: Some(4242),
            restart_required: false,
            failure_count: 3,
            last_failure_at_ms: Some(1_763_625_330_999),
        };
        let response = accepted(&state, "omlx", &CommandAck::Snapshot(answered));
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = json_of(response).await;
        assert_eq!(
            body,
            json!({
                "id": "omlx", "engine_kind": "omlx", "state": "restarting", "managed": true,
                "owned": true, "port": 19731, "pid": 4242, "restart_required": false,
                "failure_count": 3, "last_failure_at_ms": 1_763_625_330_999_u64
            }),
            "the static facts are /health's, everything the instance owns is its own snapshot"
        );
        assert!(
            idoris_contracts::parse::<AdminUpstream>(&body).is_ok(),
            "{body}"
        );
        // The shared state still says `ready` (a re-read would have shown that).
        assert_eq!(
            state.app.health_state.view().upstreams[0].state,
            UpstreamState::Ready
        );
    }

    /// 实例任务已经认领了命令、却没来得及在截止时间之前回答：没有受理那一刻的快照，回 202 加回答那一刻读到的元素（模块文档）。一个已经不在的 id
    /// 仍是 404。
    #[tokio::test]
    async fn an_accepted_command_whose_answer_was_late_renders_the_element_as_it_is_now() {
        let state = admin_state();
        let response = accepted(&state, "omlx", &CommandAck::Accepted);
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = json_of(response).await;
        assert_eq!(body["state"], "ready", "{body}");
        assert_eq!(body["id"], "omlx");
        let gone = accepted(&state, "no-such-upstream", &CommandAck::Accepted);
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
        let gone = accepted(&state, "no-such-upstream", &CommandAck::Snapshot(view()));
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    /// L1：`{id}` 解码之后不是 UTF-8。Axum 自己的 `Path<String>` 提取器对它回一个纯文本的 400，绕过统一的错误体；三个写接口都接住它，回统一的 JSON
    /// 400 `invalid_upstream_id`，体里没有被拒绝的字节、也没有它的任何写法。没有 token 仍是 401（鉴权在提取之前，没有 token 的调用方得不到任何信息）。
    #[tokio::test]
    async fn an_id_that_is_not_utf8_is_a_json_400_in_the_unified_envelope_for_every_write() {
        let (app, secret) = app();
        for hostile in ["%FF", "%C0%AF", "ab%E2%82", "%ED%A0%80", "x%FFy%FE"] {
            for action in ACTIONS {
                let path = format!("/admin/api/v1/upstreams/{hostile}/{action}");
                let (status, body, text) = send(&app, request("POST", &path, Some(&secret))).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {text}");
                let error = body["error"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{path}: the body is not the JSON envelope: {text}"));
                assert_eq!(
                    error.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                    BTreeSet::from(["type", "rule_id", "reason_code", "evidence", "remediation"]),
                    "{path}"
                );
                assert_eq!(error["type"], "invalid_upstream_id");
                assert_eq!(error["reason_code"], "invalid_upstream_id");
                assert!(error["rule_id"].is_null() && error["evidence"].is_null());
                for echoed in [hostile, "FF", "ff", "%", "\u{fffd}"] {
                    assert!(!text.contains(echoed), "{path} echoed {echoed:?}: {text}");
                }
                // 鉴权在先：没有（或错的）token 得到的是 401，不是 400。
                let (status, _, _) = send(&app, request("POST", &path, None)).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            }
        }
        // 合法的 id 不受影响，精确匹配**解码之后**的值：`omlx%2Db`、`%6Fmlx%2Db` 都解码成 `omlx-b`，是配置里那个没有自有引擎的托管上游
        // （409 `not_owned`）；解码成 `omlx-c` 的没有这个上游（404），不是 400。
        for path in [
            "/admin/api/v1/upstreams/omlx%2Db/stop",
            "/admin/api/v1/upstreams/%6Fmlx%2Db/restart",
        ] {
            let (status, body, _) = send(&app, request("POST", path, Some(&secret))).await;
            assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
            assert_eq!(error_of_body(&body), ("upstream_not_owned", "not_owned"));
        }
        let (status, body, _) = send(
            &app,
            request(
                "POST",
                "/admin/api/v1/upstreams/omlx%2Dc/stop",
                Some(&secret),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(
            error_of_body(&body),
            ("upstream_not_found", "upstream_not_found")
        );
    }

    fn error_of_body(body: &Value) -> (&str, &str) {
        (
            body["error"]["type"].as_str().unwrap(),
            body["error"]["reason_code"].as_str().unwrap(),
        )
    }

    #[tokio::test]
    async fn the_family_is_exactly_one_get_and_three_posts() {
        let (app, secret) = app();
        let token = Some(secret.as_str());
        for (method, path, expected) in [
            ("GET", "/admin/api/v1/upstreams", StatusCode::OK),
            // 路径对、方法不对：405。
            (
                "POST",
                "/admin/api/v1/upstreams",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "PUT",
                "/admin/api/v1/upstreams",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "DELETE",
                "/admin/api/v1/upstreams",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "GET",
                "/admin/api/v1/upstreams/omlx/stop",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "PUT",
                "/admin/api/v1/upstreams/omlx/stop",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "DELETE",
                "/admin/api/v1/upstreams/omlx/restart",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            // 没有的路径：404（没有按 id 读、没有别的动作、没有 `make_default`：那是预留）。
            ("GET", "/admin/api/v1/upstreams/omlx", StatusCode::NOT_FOUND),
            (
                "POST",
                "/admin/api/v1/upstreams/omlx",
                StatusCode::NOT_FOUND,
            ),
            (
                "POST",
                "/admin/api/v1/upstreams/omlx/pause",
                StatusCode::NOT_FOUND,
            ),
            (
                "POST",
                "/admin/api/v1/upstreams/omlx/make_default",
                StatusCode::NOT_FOUND,
            ),
            (
                "POST",
                "/admin/api/v1/upstreams/omlx/stop/extra",
                StatusCode::NOT_FOUND,
            ),
            (
                "POST",
                "/admin/api/v1/upstreams//stop",
                StatusCode::NOT_FOUND,
            ),
        ] {
            let (status, _, _) = send(&app, request(method, path, token)).await;
            assert_eq!(status, expected, "{method} {path}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // 元素的拼装
    // -----------------------------------------------------------------------------------------

    fn view() -> ManagedView {
        ManagedView {
            state: UpstreamState::Stopped,
            reason: Some(UpstreamReason::CrashLoop),
            owned: false,
            port: 19731,
            pid: Some(51234),
            restart_required: true,
            failure_count: 4,
            last_failure_at_ms: Some(1_763_625_330_999),
        }
    }

    #[test]
    fn a_managed_element_takes_state_reason_and_owned_from_the_managers_snapshot_and_adds_its_facts()
     {
        // `/health` 的快照落后了一步（`ready`、自有）：管理器的快照是更新的，元素取它的，与 pid、restart_required 一致。
        let health = health_entry("omlx", "omlx", true, UpstreamState::Ready, None, true);
        let element = element(&state(), health, Some(&view()));
        assert_eq!(
            serde_json::to_value(&element).unwrap(),
            json!({
                "id": "omlx", "engine_kind": "omlx", "state": "stopped", "reason": "crash_loop",
                "managed": true, "owned": false, "port": 19731, "pid": 51234,
                "restart_required": true, "failure_count": 4, "last_failure_at_ms": 1_763_625_330_999_u64
            })
        );
    }

    #[test]
    fn the_failure_facts_and_pid_are_absent_when_there_are_none() {
        let mut quiet = view();
        quiet.pid = None;
        quiet.failure_count = 0;
        quiet.last_failure_at_ms = None;
        quiet.restart_required = false;
        quiet.state = UpstreamState::Ready;
        quiet.reason = None;
        quiet.owned = true;
        let health = health_entry("omlx", "omlx", true, UpstreamState::Ready, None, true);
        assert_eq!(
            serde_json::to_value(element(&state(), health, Some(&quiet))).unwrap(),
            json!({
                "id": "omlx", "engine_kind": "omlx", "state": "ready", "managed": true,
                "owned": true, "port": 19731, "restart_required": false, "failure_count": 0
            })
        );
    }

    #[test]
    fn the_port_is_the_cards_port_and_nothing_else_of_the_endpoint() {
        for (endpoint, expected) in [
            ("http://127.0.0.1:19731", Some(19731)),
            ("http://127.0.0.1:19731/", Some(19731)),
            ("http://127.0.0.1:1234/v1", Some(1234)),
            ("http://localhost:8088", Some(8088)),
            ("http://[::1]:9000", Some(9000)),
            // 没写端口：协议的缺省端口。
            ("http://localhost", Some(80)),
            ("https://localhost", Some(443)),
            ("not a url", None),
            ("", None),
        ] {
            assert_eq!(port_of(endpoint), expected, "{endpoint:?}");
        }
        // 凭据、路径、查询不会因为端点里写了它们就出现在响应里。
        let health = health_entry(
            "lmstudio",
            "openai_compat",
            false,
            UpstreamState::Ready,
            None,
            false,
        );
        let state = AppState {
            cards: vec![card(
                "lmstudio",
                "http://user:hunter2@127.0.0.1:1234/v1?k=secret",
            )],
            ..AppState::default()
        };
        let text = serde_json::to_string(&element(&state, health, None)).unwrap();
        assert!(text.contains("1234"));
        for leaked in ["user", "hunter2", "secret", "/v1"] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }

    #[test]
    fn an_upstream_without_a_card_has_no_port() {
        let health = health_entry(
            "ghost",
            "omlx",
            false,
            UpstreamState::Unavailable,
            None,
            false,
        );
        let element = element(&AppState::default(), health, None);
        assert_eq!(element.port, None);
    }

    #[test]
    fn the_wire_strings_are_exactly_the_routers_own() {
        use UpstreamState::*;
        for state in [
            Ready,
            Starting,
            Unavailable,
            Restarting,
            Failed,
            Stopped,
            Absent,
        ] {
            assert_eq!(state_wire(state).as_str(), state.as_str());
        }
        use UpstreamReason::*;
        for reason in [
            EngineNotFound,
            PortInUse,
            AuthRequired,
            AuthFailed,
            StartupTimeout,
            CrashLoop,
            FencePending,
            BudgetZero,
            BaseUnavailable,
            StartupLatched,
        ] {
            assert_eq!(reason_wire(reason).as_str(), reason.as_str());
        }
    }

    #[tokio::test]
    async fn the_list_reads_the_live_health_state_not_a_copy_taken_at_build_time() {
        let state = state();
        let token = SharedAdminToken::new(AdminSessionToken::mint());
        let secret = token.current().expose_secret().to_owned();
        let live = state.health_state.clone();
        let app = build_admin_app_with_token(state, token);
        let state_of = |body: &Value| element_by_id(body, "lmstudio")["state"].clone();
        let (_, before, _) = send(
            &app,
            request("GET", "/admin/api/v1/upstreams", Some(&secret)),
        )
        .await;
        assert_eq!(state_of(&before), "ready");
        live.apply(
            "lmstudio",
            Observation::Unavailable(Some(UpstreamReason::AuthRequired)),
        );
        let (_, after, _) = send(
            &app,
            request("GET", "/admin/api/v1/upstreams", Some(&secret)),
        )
        .await;
        assert_eq!(state_of(&after), "unavailable");
        assert_eq!(element_by_id(&after, "lmstudio")["reason"], "auth_required");
    }
}

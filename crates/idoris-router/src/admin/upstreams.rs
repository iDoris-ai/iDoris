//! `GET /admin/api/v1/upstreams` 与受限写接口 `POST /admin/api/v1/upstreams/{id}/start|stop|restart`
//! （接口规范 §2.11.4 与勘误七；IDORIS-CORE S6）。
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
//! | 409 | `restart_required` | `restart_required` | 放行时没就绪的上游（降级放行，完整的 Router 里没有它的 Supervisor）的 `start`、`restart`（见 [`crate::lifecycle`]） |
//! | 503 | `idoris_stopping` | `idoris_stopping` | 引擎管理器已经在停止 |
//!
//! 判定的先后：先看 id，再看 `managed`，再看归属（`stop`、`restart` 要 `owned`；已经 `stopped` 的 `stop` 是幂等，不报错），
//! 最后交给管理器（它回答 `restart_required`）。所以 `owned` 为 false 的上游，`restart` 一律是 `upstream_not_owned`
//! （规范 §2.11.4「一律」；冻结的上游没有进程，`owned` 恒为 false），`restart_required` 只出现在冻结上游的 `start` 上。

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
    // 只有 `start` 与 `restart` 会得到它，而且只在放行时没就绪的上游上（`stop` 不会）。
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
        "iDoris is shutting down its engines; the command was not accepted".to_owned(),
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

    #[tokio::test]
    async fn get_is_the_health_snapshot_field_for_field_in_the_same_order() {
        let (app, secret) = app();
        let (status, body, _) =
            send(&app, request("GET", "/admin/api/v1/upstreams", Some(&secret))).await;
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
        let (_, body, _) =
            send(&app, request("GET", "/admin/api/v1/upstreams", Some(&secret))).await;
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
        let (_, body, text) =
            send(&app, request("GET", "/admin/api/v1/upstreams", Some(&secret))).await;
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
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path} {token:?}");
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
            assert_eq!(precheck(Action::Restart, &plain_upstream(true, false, state)), None);
            assert_eq!(
                precheck(Action::Stop, &plain_upstream(true, false, state)),
                Some(Ownership::NotOwned)
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // 路由表
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn the_family_is_exactly_one_get_and_three_posts() {
        let (app, secret) = app();
        let token = Some(secret.as_str());
        for (method, path, expected) in [
            ("GET", "/admin/api/v1/upstreams", StatusCode::OK),
            // 路径对、方法不对：405。
            ("POST", "/admin/api/v1/upstreams", StatusCode::METHOD_NOT_ALLOWED),
            ("PUT", "/admin/api/v1/upstreams", StatusCode::METHOD_NOT_ALLOWED),
            ("DELETE", "/admin/api/v1/upstreams", StatusCode::METHOD_NOT_ALLOWED),
            ("GET", "/admin/api/v1/upstreams/omlx/stop", StatusCode::METHOD_NOT_ALLOWED),
            ("PUT", "/admin/api/v1/upstreams/omlx/stop", StatusCode::METHOD_NOT_ALLOWED),
            ("DELETE", "/admin/api/v1/upstreams/omlx/restart", StatusCode::METHOD_NOT_ALLOWED),
            // 没有的路径：404（没有按 id 读、没有别的动作、没有 `make_default`：那是预留）。
            ("GET", "/admin/api/v1/upstreams/omlx", StatusCode::NOT_FOUND),
            ("POST", "/admin/api/v1/upstreams/omlx", StatusCode::NOT_FOUND),
            ("POST", "/admin/api/v1/upstreams/omlx/pause", StatusCode::NOT_FOUND),
            ("POST", "/admin/api/v1/upstreams/omlx/make_default", StatusCode::NOT_FOUND),
            ("POST", "/admin/api/v1/upstreams/omlx/stop/extra", StatusCode::NOT_FOUND),
            ("POST", "/admin/api/v1/upstreams//stop", StatusCode::NOT_FOUND),
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
        let health = health_entry("ghost", "omlx", false, UpstreamState::Unavailable, None, false);
        let element = element(&AppState::default(), health, None);
        assert_eq!(element.port, None);
    }

    #[test]
    fn the_wire_strings_are_exactly_the_routers_own() {
        use UpstreamState::*;
        for state in [Ready, Starting, Unavailable, Restarting, Failed, Stopped, Absent] {
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
        let (_, before, _) =
            send(&app, request("GET", "/admin/api/v1/upstreams", Some(&secret))).await;
        assert_eq!(state_of(&before), "ready");
        live.apply(
            "lmstudio",
            Observation::Unavailable(Some(UpstreamReason::AuthRequired)),
        );
        let (_, after, _) =
            send(&app, request("GET", "/admin/api/v1/upstreams", Some(&secret))).await;
        assert_eq!(state_of(&after), "unavailable");
        assert_eq!(element_by_id(&after, "lmstudio")["reason"], "auth_required");
    }
}

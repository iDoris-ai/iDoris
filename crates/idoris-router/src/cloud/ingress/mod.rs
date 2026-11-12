//! idoris.ai/api 计算服务 **S5：云入口**（路由层）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §2（部署拓扑）、§3（付款上下文由凭证决定）、§9 第 3 条与
//! 「数据面的租户」、§12 的 S5 一行、§13 的 P0 验收前两条。这是**公网入口**：一处错了，就是别人能冒用付款上下文，
//! 或者一个学校的共享出口把整个班误伤。所以每一条规则都落在一个有测试钉住的纯函数或小模块里：
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`config`] | 云入口配置（YAML）：受信反向代理、各级上限、防滥用、CORS、控制面是否对公网开放、**每个租户的数据处理要求** |
//! | [`net`] | 客户端来源：只信任配置里列出的反向代理的 `X-Forwarded-For`，其它来源的一律不信 |
//! | [`caller`] | 从凭证推导付款上下文、租户、数据类别（成员 id 逐次验证，失败不回落） |
//! | [`limits`] | 平台 / 租户 / 成员三级的请求速率与并发上限（和 S3 的花费闸门互补） |
//! | [`abuse`] | 公网防滥用：没有凭证请求的限流、按凭证前缀的认证失败计数；来源地址只是辅助信号 |
//! | [`bucket`] | 令牌桶（精确整数、时钟可拨）和有上限的键值表 |
//! | [`cors`] | 跨域：默认不开放，开放时只对配置里列出的来源、暴露 S3 的 `EXPOSED_HEADERS` |
//! | [`bind`] | 云形态的公网监听：只有云形态允许非 loopback；Admin v0 永远只听 loopback |
//! | `guard` | 中间件：一个请求依次要过的门 |
//! | [`reason`] | 入口自己的稳定原因码 |
//!
//! # 只在云形态加载
//!
//! 云入口只有在 `IDORIS_FORM=cloud` 时才会被构造（`AppState::cloud_ingress` 为 `Some`），此时 [`crate::build_app`]
//! 交给 [`build_cloud_app`]，整个数据面换成下面的路由表。个人形态、组织形态、存量 tenant 部署永远走原来的 `build_app`，
//! 一个字符都没变。本地租户模式的 `X-iDoris-Tenant` 契约不变。
//!
//! # 路由表（云形态）
//!
//! | 方法 + 路径 | 谁 | 认证 |
//! |---|---|---|
//! | `GET /health` | 既有的健康检查（不变） | 无 |
//! | `GET /api/v1/models` | 云路由簿里的模型名（OpenAI 的 `{"object":"list","data":[…]}`） | 凭证 |
//! | `POST /api/v1/chat/completions` | S4 的云处理函数 | 凭证 |
//! | `/control/v1/*` | S2 控制面——**只有**配置里 `control_plane.public: true` 才挂 | 控制面自己（凭证 + 授权） |
//! | 其它一切 | 404 `{"error":{"type":"not_found"}}`，不认证、不碰库 | — |
//!
//! 本地流水线的路由（`/v1/*`、`/capabilities`、`/idoris/tenants/*`、`/admin/*`）在云形态下**不存在**：
//! 它们是本地形态的契约，要么依赖 `X-iDoris-Tenant`，要么暴露本机信息。不保留 `/v1/*` 的理由：
//! 公开的基址是 `https://idoris.ai/api/v1`，普通 OpenAI 客户端把它当 `base_url` 用；再留一套 `/v1/*` 就是两个入口、
//! 两份要守的规则，而且 `/v1/*` 在本地形态里的含义（租户头、本地模型）在云上是错的。反向代理必须把 `/api/*` 原样转发
//! （不剥前缀）。
//!
//! ## 控制面为什么默认不挂在公网监听上
//!
//! `/control/v1` 是写操作的管理接口（开户、成员、凭证、额度）。它的认证靠凭证、授权靠「操作 × 子树」、来源地址不参与，
//! 所以**可以**安全地对公网开放（一张被盗的凭证最多只能动它自己子树里的东西，平台层面的写操作只有本机运维命令
//! `idoris control bootstrap` 能做）；但这是一个必须有人有意为之的攻击面，所以默认不挂。P0 里 eduai 的后端要用它做在线
//! 开通，配置里写一行 `control_plane.public: true` 就挂在同一个公网监听上（不单开端口：多一个监听器就多一套要保护、
//! 要测试的启动路径，S2 的设计本来就是「挂在数据面的同一个监听上」）。挂上之后它只多一道请求体读取期限，
//! 其它（认证、授权、审计、响应）一概不动，也永远不发 CORS 头：它是后端对后端的接口。
//!
//! # 一个请求的付款上下文怎么来的
//!
//! 见 [`caller`] 的模块文档。要点：凭证决定付款人；**Service 凭证**（eduai 后端）每个请求必须带 `X-iDoris-Member`
//! （[`HEADER_MEMBER`]）提交假名成员 id，用 `AccountStore::payment_context` 逐次验证它在这张凭证的范围内，
//! 失败一律拒绝；**成员凭证**不用带（带了必须是自己）；**节点凭证**不能带。租户和数据类别永远从付款账户推导，
//! 请求没有任何办法指定。
//!
//! # 交接给 S6 / S7
//!
//! - **S6（儿童安全，已接上）**：处理函数能从请求扩展里取到两样东西。[`crate::cloud::CallerPolicy`]（和 S4 一样）——
//!   `caller.tenant` 就是租户的 `TenantDataPolicy`，`caller.tenant.handles_child_data()` 是「这是不是 `child_edu`
//!   租户」，`caller.tenant.data_classes()` 是完整的类别集合，来源是云入口配置里该租户的声明；以及
//!   [`IngressContext`]（推导出来的租户账户 id、主体、凭证 id）。`CallerPolicy.tenant` 原样进 `CloudCall.tenant`，
//!   S6 的执行器就是看 `call.tenant.handles_child_data()` 决定走不走儿童管线，所以**经云入口来的 eduai 请求一定走管线**，
//!   请求里的头、字段、模型名都改不了它。数据类别绑定在租户上，不在模型别名上，换具体模型名绕不过去。
//!   衔接的三处（都在启动时，不等第一个孩子的请求）：云入口配置里声明的儿童租户（a）和静态调用方簿里的儿童调用方
//!   一起过云上游配置的 S6 校验（必须有 `safety:` 段；LLM 检查端点要满足它们每一个的区域 / 留存 / DPA；只用规则检查器要留
//!   警告，见 `cloud::config::parse_with_ingress_tenants`）；（b）`CloudIngress::new` 在云上游没有装安全 gate 时拒绝构造；
//!   （c）进程要做「正文不落盘」的加固（关 core dump、panic 信息脱敏，`bin/idoris.rs`）。S6 的旧推理路由
//!   （`/v1/messages`、`/v1/embeddings`、`/v1/rerank`）在云形态下**根本不存在**（404，不是 403）：没有路由，就没有绕过管线的路。
//! - **S7（本地到云的云端档）**：组织节点用 `Node` 凭证调云，不带 `X-iDoris-Member`；个人云账号用成员凭证。
//!   两者都只需要「地址加密钥」，调 `POST /api/v1/chat/completions`。节点凭证没有主体，成员级上限按（租户，凭证）算。
//!
//! # 对设计稿含糊处的理解（没有悄悄拍板，等 owner 确认）
//!
//! 1. **「租户」在账户树里是什么**：设计稿多处用它（§9 数据面的租户、S5 的三级上限）但没有定义。这里取付款账户的祖先链里
//!    平台根之下（跳过补贴池）的**顶层客户**。备选：Service 凭证绑定的账户；`virtual_keys.tenant_id` 标签
//!    （S2 里它是自由文本，样例里 `tenant-edu` 和账户 id `eduai` 并不相等，所以云入口不读它）。
//! 2. **租户的数据处理要求存在哪**：设计稿说「从账户 / 租户属性推导」，但账户存储里没有这个属性，S2 的控制面也没有改它的
//!    操作。这里放在运维控制的云入口配置的 `tenants:` 里：改它要改文件、重启，没有任何网络入口能动它——有意的，学校的
//!    管理员不能把自己从 `child_edu` 改成 `adult`。没有声明的租户一律拒绝。备选：账户库里一张表，只由本机运维命令写。
//! 3. **计费性质**（付费 / 补贴 / 不计费）同样按租户声明：设计稿没说谁决定一个请求是付费还是补贴。
//! 4. **成员 id 的传法**：请求头 `X-iDoris-Member`；Service 凭证每个数据面请求都要带（包括 `GET /models`——每个请求都有
//!    一个付款上下文）。
//! 5. **请求个数 / 并发上限的原因码**用路由层的新字符串（`reason`），不动 S0 的 `ReasonCode`：那是「钱 / 付款上下文」的契约，
//!    S3 的 `rate_limited` / `in_flight_limited` 仍然是花费闸门。
//! 6. **401 和 403 的分界**：凭证本身的状态（不认识、撤销、冻结、过期）是不透明的 401；凭证有效但这一次的付款上下文建立不了
//!    是带 S0 原因码的 403（eduai 后端要区分「同意撤回」和「后端发错了成员 id」）。
//! 7. **来源地址只是辅助信号**：只给没有凭证的请求分一个软配额（速率，不是封禁）；带有效凭证的请求永远不被来源地址拦住。
//!    失败计数按凭证前缀，不按纯 IP。
//! 8. **限额放在配置里、进程内存里**：重启清零（P0 只有一个活动写节点）；没做持久化，也没做多节点共享。
//! 9. **请求头读取的超时**（slowloris）交给反向代理：`axum::serve` 不暴露 hyper 的 `header_read_timeout`，不引入新依赖做不了。
//!    这里做的是请求体的读取期限，而且只对认证和限流都通过的请求读。
//! 10. **个人云账户直接挂在平台根下**时，每个人自己就是一个租户（要在配置里逐个声明）：建议把 Agent24 的个人云账户放在
//!     一个 `agent24` 组织下，声明一次。
//! 11. **S5 与 S6 真实进程测试的衔接**：S6 的 `cloud_child_safety_binary.rs` 原来用静态调用方簿、旧的虚拟 key 和 `/v1/*` 起
//!     云形态。云入口生效后云形态没有这条路（必须配 `IDORIS_CLOUD_INGRESS_CONFIG`，静态簿必须为空，数据面只有 `/api/v1/*`）。
//!     备选是「没有云入口配置时回落到旧模式」或「加一个非生产开关」，两者都会在公网入口的形态里留下第二个更弱的入口，所以
//!     没有做；这三个测试改在云入口上验收（凭证推导租户，请求走 `/api/v1/chat/completions`），断言的内容不变，只有
//!     「旧路由 403」变成「404」（403 仍由进程内的 S6 测试在非云入口的应用上钉着）。

use std::sync::Arc;

use axum::extract::State;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use idoris_tenancy::accounts::AccountStore;

use super::clock::Clock;
use crate::auth::VirtualKeyAuthenticator;
use crate::models::{ModelEntry, ModelsResponse};
use crate::{AppState, control};

pub mod abuse;
pub mod bind;
pub mod bucket;
pub mod caller;
pub mod config;
pub mod cors;
mod guard;
pub mod limits;
pub mod net;
pub mod reason;

pub use caller::{CallerResolver, IngressContext, Refusal, ResolvedCaller};
pub use config::{IngressConfig, TenantProfile};
pub use limits::{CallerLimits, LevelLimits, LimitKeys, Limiter, Scope};
pub use net::{ClientAddr, TrustedProxies};
pub use reason::IngressReason;

/// 提交假名成员 id 的请求头。**Service 凭证每个请求都必须带**（eduai 后端代理）；成员凭证带了必须是自己；节点凭证不能带。
/// 值是开通方提供的稳定假名 id（`compute-contracts::ids::PrincipalId` 的字符集），不是姓名、邮箱。
pub const HEADER_MEMBER: &str = "X-iDoris-Member";

/// 被限流时说明哪一级拦的（`member` / `tenant` / `platform`），浏览器可读（见 [`cors`]）。
pub const HEADER_LIMIT_SCOPE: &str = "X-iDoris-Limit-Scope";

/// 云入口：配置、认证器、调用方解析、三级限流、防滥用、时钟。挂在 `AppState::cloud_ingress` 上。
pub struct CloudIngress {
    pub(crate) config: Arc<IngressConfig>,
    pub(crate) authenticator: VirtualKeyAuthenticator,
    pub(crate) resolver: Arc<CallerResolver>,
    pub(crate) limiter: Arc<Limiter>,
    pub(crate) anonymous: abuse::AnonymousLimiter,
    pub(crate) failures: abuse::FailureThrottle,
    pub(crate) clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for CloudIngress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudIngress").finish_non_exhaustive()
    }
}

impl CloudIngress {
    /// 组装。`cloud` 用来做一致性检查：云入口生效时，付款上下文只能由凭证解析，
    /// **静态调用方簿（`callers:`，S4 的测试替身）必须是空的**——留着它只会让运维以为它还在起作用。
    pub fn new(
        config: IngressConfig,
        accounts: Arc<AccountStore>,
        authenticator: VirtualKeyAuthenticator,
        clock: Arc<dyn Clock>,
        cloud: &super::CloudUpstream,
    ) -> Result<Self, String> {
        // 处理儿童数据的租户（`child_edu`）的请求一定要走 S6 的儿童安全管线。管线是否生效看执行器里有没有装安全 gate；
        // 没装就在启动时拒绝，不等第一个孩子的请求才发现每个请求都是 503（执行器里还有一道失败即关闭兜底）。
        if config.has_child_data_tenants() && !cloud.has_safety_gate() {
            let names: Vec<String> = config
                .tenants()
                .filter(|tenant| tenant.data.handles_child_data())
                .map(|tenant| tenant.account.to_string())
                .collect();
            return Err(format!(
                "云入口配置里声明了处理儿童数据的租户 [{}]，但云上游没有装儿童安全检查：请在云上游配置里写 safety: 段\
                 （安全检查是儿童业务上线的前提，设计稿 §7）",
                names.join(", ")
            ));
        }
        if !cloud.callers.is_empty() {
            return Err(
                "云上游配置里还有静态调用方 callers:：云入口生效时付款上下文只由凭证解析，\
                 静态调用方簿只是 S4 的测试夹具，请把它从生产配置里删掉（写 callers: [] 或整段去掉）"
                    .to_string(),
            );
        }
        let config = Arc::new(config);
        let now = clock.now();
        Ok(Self {
            resolver: Arc::new(CallerResolver::new(accounts, Arc::clone(&config))),
            limiter: Limiter::new(config.platform, now.0),
            anonymous: abuse::AnonymousLimiter::new(
                config.anonymous_per_source,
                config.anonymous_global,
                now.0,
            ),
            failures: abuse::FailureThrottle::new(
                config.auth_failure_max,
                config.auth_failure_window_secs,
            ),
            config,
            authenticator,
            clock,
        })
    }

    pub fn config(&self) -> &IngressConfig {
        &self.config
    }

    /// 启动时的体检，每条是一句给运维看的警告（不是错误：运维可能先写配置、后建账户）：
    /// 配置里声明的租户在账户库里不存在，或者不是账户树里平台根之下的顶层客户——后者的声明永远不会生效，
    /// 因为租户总是从付款账户往上数的顶层客户（见 [`caller`]）。
    pub fn startup_warnings(&self, accounts: &AccountStore) -> Vec<String> {
        let mut warnings = Vec::new();
        for tenant in self.config.tenants() {
            match accounts.ancestors(&tenant.account) {
                Ok(chain) => {
                    let top = caller::derive_tenant(&chain).map(|account| account.id.clone());
                    if top.as_ref() != Some(&tenant.account) {
                        warnings.push(format!(
                            "租户 {} 不是账户树里平台根之下的顶层客户（它的顶层客户是 {}）：这份声明永远不会生效，\
                             请把声明写在顶层客户上",
                            tenant.account,
                            top.map_or_else(|| "?".to_string(), |id| id.to_string())
                        ));
                    }
                }
                Err(idoris_tenancy::accounts::AccountStoreError::NotFound { .. }) => {
                    warnings.push(format!(
                        "租户 {} 在云入口配置里声明了，但账户库里还没有这个账户：账户建好之前它的请求会被拒绝",
                        tenant.account
                    ));
                }
                Err(error) => {
                    warnings.push(format!("无法检查租户 {}：{error}", tenant.account));
                }
            }
        }
        warnings
    }

    /// 三级限流器（测试和诊断用）。
    pub fn limiter(&self) -> &Arc<Limiter> {
        &self.limiter
    }
}

/// `GET /api/v1/models`：云路由簿里的模型名，OpenAI 的列表形状。
async fn models(State(state): State<Arc<AppState>>) -> Response {
    let Some(cloud) = state.cloud.as_ref() else {
        return crate::error_envelope(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            reason::IngressReason::CloudUnavailable.as_str(),
            "no cloud upstream is configured",
        );
    };
    let data = cloud
        .routes
        .models()
        .map(|id| ModelEntry {
            id: id.to_string(),
            object: "model",
            owned_by: "idoris".to_string(),
        })
        .collect();
    Json(ModelsResponse {
        object: "list",
        data,
    })
    .into_response()
}

/// 云形态的整个 HTTP 应用。见模块文档的路由表和中间件顺序（`guard`）。
///
/// 控制面（`/control/v1`）合并在数据面的层**之后**，和 `build_app` 一样：它不经过数据面的中间件，自己认证、自己授权。
pub fn build_cloud_app(state: AppState, ingress: Arc<CloudIngress>) -> Router {
    let state = Arc::new(state);
    // `MethodRouter::route_layer`：认证只包住真正匹配到方法的处理函数，不包住 `fallback`。所以没有的路径、
    // 路径对但方法不对（包括 OPTIONS）一律是 404，不认证、不碰库，也不会把 404 变成 401。
    let guard = from_fn_with_state(Arc::clone(&ingress), guard::data_plane_guard);
    let mut app = Router::new().route("/health", get(crate::health)).route(
        "/api/v1/models",
        get(models)
            .fallback(crate::not_found)
            .route_layer(guard.clone()),
    );
    if state.cloud.is_some() {
        app = app.route(
            "/api/v1/chat/completions",
            post(super::chat_completions)
                .fallback(crate::not_found)
                .route_layer(guard),
        );
    }
    let mut app = app
        .fallback(crate::not_found)
        .layer(from_fn_with_state(Arc::clone(&ingress), guard::edge_guard))
        .layer(from_fn(crate::connection::request_lifecycle_middleware))
        .layer(from_fn_with_state(
            Arc::clone(&state),
            crate::record_id_middleware,
        ))
        .layer(from_fn(guard::strip_untrusted_headers))
        .with_state(Arc::clone(&state));

    if ingress.config.control_plane_public
        && let Some(plane) = state.control.clone()
    {
        app = app.merge(control::router(plane).layer(from_fn_with_state(
            Arc::clone(&ingress),
            guard::control_body_deadline,
        )));
    }
    app
}

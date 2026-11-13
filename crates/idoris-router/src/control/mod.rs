//! idoris.ai/api 计算服务 **S2：控制面**（设计稿 §9 第 1 项，评审 H5）。
//!
//! 开户、主体、成员关系、凭证、额度、担保（授信）、云账号配对的**写操作**，以及范围内的只读查询。
//! 独立于只读的 Admin v0（`admin.rs`）：Admin v0 一个字没动，仍然只读、只听 loopback、用启动时给的会话 token。
//!
//! # 在哪里
//!
//! 路由前缀 [`CONTROL_PREFIX`] = `/control/v1`，挂在**数据面的同一个监听上**（不是新端口，也不是 Admin 端口），
//! 但完全自带认证和授权，不经过数据面的中间件。**只在组织和云形态下挂载**（`AppState::control` 为 `Some`，
//! 启动时只有显式声明 `IDORIS_FORM=organization|cloud` 才会构造）；个人形态和存量 tenant 部署里这些路径不存在（404），
//! 与改动前一字不差。
//!
//! # 三类密钥互相不通用
//!
//! | 密钥 | 形状 | 能做什么 | 在控制面上 |
//! |---|---|---|---|
//! | 本地数据密钥（`idoris key issue`） | `idk_` + 64 位十六进制 | 数据面 | 认得出，但没有 `credentials` 行，`not_a_control_credential` |
//! | 云凭证（控制面签发 / 配对兑换） | 同上（同一套虚拟 key 机制） | 数据面；带 `admin_scopes` 的还有控制面 | 按它的「操作 × 子树」权限授权 |
//! | Admin v0 会话 token | 64 位十六进制，**没有 `idk_` 前缀** | 只读 Admin 接口 | 格式就不是虚拟 key，`unauthenticated`；反过来云凭证在 Admin 端口也是 401（测试钉住） |
//!
//! # 授权（每个请求，来源地址不参与）
//!
//! 每个请求先认证（`Authorization: Bearer idk_…`），再算「操作 × 目标资源所在子树」是否在调用者的权限范围里
//! （`idoris-compute-contracts::control::authorize`，规则见那个模块的文档）：**下发的权限、额度、数据面上限、有效期
//! 都不能超过授权者本人**。**请求从哪来不参与任何判断**：来自 loopback、来自局域网、带 `X-Forwarded-For: 127.0.0.1`
//! 的请求，没有凭证一律 401。组织局域网不默认互信（评审 H5）。
//!
//! # 审计
//!
//! 每个写操作（成功、被拒、被驳回、出错）在**同一个数据库事务里**落一条只追加的审计行：谁（凭证 id）、什么操作、
//! 目标资源、结果、原因码、时间。没有审计就没有写操作。不记密钥、token、配对码、设备名或正文。
//! 认不出的密钥（没有凭证 id）不写审计，避免垃圾请求写满库。细节见 `idoris-tenancy::accounts::control`。
//!
//! # 路由
//!
//! 写（`POST`，JSON 请求体，未知字段一律拒绝；成功 201 / 200，响应信封见 `wire.rs`）：
//!
//! | 路由 | 操作 | 授权（`ControlOp` × 目标账户） |
//! |---|---|---|
//! | `/principals` | 登记主体（已经被别的组织登记过的 id 是固定的 409 `already_exists`，不带主体和状态） | `create_principal` × `org` |
//! | `/principals/{id}/revoke` | 撤销主体（同意撤回） | `revoke_principal` × **登记它的组织**和它每个成员关系的账户 |
//! | `/accounts` | 建付款账户（子账户） | `create_account` × 父账户 |
//! | `/accounts/{id}/status` | 冻结 / 解冻 / 关闭 | `set_account_status` × 账户自己 |
//! | `/memberships` | 建成员关系（关联别的组织登记的主体还要对登记组织有 `revoke_principal` 权限） | `manage_membership` × 付款账户（管理员关系用组织） |
//! | `/memberships/{id}/revoke` | 撤销成员关系 | 同上 |
//! | `/credentials` | 签发凭证（响应里有一次性明文密钥） | `issue_credential` × 绑定账户，另加「新权限 ⊆ 授权者权限」 |
//! | `/credentials/{id}/status` | 冻结 / 解冻 / 撤销凭证 | `freeze_credential` / `revoke_credential` × 凭证的绑定账户 |
//! | `/allocations` | 发额度（含按 `(成员, 月份)` 幂等的免费额度——窗口必须恰好是这个月、受补贴池该账期总上限约束；授信——`purpose = credit_line`，准入读的就是这笔额度） | `grant_allocation` × **授权方（父）账户**，金额 ≤ 单笔上限 |
//! | `/allocations/{id}/revoke` | 撤销额度（关授信） | 同上（撤销资格 = 发这笔额度的资格） |
//! | `/pairing/codes` | 创建配对码（响应里有一次性明文码） | `issue_credential` × 成员关系的付款账户 |
//! | `/pairing/codes/{id}/revoke` | 撤销还没兑换的码 | 同上 |
//! | `/pairing/redeem` | 兑换配对码（**不带凭证，码本身是凭据**；限速先占名额再查库，一分钟最多 20 次「猜错」） | 无；码有效、未过期、签发者的凭证行和验证器都仍有效、设备名额没满 |
//! | `/pairing/unpair` | 设备解绑自己 | 无管理权限，只能撤销**自己**这张设备凭证 |
//!
//! 读（`GET`，授权 `read` × 被查询的账户，不写审计）：`/whoami`、`/accounts/{id}`、`/accounts/{id}/subtree`、
//! `/accounts/{id}/allocations`、`/accounts/{id}/credentials`、`/accounts/{id}/memberships`、`/principals/{id}`、
//! `/audit?account=…&limit=…&before=…`。
//!
//! 控制面前缀下的其它路径和方法一律 404（JSON）。
//!
//! # 撤销即时生效
//!
//! 撤销 / 冻结凭证、撤销成员关系或主体、冻结或关闭账户，**下一次数据面请求就被拒**：
//! 数据面认证在虚拟 key 校验之后多一道闸门（[`AccountGate`]，`auth::CredentialGate`），每次请求都读库、不缓存。
//! 代理响应缓存的键里没有 key id（S1 的发现），但缓存查找发生在认证**之后**，所以被撤销的凭证根本走不到缓存；
//! `control::tests` 里有一个测试先把缓存预热、再撤销、再重放同一个请求，证明被拒（见交付说明）。

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{any, get, post};
use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::ids::TimestampMs;
use idoris_tenancy::accounts::{AccountStore, GateVerdict};

use crate::auth::{CredentialGate, VirtualKeyAuthError};

pub mod bootstrap;
mod handlers;
mod pairing_codes;
mod wire;

#[cfg(test)]
mod tests;

/// 控制面的路由前缀。
pub const CONTROL_PREFIX: &str = "/control/v1";

/// 请求体上限。控制面的请求都很小；64 个权限范围、一串 id，远不到这个数。
const MAX_BODY_BYTES: usize = 64 * 1024;

/// 控制面用的时钟：测试要能拨。
pub trait ControlClock: Send + Sync {
    fn now(&self) -> TimestampMs;
}

/// 系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemControlClock;

impl ControlClock for SystemControlClock {
    fn now(&self) -> TimestampMs {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            });
        TimestampMs(millis)
    }
}

/// 控制面：账户存储 + 时钟 + 兑换失败的限速器。挂在 `AppState::control` 上。
pub struct ControlPlane {
    accounts: Arc<AccountStore>,
    clock: Arc<dyn ControlClock>,
    redeem_limiter: pairing_codes::FailureLimiter,
}

impl std::fmt::Debug for ControlPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlane").finish_non_exhaustive()
    }
}

impl ControlPlane {
    /// 只有需要控制面的形态（组织、云）能构造；个人形态返回错误，所以个人形态永远没有这个对象。
    pub fn new(accounts: Arc<AccountStore>, form: DeploymentForm) -> Result<Self, String> {
        if !form.profile().control_plane {
            return Err(format!(
                "the control plane does not exist in the {} form",
                form.as_str()
            ));
        }
        Ok(Self {
            accounts,
            clock: Arc::new(SystemControlClock),
            redeem_limiter: pairing_codes::FailureLimiter::standard(),
        })
    }

    /// 换时钟（测试）。
    pub fn with_clock(mut self, clock: Arc<dyn ControlClock>) -> Self {
        self.clock = clock;
        self
    }

    /// 账户存储（运维命令和装配用）。
    pub fn accounts(&self) -> &Arc<AccountStore> {
        &self.accounts
    }
}

/// 数据面认证的最后一道闸门：账户存储里的凭证、成员关系、主体、账户状态。见模块文档「撤销即时生效」。
pub struct AccountGate {
    accounts: Arc<AccountStore>,
}

impl AccountGate {
    pub fn new(accounts: Arc<AccountStore>) -> Self {
        Self { accounts }
    }
}

impl CredentialGate for AccountGate {
    fn check(&self, key_id: &str, now_ms: i64) -> Result<(), VirtualKeyAuthError> {
        match self.accounts.credential_gate(key_id, TimestampMs(now_ms)) {
            // 没有凭证行 = 本地数据密钥，账户存储不管；有凭证行且都有效 = 放行
            Ok(GateVerdict::NotManaged | GateVerdict::Allowed) => Ok(()),
            // 和未知密钥同一个答案：数据面不告诉调用方是哪一种
            Ok(GateVerdict::Denied(_)) => Err(VirtualKeyAuthError::Unauthorized),
            // 判不了：失败即关闭
            Err(_) => Err(VirtualKeyAuthError::Unavailable),
        }
    }
}

/// 构造控制面的 `Router`（状态已绑定，可以直接 `merge` 进数据面的应用）。
pub(crate) fn router(plane: Arc<ControlPlane>) -> Router {
    use handlers::*;
    let not_found = || any(not_found_handler);
    Router::new()
        .route(
            "/control/v1/whoami",
            get(whoami).fallback(not_found_handler),
        )
        // 写
        .route(
            "/control/v1/principals",
            post(create_principal).fallback(not_found_handler),
        )
        .route(
            "/control/v1/principals/{id}/revoke",
            post(revoke_principal).fallback(not_found_handler),
        )
        .route(
            "/control/v1/accounts",
            post(create_account).fallback(not_found_handler),
        )
        .route(
            "/control/v1/accounts/{id}/status",
            post(set_account_status).fallback(not_found_handler),
        )
        .route(
            "/control/v1/memberships",
            post(create_membership).fallback(not_found_handler),
        )
        .route(
            "/control/v1/memberships/{id}/revoke",
            post(revoke_membership).fallback(not_found_handler),
        )
        .route(
            "/control/v1/credentials",
            post(issue_credential).fallback(not_found_handler),
        )
        .route(
            "/control/v1/credentials/{id}/status",
            post(set_credential_status).fallback(not_found_handler),
        )
        .route(
            "/control/v1/allocations",
            post(grant_allocation).fallback(not_found_handler),
        )
        .route(
            "/control/v1/allocations/{id}/revoke",
            post(revoke_allocation).fallback(not_found_handler),
        )
        .route(
            "/control/v1/pairing/codes",
            post(create_pairing_code).fallback(not_found_handler),
        )
        .route(
            "/control/v1/pairing/codes/{id}/revoke",
            post(revoke_pairing_code).fallback(not_found_handler),
        )
        .route(
            "/control/v1/pairing/redeem",
            post(redeem_pairing_code).fallback(not_found_handler),
        )
        .route(
            "/control/v1/pairing/unpair",
            post(unpair_device).fallback(not_found_handler),
        )
        // 读
        .route(
            "/control/v1/accounts/{id}",
            get(read_account).fallback(not_found_handler),
        )
        .route(
            "/control/v1/accounts/{id}/subtree",
            get(read_subtree).fallback(not_found_handler),
        )
        .route(
            "/control/v1/accounts/{id}/allocations",
            get(read_allocations).fallback(not_found_handler),
        )
        .route(
            "/control/v1/accounts/{id}/credentials",
            get(read_credentials).fallback(not_found_handler),
        )
        .route(
            "/control/v1/accounts/{id}/memberships",
            get(read_memberships).fallback(not_found_handler),
        )
        .route(
            "/control/v1/principals/{id}",
            get(read_principal).fallback(not_found_handler),
        )
        .route(
            "/control/v1/audit",
            get(read_audit).fallback(not_found_handler),
        )
        // 前缀下其它的一切：JSON 404（精确的路由优先匹配，这条只兜底）
        .route("/control/v1", not_found())
        .route("/control/v1/{*rest}", not_found())
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(plane)
}

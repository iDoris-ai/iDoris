//! idoris.ai/api 计算服务 **S5：云入口**（路由层）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §2（部署拓扑）、§3（付款上下文由凭证决定）、§9 第 3 条与
//! 「数据面的租户」、§12 的 S5 一行、§13 的 P0 验收前两条。这是**公网入口**：一处错了，就是别人能冒用付款上下文，
//! 或者一个学校的共享出口把整个班误伤。所以每一条规则都落在一个有测试钉住的纯函数或小模块里。
//!
//! 这一步只有不依赖 HTTP 的基础模块：配置、来源（受信反向代理）、令牌桶、三级上限、防滥用、CORS、监听规则、原因码。
//! 凭证推导调用方和 HTTP 应用在后面的提交里接上。

pub mod abuse;
pub mod bind;
pub mod bucket;
pub mod caller;
pub mod config;
pub mod cors;
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

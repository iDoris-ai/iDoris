//! 启动门控（接口规范 §2.7.6「与 Supervisor、围栏、`/health` 的关系」）。
//!
//! 有托管卡时，数据面与 Admin 先服务「starting 体」（[`crate::swap::SwapApp`] 里的 [`crate::build_starting_app`]、
//! [`crate::admin::build_admin_starting_app`]）；本模块的任务等到**所有**托管引擎就绪，再创建完整的 Router（它的 Supervisor 在
//! 创建时对引擎采样一次，所以只能在引擎就绪之后创建，否则会把启动错误粘滞住）并替换上去。`phase` 只在替换完成之后才可能是
//! `ready`。引擎一直没有就绪时替换不会发生，数据面保持 503；引擎后来就绪（用户装好引擎后 `start`、半开重试成功）再替换。
//!
//! 完整的 Router 怎么建由调用方给（[`Finalize`]）：这里只管「什么时候建、建好之后怎么放上去」，所以进程内测试用不着起整个服务。

use std::sync::Arc;

use axum::Router;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::EngineManager;
use crate::readiness::{GateState, HealthState};
use crate::runtime::RuntimeRegistry;
use crate::swap::SwapApp;

/// 门控完成时创建好的完整服务体。
pub struct Finalized {
    pub data: Router,
    pub admin: Router,
    /// 它的 Supervisor 表：交给管理器，之后 Supervisor 的启动错误粘滞会反映到 `/health`。
    pub runtimes: RuntimeRegistry,
}

/// 创建完整的服务体。引擎都已就绪、`gate_open` 已置位时才调用，只调用一次。出错（配置性的，例如 runtime 构造失败）
/// 是启动失败，iDoris 以退出码 1 退出。
pub type Finalize = Box<dyn FnOnce() -> Result<Finalized, String> + Send>;

/// 门控任务的句柄。
pub struct GateTask {
    pub handle: JoinHandle<()>,
    /// 创建完整服务体失败时收到失败原因（之后任务结束）。门控完成或被取消时永远收不到。
    pub failed: oneshot::Receiver<String>,
}

/// 启动门控任务：等所有托管引擎就绪（再确认一次、同时放行 `gate_open`），创建完整服务体，替换两个 [`SwapApp`]，
/// 把门控设为 [`GateState::Open`]。`cancel` 触发后任务结束，不再替换。
pub fn spawn(
    manager: Arc<EngineManager>,
    health: HealthState,
    data: SwapApp,
    admin: SwapApp,
    finalize: Finalize,
    cancel: CancellationToken,
) -> GateTask {
    let (failure, failed) = oneshot::channel();
    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = manager.wait_all_ready() => {}
                () = cancel.cancelled() => return,
            }
            // 就绪之后引擎又失效了（`wait_all_ready` 返回与放行之间）：回去再等，不放行。
            if manager.open_gate() {
                break;
            }
        }
        if cancel.is_cancelled() {
            return;
        }
        match finalize() {
            Ok(finalized) => {
                manager.attach_runtimes(finalized.runtimes);
                data.swap(finalized.data);
                admin.swap(finalized.admin);
                // 先替换，后设 `Open`：`/health` 读到 `Open` 时，数据面已经是完整的 Router。
                health.set_gate(GateState::Open);
            }
            Err(message) => {
                let _ = failure.send(message);
            }
        }
    });
    GateTask { handle, failed }
}

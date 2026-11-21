//! 启动门控（接口规范 §2.7.6「与 Supervisor、围栏、`/health` 的关系」，勘误六）。
//!
//! 有托管卡时，数据面与 Admin 先服务「starting 体」（[`crate::swap::SwapApp`] 里的 [`crate::build_starting_app`]、
//! [`crate::admin::build_admin_starting_app`]）；本模块的任务等到**所有**托管引擎「落定」，再创建完整的 Router（它的 Supervisor
//! 在创建时对引擎采样一次，所以只能在引擎落定之后创建，否则会把启动错误粘滞住）并替换上去。`phase` 只在替换完成之后才可能是
//! `ready`。
//!
//! **落定**（[`EngineManager::wait_all_settled`]）是 `ready`，或不会自己好的终局：配置性失败、`engine_not_found`、需要 key、
//! 熔断……（`starting`、`restarting` 不算，它们还在往前走）。**门控不能被一个起不来的托管引擎无限期拖住**——那样同一个进程里
//! 健康的 resident 卡、自定义上游也跟着 503（接口规范 §2.7.6：`engine_not_found` 时 iDoris 本身继续服务）。所以：
//!
//! - 所有托管引擎都落定：放行。有没就绪的（终局的失败）就是**降级放行**（[`GateOutcome::degraded`]）：完整的 Router 照常服务别的卡，
//!   路由到没就绪的托管卡的请求直接 503 并带上原因，`/health.phase` 是 `degraded`。
//! - 到了 [`EngineManager::gate_deadline`]（管理器启动之后一个启动超时，oMLX 缺省 90 秒）还有引擎没落定：**到点就算它落定**——
//!   [`EngineManager::give_up_unsettled`] 放弃它（停掉半拉起的引擎，`failed` + `startup_timeout`，不再重试），然后降级放行。
//!   不等熔断：熔断最坏要把「每次都等满启动超时」的失败攒够 6 次，要等近 10 分钟，这段时间里别的卡都在 503。
//! - 降级放行之后，没就绪的托管引擎**冻结**：不再半开重试，不再接受 `start`，要重启 iDoris（它们的 Supervisor 在放行时创建，
//!   看到的是一个失败的引擎，之后即使引擎好了也读不到：运行期引擎换代是 v0.6.1 / S10 的事）。
//!
//! 完整的 Router 怎么建由调用方给（[`Finalize`]，它会拿到 [`GateOutcome`]）：这里只管「什么时候建、建好之后怎么放上去」，
//! 所以进程内测试用不着起整个服务。

use std::sync::Arc;

use axum::Router;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{EngineManager, GateOutcome};
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

/// 创建完整的服务体。引擎都已落定、`gate_open` 已置位时才调用，只调用一次；参数是放行的结果（哪些托管上游没就绪：降级放行时
/// 要据此不给它们建 Supervisor、并让路由到它们的请求直接 503）。出错（配置性的，例如 runtime 构造失败）是启动失败，iDoris 以
/// 退出码 1 退出。
pub type Finalize = Box<dyn FnOnce(&GateOutcome) -> Result<Finalized, String> + Send>;

/// 门控任务的句柄。
pub struct GateTask {
    pub handle: JoinHandle<()>,
    /// 创建完整服务体失败（含创建时 panic）时收到失败原因（之后任务结束）。门控完成或被取消时永远收不到。
    pub failed: oneshot::Receiver<String>,
}

/// 启动门控任务：等所有托管引擎落定（到 [`EngineManager::gate_deadline`] 还没落定的放弃），再确认一次、同时放行 `gate_open`，创建完整服务体，
/// 替换两个 [`SwapApp`]，把门控设为 [`GateState::Open`]。`cancel` 触发后任务结束，不再替换。
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
        // 创建完整服务体时 panic（一个 bug）：守卫把它当成启动失败报出去，否则数据面会一直 503 而没人知道为什么。
        let mut report = FailureReport(Some(failure));
        // 等所有引擎落定，最多等到截止时间；到点还没落定的放弃（它们变成终局的 `failed`），然后照常放行，降级。
        // 整个等待有界：确认时又有实例不落定了（一个就绪的引擎又失效了），下一轮仍受同一个截止时间约束。
        let outcome = tokio::select! {
            outcome = manager.open_when_settled() => outcome,
            () = cancel.cancelled() => return,
        };
        if cancel.is_cancelled() {
            return;
        }
        match finalize(&outcome) {
            Ok(finalized) => {
                manager.attach_runtimes(finalized.runtimes);
                data.swap(finalized.data);
                admin.swap(finalized.admin);
                // 先替换，后设 `Open`：`/health` 读到 `Open` 时，数据面已经是完整的 Router。
                health.set_gate(GateState::Open);
            }
            Err(message) => report.send(message),
        }
    });
    GateTask { handle, failed }
}

/// 把失败原因交给 [`GateTask::failed`]；任务 panic 时（守卫在展开中被丢弃）报一条通用的原因。
struct FailureReport(Option<oneshot::Sender<String>>);

impl FailureReport {
    fn send(&mut self, message: String) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(message);
        }
    }
}

impl Drop for FailureReport {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.send("启动门控任务 panic，无法创建完整的服务".to_owned());
        }
    }
}

//! 优雅停止的「为什么停」与「怎么收尾」（接口规范 §2.7.5、§2.10.4、§2.10.8；勘误十二）。
//!
//! # 停止有五个来源，同一轮里同时成立时按优先级取一个
//!
//! | 来源 | 退出码 | 性质 |
//! |---|---|---|
//! | 自有引擎失效（门控完成之后进程退出） | 5 | 发布出来的状态：发布之后一直成立 |
//! | 孤儿超时 | 4 | 同上（登记客户端发布，[`crate::register::Registration::wants_stop`]） |
//! | agent24d 的 `instance_conflict` | 3 | 同上 |
//! | 门控完成时创建完整服务体失败（启动失败） | 1 | 同上（[`crate::lifecycle::gate::GateTask::failed`]） |
//! | 信号（SIGTERM、SIGINT） | 0 | 一次性事件 |
//!
//! 前四者发布之后一直成立，谁来读都读得到；信号只有一次。它们在同一轮里同时成立（登记任务已经发布了 `instance_conflict`，
//! 这时 SIGTERM 也到了，而停止流程还没轮到被 poll）时，无 `biased` 的 `tokio::select!` 是**随机**挑一个：已经成立的退出原因
//! 会被信号盖成退出码 0，`stop_on_exit:"never"` 时本该停的引擎被留下，退出码 5 丢失（launcher 的崩溃计数少一次），
//! `bye` 的策略也跟着变（H4）。所以优先级写死在 [`StopSources`] 的字段顺序里（[`StopSources::first`] 按这个顺序、`biased` 地轮询），
//! 已经成立的永远排在信号前面：
//!
//! **5（引擎失效）> 4（孤儿超时）> 3（`instance_conflict`）> 启动失败（1）> 信号（0）**
//!
//! 4 与 3 来自同一个登记客户端，它发布一个原因就结束，所以不会同时有两个；它们之间的先后只在文字上写明。
//! 启动失败排在 3、4、5 之后、信号之前：门控没能创建完整服务体是一个真实的故障，不能被一个顺手的 SIGTERM 盖成「干净退出」，
//! 但 3、4、5 比它更具体地告诉 launcher 该怎么办（3 不要重启，4 按预期停止，5 计入崩溃熔断并走全新的启动门控）。
//!
//! # 只锁存一次，同一个结果驱动一切
//!
//! 决定（[`StopSources::first`]）之后，结果放进 [`StopLatch`]（先到先得的一次性槽：之后到的原因一概不改它）。
//! 退出码、`bye`、对自有引擎的处置都从这**一个**[`StopVerdict`] 推出（[`StopPlan::new`]、[`engines_stop_after_drain`]），
//! 没有第二处各自再判断一次。
//!
//! # 引擎的处置（H5）
//!
//! `stop_on_exit:"never"` 只对退出码 0 有效（§2.7.5）。信号一到就放下所有权句柄、之后排空失败以 1 退出，会留下一个没人管的引擎。
//! 所以 `never` 的信号停止（[`EnginePlan::DecideAfterDrain`]）在排空期间**保留所有权句柄**，排空结束、退出结果出来之后才定：
//! 以 0 退出就移交（引擎继续跑，所有权记录留给下一个 iDoris），否则停止并回收。其余情形（`owned`、退出码 3、4、5、启动失败）
//! 原因一确定就与排空并行地停（[`EnginePlan::StopNow`]），总时长仍在 §2.7.5 的预算内。

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinHandle;

use crate::launcher_frame::StopOnExit;
use crate::lifecycle::EngineManager;
use crate::register::StopCause;

/// 进程为什么停。只记一次（[`StopLatch`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopVerdict {
    /// 预期的停止：信号、登记客户端要求停止（`instance_conflict`、孤儿超时）、自有引擎失效。
    Stop(StopCause),
    /// 启动失败：门控完成时创建完整的服务体失败，带原因文本。不是任何一种预期的停止：退出码 1，不发 `bye`，停引擎。
    StartupFailed(String),
}

/// 停止原因的一次性槽：先到先得，之后到的原因一概不改它。
#[derive(Debug, Default)]
pub struct StopLatch {
    slot: OnceLock<StopVerdict>,
}

impl StopLatch {
    /// 锁存 `verdict`，返回**生效的**那一个：槽是空的就是 `verdict`，已经有了就是先到的那个（`verdict` 被丢掉）。
    pub fn latch(&self, verdict: StopVerdict) -> &StopVerdict {
        self.slot.get_or_init(|| verdict)
    }

    /// 已经锁存的停止原因；还没有时是 `None`（停止流程没有走到决定这一步：例如服务自己倒了）。
    pub fn get(&self) -> Option<&StopVerdict> {
        self.slot.get()
    }
}

/// 停止的各个来源。字段按**优先级从高到低**排列（[`Self::first`] 就按这个顺序轮询），写成命名字段是为了不会把位置参数的
/// 顺序传错（信号与引擎失效的 `Output` 都是 `()`，传反了编译器看不出来）。
///
/// 每个来源都是一个 future：**已经发布的状态在第一次被 poll 时就就绪**（`watch` 里的当前值、已经发出的 oneshot），
/// 没有的来源传 `std::future::pending()`。
pub struct StopSources<E, R, G, S> {
    /// 自有引擎失效（退出码 5）。
    pub engine_failed: E,
    /// 登记客户端要求停止：孤儿超时（4）或 `instance_conflict`（3）。
    pub registration: R,
    /// 门控创建完整服务体失败（启动失败，退出码 1）。
    pub gate_failed: G,
    /// SIGTERM、SIGINT（退出码 0）。
    pub signal: S,
}

impl<E, R, G, S> StopSources<E, R, G, S>
where
    E: Future<Output = ()>,
    R: Future<Output = StopCause>,
    G: Future<Output = String>,
    S: Future<Output = ()>,
{
    /// 等到第一个来源成立，同一轮里有几个同时成立就取优先级最高的那个。
    pub async fn first(self) -> StopVerdict {
        let Self {
            engine_failed,
            registration,
            gate_failed,
            signal,
        } = self;
        // `biased`：同一次 poll 里按书写顺序取第一个就绪的，不随机；已经发布的状态一被 poll 就就绪，所以排在信号前面的永远赢信号。
        tokio::select! {
            biased;
            () = engine_failed => StopVerdict::Stop(StopCause::EngineFailed),
            cause = registration => StopVerdict::Stop(cause),
            message = gate_failed => StopVerdict::StartupFailed(message),
            () = signal => StopVerdict::Stop(StopCause::Signal),
        }
    }
}

/// 引擎在停止流程里的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePlan {
    /// 原因一确定就开始停止自己拥有的引擎，与 HTTP 排空并行（§2.7.5 第 2、3 步）。
    StopNow,
    /// 排空期间保留所有权句柄（引擎照常跑，所有权记录不动）；排空结束、退出结果出来之后才定（[`engines_stop_after_drain`]）。
    /// 只有 `stop_on_exit:"never"` 的信号停止走这条（[`StopCause::stops_owned_engines`] 为假的唯一情形）。
    DecideAfterDrain,
}

/// 由**一个** [`StopVerdict`] 推出的收尾安排：通知登记客户端（决定发不发 `bye`）与引擎的处置都从这里来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPlan {
    /// 要告诉登记客户端的停止原因（[`StopCause::says_bye`] 决定发不发 `bye`）。启动失败不是预期的停止，没有。
    pub registration: Option<StopCause>,
    pub engines: EnginePlan,
}

impl StopPlan {
    pub fn new(verdict: &StopVerdict, stop_on_exit: StopOnExit) -> Self {
        match verdict {
            StopVerdict::Stop(cause) => Self {
                registration: Some(*cause),
                engines: if cause.stops_owned_engines(stop_on_exit) {
                    EnginePlan::StopNow
                } else {
                    EnginePlan::DecideAfterDrain
                },
            },
            // 启动失败：没有对外服务过，留着引擎没有意义，一律停。
            StopVerdict::StartupFailed(_) => Self {
                registration: None,
                engines: EnginePlan::StopNow,
            },
        }
    }
}

/// [`EnginePlan::DecideAfterDrain`] 的引擎在排空结束之后怎么办：**只有以退出码 0 退出才留给下一个 iDoris**（§2.7.5：保留引擎只限
/// 退出码 0），退出结果是 1（排空超时、订阅收尾失败）就停止并回收，否则会留下一个没人管的引擎。
pub fn engines_stop_after_drain(exit: &Result<u8, String>) -> bool {
    !matches!(exit, Ok(0))
}

/// 引擎管理器对停止流程暴露的两个动作；测试里换成会记录调用的假货。
#[async_trait]
pub trait EngineControl: Send + Sync + 'static {
    /// 自有引擎失效（门控完成之后进程退出）已经发布时立即返回；之前、或永远没有时不返回。
    async fn failed(&self);
    /// 收尾：`stop_engines` 为真就停掉自己拥有的引擎并回收所有权记录；为假就让它继续跑，所有权记录留给下一个 iDoris。
    async fn shutdown(&self, stop_engines: bool);
}

#[async_trait]
impl EngineControl for EngineManager {
    async fn failed(&self) {
        let _ = self.engine_failed().await;
    }

    async fn shutdown(&self, stop_engines: bool) {
        EngineManager::shutdown(self, stop_engines).await;
    }
}

/// 停止原因确定时调用：[`EnginePlan::StopNow`] 就在后台开始停（返回的句柄交给 [`finish_engines`]）；
/// [`EnginePlan::DecideAfterDrain`] 什么也不做，所有权句柄保留。
pub fn begin_engines<E: EngineControl>(
    engines: &Arc<E>,
    plan: EnginePlan,
) -> Option<JoinHandle<()>> {
    match plan {
        EnginePlan::StopNow => {
            let engines = engines.clone();
            Some(tokio::spawn(async move { engines.shutdown(true).await }))
        }
        EnginePlan::DecideAfterDrain => None,
    }
}

/// 排空结束、退出结果 `exit` 已经出来之后收尾引擎，至多等 `wait`。`started` 是 [`begin_engines`] 返回的句柄；`plan` 是
/// 当时锁存的停止原因推出的安排（`None` = 没有锁存任何原因：服务自己倒了，没有走停止流程，现在才停）。
pub async fn finish_engines<E: EngineControl>(
    engines: &E,
    started: Option<JoinHandle<()>>,
    plan: Option<EnginePlan>,
    exit: &Result<u8, String>,
    wait: Duration,
) {
    match (started, plan) {
        // 已经在后台停了（与排空并行）：等它做完。
        (Some(handle), _) => {
            let _ = tokio::time::timeout(wait, handle).await;
        }
        // `never` 的信号停止：现在退出结果出来了，以 0 退出才放手，否则停止并回收。
        (None, Some(EnginePlan::DecideAfterDrain)) => {
            let stop = engines_stop_after_drain(exit);
            let _ = tokio::time::timeout(wait, engines.shutdown(stop)).await;
        }
        // 没有锁存原因（服务自己倒了），或 `StopNow` 却没有句柄（不应该发生）：现在才停。
        (None, _) => {
            let _ = tokio::time::timeout(wait, engines.shutdown(true)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::future::pending;
    use std::sync::Mutex;

    use super::*;

    /// 每个来源的就绪与否：`true` 就绪（已经发布的状态），`false` 永不就绪。
    fn sources(
        engine: bool,
        registration: Option<StopCause>,
        gate: bool,
        signal: bool,
    ) -> StopSources<
        impl Future<Output = ()>,
        impl Future<Output = StopCause>,
        impl Future<Output = String>,
        impl Future<Output = ()>,
    > {
        StopSources {
            engine_failed: async move {
                if !engine {
                    pending::<()>().await;
                }
            },
            registration: async move {
                match registration {
                    Some(cause) => cause,
                    None => pending::<StopCause>().await,
                }
            },
            gate_failed: async move {
                if gate {
                    "gate".to_string()
                } else {
                    pending::<String>().await
                }
            },
            signal: async move {
                if !signal {
                    pending::<()>().await;
                }
            },
        }
    }

    /// 参照排序：与实现无关地写出规范里的优先级。
    fn rank(verdict: &StopVerdict) -> u8 {
        match verdict {
            StopVerdict::Stop(StopCause::EngineFailed) => 4,
            StopVerdict::Stop(StopCause::OrphanTimeout) => 3,
            StopVerdict::Stop(StopCause::InstanceConflict) => 2,
            StopVerdict::StartupFailed(_) => 1,
            StopVerdict::Stop(StopCause::Signal) => 0,
        }
    }

    /// 同一轮里任意几个来源同时就绪：结果永远是其中优先级最高的（5 > 4 > 3 > 启动失败 > 信号）。每种组合重复多次：
    /// 去掉 `biased` 之后 `select!` 是随机挑，单次碰巧挑对的概率不小，重复几十次才压得住。
    #[tokio::test]
    async fn the_highest_priority_among_simultaneously_ready_sources_always_wins() {
        let mut checked = 0;
        for engine in [false, true] {
            for registration in [
                None,
                Some(StopCause::InstanceConflict),
                Some(StopCause::OrphanTimeout),
            ] {
                for gate in [false, true] {
                    for signal in [false, true] {
                        if !engine && registration.is_none() && !gate && !signal {
                            continue;
                        }
                        let mut expected = Vec::new();
                        if engine {
                            expected.push(StopVerdict::Stop(StopCause::EngineFailed));
                        }
                        if let Some(cause) = registration {
                            expected.push(StopVerdict::Stop(cause));
                        }
                        if gate {
                            expected.push(StopVerdict::StartupFailed("gate".to_string()));
                        }
                        if signal {
                            expected.push(StopVerdict::Stop(StopCause::Signal));
                        }
                        let expected = expected.into_iter().max_by_key(rank).unwrap();
                        for round in 0..64 {
                            let got = sources(engine, registration, gate, signal).first().await;
                            assert_eq!(
                                got, expected,
                                "round {round}: engine={engine} registration={registration:?} gate={gate} signal={signal}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(
            checked, 23,
            "2 x 3 x 2 x 2 combinations minus the empty one"
        );
    }

    /// 单独一个来源成立就是它自己（信号也是）；全不成立时不返回。
    #[tokio::test]
    async fn a_lone_source_is_the_verdict_and_nothing_ready_never_returns() {
        assert_eq!(
            sources(false, None, false, true).first().await,
            StopVerdict::Stop(StopCause::Signal)
        );
        assert_eq!(
            sources(true, None, false, false).first().await,
            StopVerdict::Stop(StopCause::EngineFailed)
        );
        assert_eq!(
            sources(false, Some(StopCause::OrphanTimeout), false, false)
                .first()
                .await,
            StopVerdict::Stop(StopCause::OrphanTimeout)
        );
        assert_eq!(
            sources(false, None, true, false).first().await,
            StopVerdict::StartupFailed("gate".to_string())
        );
        let waiting = tokio::time::timeout(
            Duration::from_millis(50),
            sources(false, None, false, false).first(),
        )
        .await;
        assert!(waiting.is_err(), "no source is ready: keep waiting");
    }

    /// 来源先后到达（不是同一轮）：先到的先得。信号先到并被锁存之后，再来的 5、4、3 不改结果。
    #[test]
    fn the_latch_keeps_the_first_cause_and_ignores_later_ones() {
        let latch = StopLatch::default();
        assert_eq!(latch.get(), None);
        assert_eq!(
            latch.latch(StopVerdict::Stop(StopCause::Signal)),
            &StopVerdict::Stop(StopCause::Signal)
        );
        for later in [
            StopVerdict::Stop(StopCause::EngineFailed),
            StopVerdict::Stop(StopCause::OrphanTimeout),
            StopVerdict::StartupFailed("late".to_string()),
        ] {
            assert_eq!(
                latch.latch(later),
                &StopVerdict::Stop(StopCause::Signal),
                "first come, first served"
            );
        }
        assert_eq!(latch.get(), Some(&StopVerdict::Stop(StopCause::Signal)));
    }

    /// 真并发：几个线程同时锁存各不相同的原因，所有线程读到的是同一个赢家（先到先得，不是「检查再写」两步，中间没有缝）。
    #[test]
    fn concurrent_latches_agree_on_one_winner() {
        let causes = [
            StopCause::Signal,
            StopCause::InstanceConflict,
            StopCause::OrphanTimeout,
            StopCause::EngineFailed,
        ];
        for round in 0..200 {
            let latch = Arc::new(StopLatch::default());
            let barrier = Arc::new(std::sync::Barrier::new(causes.len()));
            let workers: Vec<_> = causes
                .into_iter()
                .map(|cause| {
                    let (latch, barrier) = (latch.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        latch.latch(StopVerdict::Stop(cause)).clone()
                    })
                })
                .collect();
            let seen: Vec<StopVerdict> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            let winner = latch.get().unwrap().clone();
            assert!(
                seen.iter().all(|verdict| *verdict == winner),
                "round {round}: {seen:?} vs {winner:?}"
            );
        }
    }

    /// `bye` 与引擎处置从同一个结果推出：同一轮里信号与别的原因同时成立，退出码、`bye`、引擎处置都是优先级最高的那个的。
    #[tokio::test]
    async fn bye_and_the_engine_plan_follow_the_one_verdict_even_when_the_signal_was_ready_too() {
        // 引擎失效（5）加信号，`never`：5 一律停引擎（即使 `never`），并且发 `bye`；不是信号那条「保留」的路。
        let verdict = sources(true, None, false, true).first().await;
        assert_eq!(verdict, StopVerdict::Stop(StopCause::EngineFailed));
        let plan = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(
            plan,
            StopPlan {
                registration: Some(StopCause::EngineFailed),
                engines: EnginePlan::StopNow
            }
        );
        assert!(plan.registration.unwrap().says_bye());
        assert_eq!(plan.registration.unwrap().exit_code(), 5);

        // `instance_conflict`（3）加信号，`never`：3 停引擎、不发 `bye`、退出码 3。
        let verdict = sources(false, Some(StopCause::InstanceConflict), false, true)
            .first()
            .await;
        let plan = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(plan.engines, EnginePlan::StopNow);
        assert!(!plan.registration.unwrap().says_bye());
        assert_eq!(plan.registration.unwrap().exit_code(), 3);

        // 孤儿超时（4）加信号：同理，退出码 4，不发 `bye`。
        let verdict = sources(false, Some(StopCause::OrphanTimeout), false, true)
            .first()
            .await;
        let plan = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(plan.engines, EnginePlan::StopNow);
        assert!(!plan.registration.unwrap().says_bye());
        assert_eq!(plan.registration.unwrap().exit_code(), 4);

        // 只有信号：`owned` 立刻停，`never` 保留到排空之后；都发 `bye`。
        let verdict = sources(false, None, false, true).first().await;
        let owned = StopPlan::new(&verdict, StopOnExit::Owned);
        assert_eq!(owned.engines, EnginePlan::StopNow);
        let never = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(never.engines, EnginePlan::DecideAfterDrain);
        assert!(never.registration.unwrap().says_bye());

        // 启动失败：不是预期的停止，不发 `bye`（没有通知登记客户端的原因），一律立刻停引擎，与 `stop_on_exit` 无关。
        for mode in [StopOnExit::Owned, StopOnExit::Never] {
            assert_eq!(
                StopPlan::new(&StopVerdict::StartupFailed("x".into()), mode),
                StopPlan {
                    registration: None,
                    engines: EnginePlan::StopNow
                }
            );
        }
    }

    #[test]
    fn engines_are_kept_only_for_a_clean_exit_zero() {
        assert!(!engines_stop_after_drain(&Ok(0)));
        // 退出码 1（排空失败的 `Err`）以及任何别的退出码：停止并回收。
        assert!(engines_stop_after_drain(&Err(
            "优雅停机超过 10s".to_string()
        )));
        for code in [1, 3, 4, 5] {
            assert!(engines_stop_after_drain(&Ok(code)), "{code}");
        }
    }

    /// 记录调用的假引擎管理器。`log` 里除了 `shutdown(..)`，测试还往里放「排空」的标记，以便断言先后。
    #[derive(Default)]
    struct FakeEngines {
        log: Mutex<Vec<String>>,
    }

    impl FakeEngines {
        fn note(&self, line: &str) {
            self.log.lock().unwrap().push(line.to_string());
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EngineControl for FakeEngines {
        async fn failed(&self) {
            pending::<()>().await;
        }

        async fn shutdown(&self, stop_engines: bool) {
            self.note(&format!("shutdown({stop_engines})"));
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// H5：`never` 的信号停止，排空期间引擎一个动作也没有（所有权句柄保留）；排空成功（以 0 退出）才移交（不停）。
    #[tokio::test]
    async fn never_keeps_the_handle_during_the_drain_and_hands_over_only_after_a_clean_drain() {
        let engines = Arc::new(FakeEngines::default());
        let plan = StopPlan::new(&StopVerdict::Stop(StopCause::Signal), StopOnExit::Never);
        let started = begin_engines(&engines, plan.engines);
        assert!(started.is_none(), "nothing runs in parallel with the drain");
        engines.note("drain");
        tokio::task::yield_now().await;
        assert_eq!(
            engines.log(),
            ["drain"],
            "no stop before the drain result is known"
        );
        engines.note("drain-ok");
        finish_engines(engines.as_ref(), started, Some(plan.engines), &Ok(0), WAIT).await;
        assert_eq!(engines.log(), ["drain", "drain-ok", "shutdown(false)"]);
    }

    /// H5：同一条路径，排空失败（以退出码 1 退出）：停止并回收，不留一个没人管的引擎。
    #[tokio::test]
    async fn never_stops_and_reclaims_the_engine_when_the_drain_fails() {
        let engines = Arc::new(FakeEngines::default());
        let plan = StopPlan::new(&StopVerdict::Stop(StopCause::Signal), StopOnExit::Never);
        let started = begin_engines(&engines, plan.engines);
        assert!(started.is_none());
        engines.note("drain-failed");
        finish_engines(
            engines.as_ref(),
            started,
            Some(plan.engines),
            &Err("优雅停机超过 10s，强制退出".to_string()),
            WAIT,
        )
        .await;
        assert_eq!(engines.log(), ["drain-failed", "shutdown(true)"]);
    }

    /// 其余情形原因一确定就停（与排空并行）：`finish_engines` 只等那个已经在跑的任务，不再发第二次 `shutdown`。
    #[tokio::test]
    async fn every_other_cause_stops_engines_at_once_and_finish_only_waits_for_that() {
        for (verdict, mode) in [
            (StopVerdict::Stop(StopCause::Signal), StopOnExit::Owned),
            (
                StopVerdict::Stop(StopCause::EngineFailed),
                StopOnExit::Never,
            ),
            (
                StopVerdict::Stop(StopCause::OrphanTimeout),
                StopOnExit::Never,
            ),
            (
                StopVerdict::Stop(StopCause::InstanceConflict),
                StopOnExit::Never,
            ),
            (StopVerdict::StartupFailed("x".into()), StopOnExit::Never),
        ] {
            let engines = Arc::new(FakeEngines::default());
            let plan = StopPlan::new(&verdict, mode);
            let started = begin_engines(&engines, plan.engines);
            assert!(started.is_some(), "{verdict:?} {mode:?}: stops in parallel");
            // 与排空并行：停止在「排空结束」之前就已经开始（后台任务在这里让出之后就跑起来，不等 `finish_engines`）。
            for _ in 0..100 {
                if !engines.log().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            engines.note("drain-finished");
            finish_engines(
                engines.as_ref(),
                started,
                Some(plan.engines),
                &Ok(plan.registration.map_or(1, StopCause::exit_code)),
                WAIT,
            )
            .await;
            assert_eq!(
                engines.log(),
                ["shutdown(true)", "drain-finished"],
                "{verdict:?} {mode:?}: one stop, started before the drain finished"
            );
        }
    }

    /// 停止流程没走到决定这一步（服务自己倒了）：没有原因、没有句柄，现在才停。
    #[tokio::test]
    async fn a_server_that_died_on_its_own_still_gets_its_engines_stopped() {
        let engines = FakeEngines::default();
        finish_engines(
            &engines,
            None,
            None,
            &Err("data server exited".to_string()),
            WAIT,
        )
        .await;
        assert_eq!(engines.log(), ["shutdown(true)"]);
    }
}

//! 发送前重新确认监听者的守卫（IDORIS-CORE S6，Codex 终审 H1；owner 2026-11-22 拍板；主会话之后要求「每个请求的确认 p99 < 5 毫秒、
//! 请求路径上不起子进程」；Codex 专项评审 H1、H7、M9、M13 又改了其中几处）。
//!
//! 引擎管理器登记一个端点的口径（私有 key、外部）时，把一个 [`EndpointGuard`] 一起交给 [`crate::omlx::key`] 的登记表；之后发往这个
//! 端点的每个请求，在**发出之前**由 [`crate::omlx::key::authorize_omlx_request`] 在发送它的那个任务里调守卫。确认分成两半，各有各的
//! 代价与窗口：
//!
//! - **每个请求（这里）**：登记时认的**那个进程**（pid 加启动时间，建守卫那一刻钉死）此刻还握着一个 **`dial` 这个地址连得到的**
//!   LISTEN 套接字（[`ProcessInspector::watch_listener`]：macOS 的 libproc、Linux 的 `/proc`，几个系统调用，不起子进程；Linux 上还读
//!   `/proc/net/tcp` 到这个套接字的那一行：它要还在 LISTEN 状态）。引擎放掉了端口、进程没了、pid 被回收成了另一个进程，下一个请求就
//!   发现。守卫验证的地址就是授权结果带出去的拨号地址（[`EndpointGuard::dial`]）：发送端只拨它，不拨卡里写的别名。
//! - **每个监督节拍和每次状态变化（引擎管理器，不在请求路径上）**：贵的那些——自有引擎「这个端口上只有它一个监听者」（`lsof`，
//!   [`ProcessInspector::listens_on`]），外部引擎的 uid、签名团队与「每个**能接这个连接**的监听者」的身份（[`ListenerVerifier::check`]）。
//!   引擎还握着它自己的套接字、另一个进程在同一个端口上绑了一个更具体的地址（一个并存的监听者），每个请求的确认看不见，要等
//!   监督的下一次检查才看见（自有引擎每个 `liveness_interval`，外部引擎每个探测间隔；各自的上限见规范勘误十（8））。
//!
//! **退路**（原生监视建不出来：别的 unix、读不了那个进程；或某一次读不出来）：每个请求问那个贵的问题——自有引擎 `listens_on`，外部
//! 引擎 [`ListenerVerifier::check`]——而肯定的答案至多重用 [`ProcessInspector::fallback_ttl`]（真的检查器是 [`super::FALLBACK_TTL`]，
//! 250 毫秒；测试里编排好的假检查器是 0，每个请求都问）。这个重用窗口：**同一时刻只有一个贵的检查在途**（别的请求排队等它，轮到时
//! 直接拿它的结论：是、不、读不出来都一样，不各起各的 `lsof`）；窗口从那个检查**开始**时起算，不是它结束时；一次原生的「不握了」
//! 让正在跑的检查答出的「是」作废（不会回填）；「不」与读不出来不进窗口。
//!
//! 确认不了（读不出来）就是 [`Verification::Unverifiable`]，调用方拒绝发送（fail closed）：读一次不行的立刻再问一遍，仍不行才算。
//!
//! **没有消除的间隙**：确认与发送之间仍有毫秒级的间隙；「并存的监听者」到监督的下一次检查才被发现；原生身份没有绑到通过签名校验的
//! 那个身份上（pid 复用的窗口）；`exec` 之后保留监听 fd 的进程沿用旧的校验；连接失败不使退路的缓存作废。这些是 v0.6.1 的已知限制，
//! 规范勘误十（8）逐条写明。

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::inspect::{ListenerWatch, ProcessInspector};
use super::listener::{ListenerIdentity, ListenerVerdict, ListenerVerifier};
use crate::omlx::key::{EndpointGuard, Verification};

/// 退路上那个贵的问题的一次答案。
enum Slow {
    Yes,
    No,
    Unreadable,
}

/// 退路上那个贵的问题的**肯定**答案的重用窗口，加上「同一时刻只有一个在途」，等它的请求共用它的答案。
#[derive(Debug)]
struct Reuse {
    ttl: Duration,
    state: Mutex<ReuseState>,
    /// 同一时刻只有一个贵的检查在途：别的请求等它，轮到时直接拿它的答案（是、不、读不出来都一样），不各问各的。
    one_at_a_time: tokio::sync::Mutex<()>,
}

#[derive(Debug)]
struct ReuseState {
    /// 每次失效加一：一个检查开始时的代数与它结束时的不同，说明它跑的时候有一次原生的「不握了」，它的答案比那件事旧。
    epoch: u64,
    /// 答出「是」的那个检查**开始**的时刻（窗口从这里起算）。
    yes_from: Option<Instant>,
    /// 做完的检查数，和最近一个的结论：一个请求排队的时候又有一个检查做完了，那个检查的结论就是它的结论。
    finished: u64,
    last: Verification,
}

impl Reuse {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            state: Mutex::new(ReuseState {
                epoch: 0,
                yes_from: None,
                finished: 0,
                last: Verification::Unverifiable,
            }),
            one_at_a_time: tokio::sync::Mutex::new(()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ReuseState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 原生监视说「不握了」：之前记下的「是」不再算数，而且此刻正在跑的贵的检查答出的「是」也不能再存进来。
    fn invalidate(&self) {
        let mut state = self.state();
        state.epoch += 1;
        state.yes_from = None;
    }

    /// 贵的检查：窗口之内（上一次的「是」还没过 `ttl`；`ttl` 为 0 就从不重用：`elapsed() < 0` 永远是假）直接过；否则一次一个地问
    /// （`ask`），「是」记进窗口；排队的时候有别的检查做完了，就用它的结论。
    async fn verify<F, Fut>(&self, ask: F) -> Verification
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Slow>,
    {
        let arrived = {
            let state = self.state();
            if state.yes_from.is_some_and(|from| from.elapsed() < self.ttl) {
                return Verification::Verified;
            }
            state.finished
        };
        let _only_one = self.one_at_a_time.lock().await;
        let (started, epoch) = {
            let state = self.state();
            if state.finished != arrived {
                // 我们排队的时候有一个检查做完了：它就是这一次的检查（合并），不再各问各的。
                return state.last;
            }
            (Instant::now(), state.epoch)
        };
        let answer = ask().await;
        let mut state = self.state();
        let verdict = if state.epoch != epoch {
            // 它跑的时候有一次原生的「不握了」：这个答案比那件事旧，不存，「是」也不放行。
            match answer {
                Slow::Yes | Slow::No => Verification::Mismatch,
                Slow::Unreadable => Verification::Unverifiable,
            }
        } else {
            match answer {
                Slow::Yes => {
                    state.yes_from = Some(started);
                    Verification::Verified
                }
                Slow::No => {
                    state.yes_from = None;
                    Verification::Mismatch
                }
                Slow::Unreadable => {
                    state.yes_from = None;
                    Verification::Unverifiable
                }
            }
        };
        state.finished += 1;
        state.last = verdict;
        verdict
    }
}

/// 自有引擎的守卫：它的进程（`pid`）此刻还握着一个 `dial` 这个地址连得到的 LISTEN 套接字。
pub struct OwnedListenerGuard {
    inspector: Arc<dyn ProcessInspector>,
    pid: u32,
    dial: SocketAddr,
    /// 每个请求的原生监视（建守卫那一刻钉死进程的身份）；检查器没有（`None`）就走退路。
    watch: Option<Arc<dyn ListenerWatch>>,
    reuse: Reuse,
}

impl OwnedListenerGuard {
    pub fn new(inspector: Arc<dyn ProcessInspector>, pid: u32, dial: SocketAddr) -> Self {
        let watch = inspector.watch_listener(pid, dial);
        let reuse = Reuse::new(inspector.fallback_ttl());
        Self {
            inspector,
            pid,
            dial,
            watch,
            reuse,
        }
    }

    /// 退路上贵的那一问：这个 pid 是端口上唯一的监听者（读不出来的那一次立刻再问一遍）。
    async fn ask(&self) -> Slow {
        for _ in 0..2 {
            match self.inspector.listens_on(self.pid, self.dial.port()).await {
                Ok(true) => return Slow::Yes,
                Ok(false) => return Slow::No,
                Err(_) => {}
            }
        }
        Slow::Unreadable
    }
}

impl std::fmt::Debug for OwnedListenerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedListenerGuard")
            .field("pid", &self.pid)
            .field("dial", &self.dial)
            .field("native", &self.watch.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EndpointGuard for OwnedListenerGuard {
    async fn verify(&self) -> Verification {
        if let Some(watch) = &self.watch {
            match watch.holds().await {
                Ok(true) => return Verification::Verified,
                Ok(false) => {
                    self.reuse.invalidate();
                    return Verification::Mismatch;
                }
                // 原生的这一次没能回答（读不了这个进程）：问那个贵的。
                Err(_) => {}
            }
        }
        self.reuse.verify(|| self.ask()).await
    }

    fn dial(&self) -> Option<SocketAddr> {
        Some(self.dial)
    }
}

/// 外部引擎的守卫：登记时校验通过的那些监听者（`expected`，pid 加启动时间：**每个能接 `dial` 这个连接的**监听者）此刻还都握着一个
/// `dial` 连得到的 LISTEN 套接字。绑在别的地址上、`dial` 到不了的监听者不在里面，也不影响它（Codex 专项评审 M13）。
pub struct ExternalListenerGuard {
    verifier: ListenerVerifier,
    dial: SocketAddr,
    expected: Vec<ListenerIdentity>,
    /// 每个被校验的监听者一个原生监视；有一个建不出来（检查器没有原生监视、读不了那个进程）就整个没有，走退路。
    watches: Option<Vec<Arc<dyn ListenerWatch>>>,
    reuse: Reuse,
}

impl ExternalListenerGuard {
    pub fn new(
        verifier: ListenerVerifier,
        dial: SocketAddr,
        expected: Vec<ListenerIdentity>,
    ) -> Self {
        let watches: Option<Vec<Arc<dyn ListenerWatch>>> = expected
            .iter()
            .map(|identity| verifier.inspector().watch_listener(identity.pid, dial))
            .collect();
        let reuse = Reuse::new(verifier.inspector().fallback_ttl());
        Self {
            verifier,
            dial,
            expected,
            watches,
            reuse,
        }
    }

    /// 每个监听者的原生监视都说还握着：`Some(true)`；有一个说不握了：`Some(false)`；有一个读不出来：`None`（走退路）。
    async fn native(&self) -> Option<bool> {
        let watches = self.watches.as_ref()?;
        for watch in watches {
            match watch.holds().await {
                Ok(true) => {}
                Ok(false) => return Some(false),
                Err(_) => return None,
            }
        }
        Some(true)
    }

    /// 退路上贵的那一问：现在能接这个连接的监听者，恰好还是登记时校验通过的那些（读不出来的那一次立刻再问一遍）。
    async fn ask(&self) -> Slow {
        for _ in 0..2 {
            match self.verifier.check(self.dial.port()).await {
                ListenerVerdict::Verified { listeners } if listeners == self.expected => {
                    return Slow::Yes;
                }
                // 通过了校验，但已经不是登记时认的那些（换了进程、多了一个、少了一个）；没有监听者；监听者不合格。
                ListenerVerdict::Verified { .. }
                | ListenerVerdict::NoListener
                | ListenerVerdict::Rejected(_) => return Slow::No,
                ListenerVerdict::Unverifiable(_) => {}
            }
        }
        Slow::Unreadable
    }
}

impl std::fmt::Debug for ExternalListenerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalListenerGuard")
            .field("dial", &self.dial)
            .field("expected", &self.expected)
            .field("native", &self.watches.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EndpointGuard for ExternalListenerGuard {
    async fn verify(&self) -> Verification {
        if self.expected.is_empty() {
            // 登记时没有校验过任何监听者：现在没有什么可以确认的（空的「都还握着」不能算通过）。
            return Verification::Mismatch;
        }
        match self.native().await {
            Some(true) => return Verification::Verified,
            Some(false) => {
                self.reuse.invalidate();
                return Verification::Mismatch;
            }
            None => {}
        }
        self.reuse.verify(|| self.ask()).await
    }

    fn dial(&self) -> Option<SocketAddr> {
        Some(self.dial)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::VecDeque;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::inspect::{InspectError, ListeningSocket, MANAGED_DIAL, PidState};
    use super::super::listener::ListenerPolicy;
    use super::*;

    const TEAM: &str = "PSK5Q5T46L";
    const ME: u32 = 501;
    const START: &str = "Fri Nov 20 15:14:05 2026";
    const PORT: u16 = 19731;

    fn dial() -> SocketAddr {
        SocketAddr::new(MANAGED_DIAL, PORT)
    }

    /// A watch whose answers are scripted (the last repeats) and that counts how often it was asked.
    #[derive(Debug)]
    struct ScriptedWatch {
        answers: Mutex<VecDeque<Result<bool, InspectError>>>,
        asked: AtomicUsize,
    }

    impl ScriptedWatch {
        fn new(answers: Vec<Result<bool, InspectError>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                asked: AtomicUsize::new(0),
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ListenerWatch for ScriptedWatch {
        async fn holds(&self) -> Result<bool, InspectError> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            next(&self.answers, Err(InspectError::NoAnswer))
        }
    }

    /// An inspector whose answers are scripted: each queue is consumed in order and its last entry repeats. It can offer watches (one per
    /// `watch_listener` call, in order) and a reuse window, it counts the expensive questions it is asked, and it can hold a `listens_on`
    /// answer until the test lets it go (to put a native answer in the middle of a slow check).
    struct Fake {
        listens: Mutex<VecDeque<Result<bool, InspectError>>>,
        listeners: Mutex<VecDeque<Result<Vec<u32>, InspectError>>>,
        start_time: Mutex<String>,
        team: Mutex<Option<String>>,
        watches: Mutex<VecDeque<Arc<ScriptedWatch>>>,
        ttl: Mutex<Duration>,
        listens_asked: AtomicUsize,
        listeners_asked: AtomicUsize,
        /// While set, `listens_on` announces itself on `entered` and waits for `release` before it answers.
        hold: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
        /// How long every `listens_on` takes (the `lsof` it stands for).
        delay: Mutex<Duration>,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                listens: Mutex::new(VecDeque::new()),
                listeners: Mutex::new(VecDeque::new()),
                start_time: Mutex::new(START.to_owned()),
                team: Mutex::new(Some(TEAM.to_owned())),
                watches: Mutex::new(VecDeque::new()),
                ttl: Mutex::new(Duration::ZERO),
                listens_asked: AtomicUsize::new(0),
                listeners_asked: AtomicUsize::new(0),
                hold: Mutex::new(None),
                delay: Mutex::new(Duration::ZERO),
            })
        }

        fn script_listens(&self, answers: Vec<Result<bool, InspectError>>) {
            *self.listens.lock().unwrap() = answers.into();
        }

        fn script_listeners(&self, answers: Vec<Result<Vec<u32>, InspectError>>) {
            *self.listeners.lock().unwrap() = answers.into();
        }

        fn offer_watches(&self, watches: Vec<Arc<ScriptedWatch>>) {
            *self.watches.lock().unwrap() = watches.into();
        }

        fn reuse_for(&self, ttl: Duration) {
            *self.ttl.lock().unwrap() = ttl;
        }

        fn each_question_takes(&self, delay: Duration) {
            *self.delay.lock().unwrap() = delay;
        }

        /// The next `listens_on` waits: returns (entered, release).
        fn hold_next_question(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
            let pair = (
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tokio::sync::Notify::new()),
            );
            *self.hold.lock().unwrap() = Some(pair.clone());
            pair
        }

        fn listens_asked(&self) -> usize {
            self.listens_asked.load(Ordering::SeqCst)
        }

        fn listeners_asked(&self) -> usize {
            self.listeners_asked.load(Ordering::SeqCst)
        }
    }

    fn next<T: Clone>(queue: &Mutex<VecDeque<T>>, fallback: T) -> T {
        let mut queue = queue.lock().unwrap();
        if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue.front().cloned().unwrap_or(fallback)
        }
    }

    #[async_trait]
    impl ProcessInspector for Fake {
        fn pid_state(&self, _pid: u32) -> PidState {
            PidState::Alive
        }
        async fn start_time(&self, _pid: u32) -> Result<String, InspectError> {
            Ok(self.start_time.lock().unwrap().clone())
        }
        async fn command(&self, _pid: u32) -> Result<String, InspectError> {
            Err(InspectError::NoAnswer)
        }
        async fn process_group(&self, _pid: u32) -> Result<u32, InspectError> {
            Err(InspectError::NoAnswer)
        }
        async fn listens_on(&self, _pid: u32, _port: u16) -> Result<bool, InspectError> {
            self.listens_asked.fetch_add(1, Ordering::SeqCst);
            let hold = self.hold.lock().unwrap().take();
            if let Some((entered, release)) = hold {
                entered.notify_one();
                release.notified().await;
            }
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            next(&self.listens, Err(InspectError::NoAnswer))
        }
        async fn listeners_on(&self, _port: u16) -> Result<Vec<u32>, InspectError> {
            self.listeners_asked.fetch_add(1, Ordering::SeqCst);
            next(&self.listeners, Ok(Vec::new()))
        }
        async fn uid_of(&self, _pid: u32) -> Result<u32, InspectError> {
            Ok(ME)
        }
        async fn signing_team(&self, _pid: u32) -> Result<Option<String>, InspectError> {
            Ok(self.team.lock().unwrap().clone())
        }
        fn watch_listener(&self, _pid: u32, _dial: SocketAddr) -> Option<Arc<dyn ListenerWatch>> {
            let watch = self.watches.lock().unwrap().pop_front()?;
            Some(watch)
        }
        fn fallback_ttl(&self) -> Duration {
            *self.ttl.lock().unwrap()
        }
    }

    fn identity(pid: u32) -> ListenerIdentity {
        ListenerIdentity {
            pid,
            started_at: START.to_owned(),
        }
    }

    fn verifier(fake: &Arc<Fake>) -> ListenerVerifier {
        ListenerVerifier::new(fake.clone(), ListenerPolicy::new(TEAM).with_uid(ME))
    }

    fn owned(fake: &Arc<Fake>) -> OwnedListenerGuard {
        OwnedListenerGuard::new(fake.clone(), 42, dial())
    }

    // ----- the slow question (no watch, no reuse window): what the guards asked before the per-request watch existed -----------------

    #[tokio::test]
    async fn an_owned_engine_is_verified_only_while_it_is_the_listener_and_an_unreadable_answer_is_asked_twice()
     {
        let fake = Fake::new();
        let guard = owned(&fake);
        fake.script_listens(vec![Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        fake.script_listens(vec![Ok(false)]);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        // One unreadable answer is asked again at once; two in a row are unverifiable, never a pass.
        fake.script_listens(vec![Err(InspectError::TimedOut), Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        fake.script_listens(vec![Err(InspectError::TimedOut), Ok(false)]);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        fake.script_listens(vec![
            Err(InspectError::TimedOut),
            Err(InspectError::NoAnswer),
        ]);
        assert_eq!(guard.verify().await, Verification::Unverifiable);
    }

    #[tokio::test]
    async fn an_external_engine_is_verified_only_while_the_listeners_are_exactly_the_ones_it_was_registered_for()
     {
        let fake = Fake::new();
        let guard = ExternalListenerGuard::new(verifier(&fake), dial(), vec![identity(42)]);
        fake.script_listeners(vec![Ok(vec![42])]);
        assert_eq!(guard.verify().await, Verification::Verified);
        // Gone, replaced by another process, an extra listener next to ours.
        fake.script_listeners(vec![Ok(vec![])]);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        fake.script_listeners(vec![Ok(vec![43])]);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        fake.script_listeners(vec![Ok(vec![42, 43])]);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        // The same pid, but another process (the start time changed).
        fake.script_listeners(vec![Ok(vec![42])]);
        *fake.start_time.lock().unwrap() = "Fri Nov 20 16:00:00 2026".to_owned();
        assert_eq!(guard.verify().await, Verification::Mismatch);
        *fake.start_time.lock().unwrap() = START.to_owned();
        // A listener that no longer passes (the code is not the team's).
        *fake.team.lock().unwrap() = None;
        let guard = ExternalListenerGuard::new(verifier(&fake), dial(), vec![identity(42)]);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        *fake.team.lock().unwrap() = Some(TEAM.to_owned());
        // Unreadable: asked once more, then unverifiable.
        fake.script_listeners(vec![Err(InspectError::TimedOut), Ok(vec![42])]);
        assert_eq!(guard.verify().await, Verification::Verified);
        fake.script_listeners(vec![
            Err(InspectError::TimedOut),
            Err(InspectError::TimedOut),
        ]);
        assert_eq!(guard.verify().await, Verification::Unverifiable);
    }

    // ----- the dial address: the one the guard verified is the one it hands out --------------------------------------------------------

    #[tokio::test]
    async fn a_guard_hands_out_the_address_it_verifies() {
        let fake = Fake::new();
        let v6 = SocketAddr::new(IpAddr::V6("::1".parse().unwrap()), PORT);
        assert_eq!(
            OwnedListenerGuard::new(fake.clone(), 42, dial()).dial(),
            Some(dial())
        );
        assert_eq!(
            OwnedListenerGuard::new(fake.clone(), 42, v6).dial(),
            Some(v6)
        );
        assert_eq!(
            ExternalListenerGuard::new(verifier(&fake), dial(), vec![identity(42)]).dial(),
            Some(dial())
        );
        // the watch is built for that address
        struct Recording(Mutex<Vec<SocketAddr>>);
        #[async_trait]
        impl ProcessInspector for Recording {
            fn pid_state(&self, _pid: u32) -> PidState {
                PidState::Alive
            }
            async fn start_time(&self, _pid: u32) -> Result<String, InspectError> {
                Err(InspectError::NoAnswer)
            }
            async fn command(&self, _pid: u32) -> Result<String, InspectError> {
                Err(InspectError::NoAnswer)
            }
            async fn process_group(&self, _pid: u32) -> Result<u32, InspectError> {
                Err(InspectError::NoAnswer)
            }
            async fn listens_on(&self, _pid: u32, _port: u16) -> Result<bool, InspectError> {
                Err(InspectError::NoAnswer)
            }
            fn watch_listener(
                &self,
                _pid: u32,
                dial: SocketAddr,
            ) -> Option<Arc<dyn ListenerWatch>> {
                self.0.lock().unwrap().push(dial);
                None
            }
        }
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let _ = OwnedListenerGuard::new(recording.clone(), 42, v6);
        let _ = ExternalListenerGuard::new(
            ListenerVerifier::new(recording.clone(), ListenerPolicy::new(TEAM)),
            dial(),
            vec![identity(1), identity(2)],
        );
        assert_eq!(
            *recording.0.lock().unwrap(),
            vec![v6, dial()],
            "the external guard stops at the first listener that cannot be watched"
        );
    }

    // ----- the per-request watch: no expensive question at all ---------------------------------------------------------------------

    /// An inspector that offers the given watches and **fails the test** if the guard asks it anything else: a guard whose watches
    /// answer must not start the `lsof` / `ps` of the slow questions on the request path.
    struct WatchOnly {
        watches: Mutex<VecDeque<Arc<ScriptedWatch>>>,
    }

    #[async_trait]
    impl ProcessInspector for WatchOnly {
        fn pid_state(&self, _pid: u32) -> PidState {
            PidState::Alive
        }
        async fn start_time(&self, _pid: u32) -> Result<String, InspectError> {
            panic!("start_time asked on the request path")
        }
        async fn command(&self, _pid: u32) -> Result<String, InspectError> {
            panic!("command asked on the request path")
        }
        async fn process_group(&self, _pid: u32) -> Result<u32, InspectError> {
            panic!("process_group asked on the request path")
        }
        async fn listens_on(&self, _pid: u32, _port: u16) -> Result<bool, InspectError> {
            panic!("listens_on (lsof) asked on the request path")
        }
        async fn listeners_on(&self, _port: u16) -> Result<Vec<u32>, InspectError> {
            panic!("listeners_on (lsof) asked on the request path")
        }
        async fn listening_sockets(
            &self,
            _port: u16,
        ) -> Result<Vec<ListeningSocket>, InspectError> {
            panic!("listening_sockets (lsof) asked on the request path")
        }
        async fn uid_of(&self, _pid: u32) -> Result<u32, InspectError> {
            panic!("uid_of asked on the request path")
        }
        async fn signing_team(&self, _pid: u32) -> Result<Option<String>, InspectError> {
            panic!("signing_team asked on the request path")
        }
        fn watch_listener(&self, _pid: u32, _dial: SocketAddr) -> Option<Arc<dyn ListenerWatch>> {
            let watch = self.watches.lock().unwrap().pop_front()?;
            Some(watch)
        }
    }

    fn watch_only(watches: Vec<Arc<ScriptedWatch>>) -> Arc<WatchOnly> {
        Arc::new(WatchOnly {
            watches: Mutex::new(watches.into()),
        })
    }

    #[tokio::test]
    async fn a_guard_whose_watch_answers_never_asks_the_inspector_anything_else() {
        // Owned: the watch alone decides, on every request.
        let watch = ScriptedWatch::new(vec![Ok(true)]);
        let guard = OwnedListenerGuard::new(watch_only(vec![watch.clone()]), 42, dial());
        for _ in 0..5 {
            assert_eq!(guard.verify().await, Verification::Verified);
        }
        assert_eq!(watch.asked(), 5, "asked on every request, nothing reused");

        // The watch says the engine let go of the port: the very next request is refused, and so is every one after it.
        let watch = ScriptedWatch::new(vec![Ok(true), Ok(false)]);
        let guard = OwnedListenerGuard::new(watch_only(vec![watch]), 42, dial());
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        assert_eq!(guard.verify().await, Verification::Mismatch);

        // External: one watch per verified listener; every one must still hold.
        let both = ListenerVerifier::new(
            watch_only(vec![
                ScriptedWatch::new(vec![Ok(true)]),
                ScriptedWatch::new(vec![Ok(true)]),
            ]),
            ListenerPolicy::new(TEAM).with_uid(ME),
        );
        let guard = ExternalListenerGuard::new(both, dial(), vec![identity(42), identity(43)]);
        assert_eq!(guard.verify().await, Verification::Verified);

        let one_left = ListenerVerifier::new(
            watch_only(vec![
                ScriptedWatch::new(vec![Ok(true)]),
                ScriptedWatch::new(vec![Ok(false)]),
            ]),
            ListenerPolicy::new(TEAM).with_uid(ME),
        );
        let guard = ExternalListenerGuard::new(one_left, dial(), vec![identity(42), identity(43)]);
        assert_eq!(
            guard.verify().await,
            Verification::Mismatch,
            "one of the listeners let go"
        );
    }

    /// A guard registered for no verified listener confirms nothing: the empty "all of them still hold" is not a pass (with or without
    /// watches).
    #[tokio::test]
    async fn a_guard_for_no_listener_verifies_nothing() {
        let guard = ExternalListenerGuard::new(
            ListenerVerifier::new(watch_only(vec![]), ListenerPolicy::new(TEAM).with_uid(ME)),
            dial(),
            Vec::new(),
        );
        assert_eq!(guard.verify().await, Verification::Mismatch);
    }

    // ----- the way back: a watch that cannot answer, and the short reuse window --------------------------------------------------------

    /// The native watch could not read the process: the question goes to the slow check (asked twice when unreadable), which decides.
    #[tokio::test]
    async fn a_watch_that_cannot_answer_hands_the_question_to_the_slow_check() {
        let fake = Fake::new();
        fake.offer_watches(vec![ScriptedWatch::new(vec![Err(InspectError::NoAnswer)])]);
        let guard = owned(&fake);
        fake.script_listens(vec![Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 1);
        fake.script_listens(vec![Ok(false)]);
        assert_eq!(guard.verify().await, Verification::Mismatch);
        fake.script_listens(vec![Err(InspectError::TimedOut), Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        fake.script_listens(vec![Err(InspectError::TimedOut)]);
        assert_eq!(guard.verify().await, Verification::Unverifiable);

        // External: one listener cannot be watched at all (no watch for it): the whole guard uses the slow check, which compares the
        // exact set.
        let fake = Fake::new();
        fake.offer_watches(vec![ScriptedWatch::new(vec![Ok(true)])]); // only the first listener has one
        let guard =
            ExternalListenerGuard::new(verifier(&fake), dial(), vec![identity(42), identity(43)]);
        fake.script_listeners(vec![Ok(vec![42, 43])]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listeners_asked(), 1);
        fake.script_listeners(vec![Ok(vec![42])]);
        assert_eq!(guard.verify().await, Verification::Mismatch);

        // External, a watch that errors half way: the slow check decides.
        let fake = Fake::new();
        fake.offer_watches(vec![
            ScriptedWatch::new(vec![Ok(true)]),
            ScriptedWatch::new(vec![Err(InspectError::NoAnswer)]),
        ]);
        let guard =
            ExternalListenerGuard::new(verifier(&fake), dial(), vec![identity(42), identity(43)]);
        fake.script_listeners(vec![Ok(vec![42, 43])]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listeners_asked(), 1);
    }

    /// The reuse window of the slow check (the platform has no native watch): many requests inside it cost one `lsof`; a request after
    /// it asks again; only a "yes" is reused.
    #[tokio::test]
    async fn the_slow_check_is_reused_for_a_short_while_only_and_only_when_it_said_yes() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_millis(150));
        let guard = owned(&fake);
        fake.script_listens(vec![Ok(true)]);
        for _ in 0..10 {
            assert_eq!(guard.verify().await, Verification::Verified);
        }
        assert_eq!(fake.listens_asked(), 1, "ten requests inside the window");
        // The listener changes inside the window: not seen until the window is over (the documented limit) ...
        fake.script_listens(vec![Ok(false)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 1);
        // ... and seen by the first request after it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(guard.verify().await, Verification::Mismatch);
        assert_eq!(fake.listens_asked(), 2);
        // A "no" is not reused: the next request asks again, and a "yes" that follows opens a new window.
        fake.script_listens(vec![Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 3);

        // An unreadable answer is never reused and is never mistaken for a yes: after the window, two unreadable answers in a row are
        // unverifiable, and the next request asks again.
        let fake = Fake::new();
        fake.reuse_for(Duration::from_millis(100));
        let guard = owned(&fake);
        fake.script_listens(vec![Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        tokio::time::sleep(Duration::from_millis(150)).await;
        fake.script_listens(vec![Err(InspectError::TimedOut)]);
        assert_eq!(guard.verify().await, Verification::Unverifiable);
        assert_eq!(
            fake.listens_asked(),
            3,
            "one answer, then the retry, then no reuse"
        );
        assert_eq!(guard.verify().await, Verification::Unverifiable);
        assert_eq!(fake.listens_asked(), 5);
    }

    /// A native "no" ends the reuse window of the slow check: a stale "yes" must not be served once the watch said the process let go.
    #[tokio::test]
    async fn a_no_from_the_watch_ends_the_reuse_window_of_the_slow_check() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        // First the watch cannot answer (slow check says yes, reused), then it says "no", then it cannot answer again.
        fake.offer_watches(vec![ScriptedWatch::new(vec![
            Err(InspectError::NoAnswer),
            Ok(false),
            Err(InspectError::NoAnswer),
        ])]);
        let guard = owned(&fake);
        fake.script_listens(vec![Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 1);
        assert_eq!(
            guard.verify().await,
            Verification::Mismatch,
            "the watch says no"
        );
        assert_eq!(
            guard.verify().await,
            Verification::Verified,
            "the watch cannot answer; the slow check is asked again, not the stale yes"
        );
        assert_eq!(fake.listens_asked(), 2);
    }

    /// The slow check of an external engine keeps its meaning (exactly the registered listeners, several allowed) and is reused the same
    /// way.
    #[tokio::test]
    async fn the_slow_check_of_an_external_engine_with_several_listeners_is_reused_for_a_short_while_only()
     {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_millis(150));
        let guard =
            ExternalListenerGuard::new(verifier(&fake), dial(), vec![identity(42), identity(43)]);
        fake.script_listeners(vec![Ok(vec![42, 43])]);
        for _ in 0..6 {
            assert_eq!(guard.verify().await, Verification::Verified);
        }
        assert_eq!(fake.listeners_asked(), 1);
        fake.script_listeners(vec![Ok(vec![42])]);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(guard.verify().await, Verification::Mismatch);
        assert_eq!(fake.listeners_asked(), 2);
        assert_eq!(
            guard.verify().await,
            Verification::Mismatch,
            "a no is not reused"
        );
        assert_eq!(fake.listeners_asked(), 3);
    }

    /// A native "no" from an external engine's watch also ends the reuse window of the slow check.
    #[tokio::test]
    async fn a_no_from_an_external_watch_ends_the_reuse_window_of_the_slow_check() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        fake.offer_watches(vec![ScriptedWatch::new(vec![
            Err(InspectError::NoAnswer),
            Ok(false),
            Err(InspectError::NoAnswer),
        ])]);
        let guard = ExternalListenerGuard::new(verifier(&fake), dial(), vec![identity(42)]);
        fake.script_listeners(vec![Ok(vec![42])]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listeners_asked(), 1);
        assert_eq!(
            guard.verify().await,
            Verification::Mismatch,
            "the watch says no"
        );
        assert_eq!(
            guard.verify().await,
            Verification::Verified,
            "the watch cannot answer; the slow check is asked again, not the stale yes"
        );
        assert_eq!(fake.listeners_asked(), 2);
    }

    // ----- H7: the fallback cache under concurrency --------------------------------------------------------------------------------

    /// Requests are concurrent: many racing callers share one guard and its reuse window. Every one is answered, nothing deadlocks or
    /// poisons, and **the slow checks are merged**: one question for all of them, not one per caller (Codex focused review H7).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_share_one_slow_check() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        fake.each_question_takes(Duration::from_millis(100));
        fake.script_listens(vec![Ok(true)]);
        let guard = Arc::new(owned(&fake));
        let tasks: Vec<_> = (0..32)
            .map(|_| {
                let guard = guard.clone();
                tokio::spawn(async move {
                    let mut all = true;
                    for _ in 0..50 {
                        all &= guard.verify().await == Verification::Verified;
                    }
                    all
                })
            })
            .collect();
        for task in tasks {
            assert!(task.await.unwrap());
        }
        assert_eq!(
            fake.listens_asked(),
            1,
            "32 callers that all found the window closed asked {} times",
            fake.listens_asked()
        );
    }

    /// The same for an external engine: one `check` for all the callers that found the window closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_share_one_external_check() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        fake.script_listeners(vec![Ok(vec![42])]);
        let guard = Arc::new(ExternalListenerGuard::new(
            verifier(&fake),
            dial(),
            vec![identity(42)],
        ));
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let guard = guard.clone();
                tokio::spawn(async move { guard.verify().await == Verification::Verified })
            })
            .collect();
        for task in tasks {
            assert!(task.await.unwrap());
        }
        assert_eq!(fake.listeners_asked(), 1);
    }

    /// **A slow check that started before a native "no" must not store its answer.** The check is in flight (a held `lsof`), another request's
    /// watch says the process let go, then the old check answers "yes": the request that waited for it is refused, nothing is stored, and
    /// the next request asks again instead of being served the stale yes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_slow_check_that_started_before_a_native_no_does_not_backfill_its_answer() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        // call 1 (A): the watch cannot answer -> slow check, held; call 2 (B): the watch says no; call 3 (C): cannot answer -> slow check
        fake.offer_watches(vec![ScriptedWatch::new(vec![
            Err(InspectError::NoAnswer),
            Ok(false),
            Err(InspectError::NoAnswer),
        ])]);
        let guard = Arc::new(owned(&fake));
        fake.script_listens(vec![Ok(true)]);
        let (entered, release) = fake.hold_next_question();
        let a = {
            let guard = guard.clone();
            tokio::spawn(async move { guard.verify().await })
        };
        entered.notified().await; // A is inside the slow check, holding it
        assert_eq!(
            guard.verify().await,
            Verification::Mismatch,
            "B: the watch says the process let go"
        );
        release.notify_one(); // A's check answers "yes", an answer older than B's "no"
        assert_eq!(
            a.await.unwrap(),
            Verification::Mismatch,
            "A's yes is not served after a native no that happened while it was being asked"
        );
        assert_eq!(fake.listens_asked(), 1);
        assert_eq!(
            guard.verify().await,
            Verification::Verified,
            "C asks again: nothing was stored"
        );
        assert_eq!(
            fake.listens_asked(),
            2,
            "the stale yes did not backfill the window"
        );
    }

    /// **The window starts when the check starts, not when it ends.** A check that takes 300 ms with a 400 ms window leaves 100 ms: a
    /// request 200 ms after it finished asks again (counting from the end it would still be inside the window).
    #[tokio::test]
    async fn the_reuse_window_starts_when_the_check_starts() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_millis(400));
        fake.each_question_takes(Duration::from_millis(300));
        fake.script_listens(vec![Ok(true)]);
        let guard = owned(&fake);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 1);
        tokio::time::sleep(Duration::from_millis(200)).await; // 500 ms after the check started, 200 after it ended
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 2, "the window had already run out");
    }

    /// The callers that queue behind a slow check share its outcome whatever it is: a "no" too (a stampede of requests against a port the
    /// engine lost must not run one `lsof` each, one after the other). A "no" is still not stored in the window: the next caller to arrive
    /// when nothing is in flight asks again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn callers_that_queued_behind_a_slow_check_share_its_answer_a_no_included() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        fake.each_question_takes(Duration::from_millis(100));
        fake.script_listens(vec![Ok(false)]);
        let guard = Arc::new(owned(&fake));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let guard = guard.clone();
                tokio::spawn(async move { guard.verify().await })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), Verification::Mismatch);
        }
        assert_eq!(fake.listens_asked(), 1, "eight callers, one question");
        // nothing was stored: a caller that arrives later asks for itself, and a yes then opens the window
        fake.script_listens(vec![Ok(true)]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 2);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 2);
    }

    /// Cannot tell is shared the same way, and never stored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn callers_that_queued_behind_an_unreadable_slow_check_share_that_too() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        fake.each_question_takes(Duration::from_millis(100));
        fake.script_listens(vec![Err(InspectError::TimedOut)]);
        let guard = Arc::new(owned(&fake));
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                let guard = guard.clone();
                tokio::spawn(async move { guard.verify().await })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), Verification::Unverifiable);
        }
        // one check = two questions (the retry), not twelve
        assert_eq!(fake.listens_asked(), 2);
    }

    /// A guard registered with a dial address other than the managed one verifies *that* address (the dial address is not a constant of
    /// the guard).
    #[test]
    fn the_managed_dial_address_is_ipv4_loopback() {
        assert_eq!(MANAGED_DIAL, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
}

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
//!   监督的下一次检查才看见（自有引擎每个 `liveness_interval`，外部引擎也每个 `liveness_interval`；各自的上限见规范勘误十（8））。
//!
//! **退路**（原生监视建不出来：别的 unix、读不了那个进程；或某一次读不出来）：每个请求问那个贵的问题——自有引擎 `listens_on`，外部
//! 引擎 [`ListenerVerifier::check`]——而肯定的答案至多重用 [`ProcessInspector::fallback_ttl`]（真的检查器是 [`super::FALLBACK_TTL`]，
//! 250 毫秒；测试里编排好的假检查器是 0，每个请求都问）。这个重用窗口：**同一时刻只有一个贵的检查在途**（别的请求排队等它，轮到时
//! 直接拿它的结论：是、不、读不出来都一样，不各起各的 `lsof`）；窗口从那个检查**开始**时起算，不是它结束时；一次原生的「不握了」
//! 让正在跑的检查答出的「是」作废（不会回填）；「不」与读不出来不进窗口。**每一个肯定的答案，不论谁用它，都受同一个年龄上限**（自检查开始起不超过 `ttl`）：
//! 排队的请求不会因为那个检查是在它到达之后才开始的就拿一个已经过期的「是」；一次检查自己跑过了这个上限（单次 `lsof` 超过 250 毫秒），它的「是」谁也不用，
//! 也不存：问的人再观察一次，又超了就是确认不了（[`Verification::Unverifiable`]，fail closed），排队的请求共用这个结论。`ttl` 为 0（没有窗口，每个请求自己问、
//! 答案马上自己用）没有年龄上限。
//!
//! 确认不了（读不出来）就是 [`Verification::Unverifiable`]，调用方拒绝发送（fail closed）：读一次不行的立刻再问一遍，仍不行才算。
//!
//! **没有消除的间隙**：确认与发送之间仍有毫秒级的间隙；「并存的监听者」到监督的下一次检查才被发现。原生监视绑定签名通过时的进程
//! 启动标识；进程身份用内核给出的运行映像（macOS 的代码目录哈希与映像 UUID，Linux 的 `/proc/<pid>/exe` 设备与 inode），不依赖可执行文件
//! 路径，磁盘上的文件被删除也不影响合法引擎；`exec` 成另一份程序会被每次请求的身份核对发现，同一份二进制重新 `exec` 自己沿用原来的
//! 签名结论。内核没有给出身份的进程（身份全空）不因此被拒绝，由启动时间与签名核验把关。认回旧记录时先绑定并落盘原生启动标识，
//! 请求路径稳态不起子进程。连接失败、或发送被外层期限截断，都会清除该端点代际的退路肯定缓存，也使尚未完成的旧检查无法重新填入旧结果。
//! Unix domain socket 待确认 oMLX 支持后再评估（#646）。规范勘误十（8）及勘误十五说明对外行为。

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
    /// 做完的检查数，和最近一个的结论：一个请求排队的时候又有一个检查做完了，那个检查的结论就是它的结论——前提是那之后没有原生的「不握了」
    /// 赶过它（`last_epoch` 是那个检查开始时的代数），而且一个**肯定**的结论还在新鲜期内（`last_started` 是那个检查**开始**的时刻，无论它是在这个请求
    /// 到达之前还是之后开始的）。
    finished: u64,
    last: Verification,
    last_epoch: u64,
    last_started: Instant,
}

impl ReuseState {
    /// The conclusion of a check that finished while a caller that arrived when `arrived` checks were done sat in the queue — if the caller may take
    /// it: it may not when no check finished since it arrived, when a native "no" has overtaken that check (the conclusion is older than the "no"), or
    /// when the conclusion is a **yes that is too old**: the same freshness bound as on the fast path (`ttl`, counted from when the check *started*),
    /// **whenever the check started** (Codex re-review of layer 2, #4: a yes from a check that started after the caller arrived but ran past the window
    /// used to be handed over as "as fresh as one the caller would have started itself"; but a check that outlasted the bound is exactly the case in
    /// which its yes is no longer one, for the caller as for anybody). A caller that may not take it asks for itself. A "no" and a "cannot tell" are
    /// refusals: older ones only err on the safe side, so they are shared however old they are. With no reuse at all (`ttl` 0) a yes is never handed over.
    fn shared_since(&self, arrived: u64, ttl: Duration) -> Option<Verification> {
        let fresh = self.last != Verification::Verified || self.last_started.elapsed() < ttl;
        (self.finished != arrived && self.last_epoch == self.epoch && fresh).then_some(self.last)
    }
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
                last_epoch: 0,
                last_started: Instant::now(),
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
    /// （`ask`），「是」记进窗口；排队的时候有别的检查做完了，就用它的结论（肯定的结论要还在新鲜期内）。
    ///
    /// **一次检查自己跑过了新鲜期**（`ttl` > 0 而这一问从开始到答出来用了不止 `ttl`）：它的「是」比它被用上的时候已经老了（Codex 复审 L2 的 #4）：不用、不存，
    /// 再观察一次；又超了就是 [`Verification::Unverifiable`]（fail closed），不存，排在后面的请求共用这个结论。
    async fn verify<F, Fut>(&self, mut ask: F) -> Verification
    where
        F: FnMut() -> Fut,
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
        let mut observed_again = false;
        loop {
            let (started, epoch) = {
                let state = self.state();
                if !observed_again && let Some(shared) = state.shared_since(arrived, self.ttl) {
                    // 我们排队的时候有一个检查做完了：它就是这一次的检查（合并），不再各问各的。那个结论之后若有一次原生的「不握了」赶过了它
                    // （检查做完到我们拿到锁之间），它就比那件事旧了：自己再问；一个肯定的结论过了新鲜期也一样（预审 L2 的 M3 与复审的 #4）。
                    return shared;
                }
                (Instant::now(), state.epoch)
            };
            let answer = ask().await;
            let mut state = self.state();
            let overran = !self.ttl.is_zero() && started.elapsed() >= self.ttl;
            let verdict = if state.epoch != epoch {
                // 它跑的时候有一次原生的「不握了」：这个答案比那件事旧，不存，「是」也不放行。
                match answer {
                    Slow::Yes | Slow::No => Verification::Mismatch,
                    Slow::Unreadable => Verification::Unverifiable,
                }
            } else {
                match answer {
                    Slow::Yes if overran => {
                        state.yes_from = None;
                        if !observed_again {
                            observed_again = true;
                            continue;
                        }
                        Verification::Unverifiable
                    }
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
            state.last_epoch = epoch;
            state.last_started = started;
            return verdict;
        }
    }
}

/// 自有引擎的守卫：它的进程（`pid`）此刻还握着一个 `dial` 这个地址连得到的 LISTEN 套接字。
pub struct OwnedListenerGuard {
    inspector: Arc<dyn ProcessInspector>,
    pid: u32,
    started_at: String,
    generation: Option<String>,
    dial: SocketAddr,
    /// 每个请求的原生监视（建守卫那一刻钉死进程的身份）；检查器没有（`None`）就走退路。
    watch: Option<Arc<dyn ListenerWatch>>,
    reuse: Reuse,
}

impl OwnedListenerGuard {
    pub fn new(
        inspector: Arc<dyn ProcessInspector>,
        pid: u32,
        dial: SocketAddr,
        started_at: String,
        generation: Option<String>,
    ) -> Self {
        let watch = inspector
            .watch_listener(pid, dial)
            .filter(|watch| watch.generation() == generation.as_deref());
        let reuse = Reuse::new(inspector.fallback_ttl());
        Self {
            inspector,
            pid,
            started_at,
            generation,
            dial,
            watch,
            reuse,
        }
    }

    /// The fallback asks whether this PID holds the only reachable socket.
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

    /// Confirm the recorded start time even when the socket observation was cached.
    async fn fallback_identity(&self) -> Verification {
        let (observed, expected) = match &self.generation {
            Some(expected) => (self.inspector.generation(self.pid).await, expected),
            None => (self.inspector.start_time(self.pid).await, &self.started_at),
        };
        match observed {
            Ok(observed) if &observed == expected => Verification::Verified,
            Ok(_) => Verification::Mismatch,
            Err(_) => Verification::Unverifiable,
        }
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
        let socket = self.reuse.verify(|| self.ask()).await;
        if socket != Verification::Verified {
            return socket;
        }
        let identity = self.fallback_identity().await;
        if identity != Verification::Verified {
            self.reuse.invalidate();
        }
        identity
    }

    fn invalidate_positive(&self) {
        self.reuse.invalidate();
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
            .map(|identity| {
                verifier
                    .inspector()
                    .watch_listener(identity.pid, dial)
                    .filter(|watch| watch.generation() == identity.generation.as_deref())
            })
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

    fn identity_changed(&self) -> Result<bool, super::InspectError> {
        for identity in &self.expected {
            let inspector = self.verifier.inspector();
            if inspector.executable_identity(identity.pid)? != identity.executable
                || inspector.listener_generation(identity.pid)? != identity.generation
            {
                return Ok(true);
            }
        }
        Ok(false)
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
        let native = self.native().await;
        if native == Some(false) {
            self.reuse.invalidate();
            return Verification::Mismatch;
        }
        match self.identity_changed() {
            Ok(true) => {
                self.reuse.invalidate();
                return Verification::Mismatch;
            }
            Err(_) => {
                self.reuse.invalidate();
                return Verification::Unverifiable;
            }
            Ok(false) if native == Some(true) => return Verification::Verified,
            Ok(false) => {}
        }
        self.reuse.verify(|| self.ask()).await
    }

    fn invalidate_positive(&self) {
        self.reuse.invalidate();
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
        generation: Option<String>,
        answers: Mutex<VecDeque<Result<bool, InspectError>>>,
        asked: AtomicUsize,
    }

    impl ScriptedWatch {
        fn new(answers: Vec<Result<bool, InspectError>>) -> Arc<Self> {
            Arc::new(Self {
                generation: None,
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
        fn generation(&self) -> Option<&str> {
            self.generation.as_deref()
        }

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
        generation: Mutex<Option<String>>,
        team: Mutex<Option<String>>,
        watches: Mutex<VecDeque<Arc<ScriptedWatch>>>,
        ttl: Mutex<Duration>,
        listens_asked: AtomicUsize,
        listeners_asked: AtomicUsize,
        /// While set, `listens_on` announces itself on `entered` and waits for `release` before it answers.
        hold: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
        /// How long every `listens_on` takes (the `lsof` it stands for).
        delay: Mutex<Duration>,
        /// How long the **next** `listens_on` takes on top of that, once (a one-off slow `lsof`: cold start, a scheduling hiccup).
        first_extra: Mutex<Option<Duration>>,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                listens: Mutex::new(VecDeque::new()),
                listeners: Mutex::new(VecDeque::new()),
                start_time: Mutex::new(START.to_owned()),
                generation: Mutex::new(None),
                team: Mutex::new(Some(TEAM.to_owned())),
                watches: Mutex::new(VecDeque::new()),
                ttl: Mutex::new(Duration::ZERO),
                listens_asked: AtomicUsize::new(0),
                listeners_asked: AtomicUsize::new(0),
                hold: Mutex::new(None),
                delay: Mutex::new(Duration::ZERO),
                first_extra: Mutex::new(None),
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

        /// Only the next question is slow, by `extra`.
        fn the_next_question_takes_extra(&self, extra: Duration) {
            *self.first_extra.lock().unwrap() = Some(extra);
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
            let extra = self.first_extra.lock().unwrap().take();
            let delay = *self.delay.lock().unwrap() + extra.unwrap_or(Duration::ZERO);
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
        fn listener_generation(&self, _pid: u32) -> Result<Option<String>, InspectError> {
            Ok(self.generation.lock().unwrap().clone())
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
            generation: None,
            executable: None,
        }
    }

    fn verifier(fake: &Arc<Fake>) -> ListenerVerifier {
        ListenerVerifier::new(fake.clone(), ListenerPolicy::new(TEAM).with_uid(ME))
    }

    fn owned(fake: &Arc<Fake>) -> OwnedListenerGuard {
        OwnedListenerGuard::new(fake.clone(), 42, dial(), START.to_owned(), None)
    }

    /// The real system inspector with one exception: the signing team is whatever the test says (a copy of a system tool has no team).
    /// Everything else (listening sockets, start time, generation, executable identity, uid, the native watch) is read from the system.
    #[cfg(target_os = "macos")]
    struct RealWithTeam(super::super::inspect::SystemInspector);

    #[cfg(target_os = "macos")]
    #[async_trait]
    impl ProcessInspector for RealWithTeam {
        fn pid_state(&self, pid: u32) -> PidState {
            self.0.pid_state(pid)
        }
        async fn start_time(&self, pid: u32) -> Result<String, InspectError> {
            self.0.start_time(pid).await
        }
        async fn command(&self, pid: u32) -> Result<String, InspectError> {
            self.0.command(pid).await
        }
        async fn process_group(&self, pid: u32) -> Result<u32, InspectError> {
            self.0.process_group(pid).await
        }
        async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError> {
            self.0.listens_on(pid, port).await
        }
        async fn listeners_on(&self, port: u16) -> Result<Vec<u32>, InspectError> {
            self.0.listeners_on(port).await
        }
        async fn listening_sockets(&self, port: u16) -> Result<Vec<ListeningSocket>, InspectError> {
            self.0.listening_sockets(port).await
        }
        async fn uid_of(&self, pid: u32) -> Result<u32, InspectError> {
            self.0.uid_of(pid).await
        }
        fn watch_listener(&self, pid: u32, dial: SocketAddr) -> Option<Arc<dyn ListenerWatch>> {
            self.0.watch_listener(pid, dial)
        }
        fn fallback_ttl(&self) -> Duration {
            self.0.fallback_ttl()
        }
        async fn generation(&self, pid: u32) -> Result<String, InspectError> {
            self.0.generation(pid).await
        }
        fn listener_generation(&self, pid: u32) -> Result<Option<String>, InspectError> {
            self.0.listener_generation(pid)
        }
        fn executable_identity(
            &self,
            pid: u32,
        ) -> Result<Option<super::super::inspect::ExecutableIdentity>, InspectError> {
            self.0.executable_identity(pid)
        }
        async fn signing_team(&self, _pid: u32) -> Result<Option<String>, InspectError> {
            Ok(Some(TEAM.to_owned()))
        }
    }

    /// Not a test: the body of the listener process of the test below (a copy of this binary run with the port in the environment).
    /// Under a normal run (no variable) it does nothing.
    #[cfg(target_os = "macos")]
    #[test]
    fn listener_helper() {
        let Ok(port) = std::env::var("IDORIS_GUARD_TEST_LISTEN_PORT") else {
            return;
        };
        let _listener = std::net::TcpListener::bind(("127.0.0.1", port.parse().unwrap())).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Codex pre-review H2: a running listener whose executable was deleted from disk (an app update replaced it, a package manager cleaned
    /// it up) is the same legitimate engine: it must still be accepted, by a fresh verification and by the guard built before the deletion.
    /// The identity is what the kernel runs, not a file at a path.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_listener_whose_executable_was_deleted_is_still_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("engine");
        // A copy of this test binary (ad-hoc signed, runs from anywhere) acts as the listener: see `listener_helper`.
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let mut child = std::process::Command::new(&exe)
            .args([
                "--exact",
                "engine::guard::tests::listener_helper",
                "--nocapture",
            ])
            .env("IDORIS_GUARD_TEST_LISTEN_PORT", port.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let real = || -> Arc<dyn ProcessInspector> {
            Arc::new(RealWithTeam(super::super::inspect::SystemInspector::new()))
        };
        let policy = || {
            let uid = rustix::process::getuid().as_raw();
            ListenerPolicy::new(TEAM).with_uid(uid)
        };
        let verifier = ListenerVerifier::new(real(), policy());
        let deadline = Instant::now() + Duration::from_secs(10);
        let listeners = loop {
            if let ListenerVerdict::Verified { listeners } = verifier.check(port).await {
                break listeners;
            }
            assert!(Instant::now() < deadline, "the listener never came up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let guard = ExternalListenerGuard::new(
            verifier.clone(),
            SocketAddr::new(MANAGED_DIAL, port),
            listeners,
        );
        assert_eq!(guard.verify().await, Verification::Verified);
        std::fs::remove_file(&exe).unwrap();
        assert_eq!(
            guard.verify().await,
            Verification::Verified,
            "the guard built before the deletion"
        );
        let fresh = ListenerVerifier::new(real(), policy());
        assert!(
            fresh.check(port).await.is_verified(),
            "a fresh verification after the deletion"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn an_owned_fallback_does_not_authorize_a_recycled_pid() {
        let fake = Fake::new();
        fake.script_listens(vec![Ok(true)]);
        fake.reuse_for(Duration::from_secs(60));
        let guard = owned(&fake);
        assert_eq!(guard.verify().await, Verification::Verified);
        *fake.start_time.lock().unwrap() = "recycled".into();
        assert_eq!(guard.verify().await, Verification::Mismatch);
    }

    #[tokio::test]
    async fn an_external_watch_must_be_bound_to_the_signature_verified_generation() {
        let fake = Fake::new();
        let mut watch = ScriptedWatch::new(vec![Ok(true)]);
        Arc::get_mut(&mut watch).unwrap().generation = Some("replacement generation".into());
        fake.offer_watches(vec![watch.clone()]);
        fake.script_listeners(vec![Ok(vec![42])]);
        *fake.generation.lock().unwrap() = Some("original generation".into());
        let verifier = verifier(&fake);
        let ListenerVerdict::Verified { listeners } = verifier.check(PORT).await else {
            panic!("the original process should verify");
        };
        let guard = ExternalListenerGuard::new(verifier, dial(), listeners);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(
            watch.asked(),
            0,
            "the replacement watch cannot vouch for the signed process"
        );
        assert_eq!(
            fake.listeners_asked(),
            2,
            "the original is checked via the fallback"
        );
    }

    #[tokio::test]
    async fn a_connect_failure_invalidates_only_its_generation_before_retry() {
        use crate::omlx::key::{
            EndpointKey, KeyRefusal, authorize_omlx_request, forget_endpoint,
            govern_endpoint_guarded, send_authorized,
        };

        let url = "http://127.0.0.1:0";
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let first = Fake::new();
        first.script_listens(vec![Ok(true)]);
        govern_endpoint_guarded(
            url,
            EndpointKey::Private("first".into()),
            Some(Arc::new(owned(&first))),
        );
        let old = authorize_omlx_request(url, None).await.unwrap();

        let next = Fake::new();
        next.reuse_for(Duration::from_secs(60));
        next.script_listens(vec![Ok(true)]);
        govern_endpoint_guarded(
            url,
            EndpointKey::Private("next".into()),
            Some(Arc::new(owned(&next))),
        );
        let current = authorize_omlx_request(url, None).await.unwrap();
        assert_eq!(next.listens_asked(), 1);
        next.script_listens(vec![Ok(false)]);

        let error = send_authorized(Some(&old), client.get(url).send())
            .await
            .unwrap_err();
        assert!(error.is_connect());
        assert!(
            authorize_omlx_request(url, Some(current.generation()))
                .await
                .is_ok()
        );
        assert_eq!(next.listens_asked(), 1);

        let error = send_authorized(Some(&current), client.get(url).send())
            .await
            .unwrap_err();
        assert!(error.is_connect());
        assert_eq!(
            authorize_omlx_request(url, Some(current.generation()))
                .await
                .unwrap_err(),
            KeyRefusal::ListenerChanged
        );
        assert_eq!(next.listens_asked(), 2);
        forget_endpoint(url);
    }

    /// Codex pre-review M3: a send that the caller's own deadline cancelled (possibly in the connect phase, where no connection error is ever
    /// produced) must not leave the generation's positive cache standing: the retry checks the listener again.
    #[tokio::test]
    async fn a_send_cancelled_by_an_outer_deadline_invalidates_its_generation() {
        use crate::omlx::key::{
            EndpointKey, KeyRefusal, authorize_omlx_request, forget_endpoint,
            govern_endpoint_guarded, send_authorized,
        };

        let url = "http://127.0.0.1:1";
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        fake.script_listens(vec![Ok(true)]);
        govern_endpoint_guarded(
            url,
            EndpointKey::Private("k".into()),
            Some(Arc::new(owned(&fake))),
        );
        let current = authorize_omlx_request(url, None).await.unwrap();
        assert_eq!(fake.listens_asked(), 1);
        fake.script_listens(vec![Ok(false)]);
        // Still cached: no second question.
        assert!(
            authorize_omlx_request(url, Some(current.generation()))
                .await
                .is_ok()
        );
        assert_eq!(fake.listens_asked(), 1);

        let never = std::future::pending::<Result<reqwest::Response, reqwest::Error>>();
        let cancelled = tokio::time::timeout(
            Duration::from_millis(20),
            send_authorized(Some(&current), never),
        )
        .await;
        assert!(cancelled.is_err(), "the deadline cut the send off");
        assert_eq!(
            authorize_omlx_request(url, Some(current.generation()))
                .await
                .unwrap_err(),
            KeyRefusal::ListenerChanged
        );
        assert_eq!(fake.listens_asked(), 2);
        forget_endpoint(url);
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
            OwnedListenerGuard::new(fake.clone(), 42, dial(), START.to_owned(), None).dial(),
            Some(dial())
        );
        assert_eq!(
            OwnedListenerGuard::new(fake.clone(), 42, v6, START.to_owned(), None).dial(),
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
        let _ = OwnedListenerGuard::new(recording.clone(), 42, v6, START.to_owned(), None);
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
        let guard = OwnedListenerGuard::new(
            watch_only(vec![watch.clone()]),
            42,
            dial(),
            START.to_owned(),
            None,
        );
        for _ in 0..5 {
            assert_eq!(guard.verify().await, Verification::Verified);
        }
        assert_eq!(watch.asked(), 5, "asked on every request, nothing reused");

        // The watch says the engine let go of the port: the very next request is refused, and so is every one after it.
        let watch = ScriptedWatch::new(vec![Ok(true), Ok(false)]);
        let guard =
            OwnedListenerGuard::new(watch_only(vec![watch]), 42, dial(), START.to_owned(), None);
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
        // A is inside the slow check, holding it (a bounded wait: a guard that never gets there fails the test instead of hanging it)
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
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
        // an earlier native no: the generation is not the initial one, and the callers still share the check that starts after it
        guard.reuse.invalidate();
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

    /// **A native no that overtakes a finished check.** A caller queued behind a check may take its conclusion only while no native "no" has
    /// overtaken it: the window between the check finishing and the caller getting the lock is a few microseconds wide, so the rule is
    /// tested as the predicate it is.
    fn a_state(last: Verification, last_started: Instant) -> ReuseState {
        ReuseState {
            epoch: 3,
            yes_from: None,
            finished: 5,
            last,
            last_epoch: 3,
            last_started,
        }
    }

    #[test]
    fn a_shared_conclusion_is_taken_only_while_no_native_no_has_overtaken_it() {
        let ttl = Duration::from_secs(60);
        let now = Instant::now();
        let mut state = a_state(Verification::Verified, now);
        assert_eq!(
            state.shared_since(4, ttl),
            Some(Verification::Verified),
            "a check finished while the caller was queued"
        );
        assert_eq!(
            state.shared_since(5, ttl),
            None,
            "nothing finished since the caller arrived"
        );
        // a native no after that check started: the conclusion is older than it
        state.epoch = 4;
        assert_eq!(state.shared_since(4, ttl), None);
        // a check that was itself overtaken while it ran (its conclusion is a refusal, but it was reached under the old epoch) is not shared
        // either: the callers behind it ask for themselves
        state.last = Verification::Mismatch;
        assert_eq!(state.shared_since(4, ttl), None);
    }

    /// Codex pre-review of layer 2, M3, and the re-review's #4: a queued caller takes a **yes** only inside the freshness window, counted from when that check
    /// started — **whenever it started**. A check that started after the caller arrived but ran past the window used to be handed over ("as fresh as one the
    /// caller would have started itself"); a yes that old is not one, for the caller as for anybody. A refusal ("no", "cannot tell") is shared however old: it
    /// only errs on the safe side.
    #[test]
    fn a_queued_caller_takes_a_yes_only_inside_the_freshness_window() {
        let ttl = Duration::from_millis(250);
        let now = Instant::now();
        let long_ago = now.checked_sub(Duration::from_secs(2)).unwrap();
        // a yes from a check that started `long_ago`: older than the window, and that is that — the caller arrived before it started or after, it does not matter
        let old_yes = a_state(Verification::Verified, long_ago);
        assert_eq!(
            old_yes.shared_since(4, ttl),
            None,
            "a yes older than the window is not handed to a caller that queued behind it"
        );
        // a young yes is shared
        let young_yes = a_state(Verification::Verified, now);
        assert_eq!(young_yes.shared_since(4, ttl), Some(Verification::Verified));
        // with no reuse at all (`ttl` 0) a yes is never handed over
        assert_eq!(young_yes.shared_since(4, Duration::ZERO), None);
        // refusals are shared however old
        for refusal in [Verification::Mismatch, Verification::Unverifiable] {
            assert_eq!(
                a_state(refusal, long_ago).shared_since(4, ttl),
                Some(refusal)
            );
        }
    }

    /// The same through the guard, with real queueing: a check takes 300 ms against a 100 ms window. Its yes was already stale when it ended, so **nobody** uses it:
    /// the asker observes once more (another 300 ms, stale again) and the answer is "cannot tell" — which the callers queued behind it share (a refusal is shared
    /// however old), so a slow inspection costs two questions in all, not one per caller. Nothing is stored: the window stays closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_inspection_that_outlasts_the_window_twice_is_cannot_tell_for_everybody_and_stores_nothing()
     {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_millis(100));
        fake.each_question_takes(Duration::from_millis(300));
        fake.script_listens(vec![Ok(true)]);
        let guard = Arc::new(owned(&fake));
        let first = {
            let guard = guard.clone();
            tokio::spawn(async move { guard.verify().await })
        };
        // the others arrive while the first check runs
        tokio::time::sleep(Duration::from_millis(50)).await;
        let queued: Vec<_> = (0..4)
            .map(|_| {
                let guard = guard.clone();
                tokio::spawn(async move { guard.verify().await })
            })
            .collect();
        assert_eq!(first.await.unwrap(), Verification::Unverifiable);
        for task in queued {
            assert_eq!(task.await.unwrap(), Verification::Unverifiable);
        }
        assert_eq!(
            fake.listens_asked(),
            2,
            "the first inspection and its one re-observation; the queued callers shared the refusal"
        );
        // nothing was stored: once the inspection is quick enough, the next caller asks for itself and a yes opens the window
        fake.each_question_takes(Duration::ZERO);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 3);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 3, "the fresh yes opened the window");
    }

    /// A one-off slow inspection (cold start, a scheduling hiccup) costs one more question, not a refusal: the first answer ran past the window and is not
    /// used, the re-observation is quick and its yes is used and opens the window.
    #[tokio::test]
    async fn a_one_off_slow_inspection_is_observed_once_more_and_the_quick_answer_is_used() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_millis(100));
        fake.the_next_question_takes_extra(Duration::from_millis(250));
        fake.script_listens(vec![Ok(true)]);
        let guard = owned(&fake);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 2, "the slow answer was not used");
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(
            fake.listens_asked(),
            2,
            "the quick one was stored and opened the window"
        );
    }

    /// With no reuse window (`ttl` 0: every request asks for itself and uses its own answer at once) there is no age bound: a slow inspection's yes is used.
    #[tokio::test]
    async fn without_a_reuse_window_a_requests_own_answer_has_no_age_bound() {
        let fake = Fake::new();
        fake.each_question_takes(Duration::from_millis(150));
        fake.script_listens(vec![Ok(true)]);
        let guard = owned(&fake);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listens_asked(), 1);
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

//! 发送前重新确认监听者的守卫（IDORIS-CORE S6，Codex 终审 H1；owner 2026-11-22 拍板；主会话之后要求「每个请求的确认 p99 < 5 毫秒、
//! 请求路径上不起子进程」）。
//!
//! 引擎管理器登记一个端点的口径（私有 key、外部）时，把一个 [`EndpointGuard`] 一起交给 [`crate::omlx::key`] 的登记表；之后发往这个
//! 端点的每个请求，在**发出之前**由 [`crate::omlx::key::authorize_omlx_request`] 在发送它的那个任务里调守卫。确认分成两半，各有各的
//! 代价与窗口：
//!
//! - **每个请求（这里）**：登记时认的**那个进程**（pid 加启动时间，建守卫那一刻钉死）此刻还握着这个端口上的一个 LISTEN 套接字，绑在
//!   客户端连得到的地址上（[`ProcessInspector::watch_listener`]：macOS 的 libproc、Linux 的 `/proc`，两三次系统调用，不起子进程，
//!   p99 在几微秒到几十微秒）。引擎放掉了端口、进程没了、pid 被回收成了另一个进程，下一个请求就发现。
//! - **每个监督节拍和每次状态变化（引擎管理器，不在请求路径上）**：贵的那些——自有引擎「这个端口上只有它一个监听者」（`lsof`，
//!   [`ProcessInspector::listens_on`]），外部引擎的 uid、签名团队与监听者身份相等（[`ListenerVerifier::check`]）。引擎还握着它自己的
//!   套接字、另一个进程在同一个端口上绑了一个更具体的地址（一个并存的监听者），每个请求的确认看不见，**下一个节拍**才看见（默认
//!   `liveness_interval` 是 1 秒，再加 `lsof` 自己的时间）；看见之后管理器先收回凭证再发布状态。
//!
//! **退路**（原生监视建不出来：别的 unix、读不了那个进程；或某一次读不出来）：每个请求问那个贵的问题——自有引擎 `listens_on`，外部
//! 引擎 [`ListenerVerifier::check`]——而肯定的答案至多重用 [`ProcessInspector::fallback_ttl`]（真的检查器是
//! [`super::FALLBACK_TTL`]，250 毫秒；测试里编排好的假检查器是 0，每个请求都问）。重用的窗口里换了监听者，最多晚这么久才被请求路径
//! 看到；任何一次不是「是」的答案、换一个守卫（登记换代、换进程）都立刻作废它。
//!
//! 确认不了（读不出来）就是 [`Verification::Unverifiable`]，调用方拒绝发送（fail closed）：读一次不行的立刻再问一遍，仍不行才算。
//!
//! **没有消除的间隙**：确认与发送之间仍有毫秒级的间隙，本机**另一个用户**的进程恰好在这几毫秒里抢到端口，仍能收到那一个请求（同一个
//! 用户的进程本来就读得到 `settings.json` 与进程环境；libproc 与 `lsof` 一样看不到别的用户的套接字）；「并存的监听者」到下一个节拍才
//! 被发现。不做逐连接的对端进程核对。规范勘误十（8）把这些窗口写成已知限制。

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::inspect::{ListenerWatch, ProcessInspector};
use super::listener::{ListenerIdentity, ListenerVerdict, ListenerVerifier};
use crate::omlx::key::{EndpointGuard, Verification};

/// 退路上那个贵的问题的**肯定**答案，在 `ttl` 之内可以重用（`ttl` 为 0 就是从不重用：`elapsed() < 0` 永远是假）。只记「是」：问到「不是」
/// 与读不出来的时候，窗口本来就已经过了（窗口之内不问），没有什么要作废的；只有原生监视说「不握了」时，才要把之前记下的「是」作废
/// （[`Self::forget`]）。
#[derive(Debug)]
struct Reuse {
    ttl: Duration,
    said_yes: Mutex<Option<Instant>>,
}

impl Reuse {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            said_yes: Mutex::new(None),
        }
    }

    /// 上一次的「是」还在期限之内。
    fn fresh(&self) -> bool {
        self.said_yes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|at| at.elapsed() < self.ttl)
    }

    fn remember_yes(&self) {
        *self.said_yes.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    fn forget(&self) {
        *self.said_yes.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// 自有引擎的守卫：它的进程（`pid`）此刻还握着端口 `port` 上的一个 LISTEN 套接字。
pub struct OwnedListenerGuard {
    inspector: Arc<dyn ProcessInspector>,
    pid: u32,
    port: u16,
    /// 每个请求的原生监视（建守卫那一刻钉死进程的身份）；检查器没有（`None`）就走退路。
    watch: Option<Arc<dyn ListenerWatch>>,
    reuse: Reuse,
}

impl OwnedListenerGuard {
    pub fn new(inspector: Arc<dyn ProcessInspector>, pid: u32, port: u16) -> Self {
        let watch = inspector.watch_listener(pid, port);
        let reuse = Reuse::new(inspector.fallback_ttl());
        Self {
            inspector,
            pid,
            port,
            watch,
            reuse,
        }
    }
}

impl std::fmt::Debug for OwnedListenerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedListenerGuard")
            .field("pid", &self.pid)
            .field("port", &self.port)
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
                    self.reuse.forget();
                    return Verification::Mismatch;
                }
                // 原生的这一次没能回答（读不了这个进程）：问那个贵的。
                Err(_) => {}
            }
        }
        if self.reuse.fresh() {
            return Verification::Verified;
        }
        // 读不出来的那一次立刻再问一遍；两次都读不出来才是读不出来。
        for _ in 0..2 {
            match self.inspector.listens_on(self.pid, self.port).await {
                Ok(true) => {
                    self.reuse.remember_yes();
                    return Verification::Verified;
                }
                Ok(false) => return Verification::Mismatch,
                Err(_) => {}
            }
        }
        Verification::Unverifiable
    }
}

/// 外部引擎的守卫：登记时校验通过的那些监听者（`expected`，pid 加启动时间）此刻还都握着端口 `port` 上的一个 LISTEN 套接字。
pub struct ExternalListenerGuard {
    verifier: ListenerVerifier,
    port: u16,
    expected: Vec<ListenerIdentity>,
    /// 每个被校验的监听者一个原生监视；有一个建不出来（检查器没有原生监视、读不了那个进程）就整个没有，走退路。
    watches: Option<Vec<Arc<dyn ListenerWatch>>>,
    reuse: Reuse,
}

impl ExternalListenerGuard {
    pub fn new(verifier: ListenerVerifier, port: u16, expected: Vec<ListenerIdentity>) -> Self {
        let watches: Option<Vec<Arc<dyn ListenerWatch>>> = expected
            .iter()
            .map(|identity| verifier.inspector().watch_listener(identity.pid, port))
            .collect();
        let reuse = Reuse::new(verifier.inspector().fallback_ttl());
        Self {
            verifier,
            port,
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
}

impl std::fmt::Debug for ExternalListenerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalListenerGuard")
            .field("port", &self.port)
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
                self.reuse.forget();
                return Verification::Mismatch;
            }
            None => {}
        }
        if self.reuse.fresh() {
            return Verification::Verified;
        }
        // 读不出来的那一次立刻再问一遍；两次都读不出来才是读不出来。
        for _ in 0..2 {
            match self.verifier.check(self.port).await {
                ListenerVerdict::Verified { listeners } if listeners == self.expected => {
                    self.reuse.remember_yes();
                    return Verification::Verified;
                }
                // 通过了校验，但已经不是登记时认的那个（换了进程、多了一个、少了一个）；没有监听者；监听者不合格。
                ListenerVerdict::Verified { .. }
                | ListenerVerdict::NoListener
                | ListenerVerdict::Rejected(_) => return Verification::Mismatch,
                ListenerVerdict::Unverifiable(_) => {}
            }
        }
        Verification::Unverifiable
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::inspect::{InspectError, PidState};
    use super::super::listener::ListenerPolicy;
    use super::*;

    const TEAM: &str = "PSK5Q5T46L";
    const ME: u32 = 501;
    const START: &str = "Fri Nov 20 15:14:05 2026";

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
    /// `watch_listener` call, in order) and a reuse window, and it counts the expensive questions it is asked.
    struct Fake {
        listens: Mutex<VecDeque<Result<bool, InspectError>>>,
        listeners: Mutex<VecDeque<Result<Vec<u32>, InspectError>>>,
        start_time: Mutex<String>,
        team: Mutex<Option<String>>,
        watches: Mutex<VecDeque<Arc<ScriptedWatch>>>,
        ttl: Mutex<Duration>,
        listens_asked: AtomicUsize,
        listeners_asked: AtomicUsize,
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
        fn watch_listener(&self, _pid: u32, _port: u16) -> Option<Arc<dyn ListenerWatch>> {
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

    // ----- the slow question (no watch, no reuse window): what the guards asked before the per-request watch existed -----------------

    #[tokio::test]
    async fn an_owned_engine_is_verified_only_while_it_is_the_listener_and_an_unreadable_answer_is_asked_twice()
     {
        let fake = Fake::new();
        let guard = OwnedListenerGuard::new(fake.clone(), 42, 19731);
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
        let guard = ExternalListenerGuard::new(verifier(&fake), 19731, vec![identity(42)]);
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
        let guard = ExternalListenerGuard::new(verifier(&fake), 19731, vec![identity(42)]);
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
        async fn uid_of(&self, _pid: u32) -> Result<u32, InspectError> {
            panic!("uid_of asked on the request path")
        }
        async fn signing_team(&self, _pid: u32) -> Result<Option<String>, InspectError> {
            panic!("signing_team asked on the request path")
        }
        fn watch_listener(&self, _pid: u32, _port: u16) -> Option<Arc<dyn ListenerWatch>> {
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
        let guard = OwnedListenerGuard::new(watch_only(vec![watch.clone()]), 42, 19731);
        for _ in 0..5 {
            assert_eq!(guard.verify().await, Verification::Verified);
        }
        assert_eq!(watch.asked(), 5, "asked on every request, nothing reused");

        // The watch says the engine let go of the port: the very next request is refused, and so is every one after it.
        let watch = ScriptedWatch::new(vec![Ok(true), Ok(false)]);
        let guard = OwnedListenerGuard::new(watch_only(vec![watch]), 42, 19731);
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
        let guard = ExternalListenerGuard::new(both, 19731, vec![identity(42), identity(43)]);
        assert_eq!(guard.verify().await, Verification::Verified);

        let one_left = ListenerVerifier::new(
            watch_only(vec![
                ScriptedWatch::new(vec![Ok(true)]),
                ScriptedWatch::new(vec![Ok(false)]),
            ]),
            ListenerPolicy::new(TEAM).with_uid(ME),
        );
        let guard = ExternalListenerGuard::new(one_left, 19731, vec![identity(42), identity(43)]);
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
            19731,
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
        let guard = OwnedListenerGuard::new(fake.clone(), 42, 19731);
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
            ExternalListenerGuard::new(verifier(&fake), 19731, vec![identity(42), identity(43)]);
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
            ExternalListenerGuard::new(verifier(&fake), 19731, vec![identity(42), identity(43)]);
        fake.script_listeners(vec![Ok(vec![42, 43])]);
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(fake.listeners_asked(), 1);
    }

    /// The reuse window of the slow check (the platform has no native watch): many requests inside it cost one `lsof`; a request after
    /// it asks again; only a "yes" is reused, and anything else clears it at once.
    #[tokio::test]
    async fn the_slow_check_is_reused_for_a_short_while_only_and_only_when_it_said_yes() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_millis(150));
        let guard = OwnedListenerGuard::new(fake.clone(), 42, 19731);
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
        let guard = OwnedListenerGuard::new(fake.clone(), 42, 19731);
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

    /// A native "no" clears the slow check's reuse window: a stale "yes" must not be served once the watch said the process let go.
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
        let guard = OwnedListenerGuard::new(fake.clone(), 42, 19731);
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
            ExternalListenerGuard::new(verifier(&fake), 19731, vec![identity(42), identity(43)]);
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
        let guard = ExternalListenerGuard::new(verifier(&fake), 19731, vec![identity(42)]);
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

    /// Requests are concurrent: many racing callers share one guard and its reuse window. Every one is answered, nothing deadlocks or
    /// poisons, and the expensive question is asked at most once per caller (after a caller's own first answer the window is open for it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_share_one_reuse_window() {
        let fake = Fake::new();
        fake.reuse_for(Duration::from_secs(60));
        fake.script_listens(vec![Ok(true)]);
        let guard = Arc::new(OwnedListenerGuard::new(fake.clone(), 42, 19731));
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
        assert!(
            fake.listens_asked() <= 32,
            "asked {} times for 32 callers",
            fake.listens_asked()
        );
    }
}

//! 端口属主校验（[#619](https://github.com/iDoris-ai/iDoris/issues/619)；IDORIS-CORE S5）。
//!
//! 向某个 loopback 端口发 API key 之前，先确认**现在在这个端口上监听的**就是该引擎：否则本机另一个进程只要抢在引擎前面
//! 绑住端口（引擎没在运行时，抢先 bind），就能拿到 key。本模块回答「这个端口上现在是谁，是不是我们认的那种引擎」，
//! 用的都是 [`super::ProcessInspector`]（`lsof` 找监听者，`ps` 读 uid，`codesign` 读进程正在运行的代码的签名团队），
//! 没有任何 unsafe，也没有新依赖。
//!
//! # 判据（issue 的修法，原样）
//!
//! 1. 找出监听这个端口的**全部** pid（回环、通配、IPv6 的监听都算：连到 `127.0.0.1:<port>` 的请求可能落在其中任何一个）；
//!    没有人监听不是通过（[`ListenerVerdict::NoListener`]）；
//! 2. 每个 pid 的 uid 都等于 iDoris 的有效 uid（别的用户的进程不是我们的引擎）；
//! 3. 每个 pid 正在运行的代码的签名团队都等于要求的那个（oMLX：`PSK5Q5T46L`；签名无效、没有签名、ad-hoc 都不算）；
//! 4. 校验通过之前不发 key；校验结果按 **pid + 启动时间** 缓存，进程换了（pid 变了，或同一个 pid 的启动时间变了）就重查；
//!    端口上出现过监听的空档（这个 pid 不在监听者里了）也废弃它的记录：启动时间只到秒（`ps -o lstart`），同一秒内被回收的同一个
//!    pid 再绑上这个端口，（pid、启动时间）一模一样，不能靠缓存放过它（IDORIS-CORE S6，Codex 终审 L1）；
//! 5. 读不出来（工具不存在、超时）**不是**通过，也不是被拒：[`ListenerVerdict::Unverifiable`]，调用方按「没有 key」处理。
//!
//! 「校验与发送之间的连接复用被关闭」不在这里：它是调用方发请求的方式（每个带 key 的请求都用新开的连接，
//! 即 `pool_max_idle_per_host(0)` 的客户端），见 [`fresh_connection_client`]。
//!
//! 这个校验的威胁模型是**不知情的另一个客户端**（占着端口的别的服务、沙箱里的进程、别的用户的进程），不是同 uid 的恶意进程：
//! 同 uid 的进程本来就读得到 `~/.omlx/settings.json`。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use super::fsutil::effective_uid;
use super::inspect::{InspectError, ProcessInspector};

/// 校验要求什么：uid 与签名团队。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerPolicy {
    team_id: String,
    uid: u32,
}

impl ListenerPolicy {
    /// 监听者要属于当前有效 uid，且代码的签名团队是 `team_id`。
    pub fn new(team_id: impl Into<String>) -> Self {
        Self {
            team_id: team_id.into(),
            uid: effective_uid(),
        }
    }

    /// 换掉要求的 uid（测试里造「别人的进程」用）。
    pub fn with_uid(mut self, uid: u32) -> Self {
        self.uid = uid;
        self
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }
}

/// 监听者不合格的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerRejection {
    /// 监听者是别的用户的进程。
    OtherUser { pid: u32 },
    /// 监听者的代码没有（有效的）团队签名。
    NoTeam { pid: u32 },
    /// 监听者的代码是别的团队签的。
    WrongTeam { pid: u32 },
}

impl ListenerRejection {
    /// 稳定的短码，给日志。
    pub fn code(&self) -> &'static str {
        match self {
            Self::OtherUser { .. } => "other_user",
            Self::NoTeam { .. } => "no_team",
            Self::WrongTeam { .. } => "wrong_team",
        }
    }
}

/// 通过校验的一个监听者的**身份**：pid 加 `ps` 的启动时间。pid 会被复用，所以只比 pid 分不出「还是它」与「换了一个进程」；
/// 加上启动时间就分得开（IDORIS-CORE S6：外部引擎换了进程，Supervisor 的账本就过期了）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ListenerIdentity {
    pub pid: u32,
    pub started_at: String,
}

/// [`ListenerVerifier::check`] 的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerVerdict {
    /// 监听这个端口的每个进程都通过了：uid 与签名团队都对。`listeners` 按 pid 升序，各带启动时间。
    Verified { listeners: Vec<ListenerIdentity> },
    /// 此刻没有人监听这个端口。
    NoListener,
    /// 有监听者不合格：**不能**发 key。
    Rejected(ListenerRejection),
    /// 读不出来（工具不存在、超时、没有答案；校验期间监听者换了）：既不能说是，也不能说不是，按「没有 key」处理。
    Unverifiable(InspectError),
}

impl ListenerVerdict {
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    /// 通过校验的监听者的 pid（升序）；没通过时是空的。
    pub fn pids(&self) -> Vec<u32> {
        match self {
            Self::Verified { listeners } => listeners.iter().map(|listener| listener.pid).collect(),
            _ => Vec::new(),
        }
    }
}

/// 端口属主校验器：检查器、要求、按「pid → 启动时间」缓存的通过记录。克隆很便宜，克隆共用同一份缓存。
#[derive(Clone)]
pub struct ListenerVerifier {
    inspector: Arc<dyn ProcessInspector>,
    policy: ListenerPolicy,
    /// 已通过校验的 pid 与当时的启动时间。pid 的启动时间变了就是另一个进程，缓存作废。
    verified: Arc<Mutex<BTreeMap<u32, String>>>,
}

impl std::fmt::Debug for ListenerVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerVerifier")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl ListenerVerifier {
    pub fn new(inspector: Arc<dyn ProcessInspector>, policy: ListenerPolicy) -> Self {
        Self {
            inspector,
            policy,
            verified: Arc::default(),
        }
    }

    /// 这个校验器用的检查器：守卫（[`super::ExternalListenerGuard`]）从它为每个被校验的监听者建每个请求的监视。
    pub fn inspector(&self) -> &Arc<dyn ProcessInspector> {
        &self.inspector
    }

    fn cached(&self, pid: u32, started_at: &str) -> bool {
        self.verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pid)
            .is_some_and(|cached| cached == started_at)
    }

    fn remember(&self, pid: u32, started_at: String) {
        self.verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid, started_at);
    }

    /// 校验现在监听 `port` 的是不是合格的引擎进程。每个监听者先读启动时间：缓存里有同样的（pid、启动时间）就直接通过，
    /// 否则校验 uid 与签名团队，并在**校验之后**再读一次启动时间：两次不同说明校验期间这个 pid 换了进程，不缓存、
    /// 也不算通过（[`ListenerVerdict::Unverifiable`]）。
    pub async fn check(&self, port: u16) -> ListenerVerdict {
        let pids = match self.inspector.listeners_on(port).await {
            Ok(pids) => pids,
            Err(error) => return ListenerVerdict::Unverifiable(error),
        };
        // 监听上的空档废弃通过记录（IDORIS-CORE S6，Codex 终审 L1）：`ps` 给的启动时间只到秒，同一秒内被回收、又绑上这个端口的
        // 同一个 pid，读出来的（pid、启动时间）与之前通过校验的那个一模一样，缓存会跳过 uid 与签名团队的复核。端口上没有这个
        // pid 的那一刻，它的通过记录就不再作数：它之后再出现，要重新过 uid 与签名团队。
        {
            let mut verified = self.verified.lock().unwrap_or_else(PoisonError::into_inner);
            if pids.is_empty() {
                verified.clear();
            } else {
                verified.retain(|pid, _| pids.contains(pid));
            }
        }
        if pids.is_empty() {
            return ListenerVerdict::NoListener;
        }
        let mut listeners = Vec::with_capacity(pids.len());
        for &pid in &pids {
            let started_at = match self.inspector.start_time(pid).await {
                Ok(started_at) => started_at,
                Err(error) => return ListenerVerdict::Unverifiable(error),
            };
            if !self.cached(pid, &started_at) {
                match self.inspector.uid_of(pid).await {
                    Ok(uid) if uid == self.policy.uid => {}
                    Ok(_) => {
                        return ListenerVerdict::Rejected(ListenerRejection::OtherUser { pid });
                    }
                    Err(error) => return ListenerVerdict::Unverifiable(error),
                }
                match self.inspector.signing_team(pid).await {
                    Ok(Some(team)) if team == self.policy.team_id => {}
                    Ok(Some(_)) => {
                        return ListenerVerdict::Rejected(ListenerRejection::WrongTeam { pid });
                    }
                    Ok(None) => {
                        return ListenerVerdict::Rejected(ListenerRejection::NoTeam { pid });
                    }
                    Err(error) => return ListenerVerdict::Unverifiable(error),
                }
                // 校验期间这个 pid 若换了进程（旧的退出、pid 被复用），上面查的就不是现在的它。
                match self.inspector.start_time(pid).await {
                    Ok(after) if after == started_at => {}
                    Ok(_) => return ListenerVerdict::Unverifiable(InspectError::NoAnswer),
                    Err(error) => return ListenerVerdict::Unverifiable(error),
                }
                self.remember(pid, started_at.clone());
            }
            listeners.push(ListenerIdentity { pid, started_at });
        }
        ListenerVerdict::Verified { listeners }
    }
}

/// 发带 key 的探测请求用的 HTTP 客户端：不走代理、不跟随重定向，**不保留空闲连接**
/// （`pool_max_idle_per_host(0)`）：每个请求都新开连接，所以带 key 的请求只会走属主校验**之后**才建立的连接，
/// 不会复用校验之前（比如一次匿名探测）留在池里的、通往别的进程的连接。
pub fn fresh_connection_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .pool_max_idle_per_host(0)
        .build()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::super::inspect::PidState;
    use super::*;

    const TEAM: &str = "PSK5Q5T46L";
    const ME: u32 = 501;

    /// 可编排的检查器：端口 → 监听 pid；pid → (uid, 团队, 启动时间)；并数各调用的次数。
    #[derive(Default)]
    struct Scripted {
        listeners: Mutex<BTreeMap<u16, Result<Vec<u32>, InspectError>>>,
        procs: Mutex<BTreeMap<u32, Proc>>,
        team_calls: AtomicUsize,
        uid_calls: AtomicUsize,
    }

    #[derive(Clone)]
    struct Proc {
        uid: Result<u32, InspectError>,
        team: Result<Option<String>, InspectError>,
        start: Result<String, InspectError>,
        /// 第二次读启动时间时换成这个（造「校验期间换了进程」）。
        start_after: Option<String>,
        reads: usize,
    }

    impl Proc {
        fn ok() -> Self {
            Self {
                uid: Ok(ME),
                team: Ok(Some(TEAM.to_owned())),
                start: Ok("Fri Nov 20 15:14:05 2026".to_owned()),
                start_after: None,
                reads: 0,
            }
        }
    }

    impl Scripted {
        fn listening(self, port: u16, pids: &[u32]) -> Self {
            self.listeners
                .lock()
                .unwrap()
                .insert(port, Ok(pids.to_vec()));
            self
        }

        fn process(self, pid: u32, proc: Proc) -> Self {
            self.procs.lock().unwrap().insert(pid, proc);
            self
        }
    }

    #[async_trait]
    impl ProcessInspector for Scripted {
        fn pid_state(&self, _pid: u32) -> PidState {
            PidState::Alive
        }
        async fn start_time(&self, pid: u32) -> Result<String, InspectError> {
            let mut procs = self.procs.lock().unwrap();
            let proc = procs.get_mut(&pid).ok_or(InspectError::NoAnswer)?;
            proc.reads += 1;
            if proc.reads > 1
                && let Some(after) = &proc.start_after
            {
                return Ok(after.clone());
            }
            proc.start.clone()
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
        async fn listeners_on(&self, port: u16) -> Result<Vec<u32>, InspectError> {
            self.listeners
                .lock()
                .unwrap()
                .get(&port)
                .cloned()
                .unwrap_or(Ok(Vec::new()))
        }
        async fn uid_of(&self, pid: u32) -> Result<u32, InspectError> {
            self.uid_calls.fetch_add(1, Ordering::SeqCst);
            self.procs
                .lock()
                .unwrap()
                .get(&pid)
                .ok_or(InspectError::NoAnswer)?
                .uid
        }
        async fn signing_team(&self, pid: u32) -> Result<Option<String>, InspectError> {
            self.team_calls.fetch_add(1, Ordering::SeqCst);
            self.procs
                .lock()
                .unwrap()
                .get(&pid)
                .ok_or(InspectError::NoAnswer)?
                .team
                .clone()
        }
    }

    /// A listener as `Proc::ok()` is scripted: its pid and the start time it reads.
    fn identity(pid: u32) -> ListenerIdentity {
        ListenerIdentity {
            pid,
            started_at: "Fri Nov 20 15:14:05 2026".to_owned(),
        }
    }

    fn rig(scripted: Scripted) -> (ListenerVerifier, Arc<Scripted>) {
        let scripted = Arc::new(scripted);
        let verifier =
            ListenerVerifier::new(scripted.clone(), ListenerPolicy::new(TEAM).with_uid(ME));
        (verifier, scripted)
    }

    #[tokio::test]
    async fn a_same_user_listener_signed_by_the_team_passes() {
        let (verifier, _) = rig(Scripted::default()
            .listening(19731, &[42])
            .process(42, Proc::ok()));
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Verified {
                listeners: vec![identity(42)]
            }
        );
    }

    #[tokio::test]
    async fn nobody_listening_is_not_a_pass() {
        let (verifier, _) = rig(Scripted::default());
        assert_eq!(verifier.check(19731).await, ListenerVerdict::NoListener);
        assert!(!verifier.check(19731).await.is_verified());
    }

    #[tokio::test]
    async fn another_users_process_is_rejected_before_its_signature_is_even_read() {
        let mut other = Proc::ok();
        other.uid = Ok(0);
        let (verifier, scripted) = rig(Scripted::default()
            .listening(19731, &[42])
            .process(42, other));
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Rejected(ListenerRejection::OtherUser { pid: 42 })
        );
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unsigned_or_wrongly_signed_listener_is_rejected() {
        for (team, expected) in [
            (None, ListenerRejection::NoTeam { pid: 42 }),
            (
                Some("EVILTEAM12".to_owned()),
                ListenerRejection::WrongTeam { pid: 42 },
            ),
            // A near-miss is not the team: no prefix or case-insensitive matching.
            (
                Some("psk5q5t46l".to_owned()),
                ListenerRejection::WrongTeam { pid: 42 },
            ),
            (
                Some("PSK5Q5T46".to_owned()),
                ListenerRejection::WrongTeam { pid: 42 },
            ),
        ] {
            let mut proc = Proc::ok();
            proc.team = Ok(team.clone());
            let (verifier, _) = rig(Scripted::default()
                .listening(19731, &[42])
                .process(42, proc));
            assert_eq!(
                verifier.check(19731).await,
                ListenerVerdict::Rejected(expected),
                "{team:?}"
            );
        }
    }

    #[tokio::test]
    async fn every_listener_must_pass_one_bad_one_among_good_ones_rejects() {
        let mut bad = Proc::ok();
        bad.team = Ok(None);
        let (verifier, _) = rig(Scripted::default()
            .listening(19731, &[41, 42])
            .process(41, Proc::ok())
            .process(42, bad));
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Rejected(ListenerRejection::NoTeam { pid: 42 })
        );
        // Both good: verified, both pids reported.
        let (verifier, _) = verifier_two_good();
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Verified {
                listeners: vec![identity(41), identity(42)]
            }
        );
        assert_eq!(verifier.check(19731).await.pids(), vec![41, 42]);
    }

    fn verifier_two_good() -> (ListenerVerifier, Arc<Scripted>) {
        rig(Scripted::default()
            .listening(19731, &[41, 42])
            .process(41, Proc::ok())
            .process(42, Proc::ok()))
    }

    #[tokio::test]
    async fn unreadable_answers_are_unverifiable_never_a_pass_and_never_a_rejection() {
        // lsof itself fails.
        let scripted = Scripted::default();
        scripted
            .listeners
            .lock()
            .unwrap()
            .insert(19731, Err(InspectError::TimedOut));
        let (verifier, _) = rig(scripted);
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Unverifiable(InspectError::TimedOut)
        );
        // uid, team, start time unreadable.
        for tweak in 0..3 {
            let mut proc = Proc::ok();
            match tweak {
                0 => proc.uid = Err(InspectError::NoAnswer),
                1 => proc.team = Err(InspectError::ToolMissing),
                _ => proc.start = Err(InspectError::TimedOut),
            }
            let (verifier, _) = rig(Scripted::default()
                .listening(19731, &[42])
                .process(42, proc));
            assert!(
                matches!(
                    verifier.check(19731).await,
                    ListenerVerdict::Unverifiable(_)
                ),
                "{tweak}"
            );
        }
    }

    #[tokio::test]
    async fn the_default_inspector_can_verify_nothing() {
        // An inspector that does not implement the new reads says "unreadable": no key ever goes out.
        struct Bare;
        #[async_trait]
        impl ProcessInspector for Bare {
            fn pid_state(&self, _pid: u32) -> PidState {
                PidState::Alive
            }
            async fn start_time(&self, _pid: u32) -> Result<String, InspectError> {
                Ok(String::new())
            }
            async fn command(&self, _pid: u32) -> Result<String, InspectError> {
                Ok(String::new())
            }
            async fn process_group(&self, _pid: u32) -> Result<u32, InspectError> {
                Ok(0)
            }
            async fn listens_on(&self, _pid: u32, _port: u16) -> Result<bool, InspectError> {
                Ok(true)
            }
        }
        let verifier = ListenerVerifier::new(Arc::new(Bare), ListenerPolicy::new(TEAM));
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Unverifiable(InspectError::NoAnswer)
        );
    }

    #[tokio::test]
    async fn a_pass_is_cached_by_pid_and_start_time_and_a_new_process_is_checked_again() {
        let (verifier, scripted) = rig(Scripted::default()
            .listening(19731, &[42])
            .process(42, Proc::ok()));
        assert!(verifier.check(19731).await.is_verified());
        assert!(verifier.check(19731).await.is_verified());
        assert_eq!(
            scripted.team_calls.load(Ordering::SeqCst),
            1,
            "the second check is answered from the cache"
        );
        assert_eq!(scripted.uid_calls.load(Ordering::SeqCst), 1);

        // Same pid, other start time: another process; it is checked again, and now it is unsigned.
        {
            let mut procs = scripted.procs.lock().unwrap();
            let proc = procs.get_mut(&42).unwrap();
            proc.start = Ok("Fri Nov 20 16:00:00 2026".to_owned());
            proc.team = Ok(None);
            proc.reads = 0;
        }
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Rejected(ListenerRejection::NoTeam { pid: 42 })
        );
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 2);

        // A different pid is not covered by the cached one.
        scripted
            .listeners
            .lock()
            .unwrap()
            .insert(19731, Ok(vec![43]));
        scripted.procs.lock().unwrap().insert(43, Proc::ok());
        assert!(verifier.check(19731).await.is_verified());
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 3);
    }

    /// Codex final review L1: `ps` gives the start time to the second. A pid recycled within the same second and bound
    /// to the port again reads exactly like the process that passed before; the pass must not survive a gap in the
    /// port's listeners.
    #[tokio::test]
    async fn a_pass_does_not_survive_a_gap_in_the_ports_listeners() {
        let (verifier, scripted) = rig(Scripted::default()
            .listening(19731, &[42])
            .process(42, Proc::ok()));
        assert!(verifier.check(19731).await.is_verified());
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 1);
        // A listener that is still there keeps its pass.
        assert!(verifier.check(19731).await.is_verified());
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 1);
        // The port is empty for a moment (the process left)...
        scripted.listeners.lock().unwrap().insert(19731, Ok(vec![]));
        assert_eq!(verifier.check(19731).await, ListenerVerdict::NoListener);
        // ... and a process with the very same pid and the very same start time (the second did not change) is there
        // again. It is checked from scratch, and this one is unsigned.
        scripted
            .listeners
            .lock()
            .unwrap()
            .insert(19731, Ok(vec![42]));
        {
            let mut procs = scripted.procs.lock().unwrap();
            let proc = procs.get_mut(&42).unwrap();
            proc.team = Ok(None);
            proc.reads = 0;
        }
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Rejected(ListenerRejection::NoTeam { pid: 42 })
        );
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 2);
    }

    /// A pid that is no longer among the port's listeners loses its pass even while others still listen.
    #[tokio::test]
    async fn a_pid_that_left_the_port_loses_its_pass_while_the_others_keep_theirs() {
        let (verifier, scripted) = verifier_two_good();
        assert!(verifier.check(19731).await.is_verified());
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 2);
        scripted
            .listeners
            .lock()
            .unwrap()
            .insert(19731, Ok(vec![42]));
        assert!(verifier.check(19731).await.is_verified());
        // 42 kept its pass: no new signature read; 41's was dropped.
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 2);
        scripted
            .listeners
            .lock()
            .unwrap()
            .insert(19731, Ok(vec![41, 42]));
        assert!(verifier.check(19731).await.is_verified());
        assert_eq!(
            scripted.team_calls.load(Ordering::SeqCst),
            3,
            "41 was checked again"
        );
    }

    #[tokio::test]
    async fn a_rejection_is_never_cached() {
        let mut proc = Proc::ok();
        proc.team = Ok(None);
        let (verifier, scripted) = rig(Scripted::default()
            .listening(19731, &[42])
            .process(42, proc));
        for _ in 0..3 {
            assert!(matches!(
                verifier.check(19731).await,
                ListenerVerdict::Rejected(_)
            ));
        }
        assert_eq!(scripted.team_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_process_replaced_during_the_check_is_neither_passed_nor_cached() {
        let mut proc = Proc::ok();
        proc.start_after = Some("Fri Nov 20 15:14:09 2026".to_owned());
        let (verifier, _) = rig(Scripted::default()
            .listening(19731, &[42])
            .process(42, proc));
        assert_eq!(
            verifier.check(19731).await,
            ListenerVerdict::Unverifiable(InspectError::NoAnswer)
        );
        assert!(
            verifier.verified.lock().unwrap().is_empty(),
            "nothing is remembered"
        );
    }

    #[tokio::test]
    async fn the_probe_client_does_not_keep_idle_connections_so_a_key_never_rides_an_older_one() {
        // A server that counts connections: two sequential requests through the client must open two.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    // Keep-alive HTTP/1.1: answer every request on the connection until it closes.
                    while let Ok(read) = socket.read(&mut buffer).await {
                        if read == 0 {
                            return;
                        }
                        let _ = socket
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                            .await;
                    }
                });
            }
        });
        let client = fresh_connection_client().unwrap();
        for _ in 0..3 {
            let response = client
                .get(format!("http://127.0.0.1:{port}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.text().await.unwrap(), "ok");
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            3,
            "every request opened its own connection"
        );

        // The default pooled client (what the rest of the crate uses) would have reused one: this is the
        // behaviour the probe client exists to rule out.
        let pooled = crate::http_client().unwrap();
        let before = accepted.load(Ordering::SeqCst);
        for _ in 0..3 {
            let response = pooled
                .get(format!("http://127.0.0.1:{port}/"))
                .send()
                .await
                .unwrap();
            let _ = response.text().await.unwrap();
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst) - before,
            1,
            "a pooled client reuses the first connection"
        );
        server.abort();
    }
}

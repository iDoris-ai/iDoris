//! 发送前重新确认监听者的守卫（IDORIS-CORE S6，Codex 终审 H1；owner 2026-11-22 拍板）。
//!
//! 引擎管理器登记一个端点的口径（私有 key、外部）时，把一个 [`EndpointGuard`] 一起交给 [`crate::omlx::key`] 的登记表；之后发往这个
//! 端点的每个请求，在**发出之前**由 [`crate::omlx::key::authorize_omlx_request`] 在发送它的那个任务里调守卫，**重新**确认此刻监听
//! 这个端口的还是登记时认的那个监听者。管理器自己的监管（每个节拍确认一次、先收回凭证再发布状态）仍在，但它不再是唯一的防线：
//! 两个节拍之间端口换了主人，下一个要发出去的请求自己就会发现。
//!
//! 两种守卫：
//!
//! - [`OwnedListenerGuard`]：自己拉起（或认回）的引擎。要求它的进程是这个端口上**唯一**的监听者（[`ProcessInspector::listens_on`]：
//!   别的进程也在这个端口上监听，哪怕在另一个地址上，都算不是）。
//! - [`ExternalListenerGuard`]：接管的外部引擎。要求监听这个端口的每个进程都重新通过 uid 与签名团队的校验
//!   （[`ListenerVerifier::check`]；它的通过记录在端口出现监听空档时作废，见 `listener.rs`），并且身份（pid 加启动时间）与校验通过、
//!   登记时的一模一样。
//!
//! 确认不了（读不出来）就是 [`Verification::Unverifiable`]，调用方拒绝发送（fail closed）：读 `lsof`、`ps` 偶尔会超时，所以读不出来
//! 的那一次立刻再问一遍，仍读不出来才算。
//!
//! **没有消除的间隙**：确认与发送之间仍有毫秒级的间隙，本机**另一个用户**的进程恰好在这几毫秒里抢到端口，仍能收到那一个请求
//! （同一个用户的进程本来就读得到 `settings.json` 与进程环境，什么也没多得；`lsof` 也看不到别的用户的监听）。不做逐连接的对端进程
//! 核对。规范勘误十把它写成已知限制。

use std::sync::Arc;

use async_trait::async_trait;

use super::inspect::ProcessInspector;
use super::listener::{ListenerIdentity, ListenerVerdict, ListenerVerifier};
use crate::omlx::key::{EndpointGuard, Verification};

/// 自有引擎的守卫：它的进程（`pid`）是端口 `port` 上唯一的监听者。
pub struct OwnedListenerGuard {
    inspector: Arc<dyn ProcessInspector>,
    pid: u32,
    port: u16,
}

impl OwnedListenerGuard {
    pub fn new(inspector: Arc<dyn ProcessInspector>, pid: u32, port: u16) -> Self {
        Self {
            inspector,
            pid,
            port,
        }
    }
}

impl std::fmt::Debug for OwnedListenerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedListenerGuard")
            .field("pid", &self.pid)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EndpointGuard for OwnedListenerGuard {
    async fn verify(&self) -> Verification {
        // 读不出来的那一次立刻再问一遍（`lsof` 偶尔超时）；两次都读不出来才是读不出来。
        for attempt in 0..2 {
            match self.inspector.listens_on(self.pid, self.port).await {
                Ok(true) => return Verification::Verified,
                Ok(false) => return Verification::Mismatch,
                Err(_) if attempt == 0 => {}
                Err(_) => return Verification::Unverifiable,
            }
        }
        Verification::Unverifiable
    }
}

/// 外部引擎的守卫：端口 `port` 上的监听者，每个都重新通过校验，并且就是登记时校验通过的那些（`expected`，pid 加启动时间）。
pub struct ExternalListenerGuard {
    verifier: ListenerVerifier,
    port: u16,
    expected: Vec<ListenerIdentity>,
}

impl ExternalListenerGuard {
    pub fn new(verifier: ListenerVerifier, port: u16, expected: Vec<ListenerIdentity>) -> Self {
        Self {
            verifier,
            port,
            expected,
        }
    }
}

impl std::fmt::Debug for ExternalListenerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalListenerGuard")
            .field("port", &self.port)
            .field("expected", &self.expected)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EndpointGuard for ExternalListenerGuard {
    async fn verify(&self) -> Verification {
        for attempt in 0..2 {
            match self.verifier.check(self.port).await {
                ListenerVerdict::Verified { listeners } if listeners == self.expected => {
                    return Verification::Verified;
                }
                // 通过了校验，但已经不是登记时认的那个（换了进程、多了一个、少了一个）；没有监听者；监听者不合格。
                ListenerVerdict::Verified { .. }
                | ListenerVerdict::NoListener
                | ListenerVerdict::Rejected(_) => return Verification::Mismatch,
                ListenerVerdict::Unverifiable(_) if attempt == 0 => {}
                ListenerVerdict::Unverifiable(_) => return Verification::Unverifiable,
            }
        }
        Verification::Unverifiable
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::super::inspect::{InspectError, PidState};
    use super::super::listener::ListenerPolicy;
    use super::*;

    const TEAM: &str = "PSK5Q5T46L";
    const ME: u32 = 501;
    const START: &str = "Fri Nov 20 15:14:05 2026";

    /// An inspector whose answers are scripted: each queue is consumed in order and its last entry repeats.
    struct Fake {
        listens: Mutex<VecDeque<Result<bool, InspectError>>>,
        listeners: Mutex<VecDeque<Result<Vec<u32>, InspectError>>>,
        start_time: Mutex<String>,
        team: Mutex<Option<String>>,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                listens: Mutex::new(VecDeque::new()),
                listeners: Mutex::new(VecDeque::new()),
                start_time: Mutex::new(START.to_owned()),
                team: Mutex::new(Some(TEAM.to_owned())),
            })
        }

        fn script_listens(&self, answers: Vec<Result<bool, InspectError>>) {
            *self.listens.lock().unwrap() = answers.into();
        }

        fn script_listeners(&self, answers: Vec<Result<Vec<u32>, InspectError>>) {
            *self.listeners.lock().unwrap() = answers.into();
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
            next(&self.listens, Err(InspectError::NoAnswer))
        }
        async fn listeners_on(&self, _port: u16) -> Result<Vec<u32>, InspectError> {
            next(&self.listeners, Ok(Vec::new()))
        }
        async fn uid_of(&self, _pid: u32) -> Result<u32, InspectError> {
            Ok(ME)
        }
        async fn signing_team(&self, _pid: u32) -> Result<Option<String>, InspectError> {
            Ok(self.team.lock().unwrap().clone())
        }
    }

    fn identity(pid: u32) -> ListenerIdentity {
        ListenerIdentity {
            pid,
            started_at: START.to_owned(),
        }
    }

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
        let verifier = ListenerVerifier::new(fake.clone(), ListenerPolicy::new(TEAM).with_uid(ME));
        let guard = ExternalListenerGuard::new(verifier, 19731, vec![identity(42)]);
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
        let verifier = ListenerVerifier::new(fake.clone(), ListenerPolicy::new(TEAM).with_uid(ME));
        let guard = ExternalListenerGuard::new(verifier, 19731, vec![identity(42)]);
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
}

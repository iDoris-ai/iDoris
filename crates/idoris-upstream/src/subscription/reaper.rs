#[cfg(unix)]
mod imp {
    //! Owns cleanup only for the process group created for one relay request.
    //! A deliberately malicious descendant that calls setsid()/setpgid() and
    //! escapes that group is outside this layer's guarantee; stronger OS
    //! sandboxing is a separate B3 decision, not something signal cleanup can
    //! truthfully claim to provide.

    use std::time::Duration;

    use nix::errno::Errno;
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    use tokio::process::Child;
    use tokio::task::JoinHandle;
    use tokio::time::{Instant, sleep, timeout};

    use super::super::error::{SubscriptionErrorCode, SubscriptionRelayError};

    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ReapOutcome {
        pub exit_code: Option<i32>,
        pub escalated_to_kill: bool,
    }

    pub struct ProcessGroupReaper {
        child: Child,
        group: Pid,
        armed: bool,
    }

    impl ProcessGroupReaper {
        pub fn new(child: Child) -> Result<Self, SubscriptionRelayError> {
            let raw = child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .filter(|id| *id > 0)
                .ok_or_else(cleanup_error)?;
            Ok(Self {
                child,
                group: Pid::from_raw(raw),
                armed: true,
            })
        }

        pub fn process_group(&self) -> i32 {
            self.group.as_raw()
        }

        pub async fn terminate(
            mut self,
            grace: Duration,
        ) -> Result<ReapOutcome, SubscriptionRelayError> {
            signal_group(self.group, Signal::SIGTERM)?;
            let (status, mut escalated) = match timeout(grace, self.child.wait()).await {
                Ok(result) => (result.map_err(|_| cleanup_error())?, false),
                Err(_) => {
                    signal_group(self.group, Signal::SIGKILL)?;
                    let status = timeout(grace, self.child.wait())
                        .await
                        .map_err(|_| cleanup_error())?
                        .map_err(|_| cleanup_error())?;
                    (status, true)
                }
            };
            if cleanup_descendants(self.group, grace).await? {
                escalated = true;
            }
            self.armed = false;
            Ok(ReapOutcome {
                exit_code: status.code(),
                escalated_to_kill: escalated,
            })
        }

        pub async fn finish_after_parent_exit(
            mut self,
            grace: Duration,
        ) -> Result<ReapOutcome, SubscriptionRelayError> {
            let (status, mut escalated) = match timeout(grace, self.child.wait()).await {
                Ok(result) => (result.map_err(|_| cleanup_error())?, false),
                Err(_) => {
                    signal_group(self.group, Signal::SIGTERM)?;
                    match timeout(grace, self.child.wait()).await {
                        Ok(result) => (result.map_err(|_| cleanup_error())?, false),
                        Err(_) => {
                            signal_group(self.group, Signal::SIGKILL)?;
                            let status = timeout(grace, self.child.wait())
                                .await
                                .map_err(|_| cleanup_error())?
                                .map_err(|_| cleanup_error())?;
                            (status, true)
                        }
                    }
                }
            };
            if cleanup_descendants(self.group, grace).await? {
                escalated = true;
            }
            self.armed = false;
            Ok(ReapOutcome {
                exit_code: status.code(),
                escalated_to_kill: escalated,
            })
        }
    }

    impl Drop for ProcessGroupReaper {
        fn drop(&mut self) {
            if self.armed {
                let _ = killpg(self.group, Some(Signal::SIGKILL));
            }
        }
    }

    pub async fn await_drain_bounded<T: Send + 'static>(
        mut task: JoinHandle<T>,
        limit: Duration,
    ) -> Result<T, SubscriptionRelayError> {
        match timeout(limit, &mut task).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(cleanup_error()),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(cleanup_error())
            }
        }
    }

    async fn cleanup_descendants(
        group: Pid,
        grace: Duration,
    ) -> Result<bool, SubscriptionRelayError> {
        if !group_exists(group)? {
            return Ok(false);
        }
        signal_group(group, Signal::SIGTERM)?;
        if wait_group_gone(group, grace).await? {
            return Ok(false);
        }
        signal_group(group, Signal::SIGKILL)?;
        if wait_group_gone(group, grace).await? {
            Ok(true)
        } else {
            Err(cleanup_error())
        }
    }

    async fn wait_group_gone(group: Pid, limit: Duration) -> Result<bool, SubscriptionRelayError> {
        let deadline = Instant::now() + limit;
        loop {
            if !group_exists(group)? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))).await;
        }
    }

    fn group_exists(group: Pid) -> Result<bool, SubscriptionRelayError> {
        match killpg(group, None) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(_) => Err(cleanup_error()),
        }
    }

    fn signal_group(group: Pid, signal: Signal) -> Result<(), SubscriptionRelayError> {
        match killpg(group, Some(signal)) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(_) => Err(cleanup_error()),
        }
    }

    fn cleanup_error() -> SubscriptionRelayError {
        SubscriptionRelayError::new(SubscriptionErrorCode::CleanupFailed)
    }
}

#[cfg(unix)]
pub use imp::{ProcessGroupReaper, ReapOutcome, await_drain_bounded};

#[cfg(not(unix))]
pub async fn unsupported() -> Result<(), super::error::SubscriptionRelayError> {
    Err(super::error::SubscriptionRelayError::new(
        super::error::SubscriptionErrorCode::CleanupFailed,
    ))
}

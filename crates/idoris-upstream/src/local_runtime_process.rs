//! Process ownership for B8 single-model local runtimes.
//!
//! The launch argv comes from [`crate::LocalRuntimeLaunch`]; this layer adds
//! lifecycle ownership only. Unix children get their own process group so
//! shutdown and Drop cannot strand descendants that remain in the runtime's
//! process group.
//! Engine/model readiness stays with [`idoris_backend::RuntimeAdapter::probe_ready`];
//! there is no generic health endpoint shared by the B8 runtimes.

use std::process::Stdio;
use std::time::Duration;

use idoris_backend::BackendError;
use tokio::process::{Child, Command};

use crate::LocalRuntimeLaunch;

pub struct ManagedRuntimeProcess {
    child: Child,
    #[cfg(unix)]
    process_group: i32,
    #[cfg(unix)]
    armed: bool,
}

impl ManagedRuntimeProcess {
    pub fn spawn(launch: &LocalRuntimeLaunch) -> Result<Self, BackendError> {
        Self::spawn_command(launch.executable(), launch.args())
    }

    fn spawn_command(
        executable: &std::path::Path,
        args: Vec<std::ffi::OsString>,
    ) -> Result<Self, BackendError> {
        let mut command = Command::new(executable);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let child = command
            .spawn()
            .map_err(|_| BackendError::internal("failed to spawn local runtime process"))?;
        #[cfg(unix)]
        let process_group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| BackendError::internal("spawned local runtime has no process id"))?;

        Ok(Self {
            child,
            #[cfg(unix)]
            process_group,
            #[cfg(unix)]
            armed: true,
        })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    #[cfg(unix)]
    pub fn is_running(&mut self) -> Result<bool, BackendError> {
        child_running_without_reap(self.process_group)
    }

    #[cfg(not(unix))]
    pub fn is_running(&mut self) -> Result<bool, BackendError> {
        self.child
            .try_wait()
            .map(|status| status.is_none())
            .map_err(|_| BackendError::internal("failed to inspect local runtime process"))
    }

    #[cfg(unix)]
    pub async fn shutdown(mut self, grace: Duration) -> Result<(), BackendError> {
        // Keep the direct child unreaped while group signals are possible.
        // Its PID is also this group's PGID, so the zombie/live child is
        // the ownership anchor that prevents the numeric PGID from being
        // recycled for an unrelated process group.
        self.signal_terminate()?;
        let leader_exited = wait_child_exit_without_reap(self.process_group, grace).await?;
        if !leader_exited {
            self.signal_kill()?;
        } else if group_exists(self.process_group)? {
            // The unreaped leader still anchors the PGID here. Sweep any
            // descendants that survived SIGTERM before releasing that anchor.
            self.signal_kill()?;
        }
        tokio::time::timeout(grace, self.child.wait())
            .await
            .map_err(|_| BackendError::internal("timed out reaping local runtime process"))?
            .map_err(|_| BackendError::internal("failed to reap local runtime process"))?;
        // No group signal may happen after wait() reaps the ownership
        // anchor: from this point the old PGID is allowed to be reused.
        self.armed = false;
        // Group members receive the same terminal signal as the direct child,
        // but their exit/reparent/reap can lag the child's wait() by a short
        // scheduling window (especially under loaded Linux CI).  Observe that
        // cleanup to completion before reporting shutdown success.  This is a
        // read-only probe after the ownership anchor is reaped: never signal a
        // numeric PGID again here, because it may now be reused by the OS.
        if !wait_group_gone(self.process_group, grace).await? {
            return Err(BackendError::internal(
                "timed out waiting for local runtime process group cleanup",
            ));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub async fn shutdown(mut self, grace: Duration) -> Result<(), BackendError> {
        let running = self.is_running()?;
        if running {
            self.signal_terminate()?;
            match tokio::time::timeout(grace, self.child.wait()).await {
                Ok(result) => {
                    result.map_err(|_| {
                        BackendError::internal("failed to reap local runtime process")
                    })?;
                }
                Err(_) => {
                    self.signal_kill()?;
                    tokio::time::timeout(grace, self.child.wait())
                        .await
                        .map_err(|_| {
                            BackendError::internal("timed out reaping local runtime process")
                        })?
                        .map_err(|_| {
                            BackendError::internal("failed to reap local runtime process")
                        })?;
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn signal_terminate(&mut self) -> Result<(), BackendError> {
        signal_group(self.process_group, nix::sys::signal::Signal::SIGTERM)
    }

    #[cfg(not(unix))]
    fn signal_terminate(&mut self) -> Result<(), BackendError> {
        self.child
            .start_kill()
            .map_err(|_| BackendError::internal("failed to terminate local runtime process"))
    }

    #[cfg(unix)]
    fn signal_kill(&mut self) -> Result<(), BackendError> {
        signal_group(self.process_group, nix::sys::signal::Signal::SIGKILL)
    }

    #[cfg(not(unix))]
    fn signal_kill(&mut self) -> Result<(), BackendError> {
        self.child
            .start_kill()
            .map_err(|_| BackendError::internal("failed to kill local runtime process"))
    }
}

#[cfg(unix)]
fn child_running_without_reap(pid: i32) -> Result<bool, BackendError> {
    let pid = rustix::process::Pid::from_raw(pid)
        .ok_or_else(|| BackendError::internal("local runtime process id is invalid"))?;
    rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )
    .map(|status| status.is_none())
    .map_err(|_| BackendError::internal("failed to inspect local runtime process"))
}

#[cfg(unix)]
fn signal_group(process_group: i32, signal: nix::sys::signal::Signal) -> Result<(), BackendError> {
    classify_signal_result(nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(process_group),
        signal,
    ))
}

#[cfg(unix)]
fn classify_signal_result(result: Result<(), nix::errno::Errno>) -> Result<(), BackendError> {
    match result {
        Ok(()) | Err(nix::errno::Errno::ESRCH) | Err(nix::errno::Errno::EPERM) => Ok(()),
        Err(_) => Err(BackendError::internal(
            "failed to signal local runtime process group",
        )),
    }
}

#[cfg(unix)]
async fn wait_group_gone(process_group: i32, grace: Duration) -> Result<bool, BackendError> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if !group_exists(process_group)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(
            Duration::from_millis(10)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

#[cfg(unix)]
async fn wait_child_exit_without_reap(pid: i32, grace: Duration) -> Result<bool, BackendError> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if !child_running_without_reap(pid)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(
            Duration::from_millis(10)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

#[cfg(unix)]
fn group_exists(process_group: i32) -> Result<bool, BackendError> {
    classify_group_probe(nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(process_group),
        None,
    ))
}

#[cfg(unix)]
fn classify_group_probe(result: Result<(), nix::errno::Errno>) -> Result<bool, BackendError> {
    match result {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(_) => Err(BackendError::internal(
            "failed to inspect local runtime process group",
        )),
    }
}

#[cfg(unix)]
impl Drop for ManagedRuntimeProcess {
    fn drop(&mut self) {
        if self.armed {
            let _ = signal_group(self.process_group, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn missing_executable_fails_closed() {
        let result = ManagedRuntimeProcess::spawn_command(
            std::path::Path::new("/definitely/not/a/runtime-binary"),
            Vec::new(),
        );
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_is_its_own_process_group_and_shutdown_reaps_it() {
        let mut process = ManagedRuntimeProcess::spawn_command(
            std::path::Path::new("/bin/sleep"),
            vec!["30".into()],
        )
        .unwrap();
        let pid = i32::try_from(process.id().unwrap()).unwrap();
        let pid = nix::unistd::Pid::from_raw(pid);
        assert_eq!(nix::unistd::getpgid(Some(pid)).unwrap(), pid);
        assert!(process.is_running().unwrap());
        process.shutdown(Duration::from_millis(250)).await.unwrap();
        assert!(!group_exists(pid.as_raw()).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn graceful_shutdown_returns_before_full_grace() {
        let process = ManagedRuntimeProcess::spawn_command(
            std::path::Path::new("/bin/sleep"),
            vec!["30".into()],
        )
        .unwrap();
        let started = tokio::time::Instant::now();
        process.shutdown(Duration::from_secs(2)).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "TERM-obedient child should not consume the full grace window"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_kills_owned_process_group() {
        let process = ManagedRuntimeProcess::spawn_command(
            std::path::Path::new("/bin/sleep"),
            vec!["30".into()],
        )
        .unwrap();
        let pid = nix::unistd::Pid::from_raw(i32::try_from(process.id().unwrap()).unwrap());

        drop(process);
        let cleanup = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !group_exists(pid.as_raw()).unwrap() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(cleanup.is_ok(), "dropped process group must disappear");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_escalates_and_reaps_term_ignoring_child() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let process = ManagedRuntimeProcess::spawn_command(
            std::path::Path::new("/bin/sh"),
            vec![
                "-c".into(),
                "trap '' TERM; : > \"$1\"; while :; do sleep 1; done".into(),
                "runtime-test".into(),
                ready.as_os_str().to_owned(),
            ],
        )
        .unwrap();
        let pid = i32::try_from(process.id().unwrap()).unwrap();
        let pid = nix::unistd::Pid::from_raw(pid);

        let armed = tokio::time::timeout(Duration::from_secs(1), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(armed.is_ok(), "TERM-ignore child must become ready");

        process.shutdown(Duration::from_millis(50)).await.unwrap();
        assert!(!group_exists(pid.as_raw()).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn natural_exit_is_observed_without_reaping_before_drop() {
        let mut process =
            ManagedRuntimeProcess::spawn_command(std::path::Path::new("/usr/bin/true"), Vec::new())
                .unwrap();
        let pid = process.id().unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while process.is_running().unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        assert_eq!(
            process.id(),
            Some(pid),
            "non-reaping status observation must retain the PID/PGID ownership anchor"
        );
        drop(process);
        let pgid = i32::try_from(pid).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while group_exists(pgid).unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn natural_parent_exit_still_cleans_owned_descendant_before_reap() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("child");
        let mut process = ManagedRuntimeProcess::spawn_command(
            std::path::Path::new("/bin/sh"),
            vec![
                "-c".into(),
                "sleep 30 & echo $! > \"$1\"".into(),
                "runtime-test".into(),
                marker.as_os_str().to_owned(),
            ],
        )
        .unwrap();
        let pgid = i32::try_from(process.id().unwrap()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !marker.exists() || process.is_running().unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            process.id().is_some(),
            "natural exit must stay unreaped until owned-group cleanup finishes"
        );

        process.shutdown(Duration::from_millis(100)).await.unwrap();
        assert!(!group_exists(pgid).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn darwin_eperm_defers_to_bounded_group_checks() {
        assert!(classify_signal_result(Err(nix::errno::Errno::EPERM)).is_ok());
        assert!(classify_group_probe(Err(nix::errno::Errno::EPERM)).unwrap());
        assert!(classify_signal_result(Err(nix::errno::Errno::ESRCH)).is_ok());
        assert!(!classify_group_probe(Err(nix::errno::Errno::ESRCH)).unwrap());
        assert!(classify_signal_result(Err(nix::errno::Errno::EINVAL)).is_err());
        assert!(classify_group_probe(Err(nix::errno::Errno::EINVAL)).is_err());
    }
}

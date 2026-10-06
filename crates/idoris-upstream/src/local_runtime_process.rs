//! Process ownership for B8 single-model local runtimes.
//!
//! The launch argv comes from [`crate::LocalRuntimeLaunch`]; this layer adds
//! lifecycle ownership only. Unix children get their own process group so
//! shutdown and Drop cannot strand descendants.
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

    pub fn is_running(&mut self) -> Result<bool, BackendError> {
        self.child
            .try_wait()
            .map(|status| status.is_none())
            .map_err(|_| BackendError::internal("failed to inspect local runtime process"))
    }

    pub async fn shutdown(mut self, grace: Duration) -> Result<(), BackendError> {
        let running = self.is_running()?;

        #[cfg(unix)]
        self.signal_terminate()?;
        #[cfg(not(unix))]
        if running {
            self.signal_terminate()?;
        }

        if running {
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

        #[cfg(unix)]
        {
            cleanup_group(self.process_group, grace).await?;
            self.armed = false;
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
fn signal_group(process_group: i32, signal: nix::sys::signal::Signal) -> Result<(), BackendError> {
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(process_group), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(_) => Err(BackendError::internal(
            "failed to signal local runtime process group",
        )),
    }
}

#[cfg(unix)]
async fn cleanup_group(process_group: i32, grace: Duration) -> Result<(), BackendError> {
    if wait_group_gone(process_group, grace).await? {
        return Ok(());
    }
    signal_group(process_group, nix::sys::signal::Signal::SIGKILL)?;
    if wait_group_gone(process_group, grace).await? {
        Ok(())
    } else {
        Err(BackendError::internal(
            "timed out cleaning local runtime process group",
        ))
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
fn group_exists(process_group: i32) -> Result<bool, BackendError> {
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(process_group), None) {
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
}

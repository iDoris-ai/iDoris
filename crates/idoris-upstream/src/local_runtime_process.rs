//! Process ownership for B8 single-model local runtimes.
//!
//! The launch argv comes from [`crate::LocalRuntimeLaunch`]; this layer adds
//! lifecycle ownership only. Unix children get their own process group so
//! shutdown and Drop cannot strand descendants that remain in the runtime's
//! process group.
//! Engine/model readiness stays with [`idoris_backend::RuntimeAdapter::probe_ready`];
//! there is no generic health endpoint shared by the B8 runtimes.
//!
//! `ManagedRuntimeProcess::spawn_engine` is the second constructor, for an
//! engine-level server process (oMLX, IDORIS-CORE S3): cleaned environment,
//! working directory and stdout/stderr files chosen by the caller, and a
//! per-handle choice of whether `Drop` kills the process group
//! (`stop_on_exit: "owned"`) or leaves the engine running (`"never"`). The
//! B8 constructor and everything the two share (`shutdown`, the group
//! signalling and cleanup checks) are unchanged.

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

/// Everything [`ManagedRuntimeProcess::spawn_engine`] needs. The environment is
/// the complete child environment (the parent's is not inherited).
#[cfg(unix)]
pub(crate) struct EngineSpawn {
    pub(crate) executable: std::path::PathBuf,
    pub(crate) args: Vec<std::ffi::OsString>,
    pub(crate) env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub(crate) current_dir: std::path::PathBuf,
    pub(crate) stdout: std::fs::File,
    pub(crate) stderr: std::fs::File,
    /// `true`: `Drop` SIGKILLs the whole process group (`stop_on_exit:
    /// "owned"`, the B8 behaviour). `false`: `Drop` leaves the engine running
    /// (`"never"`); only an explicit [`ManagedRuntimeProcess::shutdown`] stops it.
    pub(crate) kill_group_on_drop: bool,
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

    #[cfg(test)]
    pub(crate) fn spawn_test(
        executable: &std::path::Path,
        args: Vec<std::ffi::OsString>,
    ) -> Result<Self, BackendError> {
        Self::spawn_command(executable, args)
    }

    /// Spawns an engine-level server process (see [`EngineSpawn`]).
    ///
    /// Same ownership model as [`Self::spawn`]: the child leads its own process
    /// group, and the unreaped direct child anchors that PGID for
    /// [`Self::shutdown`]. Differences, all chosen by the caller through `spec`:
    /// the environment is cleared down to `spec.env`, the working directory
    /// and stdout/stderr are explicit, and `Drop` only kills the process group
    /// when `spec.kill_group_on_drop` is set. `kill_on_drop` is always off here:
    /// tokio's variant signals only the direct child, which is both weaker than
    /// the group kill in `Drop` and wrong for `stop_on_exit: "never"`.
    #[cfg(unix)]
    pub(crate) fn spawn_engine(spec: EngineSpawn) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(name, value)| (name, value)))
            .current_dir(&spec.current_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(spec.stdout))
            .stderr(Stdio::from(spec.stderr))
            .kill_on_drop(false);
        command.as_std_mut().process_group(0);

        let mut child = command.spawn()?;
        let Some(process_group) = child.id().and_then(|pid| i32::try_from(pid).ok()) else {
            // Unreachable in practice (a just-spawned child has a pid that fits
            // an i32), but a live child without a handle must not leak.
            let _ = child.start_kill();
            return Err(std::io::Error::other("spawned engine has no process id"));
        };
        Ok(Self {
            child,
            process_group,
            armed: spec.kill_group_on_drop,
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
    pub async fn shutdown(self, grace: Duration) -> Result<(), BackendError> {
        self.shutdown_bounded(grace, grace)
            .await
            .map_err(ShutdownError::into_backend)
    }

    /// [`Self::shutdown`] with the two waits told apart: `term_grace` is how long the group gets after SIGTERM to
    /// leave on its own (`Duration::ZERO`: SIGKILL right after the SIGTERM), `confirm` is how long to wait, after
    /// the SIGKILL, for the leader to be reaped and the group to be gone. A stop that has to finish by a deadline
    /// (the launcher's SIGKILL of iDoris) shrinks `term_grace` and keeps a short `confirm`.
    #[cfg(unix)]
    pub async fn shutdown_bounded(
        self,
        term_grace: Duration,
        confirm: Duration,
    ) -> Result<(), ShutdownError> {
        self.shutdown_bounded_cut(term_grace, confirm, None).await
    }

    /// [`Self::shutdown_bounded`] whose SIGTERM wait can be **cut short** while it runs: when `cut` turns `true` the wait ends at once and the
    /// stop goes on to the SIGKILL sweep (same pinned group, same checks). A stop that began with a long grace learns that the process has to be
    /// gone by a deadline after all (iDoris was told to stop) without being cancelled, which would drop the handle that pins the group.
    #[cfg(unix)]
    pub async fn shutdown_bounded_cut(
        mut self,
        term_grace: Duration,
        confirm: Duration,
        mut cut: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<(), ShutdownError> {
        // Keep the direct child unreaped while group signals are possible.
        // Its PID is also this group's PGID, so the zombie/live child is
        // the ownership anchor that prevents the numeric PGID from being
        // recycled for an unrelated process group.
        self.signal_terminate().map_err(ShutdownError::Pinned)?;
        let waiting = wait_child_exit_without_reap(self.process_group, term_grace);
        let leader_exited = match cut.as_mut() {
            Some(cut) => tokio::select! {
                biased;
                _ = cut.wait_for(|cut| *cut) => Ok(false),
                exited = waiting => exited,
            },
            None => waiting.await,
        }
        .map_err(ShutdownError::Pinned)?;
        if !leader_exited || group_exists(self.process_group).map_err(ShutdownError::Pinned)? {
            // The unreaped leader still anchors the PGID here. Sweep any
            // descendants that survived SIGTERM before releasing that anchor.
            self.signal_kill().map_err(ShutdownError::Pinned)?;
        }
        // One budget for every confirmation below: the SIGKILL sweeps, the reap and the post-reap check.
        let deadline = tokio::time::Instant::now() + confirm;
        // EVERY rescan that sends a signal happens here, while the unreaped leader still pins the group id: a
        // descendant forked in the instant of the first sweep is not on its list, and after the reap the numeric id
        // may belong to an unrelated process group, so no signal may follow it.
        self.sweep_while_pinned(deadline)
            .await
            .map_err(ShutdownError::Pinned)?;
        let remaining = || {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .max(Duration::from_millis(20))
        };
        match tokio::time::timeout(remaining(), self.child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                return Err(ShutdownError::Pinned(BackendError::internal(
                    "failed to reap local runtime process",
                )));
            }
            Err(_) => {
                return Err(ShutdownError::Pinned(BackendError::internal(
                    "timed out reaping local runtime process",
                )));
            }
        }
        // No group signal may happen after wait() reaps the ownership
        // anchor: from this point the old PGID is allowed to be reused.
        self.armed = false;
        #[cfg(test)]
        test_hooks::record(self.process_group, test_hooks::Event::Reaped);
        // Released: read-only from here on. Whatever this check finds is only reported, never signalled.
        let clean = wait_group_cleanup(self.process_group, remaining())
            .await
            .map_err(ShutdownError::Released)?;
        if !clean {
            return Err(ShutdownError::Released(BackendError::internal(
                "timed out waiting for local runtime process group cleanup",
            )));
        }
        Ok(())
    }

    /// SIGKILL rescans while the unreaped leader still pins the group id, until the group has no live member or the
    /// budget is spent. Linux tells a zombie from a live member, so it can wait for a really clean group; elsewhere a
    /// zombie leader keeps the group "existing", so a fixed few extra sweeps are made instead.
    #[cfg(unix)]
    async fn sweep_while_pinned(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<(), BackendError> {
        let step = || {
            Duration::from_millis(10)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now()))
        };
        #[cfg(target_os = "linux")]
        loop {
            if !group_has_live_members(self.process_group)? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            self.signal_kill()?;
            tokio::time::sleep(step()).await;
        }
        #[cfg(not(target_os = "linux"))]
        {
            for _ in 0..3 {
                tokio::time::sleep(step()).await;
                self.signal_kill()?;
            }
            Ok(())
        }
    }

    /// Non-unix: no process group, nothing to cut: the same as [`Self::shutdown_bounded`].
    #[cfg(not(unix))]
    pub async fn shutdown_bounded_cut(
        self,
        term_grace: Duration,
        confirm: Duration,
        _cut: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<(), ShutdownError> {
        self.shutdown_bounded(term_grace, confirm).await
    }

    /// Non-unix: there is no process group to sweep, so the two waits are the same wait.
    #[cfg(not(unix))]
    pub async fn shutdown_bounded(
        self,
        term_grace: Duration,
        _confirm: Duration,
    ) -> Result<(), ShutdownError> {
        self.shutdown(term_grace)
            .await
            .map_err(ShutdownError::Pinned)
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
pub(crate) fn signal_group(
    process_group: i32,
    signal: nix::sys::signal::Signal,
) -> Result<(), BackendError> {
    #[cfg(test)]
    test_hooks::record(process_group, test_hooks::Event::Signal(signal));
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
async fn wait_group_cleanup(process_group: i32, grace: Duration) -> Result<bool, BackendError> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if !group_has_live_members(process_group)? {
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

/// Whether the group still has a live member. (Tests can force "yes" for one group id.)
#[cfg(unix)]
pub(crate) fn group_has_live_members(process_group: i32) -> Result<bool, BackendError> {
    #[cfg(test)]
    if test_hooks::forced_live(process_group) {
        return Ok(true);
    }
    group_has_live_members_impl(process_group)
}

#[cfg(target_os = "linux")]
fn group_has_live_members_impl(process_group: i32) -> Result<bool, BackendError> {
    if !group_exists(process_group)? {
        return Ok(false);
    }

    // Linux keeps an orphaned descendant in its process group until PID 1
    // reaps it. killpg(pgid, 0) therefore sees a group made only of zombies.
    // Those descendants have exited, even when their reaper is slow.
    let processes = std::fs::read_dir("/proc")
        .map_err(|_| BackendError::internal("failed to inspect local runtime process group"))?;
    for process in processes {
        let process = process
            .map_err(|_| BackendError::internal("failed to inspect local runtime process group"))?;
        let Some(pid) = process.file_name().to_string_lossy().parse::<i32>().ok() else {
            continue;
        };
        match nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid))) {
            Ok(group) if group.as_raw() == process_group => {}
            Ok(_) | Err(nix::errno::Errno::ESRCH) => continue,
            Err(_) => {
                return Err(BackendError::internal(
                    "failed to inspect local runtime process group",
                ));
            }
        }
        let process_path = process.path();
        let Some(stat) = read_proc_stat(&process_path.join("stat"))? else {
            continue;
        };
        let (state, group) = proc_stat_state_and_group(&stat)?;
        if group != process_group {
            continue;
        }
        if !matches!(state, "Z" | "X" | "x") {
            return Ok(true);
        }
        // A thread-group leader can be a zombie while sibling threads still
        // hold resources. Inspect each task before treating its group as dead.
        if state == "Z" && zombie_leader_has_live_threads(&process_path, process_group)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn proc_entry_vanished(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32)
}

#[cfg(target_os = "linux")]
fn read_proc_stat(path: &std::path::Path) -> Result<Option<String>, BackendError> {
    match std::fs::read(path) {
        // Linux comm is arbitrary bytes; only the fields after it are parsed.
        Ok(stat) => Ok(Some(String::from_utf8_lossy(&stat).into_owned())),
        Err(error) if proc_entry_vanished(&error) => Ok(None),
        Err(_) => Err(BackendError::internal(
            "failed to inspect local runtime process group",
        )),
    }
}

#[cfg(target_os = "linux")]
fn proc_stat_state_and_group(stat: &str) -> Result<(&str, i32), BackendError> {
    let fields = stat
        .rsplit_once(") ")
        .map(|(_, fields)| fields)
        .ok_or_else(|| BackendError::internal("failed to inspect local runtime process group"))?;
    let mut fields = fields.split_whitespace();
    let state = fields
        .next()
        .ok_or_else(|| BackendError::internal("failed to inspect local runtime process group"))?;
    let _parent = fields.next();
    let group = fields
        .next()
        .and_then(|group| group.parse::<i32>().ok())
        .ok_or_else(|| BackendError::internal("failed to inspect local runtime process group"))?;
    Ok((state, group))
}

#[cfg(target_os = "linux")]
fn zombie_leader_has_live_threads(
    process_path: &std::path::Path,
    process_group: i32,
) -> Result<bool, BackendError> {
    let tasks = match std::fs::read_dir(process_path.join("task")) {
        Ok(tasks) => tasks,
        Err(error) if proc_entry_vanished(&error) => return Ok(false),
        Err(_) => {
            return Err(BackendError::internal(
                "failed to inspect local runtime process group",
            ));
        }
    };
    for task in tasks {
        let task = match task {
            Ok(task) => task,
            Err(error) if proc_entry_vanished(&error) => continue,
            Err(_) => {
                return Err(BackendError::internal(
                    "failed to inspect local runtime process group",
                ));
            }
        };
        let Some(stat) = read_proc_stat(&task.path().join("stat"))? else {
            continue;
        };
        let (state, group) = proc_stat_state_and_group(&stat)?;
        if group == process_group && !matches!(state, "Z" | "X" | "x") {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn group_has_live_members_impl(process_group: i32) -> Result<bool, BackendError> {
    group_exists(process_group)
}

/// What a failed stop says about the group id.
#[derive(Debug)]
pub enum ShutdownError {
    /// The direct child was not reaped yet, so it still pins the group id: the failure happened before the reap
    /// (a signal could not be sent, or the reap did not complete).
    Pinned(BackendError),
    /// The direct child has been reaped: the group id is **no longer pinned** and may already belong to an unrelated
    /// process group. The credentials are released; the caller can only report this, and must never signal the
    /// numeric group id again.
    Released(BackendError),
}

impl ShutdownError {
    pub fn into_backend(self) -> BackendError {
        match self {
            Self::Pinned(error) | Self::Released(error) => error,
        }
    }

    /// Whether the group id was already released (no further signal may be sent to it).
    pub fn is_released(&self) -> bool {
        matches!(self, Self::Released(_))
    }
}

/// Test-only recording of every group signal and of the reap, and a switch that makes one group look occupied forever.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Event {
        Signal(nix::sys::signal::Signal),
        Reaped,
    }

    static EVENTS: Mutex<Vec<(i32, Event)>> = Mutex::new(Vec::new());
    static LIVE: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    pub(crate) fn record(group: i32, event: Event) {
        EVENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((group, event));
    }

    pub(crate) fn events_for(group: i32) -> Vec<Event> {
        EVENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(id, _)| *id == group)
            .map(|(_, event)| *event)
            .collect()
    }

    pub(crate) fn force_live(group: i32) {
        LIVE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(group);
    }

    pub(crate) fn forced_live(group: i32) -> bool {
        LIVE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&group)
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
        assert!(!group_has_live_members(pid.as_raw()).unwrap());
    }

    /// H5：回收（`wait()`）之后进程组号不再被钉住，可能已经属于一个不相干的进程组。组在回收之后仍确认不了消失（这里用测试开关让它
    /// 一直「有成员」）时，底层返回 **「凭证已释放」**（`Released`），不是「还钉着」，上层据此只报告、不再按组号发任何信号。
    /// 逐条记下的信号事件证明两件事：回收之前，SIGKILL 一补再补（每一次重扫都在子进程还钉着组号的时候）；回收之后，一个信号也没有。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_group_that_cannot_be_confirmed_gone_after_the_reap_is_released_and_never_signalled_again()
     {
        use nix::sys::signal::Signal;
        use test_hooks::Event;

        let process = ManagedRuntimeProcess::spawn_command(
            std::path::Path::new("/bin/sleep"),
            vec!["600".into()],
        )
        .unwrap();
        let group = i32::try_from(process.id().unwrap()).unwrap();
        test_hooks::force_live(group);

        let result = process
            .shutdown_bounded(Duration::from_millis(50), Duration::from_millis(300))
            .await;
        let Err(error) = result else {
            panic!("the group is reported occupied after the reap");
        };
        assert!(error.is_released(), "{error:?}");

        let events = test_hooks::events_for(group);
        let reaped = events
            .iter()
            .position(|event| *event == Event::Reaped)
            .unwrap_or_else(|| panic!("the child was reaped"));
        assert!(
            events[reaped + 1..]
                .iter()
                .all(|event| !matches!(event, Event::Signal(_))),
            "no signal to the numeric group id after the reap: {events:?}"
        );
        let kills_before_the_reap = events[..reaped]
            .iter()
            .filter(|event| **event == Event::Signal(Signal::SIGKILL))
            .count();
        assert!(
            kills_before_the_reap >= 2,
            "the rescans that signal all happen while the leader still pins the group id: {events:?}"
        );
    }

    /// 回收之前的失败是「还钉着」，回收之后的才是「已释放」；两者各自如实说明，`into_backend` 取出原来的错误。
    #[test]
    fn a_stop_failure_says_whether_the_group_id_was_still_pinned() {
        let pinned = ShutdownError::Pinned(BackendError::internal("before the reap"));
        assert!(!pinned.is_released());
        let released = ShutdownError::Released(BackendError::internal("after the reap"));
        assert!(released.is_released());
        assert_eq!(
            format!("{:?}", released.into_backend()),
            format!("{:?}", BackendError::internal("after the reap"))
        );
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
        assert!(group_has_live_members(pgid).unwrap());

        process.shutdown(Duration::from_millis(100)).await.unwrap();
        assert!(!group_has_live_members(pgid).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disappearing_proc_entry_includes_esrch() {
        assert!(proc_entry_vanished(&std::io::Error::from_raw_os_error(
            nix::errno::Errno::ESRCH as i32
        )));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_proc_comm_preserves_state_and_group() {
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task/42");
        std::fs::create_dir_all(&task).unwrap();
        let stat_path = task.join("stat");
        for state in [b'S', b'Z'] {
            let mut bytes = b"42 (runtime\xff) name) ".to_vec();
            bytes.push(state);
            bytes.extend_from_slice(b" 1 123 0\n");
            std::fs::write(&stat_path, bytes).unwrap();
            let stat = read_proc_stat(&stat_path).unwrap().unwrap();
            assert_eq!(
                proc_stat_state_and_group(&stat).unwrap(),
                (if state == b'S' { "S" } else { "Z" }, 123)
            );
            assert_eq!(
                zombie_leader_has_live_threads(temp.path(), 123).unwrap(),
                state == b'S'
            );
        }
        std::fs::write(&stat_path, b"42 (runtime\xff) malformed\n").unwrap();
        assert!(zombie_leader_has_live_threads(temp.path(), 123).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn zombie_leader_checks_sibling_thread_state() {
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task/42");
        std::fs::create_dir_all(&task).unwrap();
        std::fs::write(task.join("stat"), "42 (sibling) S 1 123 0\n").unwrap();
        assert!(zombie_leader_has_live_threads(temp.path(), 123).unwrap());
        std::fs::write(task.join("stat"), "42 (sibling) Z 1 123 0\n").unwrap();
        assert!(!zombie_leader_has_live_threads(temp.path(), 123).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn zombie_only_group_is_clean_before_orphan_reap() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "local_runtime_process::tests::zombie_only_group_subprocess",
                "--nocapture",
            ])
            .env("IDORIS_ZOMBIE_ONLY_GROUP_HELPER", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated subreaper test failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn zombie_only_group_subprocess() {
        if std::env::var_os("IDORIS_ZOMBIE_ONLY_GROUP_HELPER").is_none() {
            return;
        }
        // prctl changes the calling process; the outer test launches this
        // helper in its own process so concurrent tests keep their reaper.
        nix::sys::prctl::set_child_subreaper(true).unwrap();
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
        let orphan_pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        process.shutdown(Duration::from_millis(250)).await.unwrap();
        assert!(
            group_exists(pgid).unwrap(),
            "unreaped orphan anchors the group"
        );
        assert!(!group_has_live_members(pgid).unwrap());
        nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(orphan_pid), None).unwrap();
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

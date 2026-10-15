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
    pub fn spawn(launch: &LocalRuntimeLaunch, api_key: Option<&str>) -> Result<Self, BackendError> {
        Self::spawn_command(launch.executable(), launch.args_with_api_key(api_key)?)
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
        // Linux can retain killed orphan descendants as zombies until PID 1
        // reaps them. Check for live group members while the unreaped leader
        // still prevents this PGID from being reused.
        #[cfg(target_os = "linux")]
        let group_clean = wait_group_cleanup(self.process_group, grace).await;
        tokio::time::timeout(grace, self.child.wait())
            .await
            .map_err(|_| BackendError::internal("timed out reaping local runtime process"))?
            .map_err(|_| BackendError::internal("failed to reap local runtime process"))?;
        // No group signal may happen after wait() reaps the ownership
        // anchor: from this point the old PGID is allowed to be reused.
        self.armed = false;
        // On other Unix systems, observe group removal after the leader has
        // been reaped. This is read-only: never signal a numeric PGID here.
        #[cfg(not(target_os = "linux"))]
        let group_clean = wait_group_cleanup(self.process_group, grace).await;
        if !group_clean? {
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

#[cfg(target_os = "linux")]
fn group_has_live_members(process_group: i32) -> Result<bool, BackendError> {
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
fn group_has_live_members(process_group: i32) -> Result<bool, BackendError> {
    group_exists(process_group)
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

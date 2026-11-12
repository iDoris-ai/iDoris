//! Outer deadlines for integration tests that spawn `idoris`.
//!
//! A newly copied binary's first exec on macOS can sit in assessment
//! (`syspolicyd`) for seconds before user code runs, and a loaded machine
//! stretches every later poll. These limits are how long the test is willing
//! to wait. They are not the behavior under test: a startup that must fail
//! still has to exit on its own with the expected status and stderr. Timing
//! out is still a test failure.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Default outer wait for "the process exited" or "health or the port became
/// ready". Long enough for a loaded laptop and for CI. Override with
/// `IDORIS_TEST_STARTUP_WAIT_SECS` on a slower machine. Do not set it so
/// short that a late exit is reported as a timeout.
pub const DEFAULT_STARTUP_OBSERVE_SECS: u64 = 60;

/// One readiness probe. Connection refused returns immediately; this only
/// bounds a connect that hangs. The outer observe limit is what fails the test.
pub const HTTP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn startup_observe_limit() -> Duration {
    let Ok(raw) = std::env::var("IDORIS_TEST_STARTUP_WAIT_SECS") else {
        return Duration::from_secs(DEFAULT_STARTUP_OBSERVE_SECS);
    };
    let secs: u64 = raw.parse().unwrap_or_else(|_| {
        panic!("IDORIS_TEST_STARTUP_WAIT_SECS must be a whole number of seconds, got {raw:?}")
    });
    assert!(
        secs >= 1,
        "IDORIS_TEST_STARTUP_WAIT_SECS must be at least 1, got {secs}"
    );
    Duration::from_secs(secs)
}

/// Run the binary once and wait until that process exits, before any
/// readiness or failure deadline starts.
///
/// `idoris` has no `--version` flag; the CLI rejects it and exits. That is
/// enough: the first exec of a newly copied file is what macOS assesses, and
/// the assessment finishes only once that process has started and exited. A
/// non-zero status is expected. A process that does not exit is not warmed up.
pub fn warm_exec(exe: &Path) {
    let mut command = Command::new(exe);
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
    {
        command.env_remove(key);
    }
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawn_when_executable(&mut command);
    let limit = startup_observe_limit();
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait().unwrap().is_some() {
            let _status = child.wait().unwrap();
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("warmup of {exe:?} did not exit within {}s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn, retrying only a transient text-busy exec (`ETXTBSY`) until the
/// observe limit. Every other spawn error fails the test immediately.
pub fn spawn_when_executable(command: &mut Command) -> Child {
    let deadline = Instant::now() + startup_observe_limit();
    loop {
        match command.spawn() {
            Ok(child) => return child,
            Err(err)
                if err.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!(
                "failed to spawn {}: {err}",
                command.get_program().to_string_lossy()
            ),
        }
    }
}

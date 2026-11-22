//! IDORIS-CORE S6 (Codex final review H1, then the main session's requirement that the per-request re-check adds p99 < 5 ms and starts no
//! child process): real processes, real sockets, the real [`SystemInspector`].
//!
//! The two halves of "who is behind the port", and what each sees:
//!
//! - **per request** ([`OwnedListenerGuard::verify`], native: libproc / `/proc`): the engine process we registered still holds a LISTEN
//!   socket on the port. It sees the engine let go of the port (alive or not), and a process that took the port over. It does **not**
//!   see a second listener next to a socket the engine still holds.
//! - **at every supervision tick and state change** ([`ProcessInspector::listens_on`], `lsof`): the engine is the *only* listener. That is
//!   what sees the second listener.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use idoris_upstream::engine::{OwnedListenerGuard, ProcessInspector, SystemInspector};
use idoris_upstream::omlx::key::{EndpointGuard, Verification};

/// A real process that listens on `<host>:<ephemeral port>` and, when told on its standard input, closes the listening socket **without
/// exiting** (what a misbehaving engine does).
const LISTENER: &str = r#"
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((sys.argv[1], 0))
s.listen(16)
print("listening %d" % s.getsockname()[1], flush=True)
for line in sys.stdin:
    if line.strip() == "release":
        s.close()
        print("released", flush=True)
"#;

struct Engine {
    child: Child,
    stdin: ChildStdin,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Engine {
    fn start(host: &str) -> Self {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-B", "-c", LISTENER, host])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        lines.read_line(&mut first).unwrap();
        let port = first
            .trim()
            .strip_prefix("listening ")
            .unwrap_or_else(|| panic!("the engine said {first:?}"))
            .parse()
            .unwrap();
        Self {
            child,
            stdin,
            lines,
            port,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The engine closes its listening socket and stays alive.
    fn release_port(&mut self) {
        writeln!(self.stdin, "release").unwrap();
        self.stdin.flush().unwrap();
        let mut ack = String::new();
        self.lines.read_line(&mut ack).unwrap();
        assert_eq!(ack.trim(), "released");
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A stand-in for `ps` / `lsof` that appends one line to `log` per invocation and then runs the real tool: a request path that starts a
/// child process shows up as a line.
fn counting_tool(dir: &Path, name: &str, real: &[&str], log: &Path) -> PathBuf {
    let mut body = format!("#!/bin/sh\necho {name} >> '{}'\n", log.display());
    for candidate in real {
        body.push_str(&format!("[ -x {candidate} ] && exec {candidate} \"$@\"\n"));
    }
    body.push_str("exit 127\n");
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

/// The real inspector with counting stand-ins for the two tools; timeouts far above a loaded machine's (the first `exec` of a freshly
/// written file is slow on macOS).
fn counting_inspector(dir: &Path) -> (Arc<SystemInspector>, PathBuf) {
    let log = dir.join("calls.log");
    let ps = counting_tool(dir, "ps", &["/bin/ps"], &log);
    let lsof = counting_tool(dir, "lsof", &["/usr/sbin/lsof", "/usr/bin/lsof"], &log);
    let inspector = SystemInspector::with_tools(ps, lsof)
        .with_timeouts(Duration::from_secs(60), Duration::from_secs(60))
        .with_native_probe(true);
    (Arc::new(inspector), log)
}

/// **The owner's requirement.** A request's re-check starts no child process (not one `ps`, not one `lsof`) however many requests there
/// are, and the engine letting go of its port (it is still alive) is seen by the very next request; whoever binds the port afterwards is
/// refused too. The question the supervision tick asks (`lsof`: the only listener) agrees.
#[tokio::test]
async fn the_request_path_starts_no_child_process_and_the_very_next_request_sees_the_engine_let_go_of_its_port()
 {
    let mut engine = Engine::start("127.0.0.1");
    let dir = tempfile::tempdir().unwrap();
    let (inspector, log) = counting_inspector(dir.path());
    let guard = OwnedListenerGuard::new(inspector.clone(), engine.pid(), engine.port);

    // What the manager asks at a state change (and at every tick): the expensive question, one `lsof`.
    assert_eq!(
        inspector.listens_on(engine.pid(), engine.port).await,
        Ok(true)
    );
    let asked_at_the_state_change = calls(&log);
    assert!(asked_at_the_state_change >= 1);

    for _ in 0..500 {
        assert_eq!(guard.verify().await, Verification::Verified);
    }
    assert_eq!(
        calls(&log),
        asked_at_the_state_change,
        "a child process was started on the request path"
    );

    // The engine lets go of the port and stays alive.
    engine.release_port();
    assert_eq!(
        guard.verify().await,
        Verification::Mismatch,
        "the very next request must see it"
    );
    assert_eq!(guard.verify().await, Verification::Mismatch);

    // Another process (this one) takes the port: still not the engine.
    let impostor = std::net::TcpListener::bind(("127.0.0.1", engine.port)).unwrap();
    assert_eq!(guard.verify().await, Verification::Mismatch);
    assert_eq!(
        calls(&log),
        asked_at_the_state_change,
        "refusing needs no child process either"
    );
    // The supervision tick's own question agrees (and is the one that costs a process).
    assert_eq!(
        inspector.listens_on(engine.pid(), engine.port).await,
        Ok(false)
    );
    assert!(calls(&log) > asked_at_the_state_change);
    drop(impostor);
}

/// The engine process exits: the very next request refuses, again with no child process.
#[tokio::test]
async fn an_engine_that_exits_is_refused_on_the_very_next_request() {
    let mut engine = Engine::start("127.0.0.1");
    let dir = tempfile::tempdir().unwrap();
    let (inspector, log) = counting_inspector(dir.path());
    let guard = OwnedListenerGuard::new(inspector, engine.pid(), engine.port);
    assert_eq!(guard.verify().await, Verification::Verified);
    engine.kill();
    assert_eq!(guard.verify().await, Verification::Mismatch);
    assert_eq!(calls(&log), 0, "no child process on the request path");
}

/// A guard built for a process that is not the listener (a different pid than the one holding the port) never verifies, and the watch
/// is bound to the process it was built for, not to the port: this process holds a socket on a port, the guard is for the engine.
#[tokio::test]
async fn a_port_held_by_another_process_is_not_the_engines() {
    let engine = Engine::start("127.0.0.1");
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port();
    let inspector = Arc::new(SystemInspector::new());
    // The engine's guard on the port this process holds.
    let wrong_port = OwnedListenerGuard::new(inspector.clone(), engine.pid(), other_port);
    assert_eq!(wrong_port.verify().await, Verification::Mismatch);
    // A guard for this process on the engine's port.
    let wrong_pid = OwnedListenerGuard::new(inspector.clone(), std::process::id(), engine.port);
    assert_eq!(wrong_pid.verify().await, Verification::Mismatch);
    // And the real pairs verify.
    let right = OwnedListenerGuard::new(inspector.clone(), engine.pid(), engine.port);
    assert_eq!(right.verify().await, Verification::Verified);
    let mine = OwnedListenerGuard::new(inspector, std::process::id(), other_port);
    assert_eq!(mine.verify().await, Verification::Verified);
}

/// **What the per-request check cannot see, and who does.** The engine keeps a socket on the port (a wildcard) and another process binds a
/// more specific address on the same port (`127.0.0.1`; BSD sockets, so macOS, allow that with `SO_REUSEADDR`, Linux refuses): the engine
/// still holds *its* socket, so the per-request check says yes, while connections to `127.0.0.1` go to the other process. The
/// uniqueness question (`lsof`: the engine is the *only* listener), which the manager asks at every supervision tick, says no.
///
/// This is the window spec errata ten (8) writes down: "since the last tick" for a second listener, "per request" for the engine letting
/// go of the port.
#[tokio::test]
async fn a_second_listener_next_to_the_engines_socket_is_invisible_per_request_and_seen_by_the_uniqueness_question()
 {
    let engine = Engine::start("0.0.0.0");
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    if socket
        .bind(std::net::SocketAddr::from(([127, 0, 0, 1], engine.port)))
        .is_err()
    {
        eprintln!(
            "this OS refuses a more specific bind next to a wildcard listener: there is no second listener to show"
        );
        return;
    }
    let _second_listener = socket.listen(16).unwrap();

    let inspector = Arc::new(SystemInspector::new());
    let guard = OwnedListenerGuard::new(inspector.clone(), engine.pid(), engine.port);
    assert_eq!(
        guard.verify().await,
        Verification::Verified,
        "the engine still holds its own socket: the per-request check has no way to see the second listener"
    );
    assert_eq!(
        inspector.listens_on(engine.pid(), engine.port).await,
        Ok(false),
        "the uniqueness question asked at every tick sees it"
    );
}

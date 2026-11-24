//! IDORIS-CORE S6 (Codex final review H1, then the main session's requirement that the per-request re-check adds p99 < 5 ms and starts no
//! child process, then Codex's focused review of that work): real processes, real sockets, the real [`SystemInspector`].
//!
//! The two halves of "who is behind the port", and what each sees:
//!
//! - **per request** ([`OwnedListenerGuard::verify`], native: libproc / `/proc`): the engine process we registered still holds a LISTEN
//!   socket that a connection to the dial address reaches. It sees the engine let go of the port (alive or not), and a process that took
//!   the port over. It does **not** see a second listener next to a socket the engine still holds.
//! - **at every supervision tick and state change** ([`ProcessInspector::listens_on`], `lsof`): the engine is the *only* listener. That is
//!   what sees the second listener.
//!
//! The native watch exists on macOS and Linux only (`mod native`); every Unix has the fallback contract (`mod fallback`: the expensive
//! question, a short reuse window, one check in flight), which is what the other Unixes run with.
#![allow(clippy::unwrap_used)]
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use idoris_upstream::engine::{MANAGED_DIAL, SystemInspector};

/// A real process that listens on `<host>:<port or ephemeral>` (IPv4, IPv6, or IPv6 with `IPV6_V6ONLY` set either way) and, when told on
/// its standard input, closes the listening socket **without exiting** (what a misbehaving engine does), or shuts it down for reading while
/// keeping the descriptor (what leaves the LISTEN state on Linux with the descriptor still open).
const LISTENER: &str = r#"
import socket, sys
host, family, v6only, port = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
s = socket.socket(socket.AF_INET6 if family == "6" else socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
if family == "6" and v6only in ("0", "1"):
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, int(v6only))
s.bind((host, port))
s.listen(16)
print("listening %d" % s.getsockname()[1], flush=True)
for line in sys.stdin:
    command = line.strip()
    if command == "release":
        s.close()
        print("released", flush=True)
    elif command == "shutdown":
        try:
            s.shutdown(socket.SHUT_RD)
            print("shutdown", flush=True)
        except OSError as error:
            print("refused %d" % error.errno, flush=True)
"#;

struct Engine {
    child: Child,
    stdin: ChildStdin,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Engine {
    /// An IPv4 listener on `host` (an ephemeral port).
    fn start(host: &str) -> Self {
        Self::spawn(host, "4", "-", 0)
    }

    /// An IPv6 listener on `host`; `v6only` is `"0"` (dual-stack), `"1"` (IPv6 only) or `"-"` (the system default). `None` when this
    /// machine has no IPv6 loopback to bind.
    fn start_v6(host: &str, v6only: &str, port: u16) -> Option<Self> {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-B", "-c", LISTENER, host, "6", v6only, &port.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        lines.read_line(&mut first).unwrap();
        let port = first.trim().strip_prefix("listening ")?.parse().unwrap();
        Some(Self {
            child,
            stdin,
            lines,
            port,
        })
    }

    fn spawn(host: &str, family: &str, v6only: &str, port: u16) -> Self {
        let mut child = Command::new("/usr/bin/python3")
            .args([
                "-B",
                "-c",
                LISTENER,
                host,
                family,
                v6only,
                &port.to_string(),
            ])
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

    fn dial(&self) -> SocketAddr {
        SocketAddr::new(MANAGED_DIAL, self.port)
    }

    fn tell(&mut self, command: &str) -> String {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
        let mut answer = String::new();
        self.lines.read_line(&mut answer).unwrap();
        answer.trim().to_owned()
    }

    /// The engine closes its listening socket and stays alive.
    fn release_port(&mut self) {
        assert_eq!(self.tell("release"), "released");
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
fn counting_inspector(dir: &Path, native: bool) -> (Arc<SystemInspector>, PathBuf) {
    let log = dir.join("calls.log");
    let ps = counting_tool(dir, "ps", &["/bin/ps", "/usr/bin/ps"], &log);
    let lsof = counting_tool(dir, "lsof", &["/usr/sbin/lsof", "/usr/bin/lsof"], &log);
    let inspector = SystemInspector::with_tools(ps, lsof)
        .with_timeouts(Duration::from_secs(60), Duration::from_secs(60))
        .with_native_probe(native);
    (Arc::new(inspector), log)
}

fn has_lsof() -> bool {
    ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .iter()
        .any(|path| Path::new(path).is_file())
}

/// The per-request check of the real native watch (libproc on macOS, `/proc` on Linux).
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use std::net::{IpAddr, Ipv6Addr, SocketAddr};
    use std::sync::Arc;

    use idoris_upstream::engine::{OwnedListenerGuard, ProcessInspector, SystemInspector};
    use idoris_upstream::omlx::key::{EndpointGuard, Verification};

    use super::{Engine, calls, counting_inspector, has_lsof};

    /// **The owner's requirement.** A request's re-check starts no child process (not one `ps`, not one `lsof`) however many requests there
    /// are, and the engine letting go of its port (it is still alive) is seen by the very next request; whoever binds the port afterwards is
    /// refused too. The question the supervision tick asks (`lsof`: the only listener) agrees.
    #[tokio::test]
    async fn the_request_path_starts_no_child_process_and_the_very_next_request_sees_the_engine_let_go_of_its_port()
     {
        let mut engine = Engine::start("127.0.0.1");
        let dir = tempfile::tempdir().unwrap();
        let (inspector, log) = counting_inspector(dir.path(), true);
        let guard = OwnedListenerGuard::new(inspector.clone(), engine.pid(), engine.dial());

        // What the manager asks at a state change (and at every tick): the expensive question, one `lsof`.
        let tick_question = has_lsof();
        let mut asked_at_the_state_change = 0;
        if tick_question {
            assert_eq!(
                inspector.listens_on(engine.pid(), engine.port).await,
                Ok(true)
            );
            asked_at_the_state_change = calls(&log);
            assert!(asked_at_the_state_change >= 1);
        }

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
        if tick_question {
            // The supervision tick's own question agrees (and is the one that costs a process).
            assert_eq!(
                inspector.listens_on(engine.pid(), engine.port).await,
                Ok(false)
            );
            assert!(calls(&log) > asked_at_the_state_change);
        }
        drop(impostor);
    }

    /// The engine process exits: the very next request refuses, again with no child process.
    #[tokio::test]
    async fn an_engine_that_exits_is_refused_on_the_very_next_request() {
        let mut engine = Engine::start("127.0.0.1");
        let dir = tempfile::tempdir().unwrap();
        let (inspector, log) = counting_inspector(dir.path(), true);
        let guard = OwnedListenerGuard::new(inspector, engine.pid(), engine.dial());
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
        let dial = |port| SocketAddr::new(idoris_upstream::engine::MANAGED_DIAL, port);
        // The engine's guard on the port this process holds.
        let wrong_port = OwnedListenerGuard::new(inspector.clone(), engine.pid(), dial(other_port));
        assert_eq!(wrong_port.verify().await, Verification::Mismatch);
        // A guard for this process on the engine's port.
        let wrong_pid =
            OwnedListenerGuard::new(inspector.clone(), std::process::id(), dial(engine.port));
        assert_eq!(wrong_pid.verify().await, Verification::Mismatch);
        // And the real pairs verify.
        let right = OwnedListenerGuard::new(inspector.clone(), engine.pid(), dial(engine.port));
        assert_eq!(right.verify().await, Verification::Verified);
        let mine = OwnedListenerGuard::new(inspector, std::process::id(), dial(other_port));
        assert_eq!(mine.verify().await, Verification::Verified);
    }

    /// **What the per-request check cannot see, and who does.** The engine keeps a socket on the port (a wildcard) and another process binds a
    /// more specific address on the same port (`127.0.0.1`; BSD sockets, so macOS, allow that with `SO_REUSEADDR`, Linux refuses): the engine
    /// still holds *its* socket, so the per-request check says yes, while connections to `127.0.0.1` go to the other process. The
    /// uniqueness question (`lsof`: the engine is the *only* listener), which the manager asks at every supervision tick, says no.
    ///
    /// This is the window spec errata ten (8) writes down: "until the next supervision check" for a second listener, "per request" for the
    /// engine letting go of the port.
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
        let guard = OwnedListenerGuard::new(inspector.clone(), engine.pid(), engine.dial());
        assert_eq!(
            guard.verify().await,
            Verification::Verified,
            "the engine still holds its own socket: the per-request check has no way to see the second listener"
        );
        if has_lsof() {
            assert_eq!(
                inspector.listens_on(engine.pid(), engine.port).await,
                Ok(false),
                "the uniqueness question asked at every tick sees it"
            );
        }
    }

    // ----- Codex focused review H1: the address that was verified is the address that is dialed ---------------------------------------

    /// A listener on `[::1]:<port>` only. The guard for the IPv4 loopback dial address (what every managed engine is dialed at) must not
    /// pass on its account, and the guard that hands out `[::1]` is the one that does: the verified address is the one that is dialed.
    #[tokio::test]
    async fn a_listener_on_ipv6_loopback_is_verified_for_that_address_only() {
        let Some(engine) = Engine::start_v6("::1", "-", 0) else {
            eprintln!("no IPv6 loopback on this machine: nothing to show");
            return;
        };
        let inspector = Arc::new(SystemInspector::new());
        let v4 = OwnedListenerGuard::new(inspector.clone(), engine.pid(), engine.dial());
        assert_eq!(v4.dial(), Some(engine.dial()));
        assert_eq!(
            v4.verify().await,
            Verification::Mismatch,
            "a connection to 127.0.0.1 does not reach a socket bound to ::1"
        );
        let v6_address = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), engine.port);
        let v6 = OwnedListenerGuard::new(inspector, engine.pid(), v6_address);
        assert_eq!(v6.dial(), Some(v6_address));
        assert_eq!(v6.verify().await, Verification::Verified);
    }

    // ----- Codex focused review H2: an IPv6-only wildcard is not an IPv4 listener ----------------------------------------------------

    /// A real IPv6-only listener on `::` (`IPV6_V6ONLY` set): an IPv4 connection cannot reach it, so it is **not** the engine of an IPv4
    /// dial address (the old Linux path read every `::` as "also IPv4"). A real dual-stack listener on `::` (`IPV6_V6ONLY` cleared) is
    /// reachable over IPv4; macOS reports that (`INI_IPV4` in the socket's flags) and says yes. Linux's `/proc` does not show the option,
    /// so there the guard has no evidence and does not say yes (the managed engines bind `127.0.0.1` explicitly; an external one that
    /// listens on `::` is refused rather than guessed).
    #[tokio::test]
    async fn an_ipv6_only_wildcard_listener_is_not_an_ipv4_engine_and_a_dual_stack_one_is_only_with_evidence()
     {
        let Some(v6_only) = Engine::start_v6("::", "1", 0) else {
            eprintln!("no IPv6 on this machine: nothing to show");
            return;
        };
        let inspector = Arc::new(SystemInspector::new());
        let guard = OwnedListenerGuard::new(inspector.clone(), v6_only.pid(), v6_only.dial());
        for _ in 0..3 {
            assert_eq!(
                guard.verify().await,
                Verification::Mismatch,
                "an IPv6-only socket on :: is not reachable at 127.0.0.1"
            );
        }
        let dual = Engine::start_v6("::", "0", 0).unwrap();
        let guard = OwnedListenerGuard::new(inspector, dual.pid(), dual.dial());
        let expected = if cfg!(target_os = "macos") {
            Verification::Verified
        } else {
            Verification::Mismatch
        };
        assert_eq!(guard.verify().await, expected);
    }

    // ----- Codex focused review H3 (Linux): the descriptor is kept but the socket is no longer listening -----------------------------

    /// **H3.** The engine keeps its descriptor but the socket leaves LISTEN (`shutdown(SHUT_RD)` on a listening socket does that on Linux): the
    /// old fast path trusted "this descriptor is a socket on the port" and kept saying yes while nobody was listening. The very next request
    /// is refused now. (macOS refuses `shutdown` on a listener: nothing to show there, the closed-socket case above is what it has.)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_socket_that_left_listen_with_its_descriptor_kept_is_refused_on_the_very_next_request()
     {
        let mut engine = Engine::start("127.0.0.1");
        let dir = tempfile::tempdir().unwrap();
        let (inspector, log) = counting_inspector(dir.path(), true);
        let guard = OwnedListenerGuard::new(inspector, engine.pid(), engine.dial());
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(engine.tell("shutdown"), "shutdown");
        for _ in 0..3 {
            assert_eq!(
                guard.verify().await,
                Verification::Mismatch,
                "the descriptor is still open but the socket is not listening"
            );
        }
        assert_eq!(calls(&log), 0, "no child process on the request path");
    }

    // ----- the address families of the `lsof` listing (the fallback and the supervision tick read them) -------------------------------

    /// `lsof` keeps the address family: an IPv6-only wildcard is no sole IPv4 listener, an IPv4 one is.
    #[tokio::test]
    async fn the_lsof_listing_keeps_the_address_family() {
        if !has_lsof() {
            return;
        }
        let Some(v6_only) = Engine::start_v6("::", "1", 0) else {
            return;
        };
        let inspector = SystemInspector::new();
        assert_eq!(
            inspector.listens_on(v6_only.pid(), v6_only.port).await,
            Ok(false),
            "an IPv6-only wildcard listener does not take connections to 127.0.0.1"
        );
        let v4 = Engine::start("127.0.0.1");
        assert_eq!(inspector.listens_on(v4.pid(), v4.port).await, Ok(true));
    }
}

/// The fallback contract, which every Unix runs with when it has no native watch (the native probe switched off stands in for it): the
/// expensive question per request, a short reuse window for the answer **yes** only, one check in flight at a time.
mod fallback {
    use std::sync::Arc;
    use std::time::Duration;

    use idoris_upstream::engine::{FALLBACK_TTL, OwnedListenerGuard, ProcessInspector};
    use idoris_upstream::omlx::key::{EndpointGuard, Verification};

    use super::{Engine, SystemInspector, calls, counting_inspector, has_lsof};

    /// The window of a yes starts when the check **starts**, so the first `exec` of a freshly written tool (slow on macOS, slower than
    /// the whole window) must not be part of what is measured: run it once, and count from there. Returns the count so far.
    async fn warm_up(inspector: &Arc<SystemInspector>, log: &std::path::Path, port: u16) -> usize {
        let _ = inspector.listening_sockets(port).await.unwrap();
        calls(log)
    }

    /// The engine lets go of its port: the fallback sees it by the first request after the reuse window, with the real `lsof`; and a
    /// burst of requests inside the window costs one `lsof`.
    #[tokio::test]
    async fn the_fallback_asks_lsof_at_most_once_per_window_and_sees_the_release_after_it() {
        if !has_lsof() {
            return;
        }
        let mut engine = Engine::start("127.0.0.1");
        let dir = tempfile::tempdir().unwrap();
        let (inspector, log) = counting_inspector(dir.path(), false);
        assert!(
            inspector
                .watch_listener(engine.pid(), engine.dial())
                .is_none()
        );
        assert_eq!(inspector.fallback_ttl(), FALLBACK_TTL);
        let warm = warm_up(&inspector, &log, engine.port).await;
        let guard = OwnedListenerGuard::new(inspector, engine.pid(), engine.dial());
        for _ in 0..30 {
            assert_eq!(guard.verify().await, Verification::Verified);
        }
        assert_eq!(calls(&log) - warm, 1, "thirty requests inside the window");
        engine.release_port();
        tokio::time::sleep(FALLBACK_TTL + Duration::from_millis(100)).await;
        assert_eq!(guard.verify().await, Verification::Mismatch);
        assert!(calls(&log) - warm >= 2);
    }

    /// **H7 with the real tools.** Many requests that arrive together when the window is closed start **one** `lsof` between them, not one
    /// each; the window opens from the moment that check started.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_concurrent_requests_start_one_lsof() {
        if !has_lsof() {
            return;
        }
        let engine = Engine::start("127.0.0.1");
        let dir = tempfile::tempdir().unwrap();
        let (inspector, log) = counting_inspector(dir.path(), false);
        let warm = warm_up(&inspector, &log, engine.port).await;
        let guard = Arc::new(OwnedListenerGuard::new(
            inspector,
            engine.pid(),
            engine.dial(),
        ));
        let tasks: Vec<_> = (0..32)
            .map(|_| {
                let guard = guard.clone();
                tokio::spawn(async move { guard.verify().await })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), Verification::Verified);
        }
        assert_eq!(
            calls(&log) - warm,
            1,
            "32 concurrent requests, {} lsof invocations",
            calls(&log) - warm
        );
    }
}

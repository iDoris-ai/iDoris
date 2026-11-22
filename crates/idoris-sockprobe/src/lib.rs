//! A per-request, process-free check that **one known process** still holds a LISTEN TCP socket on a port (IDORIS-CORE S6; the
//! per-request half of the "credentials only go to the verified listener" rule).
//!
//! # Why this crate exists
//!
//! Before every request the engine manager's guard re-confirms who is behind a managed port. Doing that with `lsof` costs about 100 ms
//! and a child process per request; this crate answers the narrower question the request path needs — *does the process we registered
//! (same pid, same start time) still hold a LISTEN socket on this port, bound to an address our client connects to?* — with two or three
//! system calls and no child process:
//!
//! - **macOS**: `proc_pidinfo(PROC_PIDTBSDINFO)` (is it still the same process: start time) and `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` on the
//!   file descriptor the listening socket had last time. A scan of the process's descriptors happens only when that descriptor no longer
//!   is the listening socket.
//! - **Linux**: `/proc/<pid>/stat` (start time) and `readlink /proc/<pid>/fd/<fd>` (still the same socket inode); the table
//!   `/proc/net/tcp{,6}` is read only to (re)locate the socket.
//! - **elsewhere**: no probe ([`ListenerWatch::establish`] returns `None`); the caller falls back to something slower.
//!
//! What it deliberately does **not** do: it does not scan other processes, so it cannot tell that a *second* process also listens on the
//! port (a more specific address next to the engine's wildcard socket). That uniqueness check — and the owner / signing-team
//! re-verification of an external engine — stays with the engine manager's supervision tick and every state change (start, adoption,
//! restart, external generation change). See the `idoris-upstream` crate's `engine::guard` for how the two halves are combined, and the
//! interface spec (errata ten, item 8) for the precise windows.
//!
//! # The only `unsafe` in the workspace
//!
//! The workspace sets `unsafe_code = "forbid"` and the design had chosen subprocesses (`ps`, `lsof`) over libproc for exactly that reason.
//! Making the request path cheap needs libproc, which has no safe wrapper in `libc`, `nix` or `rustix`. So the FFI lives here, in one file
//! (`macos.rs`, compiled on macOS only) behind a narrow `#![allow(unsafe_code)]`; this crate `deny`s `unsafe_code` everywhere else, and
//! every other crate keeps its `forbid`. The calls only pass pointers to zero-initialised plain-old-data buffers that the callee fills in;
//! the mirrored C structs are checked against the sizes and offsets the kernel documents (compile-time asserts) and against real sockets
//! (tests).

use std::sync::Mutex;

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod unsupported;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use unsupported as platform;

/// The answer of one [`ListenerWatch::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The watched process still holds a LISTEN TCP socket on the watched port, bound to an address the client connects to (the loopback
    /// address or a wildcard).
    Holds,
    /// It does not: the process is gone (or is a different process now: the pid was recycled), or it no longer has such a socket.
    NotHolding,
    /// This process could not be inspected (permission, an unexpected error). Not a "no": the caller decides what to do.
    Unavailable,
}

/// A watch on one process's listening socket. [`ListenerWatch::establish`] pins the identity of the process (its start time) **once**,
/// when the engine manager registers the engine; every later [`ListenerWatch::check`] compares against that, so a recycled pid is never
/// mistaken for the engine. The watch belongs to one engine life: a new engine gets a new watch.
#[derive(Debug)]
pub struct ListenerWatch {
    inner: platform::Watch,
    /// The descriptor the listening socket had at the last successful check (`None` until it is found): the fast path asks about this one
    /// descriptor instead of scanning them all.
    hint: Mutex<Option<platform::Hint>>,
}

impl ListenerWatch {
    /// Pin the identity of `pid` now and look for its listening socket on `port`. `None` when the platform has no probe, the process is
    /// already gone, or it cannot be inspected: the caller falls back to a slower check.
    ///
    /// The process does not have to hold the socket yet (the engine manager creates the watch when it spawns the engine, before the engine
    /// binds its port): until it does, [`Self::check`] says [`Probe::NotHolding`].
    pub fn establish(pid: u32, port: u16) -> Option<Self> {
        let (inner, hint) = platform::establish(pid, port)?;
        Some(Self {
            inner,
            hint: Mutex::new(hint),
        })
    }

    /// Does the watched process still hold a LISTEN socket on the watched port? No child process, no scan of other processes; two or three
    /// system calls once the descriptor is known.
    pub fn check(&self) -> Probe {
        let hint = *self
            .hint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (probe, hint) = self.inner.check(hint);
        *self
            .hint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hint;
        probe
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::{TcpListener, TcpStream};

    use super::*;

    fn me() -> u32 {
        std::process::id()
    }

    fn watch(port: u16) -> ListenerWatch {
        ListenerWatch::establish(me(), port)
            .expect("this platform has a probe and this process can be inspected")
    }

    /// The listening socket of this very process on the loopback address: held while it is open, **not held on the very next check** once
    /// it is closed (the engine released its port; whoever binds it next is not the engine).
    #[test]
    fn a_listener_is_held_until_it_is_closed_and_the_next_check_sees_it_go() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let watch = watch(port);
        assert_eq!(watch.check(), Probe::Holds);
        assert_eq!(
            watch.check(),
            Probe::Holds,
            "the fast path answers the same"
        );
        drop(listener);
        assert_eq!(watch.check(), Probe::NotHolding);
        assert_eq!(watch.check(), Probe::NotHolding);
        // The same process listens on the port again (a new socket, a new descriptor): the watch is about the process, not about one
        // descriptor, so it holds again.
        if let Ok(again) = TcpListener::bind(("127.0.0.1", port)) {
            assert_eq!(watch.check(), Probe::Holds);
            drop(again);
            assert_eq!(watch.check(), Probe::NotHolding);
        }
    }

    #[test]
    fn a_listener_on_another_port_does_not_count() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = free.local_addr().unwrap().port();
        drop(free);
        let watch = watch(free_port);
        assert_eq!(watch.check(), Probe::NotHolding);
        drop(other);
    }

    /// A socket of the process that is not listening (here: a connected client whose local port is the one asked about) is not a
    /// listener.
    #[test]
    fn a_connected_socket_is_not_a_listener() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let client_port = client.local_addr().unwrap().port();
        let watch = watch(client_port);
        assert_eq!(watch.check(), Probe::NotHolding);
        drop(client);
    }

    #[test]
    fn a_wildcard_listener_counts_because_the_client_connects_through_it() {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(watch(port).check(), Probe::Holds);
    }

    /// `::1` is a loopback address the client does not connect to (it connects to `127.0.0.1`): a listener there is not the engine.
    #[test]
    fn a_listener_on_the_ipv6_loopback_is_not_the_target_address() {
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            return; // no IPv6 loopback on this machine
        };
        let port = listener.local_addr().unwrap().port();
        assert_eq!(watch(port).check(), Probe::NotHolding);
    }

    /// Another process (the parent: cargo, the shell) does not hold this process's port: it is either not holding it or not readable
    /// (another user's process), never a yes.
    #[test]
    fn another_process_does_not_hold_this_processs_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let parent = std::os::unix::process::parent_id();
        if let Some(watch) = ListenerWatch::establish(parent, port) {
            assert!(matches!(
                watch.check(),
                Probe::NotHolding | Probe::Unavailable
            ));
        }
        drop(listener);
    }

    /// A process that has exited holds nothing; a watch on it either cannot be established or says so.
    #[test]
    fn a_process_that_has_exited_holds_nothing() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let watch = ListenerWatch::establish(pid, 1).unwrap();
        assert_eq!(watch.check(), Probe::NotHolding);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(watch.check(), Probe::NotHolding);
    }

    /// A process that has died but has not been reaped yet (a zombie) holds nothing: no watch is established for it.
    #[test]
    fn a_zombie_holds_nothing() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let watch = ListenerWatch::establish(pid, 1).unwrap();
        child.kill().unwrap(); // SIGKILL, not reaped: a zombie until `wait`
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ListenerWatch::establish(pid, 1).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "a dead, unreaped process is still treated as alive"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(watch.check(), Probe::NotHolding);
        child.wait().unwrap();
    }

    /// Micro benchmark of the per-request check on the fast path (the descriptor is known): p50 / p99 over many calls. Run with
    /// `cargo test -p idoris-sockprobe --release -- --ignored --nocapture`; the requirement is p99 < 5 ms with no child process.
    #[test]
    #[ignore = "micro benchmark"]
    fn micro_benchmark_of_the_fast_path() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let watch = watch(port);
        assert_eq!(watch.check(), Probe::Holds);
        let mut samples: Vec<u128> = Vec::with_capacity(20_000);
        for _ in 0..20_000 {
            let started = std::time::Instant::now();
            let probe = watch.check();
            samples.push(started.elapsed().as_nanos());
            assert_eq!(probe, Probe::Holds);
        }
        samples.sort_unstable();
        let at = |q: f64| {
            samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)] as f64 / 1_000.0
        };
        eprintln!(
            "sockprobe fast path: n={} p50={:.1}us p90={:.1}us p99={:.1}us max={:.1}us",
            samples.len(),
            at(0.50),
            at(0.90),
            at(0.99),
            *samples.last().unwrap() as f64 / 1_000.0
        );
    }

    /// Diagnostic, not part of the suite: point the probe at **any process of this user** that listens on a TCP port (a signed, hardened
    /// system daemon, the real engine) and see what it says; nothing is sent to that process, the probe only reads the kernel's table:
    /// `SOCKPROBE_PID=<pid> SOCKPROBE_PORT=<port> cargo test -p idoris-sockprobe --release -- --ignored --nocapture real_process`.
    #[test]
    #[ignore = "diagnostic against a real process, needs SOCKPROBE_PID and SOCKPROBE_PORT"]
    fn real_process_diagnostic() {
        let (Some(pid), Some(port)) = (
            std::env::var("SOCKPROBE_PID")
                .ok()
                .and_then(|v| v.parse::<u32>().ok()),
            std::env::var("SOCKPROBE_PORT")
                .ok()
                .and_then(|v| v.parse::<u16>().ok()),
        ) else {
            eprintln!("set SOCKPROBE_PID and SOCKPROBE_PORT");
            return;
        };
        let started = std::time::Instant::now();
        let watch = ListenerWatch::establish(pid, port);
        eprintln!(
            "establish: {:?} in {:?}",
            watch.as_ref().map(|_| "watch"),
            started.elapsed()
        );
        let Some(watch) = watch else { return };
        for round in 0..3 {
            let started = std::time::Instant::now();
            let probe = watch.check();
            eprintln!("check {round}: {probe:?} in {:?}", started.elapsed());
        }
        // The same process, another port: not held.
        let other = ListenerWatch::establish(pid, port.wrapping_add(1)).unwrap();
        eprintln!("another port: {:?}", other.check());
    }

    /// Pid 0 and 1 are never an engine: no watch.
    #[test]
    fn pids_that_cannot_be_an_engine_have_no_watch() {
        assert!(ListenerWatch::establish(0, 80).is_none());
        assert!(ListenerWatch::establish(u32::MAX, 80).is_none());
    }
}

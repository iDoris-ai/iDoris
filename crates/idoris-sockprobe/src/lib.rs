//! A per-request, process-free check that **one known process** still holds a LISTEN TCP socket that a connection to a given address
//! reaches (IDORIS-CORE S6; the per-request half of the "credentials only go to the verified listener" rule).
//!
//! # Why this crate exists
//!
//! Before every request the engine manager's guard re-confirms who is behind a managed port. Doing that with `lsof` costs about 100 ms
//! and a child process per request; this crate answers the narrower question the request path needs — *does the process we registered
//! (same pid, same start time) still hold a LISTEN socket on this port that a connection to the address we dial reaches?* — with a few
//! system calls and no child process:
//!
//! - **macOS**: `proc_pidinfo(PROC_PIDTBSDINFO)` (is it still the same process: start time) and `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` on the
//!   file descriptor the listening socket had last time, which also reports the socket's **current** state and bind address. A scan of the
//!   process's descriptors happens only when that descriptor no longer is the listening socket.
//! - **Linux**: `/proc/<pid>/stat` (start time), `readlink /proc/<pid>/fd/<fd>` (still the same socket inode) and a read of
//!   `/proc/net/tcp{,6}` up to that inode's row (the socket is **still in LISTEN state** and bound to an address that covers the dial
//!   address). The rest of the table and the descriptor directory are read only to (re)locate the socket.
//! - **elsewhere**: no probe ([`ListenerWatch::establish`] returns `None`); the caller falls back to something slower.
//!
//! What it deliberately does **not** do: it does not scan other processes, so it cannot tell that a *second* process also listens on the
//! port (a more specific address next to the engine's wildcard socket). That uniqueness check — and the owner / signing-team
//! re-verification of an external engine — stays with the engine manager's supervision ticks and every state change (start, adoption,
//! restart, external generation change). See the `idoris-upstream` crate's `engine::guard` for how the two halves are combined, and the
//! interface spec (errata ten, item 8) for the precise windows and what is not covered.
//!
//! # Addresses
//!
//! A watch is built for one **dial address** (the address the HTTP client really connects to) and answers "yes" only for a socket that
//! [`Bind::covers`] it: an IPv4 socket bound to that address or to `0.0.0.0`, an IPv6 socket bound to that address or to `::`, and an
//! IPv6 wildcard socket for an **IPv4** dial address only when there is *evidence* that it also accepts IPv4 (it is not `IPV6_V6ONLY`):
//! macOS reports that, Linux and `lsof` do not, so there an IPv6 wildcard covers IPv6 dial addresses only.
//!
//! # Cost on the slow path
//!
//! The cheap check ([`ListenerWatch::check_cached`]) never scans. When the socket has to be (re)located ([`ListenerWatch::check`] scans the
//! process's descriptors, on Linux also the whole TCP table) the caller is expected to run it off the async executor and to merge
//! concurrent scans of one watch; `idoris-upstream` does.
//!
//! # The only `unsafe` in the workspace
//!
//! The workspace sets `unsafe_code = "forbid"` and the design had chosen subprocesses (`ps`, `lsof`) over libproc for exactly that reason.
//! Making the request path cheap needs libproc, which has no safe wrapper in `libc`, `nix` or `rustix`. So the FFI lives here, in one file
//! (`macos.rs`, compiled on macOS only) behind a narrow `allow`; this crate `deny`s `unsafe_code` everywhere else — the logic on top of
//! the FFI (`macos/watch.rs`) is safe code, tested against a scripted fake of the system calls — and every other crate keeps its `forbid`. The
//! calls only pass pointers to zero-initialised plain-old-data buffers that the callee fills in; the mirrored C structs are checked against
//! the sizes and offsets the kernel documents (compile-time asserts) and against real sockets (tests).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod unsupported;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use unsupported as platform;

/// What the **kernel** says is running in a process, read without a path and without a `stat` (so it does not depend on the executable
/// still being at the path it was started from, and an `exec` cannot split it into two samples taken of different programs). macOS: the
/// code-directory hash of the running image and its Mach-O UUID. Linux: the device and inode of `/proc/<pid>/exe`. A field the kernel does
/// not give for this process is `None`; an identity with every field `None` still equals itself (no reliable identity is not a refusal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExecutableIdentity {
    /// The code-directory hash of the running image (macOS; `None` for an unsigned image the kernel has no hash for).
    pub cdhash: Option<[u8; 20]>,
    /// The identity of the image itself (macOS: Mach-O UUID; Linux: device and inode, 8 bytes each).
    pub image: Option<[u8; 16]>,
    /// macOS: the kernel's exec version of the process (advanced by every `exec`), which brackets the reads above: two different versions
    /// mean the pair was not read from one program. `None` where the kernel gives none.
    pub exec_version: Option<i32>,
}

impl ExecutableIdentity {
    /// The kernel gave nothing: an identity like this equals every other empty one, so it says nothing about whether the program changed.
    pub fn is_empty(&self) -> bool {
        self.cdhash.is_none() && self.image.is_none()
    }
}

pub fn executable_identity(pid: u32) -> Option<ExecutableIdentity> {
    platform::executable_identity(pid)
}

/// The team identifier of the code the kernel runs for `pid`, from the kernel's own record of the running image (macOS: `csops`; no path, no
/// subprocess, so an executable updated or deleted on disk after the process started does not matter). `None` when the kernel does not vouch
/// for a team (and always off macOS): the caller falls back to the path-based tool.
pub fn kernel_signing_team(pid: u32) -> Option<String> {
    platform::signing_team(pid)
}

/// The kernel start time in the same format used by ownership records.
pub fn process_generation(pid: u32) -> Option<String> {
    platform::process_generation(pid)
}

/// Does the object behind `fd` carry an extended ACL entry that grants a write-like permission to somebody other than the user `owner`?
/// `Some(true)` yes, `Some(false)` no ACL or none that does, `None` the platform cannot tell. macOS only (`acl_get_fd_np` and friends, no
/// subprocess); on Linux POSIX ACLs are **not applicable** here and the answer is always `None` (callers rely on the permission bits).
#[cfg(unix)]
pub fn acl_grants_others_write(fd: std::os::fd::RawFd, owner: u32) -> Option<bool> {
    platform::acl_grants_others_write_fd(fd, owner)
}

/// [`acl_grants_others_write`] for the object at `path` itself (a symlink is not followed).
#[cfg(unix)]
pub fn acl_grants_others_write_at(path: &std::path::Path, owner: u32) -> Option<bool> {
    platform::acl_grants_others_write_link(path, owner)
}

/// What a listening TCP socket is bound to: the address family and the address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    /// An IPv4 socket: a specific address, or `0.0.0.0`.
    V4(Ipv4Addr),
    /// An IPv6 socket: a specific address, or `::`. `v4_too` is **evidence** that it also accepts IPv4 connections (the socket is not
    /// `IPV6_V6ONLY`); macOS reports it, Linux and `lsof` do not, so it is `false` there. A wildcard IPv6 socket that was not shown to be
    /// dual-stack does not cover an IPv4 dial address (asyncio and many other servers bind `::` with `IPV6_V6ONLY` on).
    V6 { addr: Ipv6Addr, v4_too: bool },
}

impl Bind {
    /// An IPv6 bind address as the system reports it. An IPv4-mapped address (`::ffff:a.b.c.d`) is an IPv4 bind.
    pub fn v6(addr: Ipv6Addr, v4_too: bool) -> Self {
        match addr.to_ipv4_mapped() {
            Some(mapped) => Self::V4(mapped),
            None => Self::V6 { addr, v4_too },
        }
    }

    /// Does a connection to `dial` reach a socket bound like this?
    pub fn covers(&self, dial: IpAddr) -> bool {
        match (*self, dial) {
            (Self::V4(bound), IpAddr::V4(dial)) => bound.is_unspecified() || bound == dial,
            (Self::V4(_), IpAddr::V6(_)) => false,
            (Self::V6 { addr, .. }, IpAddr::V6(dial)) => addr.is_unspecified() || addr == dial,
            (Self::V6 { addr, v4_too }, IpAddr::V4(_)) => v4_too && addr.is_unspecified(),
        }
    }
}

/// The answer of one [`ListenerWatch::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The watched process still holds a LISTEN TCP socket on the watched port that a connection to the watched dial address reaches.
    Holds,
    /// It does not: the process is gone (or is a different process now: the pid was recycled), or it no longer has such a socket.
    NotHolding,
    /// This process could not be inspected (permission, an unexpected error, a short or inconsistent answer). Not a "no": the caller
    /// decides what to do (ask something slower, never reject outright).
    Unavailable,
}

/// What a platform's cheap check decided (generic over the platform's descriptor hint).
pub(crate) enum Quick<H> {
    /// Decided without scanning: the answer and the hint to remember.
    Answer(Probe, Option<H>),
    /// The remembered descriptor is missing or no longer the listening socket: only a scan can tell.
    Scan,
}

/// A watch on one process's listening socket. [`ListenerWatch::establish`] pins the identity of the process (its start time) **once**,
/// when the engine manager registers the engine; every later check compares against that, so a recycled pid is never mistaken for the
/// engine (within the limits the spec lists). The watch belongs to one engine life: a new engine gets a new watch.
#[derive(Debug)]
pub struct ListenerWatch {
    inner: platform::Watch,
    /// The descriptor the listening socket had at the last successful check (`None` until it is found): the cheap check asks about this one
    /// descriptor instead of scanning them all.
    hint: Mutex<Option<platform::Hint>>,
}

impl ListenerWatch {
    /// Pin the identity of `pid` now (one system call; **no scan**). `None` when the platform has no probe, the process is already gone, or
    /// it cannot be inspected: the caller falls back to a slower check. `dial` is the address the client connects to.
    ///
    /// The process does not have to hold the socket yet (the engine manager creates the watch when it spawns the engine, before the engine
    /// binds its port): until it does, [`Self::check`] says [`Probe::NotHolding`].
    pub fn establish(pid: u32, port: u16, dial: IpAddr) -> Option<Self> {
        let inner = platform::establish(pid, port, dial)?;
        Some(Self {
            inner,
            hint: Mutex::new(None),
        })
    }

    /// Start time captured at establishment, before any descriptor observation.
    pub fn generation(&self) -> Option<String> {
        self.inner.generation()
    }

    fn hint(&self) -> Option<platform::Hint> {
        *self
            .hint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember(&self, hint: Option<platform::Hint>) {
        *self
            .hint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hint;
    }

    /// The cheap check: `Some(answer)` when it can be given without scanning (a few system calls, no lock held across them), `None` when the
    /// socket has to be (re)located first ([`Self::check`]).
    pub fn check_cached(&self) -> Option<Probe> {
        match self.inner.quick(self.hint()) {
            Quick::Answer(probe, hint) => {
                self.remember(hint);
                Some(probe)
            }
            Quick::Scan => None,
        }
    }

    /// Does the watched process hold a LISTEN socket that a connection to the dial address reaches? Cheap when the descriptor is known;
    /// **otherwise a scan** of the process's descriptors (on Linux also of the whole TCP table), which is blocking and unbounded in the size
    /// of the machine's tables: run it off the async executor and merge concurrent ones.
    pub fn check(&self) -> Probe {
        if let Some(probe) = self.check_cached() {
            return probe;
        }
        let (probe, hint) = self.inner.scan(self.hint());
        self.remember(hint);
        probe
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const LOOPBACK6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// Which dial addresses a socket bound like this is reached by: the whole decision, in one table.
    #[test]
    fn a_bind_covers_exactly_the_dial_addresses_a_connection_reaches_it_by() {
        let any4 = Bind::V4(Ipv4Addr::UNSPECIFIED);
        let lo4 = Bind::V4(Ipv4Addr::LOCALHOST);
        let other4 = Bind::V4(Ipv4Addr::new(10, 1, 2, 3));
        let any6 = Bind::V6 {
            addr: Ipv6Addr::UNSPECIFIED,
            v4_too: false,
        };
        let dual = Bind::V6 {
            addr: Ipv6Addr::UNSPECIFIED,
            v4_too: true,
        };
        let lo6 = Bind::V6 {
            addr: Ipv6Addr::LOCALHOST,
            v4_too: false,
        };
        // an IPv4 socket: its own address or the wildcard, only for IPv4
        assert!(any4.covers(LOOPBACK) && any4.covers(v4(10, 1, 2, 3)));
        assert!(
            lo4.covers(LOOPBACK) && !lo4.covers(v4(127, 0, 0, 2)) && !lo4.covers(v4(10, 1, 2, 3))
        );
        assert!(!other4.covers(LOOPBACK) && other4.covers(v4(10, 1, 2, 3)));
        assert!(!any4.covers(LOOPBACK6) && !lo4.covers(LOOPBACK6));
        // an IPv6 socket: its own address or `::` for IPv6; for IPv4 only a wildcard that was shown to be dual-stack
        assert!(any6.covers(LOOPBACK6) && lo6.covers(LOOPBACK6) && dual.covers(LOOPBACK6));
        assert!(!lo6.covers(IpAddr::V6("::2".parse().unwrap())));
        assert!(
            !any6.covers(LOOPBACK),
            "an IPv6 wildcard with no evidence of dual stack does not accept IPv4"
        );
        assert!(dual.covers(LOOPBACK) && dual.covers(v4(10, 1, 2, 3)));
        assert!(
            !lo6.covers(LOOPBACK),
            "a specific IPv6 address never accepts IPv4, dual stack or not"
        );
        let dual_specific = Bind::V6 {
            addr: Ipv6Addr::LOCALHOST,
            v4_too: true,
        };
        assert!(!dual_specific.covers(LOOPBACK));
    }

    /// An IPv4-mapped IPv6 bind address is an IPv4 bind.
    #[test]
    fn an_ipv4_mapped_bind_address_is_an_ipv4_bind() {
        let mapped: Ipv6Addr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(Bind::v6(mapped, false), Bind::V4(Ipv4Addr::LOCALHOST));
        let unspecified_mapped: Ipv6Addr = "::ffff:0.0.0.0".parse().unwrap();
        assert_eq!(
            Bind::v6(unspecified_mapped, true),
            Bind::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            Bind::v6(Ipv6Addr::UNSPECIFIED, true),
            Bind::V6 {
                addr: Ipv6Addr::UNSPECIFIED,
                v4_too: true
            }
        );
        // the deprecated IPv4-compatible form is not the mapped one
        let compatible: Ipv6Addr = "::127.0.0.1".parse().unwrap();
        assert_eq!(
            Bind::v6(compatible, false),
            Bind::V6 {
                addr: compatible,
                v4_too: false
            }
        );
    }
}

/// The tests that need a real native watch: only on the platforms that have one. Elsewhere the fallback contract is tested instead.
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod native_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

    use super::*;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const LOOPBACK6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

    fn me() -> u32 {
        std::process::id()
    }

    fn watch(port: u16) -> ListenerWatch {
        ListenerWatch::establish(me(), port, LOOPBACK)
            .expect("this platform has a probe and this process can be inspected")
    }

    /// A real process that listens on `<host>:<ephemeral port>` over a socket family and `IPV6_V6ONLY` setting of the test's choosing, and
    /// closes or `shutdown`s the listening socket on request (what a misbehaving engine does).
    const LISTENER: &str = r#"
import socket, sys
host, family, v6only = sys.argv[1], sys.argv[2], sys.argv[3]
fam = socket.AF_INET6 if family == "6" else socket.AF_INET
s = socket.socket(fam, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
if family == "6" and v6only != "-":
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, int(v6only))
s.bind((host, 0))
s.listen(16)
print("listening %d" % s.getsockname()[1], flush=True)
for line in sys.stdin:
    command = line.strip()
    if command == "close":
        s.close()
        print("closed", flush=True)
    elif command == "shutdown":
        try:
            s.shutdown(socket.SHUT_RD)
            print("shutdown", flush=True)
        except OSError as error:
            print("refused %d" % error.errno, flush=True)
"#;

    /// A real listener in another process. `stdin`/`lines`/`tell` drive the Linux-only descriptor-kept case.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    struct Peer {
        child: Child,
        stdin: ChildStdin,
        lines: BufReader<ChildStdout>,
        port: u16,
    }

    impl Peer {
        /// `family` is `4` or `6`; `v6only` is `0`, `1` or `-` (leave the system default).
        fn start(host: &str, family: &str, v6only: &str) -> Option<Self> {
            let mut child = Command::new("/usr/bin/python3")
                .args(["-B", "-c", LISTENER, host, family, v6only])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            let mut lines = BufReader::new(child.stdout.take().unwrap());
            let mut first = String::new();
            lines.read_line(&mut first).unwrap();
            let Some(port) = first
                .trim()
                .strip_prefix("listening ")
                .and_then(|port| port.parse().ok())
            else {
                // It could not listen (no IPv6 here): do not leave the child behind.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            };
            Some(Self {
                child,
                stdin,
                lines,
                port,
            })
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        fn tell(&mut self, command: &str) -> String {
            writeln!(self.stdin, "{command}").unwrap();
            self.stdin.flush().unwrap();
            let mut answer = String::new();
            self.lines.read_line(&mut answer).unwrap();
            answer.trim().to_owned()
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Copy a long-running program to `to`. macOS: `/bin/sleep`. Linux: this test binary itself (run as the ignored `fixture_sleeper`
    /// test), because a distribution's coreutils may be a multi-call binary that misbehaves when copied and renamed (uutils on Ubuntu 26.04).
    fn copy_sleeper(to: &std::path::Path) {
        #[cfg(target_os = "linux")]
        std::fs::copy(std::env::current_exe().unwrap(), to).unwrap();
        #[cfg(not(target_os = "linux"))]
        copy_of("/bin/sleep", to);
    }

    fn spawn_sleep_from(path: &std::path::Path) -> Child {
        let mut command = Command::new(path);
        #[cfg(target_os = "linux")]
        command.args([
            "--exact",
            "native_tests::fixture_sleeper",
            "--ignored",
            "--test-threads=1",
        ]);
        #[cfg(not(target_os = "linux"))]
        command.arg("30");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Not a test: the long-running process `spawn_sleep_from` starts on Linux.
    #[test]
    #[ignore = "fixture process for the identity tests, started by them"]
    fn fixture_sleeper() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    fn copy_of(program: &str, to: &std::path::Path) {
        std::fs::copy(program, to).unwrap();
    }

    /// Fail with a clear message if the child has already exited: the identity of a dead process is `None`, which would otherwise surface
    /// as an unrelated `unwrap` failure (seen when a copied system binary was killed right after spawn on one host).
    fn assert_alive(child: &mut Child, when: &str) {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "the child exited ({status}) {when}: the fixture binary did not stay alive, so no identity can be read"
            );
        }
    }

    fn kill(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The identity is what the kernel runs, not a file at a path (Codex pre-review H2): the process keeps running after its executable was
    /// deleted, and it is still the same, readable identity.
    #[test]
    fn the_identity_survives_the_deletion_of_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("engine");
        copy_sleeper(&exe);
        let child = spawn_sleep_from(&exe);
        let before = executable_identity(child.id()).expect("a running process has an identity");
        std::fs::remove_file(&exe).unwrap();
        let after = executable_identity(child.id())
            .expect("the identity must not depend on the path still existing");
        assert_eq!(before, after);
        kill(child);
    }

    /// The identity is of the running image, not of whatever file is at its path now (H1): the file at the path being replaced while the
    /// process keeps running changes nothing about the process.
    #[test]
    fn replacing_the_file_does_not_change_the_identity_of_the_running_image() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("engine");
        let other = dir.path().join("other");
        copy_sleeper(&exe);
        copy_of("/bin/cat", &other);
        let mut child = spawn_sleep_from(&exe);
        assert_alive(&mut child, "before the first identity read");
        let before = executable_identity(child.id()).unwrap();
        std::fs::rename(&other, &exe).unwrap();
        assert_alive(&mut child, "before the identity read after the replacement");
        assert_eq!(executable_identity(child.id()).unwrap(), before);
        kill(child);
    }

    /// The same pid that `exec`s a different program has a different identity (H1), and one `exec` is observed in one read of the kernel's
    /// state.
    #[test]
    fn an_exec_into_another_program_changes_the_identity_of_the_same_pid() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read line; exec /bin/sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let before = executable_identity(pid).unwrap();
        assert_eq!(executable_identity(pid).unwrap(), before, "stable");
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "go").unwrap();
        stdin.flush().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if executable_identity(pid).is_some_and(|now| now != before) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the exec was never seen"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        kill(child);
    }

    /// Extended ACLs, read by the kernel API: no ACL, an entry for the owner only, and an entry that gives everyone write.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_acl_that_grants_others_write_is_seen_and_one_for_the_owner_is_not() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "x").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let owner = file.metadata().unwrap().uid();
        let chmod = |rule: &str| {
            assert!(
                Command::new("/bin/chmod")
                    .args(["+a", rule])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        assert_eq!(
            acl_grants_others_write(file.as_raw_fd(), owner),
            Some(false)
        );
        assert_eq!(acl_grants_others_write_at(&path, owner), Some(false));
        chmod("everyone deny write");
        assert_eq!(
            acl_grants_others_write(file.as_raw_fd(), owner),
            Some(false),
            "a deny entry grants nothing"
        );
        chmod("everyone allow read");
        assert_eq!(
            acl_grants_others_write(file.as_raw_fd(), owner),
            Some(false),
            "read is not write"
        );
        // (the earlier `deny write` for everyone comes first and refuses it: an ACL is evaluated in order)
        chmod("everyone allow write");
        assert_eq!(
            acl_grants_others_write(file.as_raw_fd(), owner),
            Some(false)
        );
        assert!(
            Command::new("/bin/chmod")
                .arg("-N")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        chmod("everyone allow write");
        assert_eq!(acl_grants_others_write(file.as_raw_fd(), owner), Some(true));
        assert_eq!(acl_grants_others_write_at(&path, owner), Some(true));
        // the same entry set seen as another owner's file: the "everyone" group is not the owner either way
        assert_eq!(
            acl_grants_others_write(file.as_raw_fd(), owner + 1),
            Some(true)
        );
    }

    /// A team-signed program (the Python that ships inside the oMLX app, when it is installed) is run from a copy that is then deleted: the
    /// kernel still names its team, which a path-based check (`codesign --verify <pid>` reads the file) cannot do any more. Skipped where the
    /// app is not installed.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_names_the_team_of_a_running_program_whose_executable_was_deleted() {
        let home =
            std::path::Path::new("/Applications/oMLX.app/Contents/Resources/Python/cpython-3.11");
        let python = home.join("bin/python3.11");
        if !python.exists() {
            eprintln!("SKIPPED: the oMLX app is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::os::unix::fs::symlink(home.join("lib"), dir.path().join("lib")).unwrap();
        let exe = dir.path().join("bin/py");
        std::fs::copy(&python, &exe).unwrap();
        let child = Command::new(&exe)
            .args(["-I", "-S", "-c", "import time; time.sleep(30)"])
            .env("PYTHONHOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // wait for it to be a running python (the exec of a fresh copy can take a moment)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let team = loop {
            if let Some(team) = kernel_signing_team(child.id()) {
                break team;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the kernel never named a team"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        std::fs::remove_file(&exe).unwrap();
        assert_eq!(kernel_signing_team(child.id()), Some(team.clone()));
        assert_eq!(team.len(), 10, "{team}");
        kill(child);
    }

    /// A program with no team (an ad-hoc signed test binary copy, a platform binary) is not given one.
    #[test]
    fn the_kernel_names_no_team_for_programs_that_have_none() {
        assert_eq!(kernel_signing_team(me()), None);
        let mut sleeper = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        assert_eq!(kernel_signing_team(sleeper.id()), None);
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    /// An inherit-only ACL entry (a template for what is created inside a directory) takes no part in access to the directory itself; an
    /// ordinary entry does; and an earlier deny for the same principal cancels a later allow.
    #[cfg(target_os = "macos")]
    #[test]
    fn inherit_only_and_denied_acl_entries_are_not_grants() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::open(dir.path()).unwrap();
        let owner = file.metadata().unwrap().uid();
        let chmod = |rule: &str| {
            assert!(
                Command::new("/bin/chmod")
                    .args(["+a", rule])
                    .arg(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        let grants = || acl_grants_others_write(file.as_raw_fd(), owner);
        chmod(
            "everyone allow add_file,add_subdirectory,file_inherit,directory_inherit,only_inherit",
        );
        assert_eq!(
            grants(),
            Some(false),
            "an inherit-only entry is not a grant"
        );
        chmod("group:staff allow add_file");
        assert_eq!(grants(), Some(true), "an ordinary entry is");
        // start over: deny first, then allow, for the same principal
        assert!(
            Command::new("/bin/chmod")
                .args(["-N"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        chmod("group:staff deny add_file");
        chmod("group:staff allow add_file");
        assert_eq!(
            grants(),
            Some(false),
            "an earlier deny cancels the later allow"
        );
        // the other way round the allow comes first and wins
        assert!(
            Command::new("/bin/chmod")
                .args(["-N"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        chmod("group:staff allow add_file");
        // (`chmod +a` alone sorts denies first; a fixed index keeps the allow in front)
        assert!(
            Command::new("/bin/chmod")
                .args(["+a#", "1", "group:staff deny add_file"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            grants(),
            Some(true),
            "an allow before the deny is effective"
        );
    }

    /// No such process: no identity (the caller's generation check says why).
    #[test]
    fn a_process_that_does_not_exist_has_no_identity() {
        assert!(executable_identity(i32::MAX as u32).is_none());
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
            watch.check_cached(),
            Some(Probe::Holds),
            "once located, the descriptor is asked about directly"
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

    /// Establishing a watch scans nothing: the first answer needs a scan, the later ones do not.
    #[test]
    fn establishing_is_cheap_and_the_first_check_locates_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let watch = watch(port);
        assert_eq!(
            watch.check_cached(),
            None,
            "nothing is known about the descriptor yet"
        );
        assert_eq!(watch.check(), Probe::Holds);
        assert_eq!(watch.check_cached(), Some(Probe::Holds));
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

    /// `::1` is a loopback address the client does not connect to (it connects to `127.0.0.1`): a listener there is not the engine for that
    /// dial address; for an IPv6 dial address it is.
    #[test]
    fn a_listener_on_the_ipv6_loopback_covers_an_ipv6_dial_address_only() {
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            return; // no IPv6 loopback on this machine
        };
        let port = listener.local_addr().unwrap().port();
        assert_eq!(watch(port).check(), Probe::NotHolding);
        let v6 = ListenerWatch::establish(me(), port, LOOPBACK6).unwrap();
        assert_eq!(v6.check(), Probe::Holds);
    }

    /// A watch for another dial address answers for **that** address: a specific IPv4 address other than the one the socket is bound to
    /// does not count.
    #[test]
    fn a_watch_is_about_its_dial_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let other = ListenerWatch::establish(me(), port, v4(127, 0, 0, 2)).unwrap();
        assert_eq!(other.check(), Probe::NotHolding);
        let wildcard = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = wildcard.local_addr().unwrap().port();
        let any = ListenerWatch::establish(me(), port, v4(127, 0, 0, 2)).unwrap();
        assert_eq!(
            any.check(),
            Probe::Holds,
            "a wildcard covers every IPv4 dial address"
        );
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// **Real IPv6-only and dual-stack wildcard listeners** (`IPV6_V6ONLY` set and not set), held by a real child process: an IPv6-only `::`
    /// socket is never an answer for an IPv4 dial address (nobody connecting to `127.0.0.1` reaches it: an IPv4 process may even bind the
    /// same port); a dual-stack one is — where the system reports that evidence (macOS does; Linux's `/proc` and `lsof` do not, so there
    /// it is only an answer for an IPv6 dial address). Both cover the IPv6 loopback.
    #[test]
    fn a_real_ipv6_only_listener_is_not_reachable_by_an_ipv4_dial_address_and_a_dual_stack_one_is_only_with_evidence()
     {
        let Some(only) = Peer::start("::", "6", "1") else {
            return; // no IPv6 on this machine
        };
        let v6only_v4 = ListenerWatch::establish(only.pid(), only.port, LOOPBACK).unwrap();
        assert_eq!(
            v6only_v4.check(),
            Probe::NotHolding,
            "IPV6_V6ONLY: IPv4 connections never reach it"
        );
        let v6only_v6 = ListenerWatch::establish(only.pid(), only.port, LOOPBACK6).unwrap();
        assert_eq!(v6only_v6.check(), Probe::Holds);

        let Some(dual) = Peer::start("::", "6", "0") else {
            return;
        };
        let dual_v6 = ListenerWatch::establish(dual.pid(), dual.port, LOOPBACK6).unwrap();
        assert_eq!(dual_v6.check(), Probe::Holds);
        let dual_v4 = ListenerWatch::establish(dual.pid(), dual.port, LOOPBACK).unwrap();
        let expected = if cfg!(target_os = "macos") {
            Probe::Holds
        } else {
            Probe::NotHolding
        };
        assert_eq!(
            dual_v4.check(),
            expected,
            "evidence of dual stack exists on macOS only"
        );
    }

    /// Another process (the parent: cargo, the shell) does not hold this process's port: it is either not holding it or not readable
    /// (another user's process), never a yes.
    #[test]
    fn another_process_does_not_hold_this_processs_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let parent = std::os::unix::process::parent_id();
        if let Some(watch) = ListenerWatch::establish(parent, port, LOOPBACK) {
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
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let watch = ListenerWatch::establish(pid, 1, LOOPBACK).unwrap();
        assert_eq!(watch.check(), Probe::NotHolding);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(watch.check(), Probe::NotHolding);
    }

    /// A process that has died but has not been reaped yet (a zombie) holds nothing: no watch is established for it.
    #[test]
    fn a_zombie_holds_nothing() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let watch = ListenerWatch::establish(pid, 1, LOOPBACK).unwrap();
        child.kill().unwrap(); // SIGKILL, not reaped: a zombie until `wait`
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ListenerWatch::establish(pid, 1, LOOPBACK).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "a dead, unreaped process is still treated as alive"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(watch.check(), Probe::NotHolding);
        child.wait().unwrap();
    }

    /// Requests are concurrent: many threads ask one watch at once. Every answer is right while the listener is open and while it is closed
    /// (the shared descriptor hint is only a hint: it is validated by every check, and no lock is held across a system call).
    #[test]
    fn concurrent_checks_agree_and_follow_the_listener_closing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let watch = std::sync::Arc::new(watch(port));
        let ask = |expected: Probe| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let watch = watch.clone();
                    std::thread::spawn(move || (0..2_000).all(|_| watch.check() == expected))
                })
                .collect();
            for thread in threads {
                assert!(
                    thread.join().unwrap(),
                    "a concurrent check said something other than {expected:?}"
                );
            }
        };
        ask(Probe::Holds);
        drop(listener);
        ask(Probe::NotHolding);
    }

    /// **A socket that stops listening while its descriptor stays open** (`shutdown(2)` on a listening socket; Linux allows it and leaves
    /// the LISTEN state): the very next check says it no longer holds the port. macOS refuses that call, so there the descriptor can only
    /// be closed (covered above).
    #[test]
    #[cfg(target_os = "linux")]
    fn a_listening_socket_that_leaves_listen_state_with_its_descriptor_kept_is_not_held() {
        let mut peer = Peer::start("127.0.0.1", "4", "-").unwrap();
        let watch = ListenerWatch::establish(peer.pid(), peer.port, LOOPBACK).unwrap();
        assert_eq!(watch.check(), Probe::Holds);
        assert_eq!(watch.check_cached(), Some(Probe::Holds));
        assert_eq!(peer.tell("shutdown"), "shutdown");
        assert_ne!(
            watch.check(),
            Probe::Holds,
            "the descriptor is still open but the socket no longer listens"
        );
        assert_eq!(watch.check(), Probe::NotHolding);
        assert_eq!(peer.tell("close"), "closed");
        assert_eq!(watch.check(), Probe::NotHolding);
    }

    /// Pid 0 and 1 are never an engine: no watch.
    #[test]
    fn pids_that_cannot_be_an_engine_have_no_watch() {
        assert!(ListenerWatch::establish(0, 80, LOOPBACK).is_none());
        assert!(ListenerWatch::establish(u32::MAX, 80, LOOPBACK).is_none());
    }

    /// Micro benchmark of the per-request check on the cheap path (the descriptor is known): p50 / p99 over many calls. Run with
    /// `cargo test -p idoris-sockprobe --release -- --ignored --nocapture`; the requirement is p99 < 5 ms with no child process.
    #[test]
    #[ignore = "micro benchmark"]
    fn micro_benchmark_of_the_cheap_path() {
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
            "sockprobe cheap path: n={} p50={:.1}us p90={:.1}us p99={:.1}us max={:.1}us",
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
        let watch = ListenerWatch::establish(pid, port, LOOPBACK);
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
        let other = ListenerWatch::establish(pid, port.wrapping_add(1), LOOPBACK).unwrap();
        eprintln!("another port: {:?}", other.check());
    }
}

/// On a platform without a native probe (not macOS, not Linux) nothing can be established and the caller falls back to something slower:
/// that is the contract, and it is what these tests pin (they run on every other Unix).
#[cfg(all(test, not(any(target_os = "macos", target_os = "linux"))))]
mod fallback_contract_tests {
    use super::*;

    #[test]
    fn no_watch_can_be_established_so_the_caller_falls_back() {
        let dial = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(ListenerWatch::establish(std::process::id(), 8088, dial).is_none());
        assert!(ListenerWatch::establish(0, 80, dial).is_none());
        assert!(ListenerWatch::establish(u32::MAX, 80, dial).is_none());
    }
}

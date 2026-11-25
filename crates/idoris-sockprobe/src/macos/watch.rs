//! macOS: libproc, **safe logic** on top of the system calls in `macos.rs` (the only file with `unsafe`). The system calls are behind
//! the [`Sys`] trait so that what this file decides — which failures mean "not holding" and which mean "cannot tell", how short and full
//! answers are retried — is tested against a scripted fake, not only against the kernel's good days.
//!
//! The rules (Codex focused review M10):
//!
//! - every call is **complete** (exactly the bytes asked for), **short** (some bytes: no `errno` to read: retried a bounded number of
//!   times, then "cannot tell") or **failed** (an `errno`);
//! - only "no such process" means the process is gone, only "bad file descriptor" / "not a socket" mean a descriptor is not the listening
//!   socket; **every other failure is [`Probe::Unavailable`]** (the caller asks the slow question), never a "no";
//! - the descriptor table is read until a buffer is not full (a full one may have been cut off), with a bounded number of attempts and a
//!   bounded size; a descriptor that cannot be asked about for any reason other than "closed" or "not a socket" makes the whole scan
//!   "cannot tell".

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::Libproc;
use crate::{Bind, Probe, Quick};

/// `SOCKINFO_TCP`: `soi_kind` of a TCP socket (the `tcp_sockinfo` member of the protocol union is the valid one).
pub(crate) const SOCKINFO_TCP: i32 = 2;
/// `TSI_S_LISTEN`: `tcpsi_state` of a listening socket.
pub(crate) const TSI_S_LISTEN: i32 = 1;
/// `INI_IPV4` (`insi_vflag`): the socket serves IPv4. A native IPv4 socket and a dual-stack IPv6 socket have it; an IPv6-only socket
/// (`INI_IPV6` alone) does not, and a client that connects to an IPv4 address never reaches it.
pub(crate) const INI_IPV4: u8 = 0x1;
/// `INI_IPV6` (`insi_vflag`): the socket is an IPv6 socket.
pub(crate) const INI_IPV6: u8 = 0x2;

/// How often a short answer is asked again before it counts as "cannot tell".
const SHORT_RETRIES: usize = 3;
/// How often the descriptor table is read again (full buffer, odd length) before it counts as "cannot tell".
const TABLE_TRIES: usize = 4;
/// The largest descriptor table that is read (entries). A process with more descriptors than this is "cannot tell", not "no".
const MAX_TABLE_ENTRIES: usize = 1 << 20;
/// Entries added to the size the kernel reports: descriptors opened between the two calls.
const TABLE_SLACK: usize = 32;

/// One libproc call's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply<T> {
    /// Exactly the bytes that were asked for.
    Complete(T),
    /// Some bytes, but not all (and so no `errno` to read).
    Short,
    /// A failure, with its `errno` (0: the call failed without saying why).
    Failed(i32),
}

/// One entry of a process's descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FdEntry {
    pub(crate) fd: i32,
    pub(crate) is_socket: bool,
}

/// A read of the descriptor table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FdTable {
    pub(crate) entries: Vec<FdEntry>,
    /// The buffer came back exactly full: there may be more entries than it could hold.
    pub(crate) full: bool,
}

/// What the kernel says about one socket descriptor, as plain data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SocketData {
    /// `soi_kind`: the protocol family of the socket info (TCP, UDP, unix, ...).
    pub(crate) kind: i32,
    /// `tcpsi_state` (0 unless the socket is TCP).
    pub(crate) tcp_state: i32,
    /// The local port, host order (0 unless the socket is TCP).
    pub(crate) port: u16,
    /// `insi_vflag`: which address families the socket serves.
    pub(crate) vflag: u8,
    /// The local address: IPv6 as is, IPv4 in the last four bytes.
    pub(crate) laddr: [u8; 16],
}

/// The system calls this file needs.
pub(crate) trait Sys {
    /// `proc_pidinfo(PROC_PIDTBSDINFO)`: the start time of a process, seconds and microseconds.
    fn start_time(&self, pid: i32) -> Reply<(u64, u64)>;
    /// `proc_pidinfo(PROC_PIDLISTFDS)` with no buffer: how many descriptor table entries the process has now.
    fn fd_table_len(&self, pid: i32) -> Reply<usize>;
    /// `proc_pidinfo(PROC_PIDLISTFDS)`: the descriptor table, at most `entries` entries.
    fn fd_table(&self, pid: i32, entries: usize) -> Reply<FdTable>;
    /// `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`: what one descriptor is, if it is a socket.
    fn socket_info(&self, pid: i32, fd: i32) -> Reply<SocketData>;
}

/// Why a question about a process or a descriptor could not be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// No such process.
    Gone,
    /// Not allowed to look (another user's process, a sandbox).
    Denied,
    /// Anything else, including an answer that stayed short.
    Other,
}

/// What an `errno` of a failed libproc call means for a process.
fn failure_of(errno: i32) -> Failure {
    match errno {
        libc::ESRCH => Failure::Gone,
        libc::EPERM | libc::EACCES => Failure::Denied,
        _ => Failure::Other,
    }
}

/// What one descriptor is, as far as this probe cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FdState {
    /// A TCP socket in the LISTEN state: its local port and what it is bound to.
    Listening { port: u16, bind: Bind },
    /// A socket that does not listen (a connected TCP socket, UDP, unix).
    NotListening,
    /// The descriptor is closed (`EBADF`): definitely not the listening socket.
    Closed,
    /// The descriptor is not a socket at all (`ENOTSOCK`).
    NotSocket,
}

/// What the kernel's answer says the descriptor is. A pure function of the data, so the cases a real socket cannot easily produce (other
/// socket kinds, other states, other address families) are tested with forged answers. An answer that is a listening TCP socket but
/// serves neither address family is not understood: "cannot tell".
fn classify(data: &SocketData) -> Result<FdState, Failure> {
    if data.kind != SOCKINFO_TCP || data.tcp_state != TSI_S_LISTEN {
        return Ok(FdState::NotListening);
    }
    let bind = if data.vflag & INI_IPV6 != 0 {
        // An IPv6 socket; it also accepts IPv4 exactly when the kernel says it serves IPv4 too (not `IPV6_V6ONLY`): evidence.
        Bind::v6(Ipv6Addr::from(data.laddr), data.vflag & INI_IPV4 != 0)
    } else if data.vflag & INI_IPV4 != 0 {
        Bind::V4(Ipv4Addr::new(
            data.laddr[12],
            data.laddr[13],
            data.laddr[14],
            data.laddr[15],
        ))
    } else {
        return Err(Failure::Other);
    };
    Ok(FdState::Listening {
        port: data.port,
        bind,
    })
}

/// The start time of `pid`, or why it cannot be had. A zombie is reported by libproc as `ESRCH`, so it is gone too.
fn process_start<S: Sys + ?Sized>(sys: &S, pid: i32) -> Result<(u64, u64), Failure> {
    for _ in 0..=SHORT_RETRIES {
        match sys.start_time(pid) {
            Reply::Complete(started) => return Ok(started),
            Reply::Short => {}
            Reply::Failed(errno) => return Err(failure_of(errno)),
        }
    }
    Err(Failure::Other)
}

/// What descriptor `fd` of `pid` is. Only a closed descriptor (`EBADF`) and a non-socket (`ENOTSOCK`) are answers; a process that is
/// gone, a refusal and any other failure (or an answer that stays short) are failures, never "not a listening socket".
fn describe<S: Sys + ?Sized>(sys: &S, pid: i32, fd: i32) -> Result<FdState, Failure> {
    for _ in 0..=SHORT_RETRIES {
        match sys.socket_info(pid, fd) {
            Reply::Complete(data) => return classify(&data),
            Reply::Short => {}
            Reply::Failed(errno) => {
                return match errno {
                    libc::EBADF => Ok(FdState::Closed),
                    libc::ENOTSOCK => Ok(FdState::NotSocket),
                    errno => Err(failure_of(errno)),
                };
            }
        }
    }
    Err(Failure::Other)
}

/// The whole descriptor table of `pid`: sized from the kernel's own count, read again with a bigger buffer when it came back full (it may
/// have been cut off) or with an odd length, a bounded number of times and up to a bounded size.
fn read_fd_table<S: Sys + ?Sized>(sys: &S, pid: i32) -> Result<Vec<FdEntry>, Failure> {
    let mut capacity: Option<usize> = None;
    for _ in 0..TABLE_TRIES {
        let entries = match capacity {
            Some(entries) => entries,
            None => match sys.fd_table_len(pid) {
                Reply::Complete(len) => len.saturating_add(TABLE_SLACK).min(MAX_TABLE_ENTRIES),
                Reply::Short => continue,
                Reply::Failed(errno) => return Err(failure_of(errno)),
            },
        };
        match sys.fd_table(pid, entries) {
            Reply::Complete(table) if !table.full => return Ok(table.entries),
            Reply::Complete(_) => {
                // Full: maybe cut off. A bigger buffer, unless it is already as big as we are willing to read.
                if entries >= MAX_TABLE_ENTRIES {
                    return Err(Failure::Other);
                }
                capacity = Some(entries.saturating_mul(2).min(MAX_TABLE_ENTRIES));
            }
            Reply::Short => {}
            Reply::Failed(errno) => return Err(failure_of(errno)),
        }
    }
    Err(Failure::Other)
}

/// Find the descriptor of `pid` that is a LISTEN socket on `port` bound to an address that covers `dial`: a scan of its descriptor table.
fn find_listener<S: Sys + ?Sized>(
    sys: &S,
    pid: i32,
    port: u16,
    dial: IpAddr,
) -> Result<Option<i32>, Failure> {
    for entry in read_fd_table(sys, pid)? {
        if !entry.is_socket {
            continue;
        }
        // Only "closed meanwhile" and "not a socket" are skipped; anything else leaves the whole answer unknown.
        if let FdState::Listening { port: bound, bind } = describe(sys, pid, entry.fd)?
            && bound == port
            && bind.covers(dial)
        {
            return Ok(Some(entry.fd));
        }
    }
    Ok(None)
}

/// The descriptor the listening socket had at the last check.
pub(crate) type Hint = i32;

/// The identity of the watched process: its start time (seconds and microseconds), pinned when the watch was established. Same pid with a
/// different start time is a different process.
#[derive(Debug)]
pub(crate) struct GenericWatch<S> {
    sys: S,
    pid: i32,
    port: u16,
    dial: IpAddr,
    started: (u64, u64),
}

pub(crate) type Watch = GenericWatch<Libproc>;

/// Pin the identity of `pid`: one system call. Nothing is scanned. Pid 0, 1 and anything that does not fit a pid are never an engine.
pub(crate) fn establish(pid: u32, port: u16, dial: IpAddr) -> Option<Watch> {
    establish_with(Libproc, pid, port, dial)
}

pub(super) fn process_generation(pid: u32) -> Option<String> {
    let (seconds, micros) = process_start(&Libproc, i32::try_from(pid).ok()?).ok()?;
    Some(format!("darwin:{seconds}.{micros:06}"))
}

fn establish_with<S: Sys>(sys: S, pid: u32, port: u16, dial: IpAddr) -> Option<GenericWatch<S>> {
    let raw = i32::try_from(pid).ok().filter(|raw| *raw >= 2)?;
    let started = process_start(&sys, raw).ok()?;
    Some(GenericWatch {
        sys,
        pid: raw,
        port,
        dial,
        started,
    })
}

impl<S: Sys> GenericWatch<S> {
    pub(crate) fn generation(&self) -> Option<String> {
        let (seconds, micros) = self.started;
        Some(format!("darwin:{seconds}.{micros:06}"))
    }

    /// Is it still the process the watch was established for? `Ok(())` yes; the probe to answer with otherwise.
    fn identity(&self, hint: Option<Hint>) -> Result<(), (Probe, Option<Hint>)> {
        match process_start(&self.sys, self.pid) {
            Ok(started) if started == self.started => Ok(()),
            // Another process under the same pid.
            Ok(_) => Err((Probe::NotHolding, None)),
            Err(Failure::Gone) => Err((Probe::NotHolding, None)),
            // Not allowed to look (another user's process), or an unexpected error: not a "no", the caller decides.
            Err(Failure::Denied | Failure::Other) => Err((Probe::Unavailable, hint)),
        }
    }

    /// The cheap check (see [`super::ListenerWatch::check_cached`]): the process is the same, and the remembered descriptor still is a
    /// listening socket on the port that the dial address reaches (the kernel reports the socket's *current* state and address).
    pub(crate) fn quick(&self, hint: Option<Hint>) -> Quick<Hint> {
        if let Err((probe, hint)) = self.identity(hint) {
            return Quick::Answer(probe, hint);
        }
        let Some(fd) = hint else {
            return Quick::Scan;
        };
        match describe(&self.sys, self.pid, fd) {
            Ok(FdState::Listening { port, bind })
                if port == self.port && bind.covers(self.dial) =>
            {
                Quick::Answer(Probe::Holds, Some(fd))
            }
            // Not the listening socket any more (closed, reused for something else, another port or address): look again.
            Ok(_) => Quick::Scan,
            Err(Failure::Gone) => Quick::Answer(Probe::NotHolding, None),
            Err(Failure::Denied | Failure::Other) => Quick::Answer(Probe::Unavailable, hint),
        }
    }

    /// The full check after the cheap one found nothing to go on: scan the descriptors.
    pub(crate) fn scan(&self, hint: Option<Hint>) -> (Probe, Option<Hint>) {
        if let Err(answer) = self.identity(hint) {
            return answer;
        }
        match find_listener(&self.sys, self.pid, self.port, self.dial) {
            Ok(Some(fd)) => self
                .identity(hint)
                .map_or_else(|answer| answer, |()| (Probe::Holds, Some(fd))),
            Ok(None) | Err(Failure::Gone) => (Probe::NotHolding, None),
            Err(Failure::Denied | Failure::Other) => (Probe::Unavailable, hint),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    use super::*;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const LOOPBACK6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

    fn v4_laddr(address: [u8; 4]) -> [u8; 16] {
        let mut bytes = [0_u8; 16];
        bytes[12..].copy_from_slice(&address);
        bytes
    }

    /// A kernel answer for a listening TCP socket of the given family flags, local address and port.
    fn listening(vflag: u8, laddr: [u8; 16], port: u16) -> SocketData {
        SocketData {
            kind: SOCKINFO_TCP,
            tcp_state: TSI_S_LISTEN,
            port,
            vflag,
            laddr,
        }
    }

    fn loopback_listener(port: u16) -> SocketData {
        listening(INI_IPV4, v4_laddr([127, 0, 0, 1]), port)
    }

    /// A scripted fake of the system calls: per call a queue of replies (the last repeats), a call counter for each.
    #[derive(Default)]
    struct Fake {
        start: Mutex<VecDeque<Reply<(u64, u64)>>>,
        len: Mutex<VecDeque<Reply<usize>>>,
        /// `fd_table` serves the first `entries` of this list; `full` is true when it had to cut it off.
        table: Mutex<Vec<FdEntry>>,
        table_failure: Mutex<VecDeque<Reply<FdTable>>>,
        sockets: Mutex<BTreeMap<i32, VecDeque<Reply<SocketData>>>>,
        socket_calls: Mutex<BTreeMap<i32, usize>>,
        table_reads: Mutex<Vec<usize>>,
    }

    fn pop<T: Clone>(queue: &Mutex<VecDeque<T>>) -> Option<T> {
        let mut queue = queue.lock().unwrap();
        if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().cloned()
        }
    }

    impl Fake {
        fn new() -> Self {
            let fake = Self::default();
            fake.start
                .lock()
                .unwrap()
                .push_back(Reply::Complete((1_700_000_000, 5)));
            fake
        }

        fn script_start(&self, replies: Vec<Reply<(u64, u64)>>) {
            *self.start.lock().unwrap() = replies.into();
        }

        fn script_len(&self, replies: Vec<Reply<usize>>) {
            *self.len.lock().unwrap() = replies.into();
        }

        fn script_table(&self, entries: Vec<FdEntry>) {
            let len = entries.len();
            *self.table.lock().unwrap() = entries;
            *self.len.lock().unwrap() = vec![Reply::Complete(len)].into();
        }

        fn script_table_replies(&self, replies: Vec<Reply<FdTable>>) {
            *self.table_failure.lock().unwrap() = replies.into();
        }

        fn script_socket(&self, fd: i32, replies: Vec<Reply<SocketData>>) {
            self.sockets.lock().unwrap().insert(fd, replies.into());
        }

        fn socket_calls(&self, fd: i32) -> usize {
            self.socket_calls
                .lock()
                .unwrap()
                .get(&fd)
                .copied()
                .unwrap_or(0)
        }

        fn table_reads(&self) -> Vec<usize> {
            self.table_reads.lock().unwrap().clone()
        }
    }

    impl Sys for Fake {
        fn start_time(&self, _pid: i32) -> Reply<(u64, u64)> {
            pop(&self.start).unwrap_or(Reply::Failed(libc::ESRCH))
        }

        fn fd_table_len(&self, _pid: i32) -> Reply<usize> {
            pop(&self.len).unwrap_or(Reply::Complete(0))
        }

        fn fd_table(&self, _pid: i32, entries: usize) -> Reply<FdTable> {
            self.table_reads.lock().unwrap().push(entries);
            if let Some(reply) = pop(&self.table_failure) {
                return reply;
            }
            let table = self.table.lock().unwrap();
            let served: Vec<FdEntry> = table.iter().take(entries).copied().collect();
            let full = table.len() >= entries;
            Reply::Complete(FdTable {
                entries: served,
                full,
            })
        }

        fn socket_info(&self, _pid: i32, fd: i32) -> Reply<SocketData> {
            *self.socket_calls.lock().unwrap().entry(fd).or_insert(0) += 1;
            let mut sockets = self.sockets.lock().unwrap();
            match sockets.get_mut(&fd) {
                Some(queue) if queue.len() > 1 => queue.pop_front().unwrap(),
                Some(queue) => queue.front().cloned().unwrap_or(Reply::Failed(libc::EBADF)),
                None => Reply::Failed(libc::EBADF),
            }
        }
    }

    fn socket(fd: i32) -> FdEntry {
        FdEntry {
            fd,
            is_socket: true,
        }
    }

    fn watch(fake: Fake) -> GenericWatch<Fake> {
        establish_with(fake, 4242, 8088, LOOPBACK).unwrap()
    }

    // ----- classify: the decision on forged kernel answers ---------------------------------------------------------------------------

    /// Only a TCP socket in LISTEN state is a listener; its port is the one in the answer; what it is bound to follows the address-family
    /// flags: an IPv6 socket accepts IPv4 only when the kernel says it serves IPv4 too.
    #[test]
    fn classify_reads_kind_state_port_and_bind_from_a_forged_answer() {
        let state = |data| classify(&data);
        let bind = |data| match state(data).unwrap() {
            FdState::Listening { port, bind } => Some((port, bind)),
            _ => None,
        };
        assert_eq!(
            bind(listening(INI_IPV4, v4_laddr([127, 0, 0, 1]), 8088)),
            Some((8088, Bind::V4(Ipv4Addr::LOCALHOST)))
        );
        assert_eq!(
            bind(listening(INI_IPV4, v4_laddr([0, 0, 0, 0]), 1)),
            Some((1, Bind::V4(Ipv4Addr::UNSPECIFIED)))
        );
        assert_eq!(
            bind(listening(INI_IPV4, v4_laddr([10, 1, 2, 3]), 2)),
            Some((2, Bind::V4(Ipv4Addr::new(10, 1, 2, 3))))
        );
        // IPv6-only wildcard (V6ONLY: IPV6 flag alone), dual-stack wildcard (both flags), IPv6 loopback
        let any6 = bind(listening(INI_IPV6, [0; 16], 3)).unwrap().1;
        assert_eq!(
            any6,
            Bind::V6 {
                addr: Ipv6Addr::UNSPECIFIED,
                v4_too: false
            }
        );
        assert!(!any6.covers(LOOPBACK) && any6.covers(LOOPBACK6));
        let dual = bind(listening(INI_IPV4 | INI_IPV6, [0; 16], 4)).unwrap().1;
        assert_eq!(
            dual,
            Bind::V6 {
                addr: Ipv6Addr::UNSPECIFIED,
                v4_too: true
            }
        );
        assert!(dual.covers(LOOPBACK) && dual.covers(LOOPBACK6));
        let mut loopback6 = [0_u8; 16];
        loopback6[15] = 1;
        let lo6 = bind(listening(INI_IPV6, loopback6, 5)).unwrap().1;
        assert!(!lo6.covers(LOOPBACK) && lo6.covers(LOOPBACK6));
        // The port is the one in the answer, not a neighbour's.
        assert_eq!(
            bind(listening(INI_IPV4, v4_laddr([127, 0, 0, 1]), 0x1F98))
                .unwrap()
                .0,
            0x1F98
        );
        // Not TCP (UDP is kind 1, a unix socket kind 3), not LISTEN (ESTABLISHED is 4): not a listener, whatever else the answer says.
        for kind in [0, 1, 3, 4, 5] {
            let data = SocketData {
                kind,
                ..listening(INI_IPV4, v4_laddr([127, 0, 0, 1]), 8088)
            };
            assert_eq!(state(data), Ok(FdState::NotListening), "kind {kind}");
        }
        for tcp_state in [0, 2, 3, 4, 5, 10] {
            let data = SocketData {
                tcp_state,
                ..listening(INI_IPV4, v4_laddr([127, 0, 0, 1]), 8088)
            };
            assert_eq!(state(data), Ok(FdState::NotListening), "state {tcp_state}");
        }
        // A listening TCP socket that serves neither family is not understood: cannot tell.
        assert_eq!(state(listening(0, [0; 16], 8088)), Err(Failure::Other));
    }

    #[test]
    fn errnos_mean_gone_denied_or_other() {
        assert_eq!(failure_of(libc::ESRCH), Failure::Gone);
        assert_eq!(failure_of(libc::EPERM), Failure::Denied);
        assert_eq!(failure_of(libc::EACCES), Failure::Denied);
        assert_eq!(failure_of(libc::EBADF), Failure::Other);
        assert_eq!(failure_of(libc::EINVAL), Failure::Other);
        assert_eq!(failure_of(libc::ENOMEM), Failure::Other);
        assert_eq!(failure_of(0), Failure::Other);
    }

    // ----- the identity ---------------------------------------------------------------------------------------------------------------

    #[test]
    fn the_identity_is_pinned_when_the_watch_is_established_and_compared_on_every_check() {
        let fake = Fake::new();
        fake.script_socket(5, vec![Reply::Complete(loopback_listener(8088))]);
        let watch = watch(fake);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::Holds, Some(5))
        ));
        // the same pid but another start time: not the engine (a recycled pid)
        watch
            .sys
            .script_start(vec![Reply::Complete((1_700_000_000, 6))]);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::NotHolding, None)
        ));
        watch
            .sys
            .script_start(vec![Reply::Complete((1_700_000_001, 5))]);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::NotHolding, None)
        ));
        assert_eq!(watch.scan(Some(5)), (Probe::NotHolding, None));
        // gone
        watch.sys.script_start(vec![Reply::Failed(libc::ESRCH)]);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::NotHolding, None)
        ));
        // not allowed, or an unexpected failure: cannot tell, never a no
        watch.sys.script_start(vec![Reply::Failed(libc::EPERM)]);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::Unavailable, Some(5))
        ));
        watch.sys.script_start(vec![Reply::Failed(libc::ENOMEM)]);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::Unavailable, Some(5))
        ));
        assert_eq!(watch.scan(Some(5)), (Probe::Unavailable, Some(5)));
        // a short answer is asked again a bounded number of times, then "cannot tell"
        watch.sys.script_start(vec![
            Reply::Short,
            Reply::Short,
            Reply::Complete((1_700_000_000, 5)),
        ]);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::Holds, Some(5))
        ));
        watch.sys.script_start(vec![Reply::Short]);
        assert!(matches!(
            watch.quick(Some(5)),
            Quick::Answer(Probe::Unavailable, Some(5))
        ));
        // pids that cannot be an engine
        assert!(establish_with(Fake::new(), 0, 80, LOOPBACK).is_none());
        assert!(establish_with(Fake::new(), 1, 80, LOOPBACK).is_none());
        assert!(establish_with(Fake::new(), u32::MAX, 80, LOOPBACK).is_none());
        // a process that cannot be asked at all cannot be watched
        let broken = Fake::new();
        broken.script_start(vec![Reply::Failed(libc::EPERM)]);
        assert!(establish_with(broken, 4242, 80, LOOPBACK).is_none());
    }

    // ----- the cheap check ------------------------------------------------------------------------------------------------------------

    /// The remembered descriptor is believed only while it still is a listening socket on this port at an address the dial address
    /// reaches; whatever else it has become, the answer is "look again" (a scan), never "holds".
    #[test]
    fn the_cheap_check_believes_the_descriptor_only_while_it_still_listens_where_the_dial_address_reaches()
     {
        let fake = Fake::new();
        let watch = watch(fake);
        let hold = |watch: &GenericWatch<Fake>| {
            matches!(watch.quick(Some(7)), Quick::Answer(Probe::Holds, Some(7)))
        };
        watch
            .sys
            .script_socket(7, vec![Reply::Complete(loopback_listener(8088))]);
        assert!(hold(&watch));
        // another port, another address, not listening, a UDP socket, closed, not a socket: look again
        for (label, reply) in [
            ("another port", Reply::Complete(loopback_listener(9000))),
            (
                "another address",
                Reply::Complete(listening(INI_IPV4, v4_laddr([10, 1, 2, 3]), 8088)),
            ),
            (
                "ipv6 loopback",
                Reply::Complete(listening(INI_IPV6, v6_loopback(), 8088)),
            ),
            (
                "ipv6 wildcard",
                Reply::Complete(listening(INI_IPV6, [0; 16], 8088)),
            ),
            (
                "connected",
                Reply::Complete(SocketData {
                    tcp_state: 4,
                    ..loopback_listener(8088)
                }),
            ),
            (
                "udp",
                Reply::Complete(SocketData {
                    kind: 1,
                    ..loopback_listener(8088)
                }),
            ),
            ("closed", Reply::Failed(libc::EBADF)),
            ("not a socket", Reply::Failed(libc::ENOTSOCK)),
        ] {
            watch.sys.script_socket(7, vec![reply]);
            assert!(matches!(watch.quick(Some(7)), Quick::Scan), "{label}");
        }
        // an IPv4 wildcard and a dual-stack IPv6 wildcard (evidence) reach 127.0.0.1
        watch.sys.script_socket(
            7,
            vec![Reply::Complete(listening(
                INI_IPV4,
                v4_laddr([0, 0, 0, 0]),
                8088,
            ))],
        );
        assert!(hold(&watch));
        watch.sys.script_socket(
            7,
            vec![Reply::Complete(listening(
                INI_IPV4 | INI_IPV6,
                [0; 16],
                8088,
            ))],
        );
        assert!(hold(&watch));
        // nothing is remembered: a scan is needed
        assert!(matches!(watch.quick(None), Quick::Scan));
    }

    fn v6_loopback() -> [u8; 16] {
        let mut bytes = [0_u8; 16];
        bytes[15] = 1;
        bytes
    }

    /// **M10.** What goes wrong when the kernel is asked about the remembered descriptor is "cannot tell" — a short answer, a refusal, any
    /// system error — never "not holding" (which rejects the request outright and skips the slow check), unless the process is gone.
    #[test]
    fn a_short_answer_or_an_odd_error_about_the_remembered_descriptor_is_cannot_tell_not_a_no() {
        let watch = watch(Fake::new());
        // short answers: retried a bounded number of times
        watch.sys.script_socket(
            7,
            vec![
                Reply::Short,
                Reply::Short,
                Reply::Complete(loopback_listener(8088)),
            ],
        );
        assert!(matches!(
            watch.quick(Some(7)),
            Quick::Answer(Probe::Holds, Some(7))
        ));
        assert_eq!(watch.sys.socket_calls(7), 3);
        watch.sys.script_socket(7, vec![Reply::Short]);
        assert!(matches!(
            watch.quick(Some(7)),
            Quick::Answer(Probe::Unavailable, Some(7))
        ));
        // errors that are neither "closed", "not a socket" nor "no such process"
        for errno in [
            libc::EINVAL,
            libc::ENOMEM,
            libc::EIO,
            libc::EPERM,
            libc::EACCES,
            0,
        ] {
            watch.sys.script_socket(7, vec![Reply::Failed(errno)]);
            assert!(
                matches!(
                    watch.quick(Some(7)),
                    Quick::Answer(Probe::Unavailable, Some(7))
                ),
                "errno {errno}"
            );
        }
        // the process vanished between the two calls
        watch.sys.script_socket(7, vec![Reply::Failed(libc::ESRCH)]);
        assert!(matches!(
            watch.quick(Some(7)),
            Quick::Answer(Probe::NotHolding, None)
        ));
    }

    // ----- the scan ------------------------------------------------------------------------------------------------------------------

    /// The scan finds the listener among the descriptors, skipping what is not a socket and what closed meanwhile.
    #[test]
    fn a_pid_reused_during_the_descriptor_scan_is_not_holding() {
        let fake = Fake::new();
        let watch = watch(fake);
        watch.sys.script_start(vec![
            Reply::Complete(watch.started),
            Reply::Complete((123, 456)),
        ]);
        watch.sys.script_table(vec![socket(7)]);
        watch
            .sys
            .script_socket(7, vec![Reply::Complete(loopback_listener(watch.port))]);
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
    }

    #[test]
    fn the_scan_finds_the_listener_and_skips_descriptors_that_cannot_be_it() {
        let fake = Fake::new();
        fake.script_table(vec![
            FdEntry {
                fd: 0,
                is_socket: false,
            },
            socket(3),
            socket(4),
            socket(5),
            socket(6),
            socket(8),
        ]);
        fake.script_socket(3, vec![Reply::Failed(libc::EBADF)]); // closed between the listing and the question
        fake.script_socket(4, vec![Reply::Failed(libc::ENOTSOCK)]);
        fake.script_socket(
            5,
            vec![Reply::Complete(SocketData {
                tcp_state: 4,
                ..loopback_listener(8088)
            })],
        ); // connected
        fake.script_socket(6, vec![Reply::Complete(loopback_listener(9000))]); // another port
        fake.script_socket(8, vec![Reply::Complete(loopback_listener(8088))]);
        let watch = watch(fake);
        assert_eq!(watch.scan(None), (Probe::Holds, Some(8)));
        // nothing matches: not holding
        watch
            .sys
            .script_socket(8, vec![Reply::Complete(loopback_listener(9001))]);
        assert_eq!(watch.scan(Some(8)), (Probe::NotHolding, None));
        // an IPv6-only wildcard is not the answer for an IPv4 dial address, a dual-stack one is
        watch
            .sys
            .script_socket(8, vec![Reply::Complete(listening(INI_IPV6, [0; 16], 8088))]);
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
        watch.sys.script_socket(
            8,
            vec![Reply::Complete(listening(
                INI_IPV4 | INI_IPV6,
                [0; 16],
                8088,
            ))],
        );
        assert_eq!(watch.scan(None), (Probe::Holds, Some(8)));
    }

    /// **M10.** A scan that could not ask about one descriptor for any reason but "closed" or "not a socket" does not know whether that
    /// one was the listener: "cannot tell", never "not holding". And a table that cannot be read, or stays odd, is the same.
    #[test]
    fn a_scan_that_could_not_ask_about_a_descriptor_does_not_say_no() {
        let fake = Fake::new();
        fake.script_table(vec![socket(3), socket(4)]);
        fake.script_socket(3, vec![Reply::Complete(loopback_listener(9000))]);
        fake.script_socket(4, vec![Reply::Failed(libc::EPERM)]);
        let watch = watch(fake);
        assert_eq!(
            watch.scan(None),
            (Probe::Unavailable, None),
            "a refusal on one descriptor"
        );
        for errno in [libc::EINVAL, libc::ENOMEM, libc::EIO] {
            watch.sys.script_socket(4, vec![Reply::Failed(errno)]);
            assert_eq!(
                watch.scan(None),
                (Probe::Unavailable, None),
                "errno {errno}"
            );
        }
        watch.sys.script_socket(4, vec![Reply::Short]);
        assert_eq!(
            watch.scan(None),
            (Probe::Unavailable, None),
            "an answer that stays short"
        );
        // but the listener found before the unreadable one still holds
        watch
            .sys
            .script_socket(3, vec![Reply::Complete(loopback_listener(8088))]);
        assert_eq!(watch.scan(None), (Probe::Holds, Some(3)));
        // the process vanished during the scan: gone
        watch.sys.script_socket(3, vec![Reply::Failed(libc::ESRCH)]);
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
    }

    /// **M10.** The descriptor table: a buffer that comes back full is read again with a bigger one (the listener may be beyond the first
    /// buffer); an odd length is read again; a table that cannot be read or stays odd is "cannot tell"; the size is bounded.
    #[test]
    fn the_descriptor_table_is_read_again_when_it_may_have_been_cut_off() {
        // 100 descriptors, the listener the last one; the kernel's count is stale (the process opened more since)
        let fake = Fake::new();
        let mut entries: Vec<FdEntry> = (0..99)
            .map(|fd| FdEntry {
                fd,
                is_socket: false,
            })
            .collect();
        entries.push(socket(99));
        fake.script_table(entries);
        fake.script_len(vec![Reply::Complete(10)]);
        fake.script_socket(99, vec![Reply::Complete(loopback_listener(8088))]);
        let watch = watch(fake);
        assert_eq!(watch.scan(None), (Probe::Holds, Some(99)));
        let reads = watch.sys.table_reads();
        assert_eq!(
            reads[0],
            10 + TABLE_SLACK,
            "sized from the kernel's count plus slack"
        );
        assert!(
            reads.len() >= 2 && reads[1] > reads[0],
            "a full buffer is read again with a bigger one: {reads:?}"
        );

        // odd lengths and failures
        let odd = watch.sys;
        odd.table_reads.lock().unwrap().clear();
        odd.script_table_replies(vec![Reply::Short, Reply::Short, Reply::Short, Reply::Short]);
        let watch = establish_with(odd, 4242, 8088, LOOPBACK).unwrap();
        assert_eq!(watch.scan(None), (Probe::Unavailable, None), "stays odd");
        watch
            .sys
            .script_table_replies(vec![Reply::Failed(libc::ENOMEM)]);
        assert_eq!(watch.scan(None), (Probe::Unavailable, None));
        watch
            .sys
            .script_table_replies(vec![Reply::Failed(libc::EPERM)]);
        assert_eq!(watch.scan(None), (Probe::Unavailable, None));
        watch
            .sys
            .script_table_replies(vec![Reply::Failed(libc::ESRCH)]);
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
        watch.sys.script_table_replies(vec![
            Reply::Short,
            Reply::Complete(FdTable {
                entries: vec![socket(99)],
                full: false,
            }),
        ]);
        assert_eq!(
            watch.scan(None),
            (Probe::Holds, Some(99)),
            "an odd length is read again"
        );
        // the size of the table is not trusted: the kernel's count failing is a failure, an absurd count is bounded
        watch.sys.script_table_replies(Vec::new());
        watch.sys.script_len(vec![Reply::Failed(libc::EPERM)]);
        assert_eq!(watch.scan(None), (Probe::Unavailable, None));
        watch.sys.script_len(vec![Reply::Failed(libc::ESRCH)]);
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
        watch.sys.script_len(vec![Reply::Complete(usize::MAX)]);
        watch.sys.table_reads.lock().unwrap().clear();
        let _ = watch.scan(None);
        assert!(
            watch
                .sys
                .table_reads()
                .iter()
                .all(|entries| *entries <= MAX_TABLE_ENTRIES),
            "the buffer is bounded: {:?}",
            watch.sys.table_reads()
        );
    }

    /// At the largest size that is read, a table that still comes back full is given up on at once: one read at that size, not the same
    /// read again and again.
    #[test]
    fn a_table_that_is_full_at_the_largest_size_is_given_up_on_at_once() {
        let fake = Fake::new();
        fake.script_len(vec![Reply::Complete(usize::MAX)]);
        fake.script_table_replies(vec![Reply::Complete(FdTable {
            entries: Vec::new(),
            full: true,
        })]);
        assert_eq!(read_fd_table(&fake, 4242), Err(Failure::Other));
        assert_eq!(fake.table_reads(), vec![MAX_TABLE_ENTRIES]);
    }

    /// A process whose table is bigger than the bound is "cannot tell", not "does not hold".
    #[test]
    fn a_table_that_never_stops_coming_back_full_is_cannot_tell() {
        let fake = Fake::new();
        fake.script_table_replies(vec![Reply::Complete(FdTable {
            entries: vec![socket(3)],
            full: true,
        })]);
        fake.script_socket(3, vec![Reply::Complete(loopback_listener(9000))]);
        let watch = watch(fake);
        assert_eq!(watch.scan(None), (Probe::Unavailable, None));
        assert!(watch.sys.table_reads().len() <= TABLE_TRIES);
    }
}

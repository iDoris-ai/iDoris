//! macOS: libproc. **This file holds the only `unsafe` code of the workspace** (see the crate docs for why it has to).
//!
//! `proc_pidinfo` / `proc_pidfdinfo` are plain system calls that fill a caller-provided buffer. Every call here passes a pointer to a
//! zero-initialised, correctly aligned plain-old-data struct (or a `Vec` of them) together with its exact size, and reads the struct only
//! after the call reported that many bytes written. The socket-info structs below mirror `sys/proc_info.h` (`libc` has the process and
//! descriptor structs, not these); their sizes and the offsets we read are asserted at compile time against what the kernel header defines
//! (`PROC_PIDFDSOCKETINFO_SIZE` is 792 bytes), and the tests below run them against real sockets.
#![allow(unsafe_code)]

use std::io;
use std::mem::{offset_of, size_of, zeroed};
use std::os::raw::c_void;

use libc::{
    PROC_PIDLISTFDS, PROC_PIDTBSDINFO, PROX_FDTYPE_SOCKET, c_int, proc_bsdinfo, proc_fdinfo,
    proc_pidfdinfo, proc_pidinfo,
};

use super::Probe;

/// `PROC_PIDFDSOCKETINFO` (sys/proc_info.h): the flavor of `proc_pidfdinfo` that returns a `struct socket_fdinfo`.
const PROC_PIDFDSOCKETINFO: c_int = 3;
/// `SOCKINFO_TCP`: `soi_kind` of a TCP socket (the `tcp_sockinfo` member of the protocol union is the valid one).
const SOCKINFO_TCP: i32 = 2;
/// `TSI_S_LISTEN`: `tcpsi_state` of a listening socket.
const TSI_S_LISTEN: i32 = 1;
/// `INI_IPV4` (`insi_vflag`): the socket serves IPv4. A native IPv4 socket and a dual-stack IPv6 socket have it; an IPv6-only socket
/// (`INI_IPV6` alone) does not, and a client that connects to `127.0.0.1` never reaches it.
const INI_IPV4: u8 = 0x1;
/// `SZOMB`: a zombie (dead, not yet reaped): it holds nothing.
const SZOMB: u32 = 5;

// ----- `struct socket_fdinfo` and what it contains (sys/proc_info.h) -----------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VinfoStat {
    vst_dev: u32,
    vst_mode: u16,
    vst_nlink: u16,
    vst_ino: u64,
    vst_uid: u32,
    vst_gid: u32,
    vst_atime: i64,
    vst_atimensec: i64,
    vst_mtime: i64,
    vst_mtimensec: i64,
    vst_ctime: i64,
    vst_ctimensec: i64,
    vst_birthtime: i64,
    vst_birthtimensec: i64,
    vst_size: i64,
    vst_blocks: i64,
    vst_blksize: i32,
    vst_flags: u32,
    vst_gen: u32,
    vst_rdev: u32,
    vst_qspare: [i64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockbufInfo {
    sbi_cc: u32,
    sbi_hiwat: u32,
    sbi_mbcnt: u32,
    sbi_mbmax: u32,
    sbi_lowat: u32,
    sbi_flags: i16,
    sbi_timeo: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct In6Extra {
    in6_hlim: u8,
    in6_cksum: i32,
    in6_ifindex: u16,
    in6_hops: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InSockInfo {
    insi_fport: i32,
    /// The local port: the 16-bit network-order port widened to an `int`.
    insi_lport: i32,
    insi_gencnt: u64,
    insi_flags: u32,
    insi_flow: u32,
    insi_vflag: u8,
    insi_ip_ttl: u8,
    rfu_1: u32,
    insi_faddr: [u8; 16],
    /// The local address: for IPv4 the address is in the **last four bytes** (`in4in6_addr`), for IPv6 all sixteen.
    insi_laddr: [u8; 16],
    insi_v4_tos: u8,
    insi_v6: In6Extra,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TcpSockInfo {
    tcpsi_ini: InSockInfo,
    tcpsi_state: i32,
    tcpsi_timer: [i32; 4],
    tcpsi_mss: i32,
    tcpsi_flags: u32,
    rfu_1: u32,
    tcpsi_tp: u64,
}

/// `soi_proto`: a union in C. Only the TCP member is read; the padding member makes the union as large as the biggest C member
/// (`un_sockinfo`, 528 bytes), so the whole `socket_fdinfo` is the 792 bytes the kernel writes.
#[repr(C)]
#[derive(Clone, Copy)]
union ProtoUnion {
    tcp: TcpSockInfo,
    raw: [u64; 66],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SocketInfo {
    soi_stat: VinfoStat,
    soi_so: u64,
    soi_pcb: u64,
    soi_type: i32,
    soi_protocol: i32,
    soi_family: i32,
    soi_options: i16,
    soi_linger: i16,
    soi_state: i16,
    soi_qlen: i16,
    soi_incqlen: i16,
    soi_qlimit: i16,
    soi_timeo: i16,
    soi_error: u16,
    soi_oobmark: u32,
    soi_rcv: SockbufInfo,
    soi_snd: SockbufInfo,
    soi_kind: i32,
    rfu_1: u32,
    soi_proto: ProtoUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SocketFdInfo {
    pfi: ProcFileInfo,
    psi: SocketInfo,
}

// The layout the kernel documents (computed from sys/proc_info.h). A mistake in the mirrored structs fails the build here, not at run time.
const _: () = assert!(size_of::<VinfoStat>() == 136);
const _: () = assert!(size_of::<SockbufInfo>() == 24);
const _: () = assert!(size_of::<InSockInfo>() == 80);
const _: () = assert!(size_of::<TcpSockInfo>() == 120);
const _: () = assert!(size_of::<ProtoUnion>() == 528);
const _: () = assert!(size_of::<SocketInfo>() == 768);
const _: () = assert!(size_of::<SocketFdInfo>() == 792);
const _: () = assert!(offset_of!(SocketFdInfo, psi) == 24);
const _: () = assert!(offset_of!(SocketInfo, soi_protocol) == 156);
const _: () = assert!(offset_of!(SocketInfo, soi_kind) == 232);
const _: () = assert!(offset_of!(SocketInfo, soi_proto) == 240);
const _: () = assert!(offset_of!(InSockInfo, insi_lport) == 4);
const _: () = assert!(offset_of!(InSockInfo, insi_vflag) == 24);
const _: () = assert!(offset_of!(InSockInfo, insi_laddr) == 48);
const _: () = assert!(offset_of!(TcpSockInfo, tcpsi_state) == 80);

// ----- what the watch remembers --------------------------------------------------------------------------------------------------------

/// The identity of the watched process: its start time (seconds and microseconds), pinned when the watch was established. Same pid with a
/// different start time is a different process.
#[derive(Debug)]
pub(crate) struct Watch {
    pid: i32,
    port: u16,
    started: (u64, u64),
}

/// The descriptor the listening socket had at the last check.
pub(crate) type Hint = i32;

/// What went wrong asking about one process or one descriptor.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The process (or, for a descriptor, the descriptor) does not exist any more.
    Gone,
    /// Not allowed to look (another user's process, a sandbox).
    Denied,
    /// Anything else.
    Other,
}

fn failure_of_errno() -> Failure {
    failure_of(io::Error::last_os_error().raw_os_error())
}

/// What an errno of a failed libproc call means: no such process, not allowed to look, or anything else.
fn failure_of(errno: Option<i32>) -> Failure {
    match errno {
        Some(libc::ESRCH) => Failure::Gone,
        Some(libc::EPERM | libc::EACCES) => Failure::Denied,
        _ => Failure::Other,
    }
}

/// The start time of `pid`, or why it cannot be had. A zombie is gone.
fn process_start(pid: i32) -> Result<(u64, u64), Failure> {
    // SAFETY: `proc_bsdinfo` is plain old data; the all-zero value is valid.
    let mut info: proc_bsdinfo = unsafe { zeroed() };
    let size = size_of::<proc_bsdinfo>() as c_int;
    // SAFETY: the pointer is to `info`, `size` is its size; `proc_pidinfo` writes at most `size` bytes and returns how many.
    let written = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut proc_bsdinfo).cast::<c_void>(),
            size,
        )
    };
    if written == size {
        if info.pbi_status == SZOMB {
            return Err(Failure::Gone);
        }
        Ok((info.pbi_start_tvsec, info.pbi_start_tvusec))
    } else if written <= 0 {
        Err(failure_of_errno())
    } else {
        Err(Failure::Other)
    }
}

/// What the descriptor `fd` of `pid` is, as far as this probe cares.
#[derive(Debug, PartialEq, Eq)]
enum Descriptor {
    /// A TCP socket in the LISTEN state: its local port, and whether it is bound to an address the client connects to.
    Listening { port: u16, target: bool },
    /// Anything else (a connected socket, a UDP socket, a file, a descriptor that is not there any more).
    Other,
}

fn describe_descriptor(pid: i32, fd: i32) -> Result<Descriptor, Failure> {
    // SAFETY: `SocketFdInfo` is plain old data (integers, byte arrays, a union of such); the all-zero value is valid.
    let mut info: SocketFdInfo = unsafe { zeroed() };
    let size = size_of::<SocketFdInfo>() as c_int;
    // SAFETY: the pointer is to `info`, `size` is its size; `proc_pidfdinfo` writes at most `size` bytes and returns how many.
    let written = unsafe {
        proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDSOCKETINFO,
            (&mut info as *mut SocketFdInfo).cast::<c_void>(),
            size,
        )
    };
    if written != size {
        // A closed descriptor, or one that is not a socket: errno says so (EBADF / ENOENT / EINVAL); only "the process is gone" and "not
        // allowed" are different from "not a listening socket".
        return match failure_of_errno() {
            Failure::Other => Ok(Descriptor::Other),
            failure => Err(failure),
        };
    }
    Ok(classify(&info))
}

/// What the kernel's answer says the descriptor is. A pure function of the struct, so the cases a real socket cannot easily produce (other
/// socket kinds, other states, other addresses) are tested with forged answers.
fn classify(info: &SocketFdInfo) -> Descriptor {
    let socket = &info.psi;
    if socket.soi_kind != SOCKINFO_TCP {
        return Descriptor::Other;
    }
    // SAFETY: `soi_kind` says the TCP member of the union is the one the kernel filled in; every bit pattern is a valid `TcpSockInfo`.
    let tcp = unsafe { socket.soi_proto.tcp };
    if tcp.tcpsi_state != TSI_S_LISTEN {
        return Descriptor::Other;
    }
    let ini = &tcp.tcpsi_ini;
    Descriptor::Listening {
        port: u16::from_be(ini.insi_lport as u16),
        target: is_target_address(ini),
    }
}

/// Is the socket bound to an address a client that connects to `127.0.0.1` reaches: the IPv4 loopback address or the IPv4 wildcard, or an
/// IPv6 dual-stack wildcard (which has the IPv4 flag too). `::1` and an IPv6-only wildcard are not.
fn is_target_address(ini: &InSockInfo) -> bool {
    if ini.insi_vflag & INI_IPV4 != 0 {
        let v4 = [
            ini.insi_laddr[12],
            ini.insi_laddr[13],
            ini.insi_laddr[14],
            ini.insi_laddr[15],
        ];
        return v4 == [127, 0, 0, 1] || v4 == [0, 0, 0, 0];
    }
    // An IPv6-only socket is not reachable through 127.0.0.1.
    false
}

/// Find the descriptor of `pid` that is a LISTEN socket on `port` at a target address: a scan of its descriptor table.
fn find_listener(pid: i32, port: u16) -> Result<Option<i32>, Failure> {
    let entry = size_of::<proc_fdinfo>();
    // libproc reports a failure as 0 with `errno` set, which is also what an empty table looks like: start from a clean `errno`.
    // SAFETY: `__error` returns the address of this thread's `errno`.
    unsafe { *libc::__error() = 0 };
    // SAFETY: a null buffer with size 0 asks for the number of bytes needed.
    let needed = unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed < 0 {
        return Err(failure_of_errno());
    }
    if needed == 0 {
        // No descriptors (or the call could not tell): `errno` separates a vanished process from an empty table.
        return match failure_of_errno() {
            Failure::Other => Ok(None),
            failure => Err(failure),
        };
    }
    // Room for descriptors opened between the two calls.
    let capacity = needed as usize / entry + 32;
    // SAFETY: `proc_fdinfo` is plain old data; the all-zero value is valid.
    let mut table: Vec<proc_fdinfo> = vec![unsafe { zeroed() }; capacity];
    // SAFETY: the pointer is to the vector's storage, the size is exactly its length in bytes.
    let written = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDLISTFDS,
            0,
            table.as_mut_ptr().cast::<c_void>(),
            (capacity * entry) as c_int,
        )
    };
    if written <= 0 {
        return Err(failure_of_errno());
    }
    let count = (written as usize / entry).min(capacity);
    for descriptor in &table[..count] {
        if descriptor.proc_fdtype != PROX_FDTYPE_SOCKET as u32 {
            continue;
        }
        match describe_descriptor(pid, descriptor.proc_fd) {
            Ok(Descriptor::Listening {
                port: bound,
                target,
            }) if bound == port && target => {
                return Ok(Some(descriptor.proc_fd));
            }
            Ok(_) => {}
            // A descriptor closed between the listing and the question: not the one.
            Err(Failure::Other) => {}
            Err(failure) => return Err(failure),
        }
    }
    Ok(None)
}

/// Pin the identity of `pid` and look for its listening socket on `port` once.
pub(crate) fn establish(pid: u32, port: u16) -> Option<(Watch, Option<Hint>)> {
    // Pid 0, 1 and anything that does not fit a pid are never an engine (`kill(0, 0)` would even probe a whole process group).
    let raw = i32::try_from(pid).ok().filter(|raw| *raw >= 2)?;
    let started = process_start(raw).ok()?;
    let hint = find_listener(raw, port).ok().flatten();
    Some((
        Watch {
            pid: raw,
            port,
            started,
        },
        hint,
    ))
}

impl Watch {
    pub(crate) fn check(&self, hint: Option<Hint>) -> (Probe, Option<Hint>) {
        match self.find(hint) {
            Ok(Some(fd)) => (Probe::Holds, Some(fd)),
            Ok(None) | Err(Failure::Gone) => (Probe::NotHolding, None),
            // Not allowed to look (another user's process), or an unexpected error: not a "no", the caller decides.
            Err(Failure::Denied | Failure::Other) => (Probe::Unavailable, hint),
        }
    }

    /// The descriptor of the process that is a LISTEN socket on the port at a target address, if there is one.
    fn find(&self, hint: Option<Hint>) -> Result<Option<Hint>, Failure> {
        // 1. Is it still the process we registered? (Another process under the same pid is not.)
        if process_start(self.pid)? != self.started {
            return Ok(None);
        }
        // 2. The fast path: the descriptor the socket had last time. Believed only if it still is that listening socket.
        if let Some(fd) = hint
            && let Descriptor::Listening { port, target } = describe_descriptor(self.pid, fd)?
            && port == self.port
            && target
        {
            return Ok(Some(fd));
        }
        // 3. The slow path: scan the descriptors.
        find_listener(self.pid, self.port)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    use super::*;

    /// The mirrored struct reads the real kernel answer: the port, the state and the address of a real listener of this process.
    #[test]
    fn the_mirrored_layout_reads_a_real_listening_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        match describe_descriptor(me, listener.as_raw_fd()).unwrap() {
            Descriptor::Listening { port: seen, target } => {
                assert_eq!(seen, port);
                assert!(target);
            }
            Descriptor::Other => panic!("a listening socket was not recognised"),
        }
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(matches!(
            describe_descriptor(me, client.as_raw_fd()).unwrap(),
            Descriptor::Other
        ));
        // A descriptor that is not a socket at all.
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(matches!(
            describe_descriptor(me, file.as_raw_fd()).unwrap(),
            Descriptor::Other
        ));
        // A descriptor that is not open.
        assert!(matches!(
            describe_descriptor(me, 9_999).unwrap(),
            Descriptor::Other
        ));
    }

    /// A kernel answer with the given socket kind, TCP state, address family flags, local address and port.
    fn forged(kind: i32, state: i32, vflag: u8, laddr: [u8; 16], port: u16) -> SocketFdInfo {
        // SAFETY: plain old data; the all-zero value is valid.
        let mut tcp: TcpSockInfo = unsafe { zeroed() };
        tcp.tcpsi_state = state;
        tcp.tcpsi_ini.insi_vflag = vflag;
        tcp.tcpsi_ini.insi_laddr = laddr;
        tcp.tcpsi_ini.insi_lport = i32::from(port.to_be());
        // SAFETY: as above.
        let mut info: SocketFdInfo = unsafe { zeroed() };
        info.psi.soi_kind = kind;
        info.psi.soi_proto = ProtoUnion { tcp };
        info
    }

    fn v4(address: [u8; 4]) -> [u8; 16] {
        let mut bytes = [0_u8; 16];
        bytes[12..].copy_from_slice(&address);
        bytes
    }

    /// The decision on forged kernel answers: only a TCP socket in LISTEN state is a listener, its port is read from the network-order
    /// field, and it is "on the target address" only when a client connecting to `127.0.0.1` reaches it.
    #[test]
    fn classify_reads_kind_state_port_and_address_from_a_forged_answer() {
        let listening = |laddr, vflag| forged(SOCKINFO_TCP, TSI_S_LISTEN, vflag, laddr, 8088);
        let target = |info: &SocketFdInfo| {
            matches!(
                classify(info),
                Descriptor::Listening {
                    port: 8088,
                    target: true
                }
            )
        };
        let elsewhere = |info: &SocketFdInfo| {
            matches!(
                classify(info),
                Descriptor::Listening {
                    port: 8088,
                    target: false
                }
            )
        };
        // IPv4 loopback and wildcard are targets; another IPv4 address is not.
        assert!(target(&listening(v4([127, 0, 0, 1]), INI_IPV4)));
        assert!(target(&listening(v4([0, 0, 0, 0]), INI_IPV4)));
        assert!(elsewhere(&listening(v4([10, 1, 2, 3]), INI_IPV4)));
        assert!(elsewhere(&listening(v4([127, 0, 0, 2]), INI_IPV4)));
        // IPv6: a dual-stack wildcard (it also serves IPv4) is a target, an IPv6-only socket (`::1`, or the wildcard with V6ONLY) is not.
        assert!(target(&listening([0; 16], INI_IPV4 | 0x2)));
        assert!(elsewhere(&listening([0; 16], 0x2)));
        let mut loopback6 = [0_u8; 16];
        loopback6[15] = 1;
        assert!(elsewhere(&listening(loopback6, 0x2)));
        // The port is the one in the answer, not a neighbour's.
        assert_eq!(
            classify(&forged(
                SOCKINFO_TCP,
                TSI_S_LISTEN,
                INI_IPV4,
                v4([127, 0, 0, 1]),
                0x1F98
            )),
            Descriptor::Listening {
                port: 0x1F98,
                target: true
            }
        );
        assert_ne!(
            classify(&forged(
                SOCKINFO_TCP,
                TSI_S_LISTEN,
                INI_IPV4,
                v4([127, 0, 0, 1]),
                0x981F
            )),
            Descriptor::Listening {
                port: 0x1F98,
                target: true
            }
        );
        // Not TCP (UDP is kind 1, a unix socket kind 3), not LISTEN (ESTABLISHED is 4): not a listener, whatever else the answer says.
        for kind in [0, 1, 3, 4, 5] {
            assert_eq!(
                classify(&forged(
                    kind,
                    TSI_S_LISTEN,
                    INI_IPV4,
                    v4([127, 0, 0, 1]),
                    8088
                )),
                Descriptor::Other,
                "kind {kind}"
            );
        }
        for state in [0, 2, 3, 4, 5, 10] {
            assert_eq!(
                classify(&forged(
                    SOCKINFO_TCP,
                    state,
                    INI_IPV4,
                    v4([127, 0, 0, 1]),
                    8088
                )),
                Descriptor::Other,
                "state {state}"
            );
        }
    }

    /// A hint that names a listener of this process on **another port**, or on **an address the client does not connect to**, is not
    /// believed: the answer comes from the scan, and a stale hint alone never says yes.
    #[test]
    fn a_hint_that_names_the_wrong_listener_is_not_believed() {
        let me = std::process::id();
        let watched = TcpListener::bind("127.0.0.1:0").unwrap();
        let watched_port = watched.local_addr().unwrap().port();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let (watch, _) = establish(me, watched_port).unwrap();
        // The hint is the descriptor of a listener on another port: the scan finds the real one and replaces the hint.
        let (probe, hint) = watch.check(Some(other.as_raw_fd()));
        assert_eq!(probe, Probe::Holds);
        assert_eq!(hint, Some(watched.as_raw_fd()));
        // With the real listener gone, the same stale hint says nothing.
        drop(watched);
        assert_eq!(
            watch.check(Some(other.as_raw_fd())),
            (Probe::NotHolding, None)
        );

        // The process holds the port only on `::1` (not an address the client connects to); a hint naming that socket is not believed.
        if let Ok(v6) = TcpListener::bind("[::1]:0") {
            let v6_port = v6.local_addr().unwrap().port();
            let (watch, hint) = establish(me, v6_port).unwrap();
            assert_eq!(hint, None, "a `::1` listener is not the target address");
            assert_eq!(watch.check(Some(v6.as_raw_fd())), (Probe::NotHolding, None));
        }
    }

    #[test]
    fn the_scan_finds_the_descriptor_of_the_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        assert_eq!(find_listener(me, port).unwrap(), Some(listener.as_raw_fd()));
        // A port nobody of this process listens on (one that was just released: not "the next number", which another test may hold).
        let released = TcpListener::bind("127.0.0.1:0").unwrap();
        let released_port = released.local_addr().unwrap().port();
        drop(released);
        assert_eq!(find_listener(me, released_port).unwrap(), None);
    }

    /// The watch is pinned to the start time it saw: a different start time under the same pid (a recycled pid) is not the engine, even if
    /// that process holds the very socket.
    #[test]
    fn a_recycled_pid_is_not_the_engine_even_when_it_holds_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        let (watch, hint) = establish(me, port).unwrap();
        assert_eq!(watch.check(hint).0, Probe::Holds);
        let forged = Watch {
            pid: watch.pid,
            port,
            started: (watch.started.0, watch.started.1 + 1),
        };
        assert_eq!(forged.check(hint).0, Probe::NotHolding);
        let forged = Watch {
            started: (watch.started.0 + 1, watch.started.1),
            ..forged
        };
        assert_eq!(forged.check(None).0, Probe::NotHolding);
    }

    /// A hint that points at a descriptor which is not the listening socket (the descriptor number was reused) is not believed: the scan
    /// decides.
    #[test]
    fn a_stale_descriptor_hint_is_not_believed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(server_addr).unwrap();
        let (watch, _) = establish(std::process::id(), port).unwrap();
        // The hint names a connected socket, not the listener.
        let (probe, hint) = watch.check(Some(client.as_raw_fd()));
        assert_eq!(probe, Probe::Holds, "the scan finds the real listener");
        assert_eq!(hint, Some(listener.as_raw_fd()));
        // With the listener closed, the same stale hint gives nothing.
        let stale = client.as_raw_fd();
        drop(listener);
        assert_eq!(watch.check(Some(stale)).0, Probe::NotHolding);
    }

    #[test]
    fn errnos_mean_gone_denied_or_other() {
        assert_eq!(failure_of(Some(libc::ESRCH)), Failure::Gone);
        assert_eq!(failure_of(Some(libc::EPERM)), Failure::Denied);
        assert_eq!(failure_of(Some(libc::EACCES)), Failure::Denied);
        assert_eq!(failure_of(Some(libc::EBADF)), Failure::Other);
        assert_eq!(failure_of(Some(libc::EINVAL)), Failure::Other);
        assert_eq!(failure_of(None), Failure::Other);
    }

    /// A process of another user (launchd, pid 1) cannot be looked at by an ordinary user: that is "cannot tell", not "does not hold" (the
    /// caller then asks the slow question instead of refusing or accepting). Skipped when run as root (root can look at everything).
    #[test]
    fn another_users_process_cannot_be_inspected_and_that_is_not_a_no() {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert_eq!(process_start(1), Err(Failure::Denied));
        let watch = Watch {
            pid: 1,
            port: 80,
            started: (0, 0),
        };
        assert_eq!(watch.check(None), (Probe::Unavailable, None));
        assert_eq!(watch.check(Some(3)), (Probe::Unavailable, Some(3)));
    }

    /// Start times are real: seconds since the epoch and microseconds within the second (some of several fresh processes have a non-zero
    /// microsecond part).
    #[test]
    fn a_process_start_time_is_seconds_and_microseconds_since_the_epoch() {
        let mut children: Vec<std::process::Child> = (0..6)
            .map(|_| {
                std::process::Command::new("/bin/sleep")
                    .arg("30")
                    .spawn()
                    .unwrap()
            })
            .collect();
        let starts: Vec<(u64, u64)> = children
            .iter()
            .map(|child| process_start(child.id() as i32).unwrap())
            .collect();
        for child in &mut children {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(
            starts
                .iter()
                .all(|(seconds, micros)| *seconds > 1_600_000_000 && *micros < 1_000_000)
        );
        assert!(
            starts.iter().any(|(_, micros)| *micros != 0),
            "the microsecond part is never read: {starts:?}"
        );
    }

    #[test]
    fn the_identity_of_a_process_and_its_failures() {
        let me = std::process::id() as i32;
        assert!(process_start(me).is_ok());
        // A pid that cannot exist.
        assert!(matches!(process_start(i32::MAX), Err(Failure::Gone)));
    }
}

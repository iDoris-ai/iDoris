//! macOS: libproc. **This file holds the only `unsafe` code of the workspace** (see the crate docs for why it has to); `lib.rs` applies the
//! narrow `allow(unsafe_code)` to this module alone, the logic on top of it (`macos.rs`) is safe code.
//!
//! `proc_pidinfo` / `proc_pidfdinfo` are plain system calls that fill a caller-provided buffer. Every call here passes a pointer to a
//! zero-initialised, correctly aligned plain-old-data struct (or a `Vec` of them) together with its exact size, and reads the struct only
//! after the call reported that many bytes written. A call is reported to the logic as one of three things ([`Reply`]): a **complete**
//! answer (exactly the bytes asked for), a **short** one (some bytes, not all: no `errno` to read), or a **failure** (the `errno`, which
//! is cleared before the call because libproc returns 0 for both an error and an empty result). The socket-info structs below mirror
//! `sys/proc_info.h` (`libc` has the process and descriptor structs, not these); their sizes and the offsets we read are asserted at
//! compile time against what the kernel header defines (`PROC_PIDFDSOCKETINFO_SIZE` is 792 bytes), and the tests below run them against
//! real sockets.

use std::mem::{offset_of, size_of, zeroed};
use std::os::raw::c_void;

use libc::{
    PROC_PIDLISTFDS, PROC_PIDTBSDINFO, PROX_FDTYPE_SOCKET, c_int, proc_bsdinfo, proc_fdinfo,
    proc_pidfdinfo, proc_pidinfo,
};

use super::macos::{FdEntry, FdTable, Reply, SOCKINFO_TCP, SocketData, Sys};

/// `PROC_PIDFDSOCKETINFO` (sys/proc_info.h): the flavor of `proc_pidfdinfo` that returns a `struct socket_fdinfo`.
const PROC_PIDFDSOCKETINFO: c_int = 3;

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
// `struct proc_fdinfo` is two 32-bit integers.
const _: () = assert!(size_of::<proc_fdinfo>() == 8);

/// The bytes of one `proc_fdinfo` entry of the descriptor table.
const FD_ENTRY: usize = size_of::<proc_fdinfo>();

/// What the kernel's answer says, as plain data: the unions and C layouts stop here.
fn decode(info: &SocketFdInfo) -> SocketData {
    let socket = &info.psi;
    if socket.soi_kind != SOCKINFO_TCP {
        return SocketData {
            kind: socket.soi_kind,
            tcp_state: 0,
            port: 0,
            vflag: 0,
            laddr: [0; 16],
        };
    }
    // SAFETY: `soi_kind` says the TCP member of the union is the one the kernel filled in; every bit pattern is a valid `TcpSockInfo`.
    let tcp = unsafe { socket.soi_proto.tcp };
    SocketData {
        kind: socket.soi_kind,
        tcp_state: tcp.tcpsi_state,
        port: u16::from_be(tcp.tcpsi_ini.insi_lport as u16),
        vflag: tcp.tcpsi_ini.insi_vflag,
        laddr: tcp.tcpsi_ini.insi_laddr,
    }
}

fn clear_errno() {
    // SAFETY: `__error` returns the address of this thread's `errno`.
    unsafe { *libc::__error() = 0 };
}

fn last_errno() -> i32 {
    // SAFETY: as above.
    unsafe { *libc::__error() }
}

/// A libproc call's return value against the size that was asked for.
fn reply<T>(written: c_int, size: c_int, value: impl FnOnce() -> T) -> Reply<T> {
    if written == size {
        Reply::Complete(value())
    } else if written <= 0 {
        Reply::Failed(last_errno())
    } else {
        Reply::Short
    }
}

/// The real system calls.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Libproc;

impl Sys for Libproc {
    fn start_time(&self, pid: i32) -> Reply<(u64, u64)> {
        // SAFETY: `proc_bsdinfo` is plain old data; the all-zero value is valid.
        let mut info: proc_bsdinfo = unsafe { zeroed() };
        let Ok(size) = c_int::try_from(size_of::<proc_bsdinfo>()) else {
            return Reply::Failed(libc::EINVAL);
        };
        clear_errno();
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
        reply(written, size, || {
            (info.pbi_start_tvsec, info.pbi_start_tvusec)
        })
    }

    fn fd_table_len(&self, pid: i32) -> Reply<usize> {
        clear_errno();
        // SAFETY: a null buffer with size 0 asks for the number of bytes needed.
        let needed = unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        match usize::try_from(needed) {
            Ok(0) => match last_errno() {
                0 => Reply::Complete(0), // no descriptors
                errno => Reply::Failed(errno),
            },
            Ok(bytes) => Reply::Complete(bytes.div_ceil(FD_ENTRY)),
            Err(_) => Reply::Failed(match last_errno() {
                0 => libc::EINVAL,
                errno => errno,
            }),
        }
    }

    fn fd_table(&self, pid: i32, entries: usize) -> Reply<FdTable> {
        let Some(bytes) = entries
            .checked_mul(FD_ENTRY)
            .and_then(|bytes| c_int::try_from(bytes).ok())
        else {
            return Reply::Failed(libc::EOVERFLOW);
        };
        // SAFETY: `proc_fdinfo` is plain old data; the all-zero value is valid.
        let mut table: Vec<proc_fdinfo> = vec![unsafe { zeroed() }; entries];
        clear_errno();
        // SAFETY: the pointer is to the vector's storage, the size is exactly its length in bytes.
        let written = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDLISTFDS,
                0,
                table.as_mut_ptr().cast::<c_void>(),
                bytes,
            )
        };
        let written = match usize::try_from(written) {
            Ok(0) => {
                return match last_errno() {
                    0 => Reply::Complete(FdTable {
                        entries: Vec::new(),
                        full: false,
                    }),
                    errno => Reply::Failed(errno),
                };
            }
            Ok(written) => written,
            Err(_) => {
                return Reply::Failed(match last_errno() {
                    0 => libc::EINVAL,
                    errno => errno,
                });
            }
        };
        // The kernel writes whole entries; anything else is not an answer we understand.
        if written % FD_ENTRY != 0 || written > entries * FD_ENTRY {
            return Reply::Short;
        }
        let count = written / FD_ENTRY;
        Reply::Complete(FdTable {
            entries: table[..count]
                .iter()
                .map(|entry| FdEntry {
                    fd: entry.proc_fd,
                    is_socket: entry.proc_fdtype == PROX_FDTYPE_SOCKET as u32,
                })
                .collect(),
            // A buffer that came back exactly full may have been cut off.
            full: written == entries * FD_ENTRY,
        })
    }

    fn socket_info(&self, pid: i32, fd: i32) -> Reply<SocketData> {
        // SAFETY: `SocketFdInfo` is plain old data (integers, byte arrays, a union of such); the all-zero value is valid.
        let mut info: SocketFdInfo = unsafe { zeroed() };
        let Ok(size) = c_int::try_from(size_of::<SocketFdInfo>()) else {
            return Reply::Failed(libc::EINVAL);
        };
        clear_errno();
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
        reply(written, size, || decode(&info))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    use super::super::macos::{INI_IPV4, TSI_S_LISTEN};
    use super::*;

    fn me() -> i32 {
        std::process::id() as i32
    }

    fn complete<T>(reply: Reply<T>) -> T {
        match reply {
            Reply::Complete(value) => value,
            Reply::Short => panic!("a short answer"),
            Reply::Failed(errno) => panic!("failed with errno {errno}"),
        }
    }

    fn failed<T>(reply: Reply<T>) -> i32 {
        match reply {
            Reply::Failed(errno) => errno,
            Reply::Complete(_) => panic!("unexpectedly complete"),
            Reply::Short => panic!("a short answer"),
        }
    }

    /// The mirrored struct reads the real kernel answer: the kind, the state, the port and the address of a real listener of this process.
    #[test]
    fn the_mirrored_layout_reads_a_real_listening_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let data = complete(Libproc.socket_info(me(), listener.as_raw_fd()));
        assert_eq!(data.kind, SOCKINFO_TCP);
        assert_eq!(data.tcp_state, TSI_S_LISTEN);
        assert_eq!(data.port, port);
        assert_eq!(data.vflag, INI_IPV4);
        assert_eq!(
            &data.laddr[12..],
            &[127, 0, 0, 1],
            "an IPv4 address is in the last four bytes"
        );

        // a connected client socket: TCP, not listening
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let data = complete(Libproc.socket_info(me(), client.as_raw_fd()));
        assert_eq!(data.kind, SOCKINFO_TCP);
        assert_ne!(data.tcp_state, TSI_S_LISTEN);
    }

    /// What the system says about descriptors that are not listening sockets, and about processes that cannot be asked.
    #[test]
    fn failures_carry_the_errno_that_tells_them_apart() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            failed(Libproc.socket_info(me(), file.as_raw_fd())),
            libc::ENOTSOCK,
            "not a socket"
        );
        assert_eq!(
            failed(Libproc.socket_info(me(), 9_999)),
            libc::EBADF,
            "closed"
        );
        assert_eq!(
            failed(Libproc.socket_info(i32::MAX, 3)),
            libc::ESRCH,
            "no such process"
        );
        // launchd belongs to another user: not allowed to look (skipped as root)
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(failed(Libproc.socket_info(1, 3)), libc::EPERM);
            assert_eq!(failed(Libproc.start_time(1)), libc::EPERM);
        }
        assert_eq!(failed(Libproc.start_time(i32::MAX)), libc::ESRCH);
        // a unix-domain socket is a socket: complete, but not TCP
        let path = std::env::temp_dir().join(format!("sockprobe-{}.sock", std::process::id()));
        let unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let data = complete(Libproc.socket_info(me(), unix.as_raw_fd()));
        assert_ne!(data.kind, SOCKINFO_TCP);
        let _ = std::fs::remove_file(path);
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
            .map(|child| complete(Libproc.start_time(child.id() as i32)))
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

    /// The descriptor table: it lists this process's descriptors (the listener among them, as a socket), a buffer that comes back exactly
    /// full says so (it may have been cut off), and an absurd size is refused instead of overflowing.
    #[test]
    fn the_descriptor_table_is_listed_and_a_full_buffer_says_so() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = listener.as_raw_fd();
        let needed = complete(Libproc.fd_table_len(me()));
        assert!(
            needed >= 3,
            "stdin, stdout, stderr and the listener: {needed}"
        );
        let table = complete(Libproc.fd_table(me(), needed + 32));
        assert!(!table.full);
        let entry = table.entries.iter().find(|entry| entry.fd == fd).unwrap();
        assert!(entry.is_socket);
        // exactly as many entries as there are descriptors fills the buffer completely, and so does asking for fewer
        let few = complete(Libproc.fd_table(me(), 1));
        assert_eq!(few.entries.len(), 1);
        assert!(
            few.full,
            "a buffer that came back full may have been cut off"
        );
        assert_eq!(
            failed(Libproc.fd_table(me(), usize::MAX / 4)),
            libc::EOVERFLOW
        );
        assert_eq!(failed(Libproc.fd_table_len(i32::MAX)), libc::ESRCH);
    }
}

//! Linux: `/proc`. Safe code only (`std::fs`).
//!
//! The parsers are pure functions of the text the kernel writes, and the glue takes the root of the proc tree as a parameter: both are
//! compiled (and tested, against a fake tree in a temporary directory) on every platform; only [`establish`] with the real `/proc` is
//! Linux-only.
//!
//! Per request the watch reads `/proc/<pid>/stat` (is it still the same process: the start time in clock ticks since boot), reads the link
//! `/proc/<pid>/fd/<fd>` (is it still the same socket: `socket:[<inode>]`) and then reads `/proc/net/tcp{,6}` **up to that inode's row**:
//! the socket must still be in the LISTEN state and bound to an address that covers the dial address. (The kernel lists the listening
//! sockets first and formats the rows lazily, so the row of a socket that does listen is reached after reading a few kilobytes; a socket
//! that left LISTEN has no row, and then the table is read to its end and the socket is looked for again by a scan.) The tables are in the
//! network namespace of **this** process, which is the one the client connects in: a socket of another namespace has no row here and is
//! never "held".
//!
//! `/proc/net/tcp6` does not say whether an IPv6 socket is `IPV6_V6ONLY`: an IPv6 wildcard is taken to cover IPv6 dial addresses only
//! ([`Bind::v6`] with `v4_too = false`).
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use super::{Bind, Probe, Quick};

/// The real proc tree.
#[cfg(target_os = "linux")]
const PROC: &str = "/proc";

/// The descriptor and socket inode the listening socket had at the last check, and whether it was found in the IPv6 table.
pub(crate) type Hint = (i32, u64, bool);

#[derive(Debug)]
pub(crate) struct Watch {
    root: PathBuf,
    pid: u32,
    port: u16,
    dial: IpAddr,
    /// `starttime` (field 22 of `/proc/<pid>/stat`), pinned when the watch was established.
    started: u64,
}

#[cfg(target_os = "linux")]
pub(crate) fn establish(pid: u32, port: u16, dial: IpAddr) -> Option<Watch> {
    establish_at(Path::new(PROC), pid, port, dial)
}

#[cfg(target_os = "linux")]
fn generation_from_ticks(ticks: u64) -> Option<String> {
    static BOOT_ID: std::sync::LazyLock<Option<String>> =
        std::sync::LazyLock::new(|| fs::read_to_string("/proc/sys/kernel/random/boot_id").ok());
    Some(format!("linux:{}:{ticks}", BOOT_ID.as_deref()?.trim()))
}

#[cfg(target_os = "linux")]
pub(crate) fn process_generation(pid: u32) -> Option<String> {
    generation_from_ticks(read_start(Path::new(PROC), pid).ok()??)
}

#[cfg(target_os = "linux")]
pub(crate) fn executable_identity(pid: u32) -> Option<crate::ExecutableIdentity> {
    let exe = Path::new(PROC).join(pid.to_string()).join("exe");
    let path = fs::read_link(&exe).ok()?;
    let metadata = fs::metadata(exe).ok()?;
    crate::ExecutableIdentity::from_metadata(path, &metadata)
}

/// Pin the identity of `pid`: one read of its `stat`. Nothing is scanned.
pub(crate) fn establish_at(root: &Path, pid: u32, port: u16, dial: IpAddr) -> Option<Watch> {
    if pid < 2 || i32::try_from(pid).is_err() {
        return None;
    }
    let started = match read_start(root, pid) {
        Ok(Some(started)) => started,
        _ => return None,
    };
    Some(Watch {
        root: root.to_owned(),
        pid,
        port,
        dial,
        started,
    })
}

/// Is it still the process the watch was established for?
enum Identity {
    Same,
    /// Gone, a zombie, or another process under the same pid.
    Different,
    /// Cannot be read (permission, an I/O error).
    Unreadable,
}

impl Watch {
    #[cfg(target_os = "linux")]
    pub(crate) fn generation(&self) -> Option<String> {
        generation_from_ticks(self.started)
    }

    fn identity(&self) -> Identity {
        match read_start(&self.root, self.pid) {
            Ok(Some(started)) if started == self.started => Identity::Same,
            Ok(_) => Identity::Different,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Identity::Different,
            Err(_) => Identity::Unreadable,
        }
    }

    fn fd_dir(&self) -> PathBuf {
        self.root.join(self.pid.to_string()).join("fd")
    }

    /// The cheap check (see [`super::ListenerWatch::check_cached`]).
    pub(crate) fn quick(&self, hint: Option<Hint>) -> Quick<Hint> {
        match self.identity() {
            Identity::Same => {}
            Identity::Different => return Quick::Answer(Probe::NotHolding, None),
            Identity::Unreadable => return Quick::Answer(Probe::Unavailable, hint),
        }
        let Some((fd, inode, v6)) = hint else {
            return Quick::Scan;
        };
        // 1. The descriptor still names that socket?
        match fs::read_link(self.fd_dir().join(fd.to_string())) {
            Ok(target) if socket_inode(&target.to_string_lossy()) == Some(inode) => {}
            Ok(_) => return Quick::Scan,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Quick::Scan,
            Err(_) => return Quick::Answer(Probe::Unavailable, hint),
        }
        // 2. And is that socket still listening, on an address the dial address reaches? The descriptor alone proves nothing: a listening
        //    socket that was `shutdown(2)` keeps its descriptor and its inode but is no longer in LISTEN state.
        match self.listening(inode, v6) {
            Ok(true) => Quick::Answer(Probe::Holds, hint),
            // Not listening any more (or not on such an address): the process may still hold another socket that does, so look again.
            Ok(false) => Quick::Scan,
            Err(_) => Quick::Answer(Probe::Unavailable, hint),
        }
    }

    /// The full check after the cheap one found nothing to go on: locate the socket again.
    pub(crate) fn scan(&self, hint: Option<Hint>) -> (Probe, Option<Hint>) {
        match self.identity() {
            Identity::Same => {}
            Identity::Different => return (Probe::NotHolding, None),
            Identity::Unreadable => return (Probe::Unavailable, hint),
        }
        match self.locate() {
            Ok(Some(found)) => match self.identity() {
                Identity::Same => (Probe::Holds, Some(found)),
                Identity::Different => (Probe::NotHolding, None),
                Identity::Unreadable => (Probe::Unavailable, hint),
            },
            Ok(None) => (Probe::NotHolding, None),
            // The process (or its descriptor table) is gone: it holds nothing.
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Probe::NotHolding, None),
            // Not allowed to look (another user's process, a restricted `/proc`), a table that cannot be asked, or an unexpected error: not
            // a "no", the caller decides.
            Err(_) => (Probe::Unavailable, hint),
        }
    }

    fn table(&self, v6: bool) -> PathBuf {
        self.root.join("net").join(if v6 { "tcp6" } else { "tcp" })
    }

    /// Is the socket with this inode, in the given table, in LISTEN state on this watch's port and bound to an address that covers the dial
    /// address? Read up to its row; a socket with no row is not listening.
    fn listening(&self, inode: u64, v6: bool) -> io::Result<bool> {
        let reader = BufReader::with_capacity(8 * 1024, File::open(self.table(v6))?);
        for line in reader.lines().skip(1) {
            let line = line?;
            let Some(row) = parse_row(&line, v6) else {
                continue;
            };
            if row.inode == inode {
                return Ok(row.listening && row.port == self.port && row.bind.covers(self.dial));
            }
        }
        Ok(false)
    }

    /// The descriptor and socket inode of a LISTEN socket of this process on the port that covers the dial address.
    fn locate(&self) -> io::Result<Option<Hint>> {
        let mut candidates: Vec<(u64, bool)> = Vec::new();
        for v6 in [false, true] {
            let reader = match File::open(self.table(v6)) {
                Ok(file) => BufReader::with_capacity(64 * 1024, file),
                // No IPv6 on this host.
                Err(error) if v6 && error.kind() == io::ErrorKind::NotFound => continue,
                // No IPv4 table at all (an unusual /proc): this host cannot be asked, which is not "the process is gone".
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "no /proc/net/tcp",
                    ));
                }
                Err(error) => return Err(error),
            };
            for line in reader.lines().skip(1) {
                let line = line?;
                if let Some(row) = parse_row(&line, v6)
                    && row.listening
                    && row.port == self.port
                    && row.bind.covers(self.dial)
                {
                    candidates.push((row.inode, v6));
                }
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        let entries = fs::read_dir(self.fd_dir())?.map(|entry| -> Listing {
            let entry = entry?;
            let target = fs::read_link(entry.path());
            Ok((entry.file_name(), target))
        });
        find_socket(&candidates, entries)
    }
}

/// One entry of the descriptor directory as the scan saw it: its name and what reading its link said (both can fail on their own).
type Listing = io::Result<(OsString, io::Result<PathBuf>)>;

/// Which descriptor holds one of the `candidates` (socket inode, found in the IPv6 table)? A descriptor that vanishes while the directory
/// is read (the process closed it) is skipped; one that cannot be read for any other reason (permission, an I/O error) leaves the answer
/// **unknown** unless the socket is found anyway — an unreadable descriptor might be the very socket, so "not found" would be a guess.
fn find_socket(
    candidates: &[(u64, bool)],
    entries: impl IntoIterator<Item = Listing>,
) -> io::Result<Option<Hint>> {
    let mut unchecked: Option<io::Error> = None;
    for entry in entries {
        let (name, target) = match entry {
            Ok(listed) => listed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                unchecked = Some(error);
                continue;
            }
        };
        let target = match target {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                unchecked = Some(error);
                continue;
            }
        };
        let Some(inode) = socket_inode(&target.to_string_lossy()) else {
            continue;
        };
        if let Some(&(_, v6)) = candidates.iter().find(|(candidate, _)| *candidate == inode)
            && let Some(fd) = name.to_str().and_then(|name| name.parse::<i32>().ok())
        {
            return Ok(Some((fd, inode, v6)));
        }
    }
    match unchecked {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// `starttime` of `/proc/<pid>/stat`; `None` for a zombie or a dead process (`Z`, `X`).
fn read_start(root: &Path, pid: u32) -> io::Result<Option<u64>> {
    let text = fs::read_to_string(root.join(pid.to_string()).join("stat"))?;
    Ok(parse_stat_start(&text))
}

/// `/proc/<pid>/stat`: `pid (comm) state ppid ... starttime ...`. The command name may contain spaces and parentheses, so the fields
/// are counted from the **last** `)`. `starttime` is field 22; the state is field 3 (`Z` zombie, `X` dead).
pub(crate) fn parse_stat_start(text: &str) -> Option<u64> {
    let after = &text[text.rfind(')')? + 1..];
    let mut fields = after.split_whitespace();
    let state = fields.next()?;
    if state == "Z" || state == "X" {
        return None;
    }
    // Fields 4 ... 21 are 18 values; field 22 is the 19th after the state.
    fields.nth(18)?.parse().ok()
}

/// `socket:[12345]` -> 12345.
pub(crate) fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// One row of `/proc/net/tcp{,6}`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) port: u16,
    /// The socket is in the LISTEN state (`0A`).
    pub(crate) listening: bool,
    /// What it is bound to (family and address).
    pub(crate) bind: Bind,
    pub(crate) inode: u64,
}

/// One row. Columns: `sl local_address rem_address st tx:rx tr:tm->when retrnsmt uid timeout inode ...`; the address is the kernel's
/// in-memory words in hex (so the bytes are those of `to_ne_bytes`), the port is in host order. A row that does not parse is `None`.
pub(crate) fn parse_row(line: &str, v6: bool) -> Option<Row> {
    let mut columns = line.split_ascii_whitespace();
    columns.next()?; // sl
    let local = columns.next()?;
    columns.next()?; // rem_address
    let state = columns.next()?;
    // tx_queue:rx_queue, tr:tm->when, retrnsmt, uid, timeout, then the inode
    let inode = columns.nth(5)?.parse::<u64>().ok()?;
    let (address, port) = local.split_once(':')?;
    Some(Row {
        port: u16::from_str_radix(port, 16).ok()?,
        listening: state == "0A",
        bind: bind_of(address, v6)?,
        inode,
    })
}

/// The rows of a whole table (the header line is skipped), those that parse. (The probe itself reads the table line by line; this is the
/// whole-table view the tests use.)
#[cfg(test)]
pub(crate) fn parse_net_tcp(text: &str, v6: bool) -> Vec<Row> {
    text.lines()
        .skip(1)
        .filter_map(|line| parse_row(line, v6))
        .collect()
}

/// The address of a row: 8 hex digits (one word) for IPv4, 32 (four words) for IPv6.
fn bind_of(hex: &str, v6: bool) -> Option<Bind> {
    let words = if v6 { 4 } else { 1 };
    if hex.len() != words * 8 {
        return None;
    }
    let mut bytes = Vec::with_capacity(words * 4);
    for word in 0..words {
        let value = u32::from_str_radix(hex.get(word * 8..word * 8 + 8)?, 16).ok()?;
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    if v6 {
        let octets: [u8; 16] = bytes.try_into().ok()?;
        // Nothing in the table says the socket is not IPV6_V6ONLY: no evidence of dual stack.
        Some(Bind::v6(Ipv6Addr::from(octets), false))
    } else {
        let octets: [u8; 4] = bytes.try_into().ok()?;
        Some(Bind::V4(Ipv4Addr::from(octets)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::symlink;

    use super::*;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const LOOPBACK6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

    #[test]
    fn the_start_time_is_counted_from_the_last_parenthesis() {
        // A command name with spaces and parentheses must not shift the fields.
        let stat = "4242 (a (weird) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 12345678 100 18446744073709551615";
        assert_eq!(parse_stat_start(stat), Some(987_654));
        assert_eq!(
            parse_stat_start("1 (init) Z 0 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 5 0 0"),
            None
        );
        assert_eq!(
            parse_stat_start("1 (init) X 0 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 5 0 0"),
            None
        );
        assert_eq!(parse_stat_start("garbage"), None);
        assert_eq!(parse_stat_start("1 (x) S 0 1"), None);
    }

    /// Rows that are cut short or have an address of the wrong length are skipped, never panicked over.
    #[test]
    fn malformed_rows_are_skipped() {
        let text = "  sl  local_address ...\n\
                    0: 0100007F:1F98 00000000:0000 0A\n\
                    1: 0100007F00:1F98 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 5 1\n\
                    2: zzzzzzzz:1F98 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 6 1\n\
                    3: 0100007F:1F98 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 notanumber 1\n\
                    \n";
        assert_eq!(parse_net_tcp(text, false), Vec::new());
    }

    #[test]
    fn a_socket_link_names_its_inode() {
        assert_eq!(socket_inode("socket:[12345]"), Some(12_345));
        assert_eq!(socket_inode("pipe:[7]"), None);
        assert_eq!(socket_inode("/dev/null"), None);
        assert_eq!(socket_inode("socket:[x]"), None);
    }

    /// One LISTEN row of `/proc/net/tcp`: the address as the kernel prints it on a little-endian host.
    fn tcp_row(address: [u8; 4], port: u16, inode: u64) -> String {
        tcp_row_in_state(address, port, inode, "0A")
    }

    fn tcp_row_in_state(address: [u8; 4], port: u16, inode: u64, state: &str) -> String {
        format!(
            "0: {:08X}:{port:04X} 00000000:0000 {state} 00000000:00000000 00:00000000 00000000  501        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
            u32::from_ne_bytes(address)
        )
    }

    fn words(bytes: [u8; 16]) -> String {
        bytes
            .chunks(4)
            .map(|word| {
                format!(
                    "{:08X}",
                    u32::from_ne_bytes([word[0], word[1], word[2], word[3]])
                )
            })
            .collect()
    }

    fn tcp6_row(bytes: [u8; 16], port: u16, inode: u64) -> String {
        format!(
            "0: {}:{port:04X} {}:0000 0A 00000000:00000000 00:00000000 00000000  501        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
            words(bytes),
            words([0; 16])
        )
    }

    const TCP_HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    /// Rows as the kernel prints them on a little-endian host: `0100007F` is 127.0.0.1, `00000000` the wildcard.
    #[test]
    fn rows_say_state_address_port_and_inode() {
        let text = format!(
            "{TCP_HEADER}{}{}{}{}",
            tcp_row([127, 0, 0, 1], 0x1F98, 111),
            tcp_row([0, 0, 0, 0], 0x1F99, 222),
            tcp_row([10, 1, 2, 3], 0x1F9A, 333),
            tcp_row_in_state([127, 0, 0, 1], 0x1F9B, 444, "01"),
        );
        let rows = parse_net_tcp(&text, false);
        let shown: Vec<_> = rows
            .iter()
            .map(|r| (r.port, r.listening, r.bind, r.inode))
            .collect();
        assert_eq!(
            shown,
            vec![
                (0x1F98, true, Bind::V4(Ipv4Addr::LOCALHOST), 111),
                (0x1F99, true, Bind::V4(Ipv4Addr::UNSPECIFIED), 222),
                (0x1F9A, true, Bind::V4(Ipv4Addr::new(10, 1, 2, 3)), 333),
                (0x1F9B, false, Bind::V4(Ipv4Addr::LOCALHOST), 444),
            ],
            "the established row (state 01) is listed, but not as listening"
        );
    }

    #[test]
    fn ipv6_rows_use_four_words_and_an_ipv6_wildcard_is_not_taken_for_dual_stack() {
        let mut loopback6 = [0_u8; 16];
        loopback6[15] = 1;
        let mut mapped = [0_u8; 16];
        mapped[10] = 0xff;
        mapped[11] = 0xff;
        mapped[12..].copy_from_slice(&[127, 0, 0, 1]);
        let text = format!(
            "{TCP_HEADER}{}{}{}",
            tcp6_row([0; 16], 0x1F98, 11),
            tcp6_row(loopback6, 0x1F99, 22),
            tcp6_row(mapped, 0x1F9A, 33),
        );
        let rows = parse_net_tcp(&text, true);
        let binds: Vec<_> = rows.iter().map(|r| (r.inode, r.bind)).collect();
        assert_eq!(
            binds,
            vec![
                (
                    11,
                    Bind::V6 {
                        addr: Ipv6Addr::UNSPECIFIED,
                        v4_too: false
                    }
                ),
                (
                    22,
                    Bind::V6 {
                        addr: Ipv6Addr::LOCALHOST,
                        v4_too: false
                    }
                ),
                (33, Bind::V4(Ipv4Addr::LOCALHOST)),
            ],
            "`::` carries no evidence of dual stack, `::1` is IPv6 only, the mapped form is an IPv4 bind"
        );
        // ... and that is what decides who a dial address reaches
        assert!(!rows[0].bind.covers(LOOPBACK) && rows[0].bind.covers(LOOPBACK6));
        assert!(!rows[1].bind.covers(LOOPBACK) && rows[1].bind.covers(LOOPBACK6));
        assert!(rows[2].bind.covers(LOOPBACK) && !rows[2].bind.covers(LOOPBACK6));
    }

    /// A fake proc tree: `net/tcp{,6}`, and `<pid>/stat` with a symlink per descriptor.
    struct FakeProc {
        dir: tempfile::TempDir,
        pid: u32,
    }

    impl FakeProc {
        fn new(pid: u32) -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                pid,
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn stat(&self, start: u64) {
            fs::create_dir_all(self.root().join(self.pid.to_string())).unwrap();
            fs::write(
                self.root().join(self.pid.to_string()).join("stat"),
                format!(
                    "{0} (engine) S 1 {0} {0} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0",
                    self.pid
                ),
            )
            .unwrap();
        }

        fn net(&self, table: &str, rows: &str) {
            fs::create_dir_all(self.root().join("net")).unwrap();
            fs::write(
                self.root().join("net").join(table),
                format!("{TCP_HEADER}{rows}"),
            )
            .unwrap();
        }

        /// Descriptor `fd` points at `link` (`None`: the descriptor is closed).
        fn fd(&self, fd: i32, link: Option<&str>) {
            let fds = self.root().join(self.pid.to_string()).join("fd");
            fs::create_dir_all(&fds).unwrap();
            let path = fds.join(fd.to_string());
            let _ = fs::remove_file(&path);
            if let Some(link) = link {
                symlink(link, path).unwrap();
            }
        }
    }

    const LO: [u8; 4] = [127, 0, 0, 1];

    /// The watch is scan-free to establish, finds its socket with a scan once, and then asks about it directly: the descriptor, **and the
    /// table row** (still LISTEN, still an address the dial address reaches).
    #[test]
    fn establishing_scans_nothing_and_the_cheap_check_needs_the_descriptor_and_the_row() {
        let fake = FakeProc::new(4242);
        fake.stat(1_000);
        // no net/tcp at all: establishing still works because it reads nothing but `stat`
        let watch = establish_at(fake.root(), 4242, 8088, LOOPBACK).unwrap();
        assert!(matches!(watch.quick(None), Quick::Scan));
        fake.net("tcp", &tcp_row(LO, 8088, 555));
        fake.fd(3, Some("socket:[555]"));
        fake.fd(4, Some("/dev/null"));
        let (probe, hint) = watch.scan(None);
        assert_eq!((probe, hint), (Probe::Holds, Some((3, 555, false))));
        assert!(matches!(
            watch.quick(hint),
            Quick::Answer(Probe::Holds, Some((3, 555, false)))
        ));
    }

    /// **The descriptor is kept but the socket left LISTEN** (`shutdown(2)` on a listening socket): the inode is the same, the row is gone
    /// or no longer in LISTEN state. The cheap check must not say "holds"; the scan then says nothing holds.
    #[test]
    fn a_kept_descriptor_whose_socket_left_listen_state_is_not_held() {
        let fake = FakeProc::new(4242);
        fake.stat(1_000);
        fake.net("tcp", &tcp_row(LO, 8088, 555));
        fake.fd(3, Some("socket:[555]"));
        let watch = establish_at(fake.root(), 4242, 8088, LOOPBACK).unwrap();
        let (_, hint) = watch.scan(None);
        assert!(matches!(watch.quick(hint), Quick::Answer(Probe::Holds, _)));

        // (a) the row is gone from the table (an unhashed socket is not listed), the descriptor still names the inode
        fake.net("tcp", &tcp_row(LO, 9, 1));
        assert!(matches!(watch.quick(hint), Quick::Scan));
        assert_eq!(watch.scan(hint), (Probe::NotHolding, None));

        // (b) the row is still listed but not in LISTEN state
        fake.net("tcp", &tcp_row_in_state(LO, 8088, 555, "07"));
        assert!(matches!(watch.quick(hint), Quick::Scan));
        assert_eq!(watch.scan(hint), (Probe::NotHolding, None));

        // (c) the row is listening again (on the same inode): holds again
        fake.net("tcp", &tcp_row(LO, 8088, 555));
        assert!(matches!(watch.quick(hint), Quick::Answer(Probe::Holds, _)));
    }

    /// The row decides the address too: the same inode listening on a port or an address the dial address does not reach is not held.
    #[test]
    fn the_row_must_be_on_this_port_and_cover_the_dial_address() {
        let fake = FakeProc::new(7);
        fake.stat(5);
        fake.fd(3, Some("socket:[10]"));
        fake.net("tcp", &tcp_row(LO, 8088, 10));
        let watch = establish_at(fake.root(), 7, 8088, LOOPBACK).unwrap();
        let (_, hint) = watch.scan(None);
        assert!(hint.is_some());
        fake.net("tcp", &tcp_row(LO, 9000, 10));
        assert!(matches!(watch.quick(hint), Quick::Scan), "another port");
        fake.net("tcp", &tcp_row([10, 1, 2, 3], 8088, 10));
        assert!(matches!(watch.quick(hint), Quick::Scan), "another address");
        fake.net("tcp", &tcp_row([0, 0, 0, 0], 8088, 10));
        assert!(
            matches!(watch.quick(hint), Quick::Answer(Probe::Holds, _)),
            "a wildcard"
        );
    }

    #[test]
    fn a_pid_reused_during_the_proc_scan_is_not_holding() {
        use std::io::Write;
        use std::process::Command;

        let fake = FakeProc::new(4242);
        fake.stat(1_000);
        fake.fd(3, Some("socket:[555]"));
        let watch = establish_at(fake.root(), 4242, 8088, LOOPBACK).unwrap();
        let table = fake.root().join("net/tcp");
        fs::create_dir(table.parent().unwrap()).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(&table)
                .status()
                .unwrap()
                .success()
        );

        std::thread::scope(|scope| {
            let check = scope.spawn(|| watch.scan(None));
            // Opening the write end waits for the scanner to open the read end,
            // which it does only after its first identity check.
            let mut writer = File::options().write(true).open(&table).unwrap();
            fake.stat(2_000);
            write!(writer, "{TCP_HEADER}{}", tcp_row(LO, 8088, 555)).unwrap();
            drop(writer);
            assert_eq!(check.join().unwrap(), (Probe::NotHolding, None));
        });
    }

    /// The watch follows a socket through descriptor changes, notices that the descriptor names another socket, finds it again under
    /// another number, and notices a recycled pid.
    #[test]
    fn the_watch_follows_a_socket_through_a_fake_proc_tree() {
        let fake = FakeProc::new(4242);
        fake.stat(1_000);
        fake.net("tcp", &tcp_row(LO, 8088, 555));
        fake.fd(3, Some("socket:[555]"));
        fake.fd(4, Some("/dev/null"));

        let watch = establish_at(fake.root(), 4242, 8088, LOOPBACK).unwrap();
        let (probe, hint) = watch.scan(None);
        assert_eq!((probe, hint), (Probe::Holds, Some((3, 555, false))));

        // The descriptor now names another socket (the number was reused; that socket is not listening on the port), or something that is
        // not a socket at all: the cheap check does not believe the hint and the scan finds nothing.
        fake.fd(3, Some("socket:[999]"));
        assert!(matches!(watch.quick(hint), Quick::Scan));
        assert_eq!(watch.scan(hint), (Probe::NotHolding, None));
        fake.fd(3, Some("/dev/null"));
        assert!(matches!(watch.quick(hint), Quick::Scan));
        assert_eq!(watch.scan(hint), (Probe::NotHolding, None));
        fake.fd(3, Some("socket:[555]"));
        assert!(matches!(watch.quick(hint), Quick::Answer(Probe::Holds, _)));

        // The listener is closed.
        fake.fd(3, None);
        fake.net("tcp", &tcp_row(LO, 1, 0));
        assert!(matches!(watch.quick(hint), Quick::Scan));
        assert_eq!(watch.scan(hint), (Probe::NotHolding, None));

        // The same process listens again, under another descriptor number and another inode.
        fake.net("tcp", &tcp_row(LO, 8088, 777));
        fake.fd(9, Some("socket:[777]"));
        assert_eq!(watch.scan(None), (Probe::Holds, Some((9, 777, false))));

        // A different process under the same pid (another start time) that holds the very socket is not the engine; and when the process
        // is gone altogether there is nothing to hold.
        fake.stat(2_000);
        assert!(matches!(
            watch.quick(Some((9, 777, false))),
            Quick::Answer(Probe::NotHolding, None)
        ));
        fs::remove_dir_all(fake.root().join("4242")).unwrap();
        assert!(matches!(
            watch.quick(Some((9, 777, false))),
            Quick::Answer(Probe::NotHolding, None)
        ));
        assert_eq!(watch.scan(Some((9, 777, false))).0, Probe::NotHolding);

        // Pids that cannot be an engine, even when a /proc entry exists for them, and a pid that is not there.
        for never in [0, 1] {
            let entry = FakeProc::new(never);
            entry.stat(5);
            assert!(
                establish_at(entry.root(), never, 8088, LOOPBACK).is_none(),
                "pid {never}"
            );
        }
        assert!(establish_at(fake.root(), 99_999, 8088, LOOPBACK).is_none());
    }

    /// Only a target listener of this process on the port counts: not another address, not another port, not another process's socket.
    #[test]
    fn only_a_target_listener_of_this_process_on_the_port_counts() {
        let fake = FakeProc::new(31);
        fake.stat(5);
        // Port 8088 listens on another address (inode 888, held by this process): not the dial address. Port 9000 listens on loopback
        // (inode 900, held by this process) but is another port. Inode 700 listens on loopback:8088 but belongs to another process.
        fake.net(
            "tcp",
            &format!(
                "{}{}{}",
                tcp_row([10, 1, 2, 3], 8088, 888),
                tcp_row(LO, 9000, 900),
                tcp_row(LO, 8088, 700)
            ),
        );
        fake.fd(5, Some("socket:[888]"));
        fake.fd(6, Some("socket:[900]"));
        let watch = establish_at(fake.root(), 31, 8088, LOOPBACK).unwrap();
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
        // The wildcard counts, once this process holds it.
        fake.net("tcp", &tcp_row([0, 0, 0, 0], 8088, 901));
        fake.fd(7, Some("socket:[901]"));
        assert_eq!(watch.scan(None), (Probe::Holds, Some((7, 901, false))));
    }

    /// A process that exits while it is scanned (its descriptor directory is gone by the time it is read) holds nothing: a no, not "cannot
    /// tell".
    #[test]
    fn a_process_whose_descriptor_directory_vanished_during_the_scan_holds_nothing() {
        let fake = FakeProc::new(52);
        fake.stat(5);
        fake.net("tcp", &tcp_row(LO, 8088, 10));
        // the process has a `stat` and the table lists a listening socket, but there is no `fd` directory (it was just removed)
        let watch = establish_at(fake.root(), 52, 8088, LOOPBACK).unwrap();
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
    }

    fn gone() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }

    fn denied() -> io::Error {
        io::Error::from(io::ErrorKind::PermissionDenied)
    }

    fn link(inode: u64) -> io::Result<PathBuf> {
        Ok(PathBuf::from(format!("socket:[{inode}]")))
    }

    /// A descriptor (or its link) that vanishes while the directory is read is skipped: the socket is found among the others, and when
    /// it is not there at all that is a no, not "unknown".
    #[test]
    fn descriptors_that_vanish_while_the_directory_is_read_are_skipped() {
        let candidates = [(900, false), (901, true)];
        let listing = |socket: Option<u64>| -> Vec<Listing> {
            let mut entries: Vec<Listing> = vec![
                Err(gone()),
                Ok((OsString::from("5"), Err(gone()))),
                Ok((OsString::from("6"), link(1))),
                Ok((OsString::from("not-a-number"), link(900))),
            ];
            if let Some(inode) = socket {
                entries.push(Ok((OsString::from("7"), link(inode))));
            }
            entries
        };
        assert_eq!(
            find_socket(&candidates, listing(Some(900))).unwrap(),
            Some((7, 900, false))
        );
        assert_eq!(
            find_socket(&candidates, listing(Some(901))).unwrap(),
            Some((7, 901, true)),
            "the table the socket was found in comes with it"
        );
        assert_eq!(
            find_socket(&candidates, listing(None)).unwrap(),
            None,
            "nothing unreadable, nothing found: a no"
        );
    }

    /// A descriptor that cannot be read for another reason leaves the answer **unknown** — unless the socket is found anyway.
    #[test]
    fn descriptors_that_cannot_be_read_leave_the_answer_unknown_unless_the_socket_is_found() {
        let candidates = [(900, false)];
        // the directory entry could not be read
        let entries: Vec<Listing> = vec![Err(denied()), Ok((OsString::from("6"), link(1)))];
        assert_eq!(
            find_socket(&candidates, entries).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // its link could not be read
        let entries: Vec<Listing> = vec![
            Ok((OsString::from("5"), Err(denied()))),
            Ok((OsString::from("6"), link(1))),
        ];
        assert_eq!(
            find_socket(&candidates, entries).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // the socket is among the descriptors that could be read: the answer is known
        let entries: Vec<Listing> = vec![
            Err(denied()),
            Ok((OsString::from("5"), Err(denied()))),
            Ok((OsString::from("7"), link(900))),
        ];
        assert_eq!(
            find_socket(&candidates, entries).unwrap(),
            Some((7, 900, false))
        );
    }

    /// **IPv6**: `/proc/net/tcp6` is read too. An IPv6 wildcard (`::`) is **not** taken for something that accepts IPv4: `/proc` does not
    /// say whether the socket is `IPV6_V6ONLY`, and a server that binds `::` with it set (asyncio does) is not reached by a connection to
    /// `127.0.0.1`. It covers an IPv6 dial address, as does `::1`; a host without IPv6 has no `tcp6` file.
    #[test]
    fn an_ipv6_wildcard_is_not_an_answer_for_an_ipv4_dial_address_but_is_for_an_ipv6_one() {
        let mut loopback6 = [0_u8; 16];
        loopback6[15] = 1;
        // `::127.0.0.1` (the deprecated IPv4-compatible form, not the mapped one) is not an IPv4 bind either.
        let mut compatible = [0_u8; 16];
        compatible[12..].copy_from_slice(&[127, 0, 0, 1]);
        let fake = FakeProc::new(77);
        fake.stat(9);
        fake.net("tcp", "");
        fake.net(
            "tcp6",
            &(tcp6_row(loopback6, 8088, 61)
                + &tcp6_row(compatible, 8088, 64)
                + &tcp6_row([0; 16], 8088, 62)),
        );
        fake.fd(3, Some("socket:[61]"));
        fake.fd(6, Some("socket:[64]"));
        fake.fd(4, Some("socket:[62]"));
        let ipv4 = establish_at(fake.root(), 77, 8088, LOOPBACK).unwrap();
        assert_eq!(
            ipv4.scan(None),
            (Probe::NotHolding, None),
            "`::1`, `::127.0.0.1` and an IPv6 wildcard are not reached by 127.0.0.1"
        );
        let ipv6 = establish_at(fake.root(), 77, 8088, LOOPBACK6).unwrap();
        let (probe, hint) = ipv6.scan(None);
        assert_eq!(probe, Probe::Holds);
        assert!(
            matches!(hint, Some((_, _, true))),
            "found in the IPv6 table"
        );
        // the cheap check follows the IPv6 table too
        assert!(matches!(ipv6.quick(hint), Quick::Answer(Probe::Holds, _)));
        // No IPv6 table at all.
        fs::remove_file(fake.root().join("net/tcp6")).unwrap();
        fake.net("tcp", &tcp_row(LO, 8088, 63));
        fake.fd(5, Some("socket:[63]"));
        assert_eq!(ipv4.scan(None), (Probe::Holds, Some((5, 63, false))));
    }

    /// A process whose descriptor table cannot be read (another user's) is "cannot tell", not "does not hold". Skipped as root, which can
    /// read everything.
    #[test]
    fn an_unreadable_descriptor_table_is_not_a_no() {
        use std::os::unix::fs::PermissionsExt;
        let fake = FakeProc::new(55);
        fake.stat(3);
        fake.net("tcp", &tcp_row(LO, 8088, 41));
        fake.fd(3, Some("socket:[41]"));
        let watch = establish_at(fake.root(), 55, 8088, LOOPBACK).unwrap();
        let (_, hint) = watch.scan(None);
        assert_eq!(hint, Some((3, 41, false)));
        let fds = fake.root().join("55").join("fd");
        fs::set_permissions(&fds, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&fds).is_ok() {
            fs::set_permissions(&fds, fs::Permissions::from_mode(0o755)).unwrap();
            return; // root
        }
        // The cheap check cannot read the link either (no search permission on the directory).
        assert!(matches!(
            watch.quick(hint),
            Quick::Answer(Probe::Unavailable, _)
        ));
        assert_eq!(watch.scan(None), (Probe::Unavailable, None));
        fs::set_permissions(&fds, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(watch.quick(hint), Quick::Answer(Probe::Holds, _)));
    }

    /// **Descriptors that cannot be read during the scan** (the directory lists them, the link cannot be read: a restricted `/proc`): if the
    /// socket is found among the descriptors that can be read, it holds; if it is not found and some could not be read, the answer is
    /// "cannot tell" — never a "no". A descriptor that merely vanished (closed meanwhile) is skipped. Skipped as root.
    #[test]
    fn a_scan_that_could_not_read_some_descriptors_does_not_say_no() {
        use std::os::unix::fs::PermissionsExt;
        let fake = FakeProc::new(66);
        fake.stat(3);
        fake.net("tcp", &tcp_row(LO, 8088, 41));
        // the directory is readable but not searchable: the entries are listed, their links cannot be read
        fake.fd(3, Some("socket:[41]"));
        fake.fd(4, Some("/dev/null"));
        let fds = fake.root().join("66").join("fd");
        fs::set_permissions(&fds, fs::Permissions::from_mode(0o444)).unwrap();
        if fs::read_link(fds.join("3")).is_ok() {
            fs::set_permissions(&fds, fs::Permissions::from_mode(0o755)).unwrap();
            return; // root, or a system that lets us through
        }
        let watch = establish_at(fake.root(), 66, 8088, LOOPBACK).unwrap();
        assert_eq!(
            watch.scan(None),
            (Probe::Unavailable, None),
            "the socket may be among the descriptors that could not be read"
        );
        fs::set_permissions(&fds, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(watch.scan(None), (Probe::Holds, Some((3, 41, false))));
        // a dangling link (the descriptor was closed between listing and reading) is just skipped
        fake.fd(3, None);
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
    }

    /// A /proc without a TCP table cannot be asked: "cannot tell" (the caller asks the slow question), not "the process is gone".
    #[test]
    fn a_host_without_a_tcp_table_cannot_be_asked() {
        let fake = FakeProc::new(88);
        fake.stat(4);
        fake.fd(3, Some("socket:[41]"));
        let watch = establish_at(fake.root(), 88, 8088, LOOPBACK).unwrap();
        assert_eq!(watch.scan(None), (Probe::Unavailable, None));
        // the cheap check with a hint cannot read the table either
        assert!(matches!(
            watch.quick(Some((3, 41, false))),
            Quick::Answer(Probe::Unavailable, _)
        ));
        // And a process that is gone is still gone.
        fs::remove_dir_all(fake.root().join("88")).unwrap();
        assert_eq!(watch.scan(None), (Probe::NotHolding, None));
    }
}

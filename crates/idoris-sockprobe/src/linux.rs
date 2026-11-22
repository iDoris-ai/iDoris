//! Linux: `/proc`. Safe code only (`std::fs`).
//!
//! The parsers are pure functions of the text the kernel writes, and the glue takes the root of the proc tree as a parameter: both are
//! compiled (and tested, against a fake tree in a temporary directory) on every platform; only [`establish`] with the real `/proc` is
//! Linux-only. Per request the watch reads `/proc/<pid>/stat` (is it still the same process: the start time in clock ticks since boot) and
//! reads the link `/proc/<pid>/fd/<fd>` (is it still the same socket: `socket:[<inode>]`). The tables `/proc/net/tcp{,6}` are read only
//! to (re)locate the socket, when the descriptor is not known or no longer points at it.
//!
//! A listening socket that stays open but stops listening (`shutdown(2)` on it) is not noticed by the fast path; it is noticed the next
//! time the table is read. No engine does that.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::Probe;

/// The real proc tree.
#[cfg(target_os = "linux")]
const PROC: &str = "/proc";

/// The descriptor and socket inode the listening socket had at the last check.
pub(crate) type Hint = (i32, u64);

#[derive(Debug)]
pub(crate) struct Watch {
    root: PathBuf,
    pid: u32,
    port: u16,
    /// `starttime` (field 22 of `/proc/<pid>/stat`), pinned when the watch was established.
    started: u64,
}

#[cfg(target_os = "linux")]
pub(crate) fn establish(pid: u32, port: u16) -> Option<(Watch, Option<Hint>)> {
    establish_at(Path::new(PROC), pid, port)
}

pub(crate) fn establish_at(root: &Path, pid: u32, port: u16) -> Option<(Watch, Option<Hint>)> {
    if pid < 2 || i32::try_from(pid).is_err() {
        return None;
    }
    let started = match read_start(root, pid) {
        Ok(Some(started)) => started,
        _ => return None,
    };
    let watch = Watch {
        root: root.to_owned(),
        pid,
        port,
        started,
    };
    let hint = watch.locate().ok().flatten();
    Some((watch, hint))
}

impl Watch {
    pub(crate) fn check(&self, hint: Option<Hint>) -> (Probe, Option<Hint>) {
        match self.find(hint) {
            Ok(Some(found)) => (Probe::Holds, Some(found)),
            // The process (or its descriptor table) is gone: it holds nothing.
            Ok(None) => (Probe::NotHolding, None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Probe::NotHolding, None),
            // Not allowed to look (another user's process), or an unexpected error: not a "no", the caller decides.
            Err(_) => (Probe::Unavailable, hint),
        }
    }

    /// The descriptor and socket inode of a LISTEN socket of this process on the port at a target address, if there is one.
    fn find(&self, hint: Option<Hint>) -> io::Result<Option<Hint>> {
        // 1. Is it still the process we registered? (Another process under the same pid, or a zombie, is not.)
        match read_start(&self.root, self.pid)? {
            Some(started) if started == self.started => {}
            _ => return Ok(None),
        }
        // 2. The fast path: the descriptor still points at the same socket.
        if let Some((fd, inode)) = hint {
            let link = self
                .root
                .join(self.pid.to_string())
                .join("fd")
                .join(fd.to_string());
            match fs::read_link(link) {
                Ok(target) if socket_inode(&target.to_string_lossy()) == Some(inode) => {
                    return Ok(Some((fd, inode)));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        // 3. The slow path: find the socket again.
        self.locate()
    }

    /// The descriptor and socket inode of a LISTEN socket of this process on the port at a target address.
    fn locate(&self) -> io::Result<Option<Hint>> {
        let mut listening = Vec::new();
        for (name, v6) in [("tcp", false), ("tcp6", true)] {
            let text = match fs::read_to_string(self.root.join("net").join(name)) {
                Ok(text) => text,
                // No IPv6 on this host.
                Err(error) if v6 && error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            listening.extend(
                parse_net_tcp(&text, v6)
                    .into_iter()
                    .filter(|entry| entry.port == self.port && entry.target)
                    .map(|entry| entry.inode),
            );
        }
        if listening.is_empty() {
            return Ok(None);
        }
        for entry in fs::read_dir(self.root.join(self.pid.to_string()).join("fd"))? {
            let Ok(entry) = entry else { continue };
            let Ok(target) = fs::read_link(entry.path()) else {
                continue;
            };
            let Some(inode) = socket_inode(&target.to_string_lossy()) else {
                continue;
            };
            if listening.contains(&inode)
                && let Some(fd) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<i32>().ok())
            {
                return Ok(Some((fd, inode)));
            }
        }
        Ok(None)
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

/// One LISTEN entry of `/proc/net/tcp{,6}`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ListenEntry {
    pub(crate) port: u16,
    /// Bound to an address a client connecting to `127.0.0.1` reaches (the IPv4 loopback address, a wildcard).
    pub(crate) target: bool,
    pub(crate) inode: u64,
}

/// The LISTEN (`0A`) rows of `/proc/net/tcp` or `tcp6`. Columns: `sl local_address rem_address st tx:rx tr:tm->when retrnsmt uid timeout
/// inode ...`; the address is the kernel's in-memory words in hex (so the bytes are those of `to_ne_bytes`), the port is in host order.
pub(crate) fn parse_net_tcp(text: &str, v6: bool) -> Vec<ListenEntry> {
    let mut entries = Vec::new();
    for line in text.lines().skip(1) {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 10 || columns[3] != "0A" {
            continue;
        }
        let Some((address, port)) = columns[1].split_once(':') else {
            continue;
        };
        let (Ok(port), Some(bytes), Ok(inode)) = (
            u16::from_str_radix(port, 16),
            address_bytes(address, v6),
            columns[9].parse::<u64>(),
        ) else {
            continue;
        };
        entries.push(ListenEntry {
            port,
            target: is_target_address(&bytes),
            inode,
        });
    }
    entries
}

/// The 4 or 16 address bytes in network order from the hex words.
fn address_bytes(hex: &str, v6: bool) -> Option<Vec<u8>> {
    let words = if v6 { 4 } else { 1 };
    if hex.len() != words * 8 {
        return None;
    }
    let mut bytes = Vec::with_capacity(words * 4);
    for word in 0..words {
        let value = u32::from_str_radix(hex.get(word * 8..word * 8 + 8)?, 16).ok()?;
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    Some(bytes)
}

/// IPv4 `127.0.0.1` or `0.0.0.0`; IPv6 `::` (a dual-stack wildcard serves IPv4 too) or the mapped forms of those two.
fn is_target_address(bytes: &[u8]) -> bool {
    match bytes.len() {
        4 => bytes == [127, 0, 0, 1] || bytes == [0, 0, 0, 0],
        16 => {
            let zero = bytes.iter().all(|byte| *byte == 0);
            let mapped =
                bytes[..10].iter().all(|byte| *byte == 0) && bytes[10] == 0xff && bytes[11] == 0xff;
            zero || (mapped && (bytes[12..] == [127, 0, 0, 1] || bytes[12..] == [0, 0, 0, 0]))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::symlink;

    use super::*;

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

    /// Rows as the kernel prints them on a little-endian host: `0100007F` is 127.0.0.1, `00000000` the wildcard.
    #[test]
    fn only_listening_rows_on_a_target_address_are_targets() {
        let loopback = u32::from_ne_bytes([127, 0, 0, 1]);
        let wildcard = 0_u32;
        let elsewhere = u32::from_ne_bytes([10, 1, 2, 3]);
        let text = format!(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             0: {loopback:08X}:1F98 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 111 1 0000000000000000 100 0 0 10 0\n\
             1: {wildcard:08X}:1F99 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 222 1 0000000000000000 100 0 0 10 0\n\
             2: {elsewhere:08X}:1F9A 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 333 1 0000000000000000 100 0 0 10 0\n\
             3: {loopback:08X}:1F9B {loopback:08X}:C350 01 00000000:00000000 00:00000000 00000000  501        0 444 1 0000000000000000 100 0 0 10 0\n"
        );
        assert_eq!(
            parse_net_tcp(&text, false),
            vec![
                ListenEntry {
                    port: 0x1F98,
                    target: true,
                    inode: 111
                },
                ListenEntry {
                    port: 0x1F99,
                    target: true,
                    inode: 222
                },
                ListenEntry {
                    port: 0x1F9A,
                    target: false,
                    inode: 333
                },
            ],
            "the established row (state 01) is not listed"
        );
    }

    #[test]
    fn ipv6_rows_use_four_words_and_a_dual_stack_wildcard_is_a_target() {
        let words = |bytes: [u8; 16]| -> String {
            bytes
                .chunks(4)
                .map(|word| {
                    format!(
                        "{:08X}",
                        u32::from_ne_bytes([word[0], word[1], word[2], word[3]])
                    )
                })
                .collect()
        };
        let any = words([0; 16]);
        let mut loopback6 = [0_u8; 16];
        loopback6[15] = 1;
        let loopback6 = words(loopback6);
        let mut mapped = [0_u8; 16];
        mapped[10] = 0xff;
        mapped[11] = 0xff;
        mapped[12..].copy_from_slice(&[127, 0, 0, 1]);
        let mapped = words(mapped);
        let text = format!(
            "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             0: {any}:1F98 {any}:0000 0A 00000000:00000000 00:00000000 00000000  501        0 11 1 0000000000000000 100 0 0 10 0\n\
             1: {loopback6}:1F99 {any}:0000 0A 00000000:00000000 00:00000000 00000000  501        0 22 1 0000000000000000 100 0 0 10 0\n\
             2: {mapped}:1F9A {any}:0000 0A 00000000:00000000 00:00000000 00000000  501        0 33 1 0000000000000000 100 0 0 10 0\n"
        );
        let entries = parse_net_tcp(&text, true);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.inode, e.target))
                .collect::<Vec<_>>(),
            vec![(11, true), (22, false), (33, true)],
            "`::` and the mapped loopback are targets, `::1` is not"
        );
    }

    /// A fake proc tree: `net/tcp{,6}`, and `<pid>/stat` with a symlink per descriptor.
    struct FakeProc {
        dir: tempfile::TempDir,
        pid: u32,
    }

    /// One LISTEN row of `/proc/net/tcp`: the address as the kernel prints it on a little-endian host.
    fn tcp_row(address: [u8; 4], port: u16, inode: u64) -> String {
        format!(
            "0: {:08X}:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
            u32::from_ne_bytes(address)
        )
    }

    const TCP_HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

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

    const LOOPBACK: [u8; 4] = [127, 0, 0, 1];

    /// The watch follows a socket through the fast path, notices when the descriptor is closed, finds it again under another descriptor
    /// number, notices that the descriptor now names a different socket, and notices a recycled pid.
    #[test]
    fn the_watch_follows_a_socket_through_a_fake_proc_tree() {
        let fake = FakeProc::new(4242);
        fake.stat(1_000);
        fake.net("tcp", &tcp_row(LOOPBACK, 8088, 555));
        fake.fd(3, Some("socket:[555]"));
        fake.fd(4, Some("/dev/null"));

        let (watch, hint) = establish_at(fake.root(), 4242, 8088).unwrap();
        assert_eq!(hint, Some((3, 555)));
        assert_eq!(watch.check(hint), (Probe::Holds, Some((3, 555))));

        // The descriptor now names another socket (the number was reused; that socket is not listening on the port), or something that is
        // not a socket at all: the fast path does not believe the hint and the scan finds nothing.
        fake.fd(3, Some("socket:[999]"));
        assert_eq!(watch.check(hint), (Probe::NotHolding, None));
        fake.fd(3, Some("/dev/null"));
        assert_eq!(watch.check(hint), (Probe::NotHolding, None));
        fake.fd(3, Some("socket:[555]"));
        assert_eq!(watch.check(hint), (Probe::Holds, Some((3, 555))));

        // The listener is closed.
        fake.fd(3, None);
        fake.net("tcp", &tcp_row(LOOPBACK, 1, 0));
        assert_eq!(watch.check(hint), (Probe::NotHolding, None));

        // The same process listens again, under another descriptor number and another inode.
        fake.net("tcp", &tcp_row(LOOPBACK, 8088, 777));
        fake.fd(9, Some("socket:[777]"));
        assert_eq!(watch.check(None), (Probe::Holds, Some((9, 777))));

        // A different process under the same pid (another start time) that holds the very socket is not the engine; and when the process
        // is gone altogether there is nothing to hold.
        fake.stat(2_000);
        assert_eq!(watch.check(Some((9, 777))).0, Probe::NotHolding);
        fs::remove_dir_all(fake.root().join("4242")).unwrap();
        assert_eq!(watch.check(Some((9, 777))).0, Probe::NotHolding);

        // Pids that cannot be an engine, even when a /proc entry exists for them, and a pid that is not there.
        for never in [0, 1] {
            let entry = FakeProc::new(never);
            entry.stat(5);
            assert!(
                establish_at(entry.root(), never, 8088).is_none(),
                "pid {never}"
            );
        }
        assert!(establish_at(fake.root(), 99_999, 8088).is_none());
    }

    /// Only a LISTEN row **on a target address and on the watched port** that belongs to a descriptor of **this process** counts.
    #[test]
    fn only_a_target_listener_of_this_process_on_the_port_counts() {
        let fake = FakeProc::new(31);
        fake.stat(5);
        // Port 8088 listens on another address (inode 888, held by this process): not the target address. Port 9000 listens on loopback
        // (inode 900, held by this process) but is another port. Inode 700 listens on loopback:8088 but belongs to another process.
        fake.net(
            "tcp",
            &format!(
                "{}{}{}",
                tcp_row([10, 1, 2, 3], 8088, 888),
                tcp_row(LOOPBACK, 9000, 900),
                tcp_row(LOOPBACK, 8088, 700)
            ),
        );
        fake.fd(5, Some("socket:[888]"));
        fake.fd(6, Some("socket:[900]"));
        let (watch, hint) = establish_at(fake.root(), 31, 8088).unwrap();
        assert_eq!(hint, None);
        assert_eq!(watch.check(None), (Probe::NotHolding, None));
        // The wildcard counts, once this process holds it.
        fake.net("tcp", &tcp_row([0, 0, 0, 0], 8088, 901));
        fake.fd(7, Some("socket:[901]"));
        assert_eq!(watch.check(None), (Probe::Holds, Some((7, 901))));
    }

    /// A process whose descriptor table cannot be read (another user's) is "cannot tell", not "does not hold". Skipped as root, which can
    /// read everything.
    #[test]
    fn an_unreadable_descriptor_table_is_not_a_no() {
        use std::os::unix::fs::PermissionsExt;
        let fake = FakeProc::new(55);
        fake.stat(3);
        fake.net("tcp", &tcp_row(LOOPBACK, 8088, 41));
        fake.fd(3, Some("socket:[41]"));
        let (watch, hint) = establish_at(fake.root(), 55, 8088).unwrap();
        assert_eq!(hint, Some((3, 41)));
        let fds = fake.root().join("55").join("fd");
        fs::set_permissions(&fds, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&fds).is_ok() {
            fs::set_permissions(&fds, fs::Permissions::from_mode(0o755)).unwrap();
            return; // root
        }
        // The fast path cannot read the link either (no search permission on the directory).
        assert_eq!(watch.check(hint), (Probe::Unavailable, hint));
        assert_eq!(watch.check(None), (Probe::Unavailable, None));
        fs::set_permissions(&fds, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(watch.check(hint), (Probe::Holds, hint));
    }

    /// `/proc/net/tcp6` is read too: an IPv6 dual-stack wildcard is a target (and `::1` is not); a host without IPv6 has no `tcp6` file.
    #[test]
    fn the_ipv6_table_is_read_too_and_a_missing_one_is_fine() {
        let words = |bytes: [u8; 16]| -> String {
            bytes
                .chunks(4)
                .map(|word| {
                    format!(
                        "{:08X}",
                        u32::from_ne_bytes([word[0], word[1], word[2], word[3]])
                    )
                })
                .collect()
        };
        let row = |bytes: [u8; 16], port: u16, inode: u64| {
            format!(
                "0: {}:{port:04X} {}:0000 0A 00000000:00000000 00:00000000 00000000  501        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
                words(bytes),
                words([0; 16])
            )
        };
        let fake = FakeProc::new(77);
        fake.stat(9);
        fake.net("tcp", "");
        let mut loopback6 = [0_u8; 16];
        loopback6[15] = 1;
        // `::127.0.0.1` (the deprecated IPv4-compatible form, not the mapped one) is not a target either.
        let mut compatible = [0_u8; 16];
        compatible[12..].copy_from_slice(&[127, 0, 0, 1]);
        fake.net(
            "tcp6",
            &(row(loopback6, 8088, 61) + &row(compatible, 8088, 64) + &row([0; 16], 8088, 62)),
        );
        fake.fd(3, Some("socket:[61]"));
        fake.fd(6, Some("socket:[64]"));
        let (watch, hint) = establish_at(fake.root(), 77, 8088).unwrap();
        assert_eq!(hint, None, "`::1` and `::127.0.0.1` only");
        assert_eq!(watch.check(None).0, Probe::NotHolding);
        fake.fd(4, Some("socket:[62]"));
        assert_eq!(watch.check(None), (Probe::Holds, Some((4, 62))));
        // No IPv6 table at all.
        fs::remove_file(fake.root().join("net/tcp6")).unwrap();
        fake.net("tcp", &tcp_row(LOOPBACK, 8088, 63));
        fake.fd(5, Some("socket:[63]"));
        assert_eq!(watch.check(None), (Probe::Holds, Some((5, 63))));
    }
}

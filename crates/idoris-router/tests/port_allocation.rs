//! `tests/support/ports.rs` 的 `free_port()`：先挑好、过一会儿才由别的进程去绑的端口，不能是 `bind(0)` 取个号码再放掉的那种
//! （放掉之后内核可以把同一个号码发给 iDoris 自己的 `IDORIS_PORT=0` 监听器，托管卡的端口上就有了 iDoris 自己；
//! 来龙去脉见 `ports.rs` 的模块说明）。
//!
//! 这里验证它的三条性质：号码在内核临时端口范围之外；同一个进程里（含多线程）从不重复；内核自己的 `bind(0)` 确实从来不落进这个区间
//! （前提，换一个内核或改过 `ip_local_port_range` 就可能不成立，那时 `private_range()` 是 `None`，helper 退回老办法，这些测试说明并跳过）。
//!
//! 末尾两条是静态守卫（读源文件，不起进程）：新的测试文件不许再自己写一份 `bind(0)` 取号再放掉的 `free_port()`；
//! 还在自己写的旧文件列在清单里，迁走一个就得从清单里删一个。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ports.rs"]
mod ports;

use std::collections::HashSet;
use std::net::{Ipv4Addr, TcpListener};
use std::thread;

use ports::{PRIVATE_HIGH, PRIVATE_LOW, ephemeral_floor, free_port, private_range};

/// 私有区间在这台机器上不可用（内核的临时范围盖住了它）：明确写一行 `SKIPPED`，不静默。
fn private_range_or_skip(test: &str) -> Option<(u16, u16)> {
    let range = private_range();
    if range.is_none() {
        eprintln!(
            "SKIPPED {test}: the kernel's ephemeral port range starts at {} and covers the private range {PRIVATE_LOW}..={PRIVATE_HIGH} (the test did not run)",
            ephemeral_floor()
        );
    }
    range
}

#[test]
fn ports_come_from_the_private_range_below_the_kernels_ephemeral_range_and_can_be_bound() {
    let Some((low, high)) = private_range_or_skip("ports_come_from_the_private_range") else {
        return;
    };
    let floor = ephemeral_floor();
    assert!(
        high < floor,
        "the private range {low}..={high} must end below the kernel's ephemeral floor {floor}"
    );
    for _ in 0..120 {
        let port = free_port();
        assert!(
            (low..=high).contains(&port),
            "{port} is outside the private range {low}..={high}"
        );
        // 此刻没人监听：绑得上；绑上之后再放掉，号码依然可用。
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap_or_else(|error| {
            panic!("{port} must be bindable right after it is issued: {error}")
        });
        drop(listener);
    }
}

#[test]
fn a_port_is_never_issued_twice_in_one_process_even_from_many_threads() {
    if private_range_or_skip("a_port_is_never_issued_twice").is_none() {
        return;
    }
    let handles: Vec<_> = (0..8)
        .map(|_| thread::spawn(|| (0..100).map(|_| free_port()).collect::<Vec<_>>()))
        .collect();
    let mut seen = HashSet::new();
    let mut issued = 0_usize;
    for handle in handles {
        for port in handle.join().unwrap() {
            issued += 1;
            assert!(
                seen.insert(port),
                "{port} was issued twice: two parallel tests would fight over it"
            );
        }
    }
    assert_eq!(issued, 800);
}

#[test]
fn the_kernels_own_bind_zero_never_lands_in_the_private_range() {
    let Some((low, high)) = private_range_or_skip("the_kernels_own_bind_zero") else {
        return;
    };
    // 前提：iDoris 自己的 `IDORIS_PORT=0` 监听器、别的用例的 `bind(0)` 拿到的号码都在私有区间之外，所以它们抢不走 `free_port()` 发出的号码。
    // 与 helper 的实现无关地直接问内核（设了 `SO_REUSEADDR` 的监听器：std 的 `TcpListener::bind` 在 unix 上就是这样）。
    for _ in 0..300 {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(
            !(low..=high).contains(&port),
            "the kernel handed out {port}, inside the private range {low}..={high}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 守卫：不再有新的测试文件自己写一份 `bind(0)` 取号再放掉的 `free_port()`
// ---------------------------------------------------------------------------------------------

/// 这些文件还在用自己的 `free_port()`（`bind(0)`、读号码、放掉）。它们把端口马上交给子进程，窗口比托管卡的短得多，没有在 CI 里出现过
/// 失败，逐步迁到 `support/ports.rs`。迁完一个就从清单里去掉（清单里的文件若已不再自己定义 `free_port`，下面第二条守卫会点名）；
/// 新文件不许再自己写一份。
const LEGACY_LOCAL_FREE_PORT: &[&str] = &[
    "allocated_budget_cli.rs",
    "cli.rs",
    "cloud_child_safety_binary.rs",
    "cloud_form_startup.rs",
    "cloud_tier_process.rs",
    "engine_lifecycle.rs",
    "form_startup.rs",
    "portable_startup.rs",
    "setup_upstream.rs",
    "upstream_flavor_serve.rs",
];

fn files_defining_their_own_free_port() -> Vec<String> {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    for krate in ["idoris-router", "idoris-upstream"] {
        let mut stack = vec![crates.join(krate).join("tests")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    // 帮手本身与本文件不算（本文件的说明里会提到这个名字）。
                    if name == "ports.rs" || name == "port_allocation.rs" {
                        continue;
                    }
                    // 整行注释不算。
                    let code: String = std::fs::read_to_string(&path)
                        .unwrap()
                        .lines()
                        .filter(|line| !line.trim_start().starts_with("//"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if code.contains("fn free_port") {
                        found.push(name);
                    }
                }
            }
        }
    }
    found.sort();
    found
}

#[test]
fn no_test_file_outside_the_legacy_list_defines_its_own_free_port() {
    let offenders: Vec<String> = files_defining_their_own_free_port()
        .into_iter()
        .filter(|name| !LEGACY_LOCAL_FREE_PORT.contains(&name.as_str()))
        .collect();
    assert!(
        offenders.is_empty(),
        "use `#[path = \"support/ports.rs\"] mod ports; use ports::free_port;` instead of a local bind(0)-and-release free_port(): {offenders:?}"
    );
}

#[test]
fn the_legacy_list_only_names_files_that_still_define_their_own_free_port() {
    let defining = files_defining_their_own_free_port();
    let stale: Vec<&&str> = LEGACY_LOCAL_FREE_PORT
        .iter()
        .filter(|name| !defining.iter().any(|defined| defined == **name))
        .collect();
    assert!(
        stale.is_empty(),
        "these files were migrated to support/ports.rs (or renamed): remove them from LEGACY_LOCAL_FREE_PORT: {stale:?}"
    );
}

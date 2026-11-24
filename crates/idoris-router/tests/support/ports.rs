//! 测试里「先挑一个端口、过一会儿才让另一个进程去绑」用的端口分配。
//!
//! # 为什么不能 `bind(0)` 取个号码、立刻放掉
//!
//! 老写法是 `TcpListener::bind("127.0.0.1:0")`、读出端口、丢掉监听器，再把这个号码写进托管卡的 endpoint 或交给子进程。
//! 号码被放掉的那一刻起，内核就可以把它发给**任何一个**随后 `bind(0)` 的人，而同一个测试进程里并行的别的用例、以及这个用例
//! 自己起的 iDoris（`IDORIS_PORT=0`、`IDORIS_ADMIN_PORT=0`，动态端口是 launcher 的真实用法）正是这样的人。Linux 对设了 `SO_REUSEADDR`
//! 的监听器（tokio、std 的 `TcpListener::bind` 都设）在默认临时端口范围（32768–60999）的**前半段**里随机挑一个奇数端口，也就是
//! 32768–46883 里的七千多个奇数之一，被放掉的那个号码每绑一个新监听器就有约 1/7000 的机会落到它手里；一次 `lifecycle_managed`
//! 要起几十个 iDoris、各绑两个端口，所以这是一个百分之几一次的真实失败率，只在 Linux CI 上出现过。
//!
//! 撞上之后的症状取决于谁拿到了托管卡的端口，两种都在 CI 里出现过：
//!
//! - iDoris 自己的**数据端口**：管理器探测的就是 iDoris 自己，门控放行之前它对 `/v1/*` 一律回 503，被当作「引擎正在启动或很忙，等它」，
//!   不拉起第二个，一直等到启动超时（90 秒）：`/health` 永远是 `phase:"starting"`、`omlx` 是 `starting`、`owned:false`，没有 `reason`。
//! - iDoris 自己的**管理端口**：回的不是 oMLX 的形状，`failed` + `port_in_use`，`phase:"degraded"`。
//!
//! # 这里怎么做
//!
//! 从内核临时端口范围**之外**的一段私有区间（20000–29999，低于 Linux 的 32768 与 macOS/BSD 的 49152）里随机挑：内核的 `bind(0)` 与
//! `connect` 永远不会把这个区间里的号码发给别人，所以没有人能在空档里抢走它。同一个进程里发出去的号码不重复（并行用例拿到同一个
//! 号码、各自的引擎抢同一个端口）；每个候选号码最后还要确认此刻真的能绑上（别的程序可能占着它）。
//!
//! 不防的：同一台机器上**另一个**同样用这个 helper 的测试进程（几个 worktree 同时跑测试）恰好在同一两秒里挑中同一个号码，
//! 约万分之几；之前是每次都有百分之几。不为它给每个号码占一个文件锁：每个锁要占一个文件描述符直到进程结束，macOS 的默认上限
//! 才 256，而这些测试同时开着几十对管道。
//!
//! 内核的临时范围被改到盖住这个区间时（`ip_local_port_range` 的下界不高于 29999 加一千），这个区间不再安全，退回老办法
//! （并保留它的竞争），而不是假装安全。
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

/// 私有区间：低于两个平台默认的临时端口范围（Linux 32768 起，macOS/BSD 49152 起）。
pub const PRIVATE_LOW: u16 = 20_000;
pub const PRIVATE_HIGH: u16 = 29_999;
/// 区间至少要有这么宽才算数（临时范围被改到盖住大半个区间时不用它）。
const MIN_SPAN: u16 = 1_000;

/// 本进程已发出的号码。
fn issued() -> &'static Mutex<HashSet<u16>> {
    static ISSUED: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    ISSUED.get_or_init(Mutex::default)
}

/// 内核临时端口范围的下界。Linux 读 `/proc`（容器与 CI 镜像可能改过）；读不到（macOS/BSD）按它们的默认值 49152。
pub fn ephemeral_floor() -> u16 {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse().ok())
        .unwrap_or(49_152)
}

/// 私有区间的实际范围（含两端）；内核的临时范围盖住了它的大半时是 `None`。
pub fn private_range() -> Option<(u16, u16)> {
    let high = PRIVATE_HIGH.min(ephemeral_floor().saturating_sub(1));
    (high >= PRIVATE_LOW + MIN_SPAN - 1).then_some((PRIVATE_LOW, high))
}

fn random() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    // `RandomState` 每个实例带操作系统给的随机种子；再混进一个递增的序号，同一线程里连续取也不会相同。
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(SEQUENCE.fetch_add(1, Ordering::Relaxed));
    hasher.finish()
}

/// 此刻本机 127.0.0.1 上能不能绑这个端口（绑上立刻放掉）。
fn bindable(port: u16) -> bool {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).is_ok()
}

/// 老办法：让内核挑一个，读出来，放掉。只在私有区间不可用时退回到它。
fn os_assigned() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 一个事先挑好的本机端口：此刻没人监听，本进程不会再发给别人，而且内核不会把它当作临时端口发给任何 `bind(0)`、`connect` 的人
/// （见模块说明）。
pub fn free_port() -> u16 {
    let Some((low, high)) = private_range() else {
        return os_assigned();
    };
    let span = u64::from(high - low) + 1;
    let mut issued = issued().lock().unwrap_or_else(PoisonError::into_inner);
    for _ in 0..4_000 {
        let candidate = low + u16::try_from(random() % span).expect("the span fits in u16");
        if !issued.contains(&candidate) && bindable(candidate) {
            issued.insert(candidate);
            return candidate;
        }
    }
    panic!("no usable port found in {low}..={high} after 4000 attempts");
}

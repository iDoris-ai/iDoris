//! 客户端来源（计算服务 S5）：只信任配置里列出的反向代理。
//!
//! TLS 由反向代理终止，所以 iDoris 看到的 TCP 对端永远是代理。真正的客户端地址只能从代理写的
//! `X-Forwarded-For` 里取，而这个头**谁都能写**：直接连上来的客户端可以写任何值。规则只有一条：
//!
//! > 只有当 TCP 对端本身在 `trusted_proxies` 里，才读 `X-Forwarded-For`；
//! > 对端不在名单里，这个头一律忽略，客户端地址就是对端地址。
//!
//! 对端受信时，从右往左（最靠近我们的一跳开始）走链：**跳过受信代理的地址，第一个不受信的地址就是客户端**。
//! 客户端自己在链最左边伪造的那些地址永远走不到（走到第一个不受信的地址就停）。链里有任何一个读不懂的
//! 条目（`unknown`、乱码），或链太长，一律退回对端地址——保守，不猜。
//!
//! 只认 `X-Forwarded-For`（不认 `X-Real-IP`、`Forwarded`）：反向代理的配置里只需要写一个头，攻击面少一块。
//!
//! # 来源地址是什么、不是什么
//!
//! 来源地址**只是辅助信号**（设计稿 §9 第 3 条）：它只用来给「没有凭证的请求」分一个软配额（见 `abuse`），
//! 永远不用来拦截带着有效凭证的请求，也不进任何授权判断。学校的共享出口下，一个班的学生是同一个地址。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use axum::http::HeaderMap;

/// 一条受信地址：单个 IP，或 CIDR 网段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

/// CIDR 解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CidrError {
    #[error("{0:?} is not an IP address or CIDR block")]
    Invalid(String),
    #[error("prefix length {prefix} is out of range for {network} (max {max})")]
    PrefixOutOfRange {
        network: IpAddr,
        prefix: u8,
        max: u8,
    },
    #[error("{text:?} has host bits set; write the network address {network}/{prefix}")]
    HostBitsSet {
        text: String,
        network: IpAddr,
        prefix: u8,
    },
}

impl Cidr {
    /// 网段是否包含 `ip`。IPv4 映射的 IPv6 地址（`::ffff:a.b.c.d`）按它的 IPv4 地址比；IPv4 网段不含真正的 IPv6 地址。
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                mask_v4(u32::from(network), self.prefix) == mask_v4(u32::from(ip), self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                mask_v6(u128::from(network), self.prefix) == mask_v6(u128::from(ip), self.prefix)
            }
            _ => false,
        }
    }
}

fn mask_v4(bits: u32, prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        1..=31 => bits & (u32::MAX << (32 - u32::from(prefix))),
        _ => bits,
    }
}

fn mask_v6(bits: u128, prefix: u8) -> u128 {
    match prefix {
        0 => 0,
        1..=127 => bits & (u128::MAX << (128 - u32::from(prefix))),
        _ => bits,
    }
}

impl FromStr for Cidr {
    type Err = CidrError;

    /// `a.b.c.d`（= `/32`）、`a.b.c.d/p`、`x::y`（= `/128`）、`x::y/p`。**严格**：网段地址不能带主机位
    /// （`10.0.0.5/8` 是错误，不是悄悄变成 `10.0.0.0/8`）：这是一份安全配置，歧义一律拒绝。
    fn from_str(text: &str) -> Result<Self, CidrError> {
        let invalid = || CidrError::Invalid(text.to_string());
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let network = IpAddr::from_str(address.trim())
            .map_err(|_| invalid())?
            .to_canonical();
        let max: u8 = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(prefix) => {
                let prefix = prefix.trim();
                if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                let parsed: u8 = prefix.parse().map_err(|_| invalid())?;
                if parsed > max {
                    return Err(CidrError::PrefixOutOfRange {
                        network,
                        prefix: parsed,
                        max,
                    });
                }
                parsed
            }
        };
        let masked = match network {
            IpAddr::V4(ip) => IpAddr::V4(Ipv4Addr::from(mask_v4(u32::from(ip), prefix))),
            IpAddr::V6(ip) => IpAddr::V6(Ipv6Addr::from(mask_v6(u128::from(ip), prefix))),
        };
        if masked != network {
            return Err(CidrError::HostBitsSet {
                text: text.to_string(),
                network: masked,
                prefix,
            });
        }
        Ok(Self { network, prefix })
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// 受信的反向代理名单。空 = 没有任何受信代理：`X-Forwarded-For` 永远被忽略。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies(Vec<Cidr>);

impl TrustedProxies {
    pub fn new(entries: Vec<Cidr>) -> Self {
        Self(entries)
    }

    pub fn parse<'a>(entries: impl IntoIterator<Item = &'a str>) -> Result<Self, CidrError> {
        entries
            .into_iter()
            .map(Cidr::from_str)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|cidr| cidr.contains(ip))
    }
}

/// 一个请求的客户端来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddr {
    /// 用来归属辅助信号（软配额）的地址：受信代理转发的真实客户端，或者对端本身。
    pub ip: IpAddr,
    /// TCP 对端。没有连接信息（测试里直接驱动路由）时是 `None`。
    pub peer: Option<IpAddr>,
    /// `ip` 是不是从受信代理的 `X-Forwarded-For` 里取出来的。
    pub forwarded: bool,
}

/// `X-Forwarded-For` 里最多看这么多个条目，总长度也有上限：链是别人写的，不能让解析成本由别人决定。
const MAX_FORWARDED_ENTRIES: usize = 32;
const MAX_FORWARDED_BYTES: usize = 1024;

/// 解析一个 `X-Forwarded-For` 条目：`1.2.3.4`、`1.2.3.4:5678`、`2001:db8::1`、`[2001:db8::1]`、`[2001:db8::1]:5678`。
/// 其余（`unknown`、`_hidden`、乱码）是 `None`。
fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if !(tail.is_empty() || tail.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }
        return Ipv6Addr::from_str(host).ok().map(IpAddr::V6);
    }
    if let Ok(ip) = IpAddr::from_str(entry) {
        return Some(ip.to_canonical());
    }
    let (host, port) = entry.split_once(':')?;
    if !is_port(port) {
        return None;
    }
    Ipv4Addr::from_str(host).ok().map(IpAddr::V4)
}

fn is_port(text: &str) -> bool {
    !text.is_empty() && text.len() <= 5 && text.bytes().all(|b| b.is_ascii_digit())
}

/// 从 TCP 对端和请求头算出客户端来源。规则见模块文档。
pub fn resolve_client(
    trusted: &TrustedProxies,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
) -> ClientAddr {
    let Some(peer) = peer.map(|ip| ip.to_canonical()) else {
        // 没有对端信息就没有任何东西能证明「这是受信代理转发的」：不读 X-Forwarded-For。
        return ClientAddr {
            ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            peer: None,
            forwarded: false,
        };
    };
    let direct = ClientAddr {
        ip: peer,
        peer: Some(peer),
        forwarded: false,
    };
    if !trusted.contains(peer) {
        return direct;
    }
    // 对端受信：把所有 X-Forwarded-For 头按出现顺序拼成一条链
    let mut chain: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for value in headers.get_all("x-forwarded-for") {
        let Ok(text) = value.to_str() else {
            return direct;
        };
        total = total.saturating_add(text.len());
        if total > MAX_FORWARDED_BYTES {
            return direct;
        }
        chain.extend(text.split(','));
        if chain.len() > MAX_FORWARDED_ENTRIES {
            return direct;
        }
    }
    for entry in chain.into_iter().rev() {
        let Some(ip) = parse_forwarded_entry(entry) else {
            return direct;
        };
        if trusted.contains(ip) {
            continue;
        }
        return ClientAddr {
            ip,
            peer: Some(peer),
            forwarded: true,
        };
    }
    direct
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append("x-forwarded-for", value.parse().unwrap());
        }
        map
    }

    fn proxies(entries: &[&str]) -> TrustedProxies {
        TrustedProxies::parse(entries.iter().copied()).unwrap()
    }

    // ---------- CIDR ----------

    #[test]
    fn cidr_contains_follows_the_prefix_exactly() {
        let block: Cidr = "10.1.0.0/16".parse().unwrap();
        for inside in ["10.1.0.0", "10.1.255.255", "10.1.2.3"] {
            assert!(block.contains(ip(inside)), "{inside}");
        }
        for outside in ["10.0.255.255", "10.2.0.0", "11.1.0.1", "::1"] {
            assert!(!block.contains(ip(outside)), "{outside}");
        }
        let host: Cidr = "192.0.2.7".parse().unwrap();
        assert!(host.contains(ip("192.0.2.7")));
        assert!(!host.contains(ip("192.0.2.8")));
        let everything: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(everything.contains(ip("8.8.8.8")));
        assert!(
            !everything.contains(ip("2001:db8::1")),
            "an IPv4 block never contains a real IPv6 address"
        );
        let v6: Cidr = "fd00::/8".parse().unwrap();
        assert!(v6.contains(ip("fd12:3456::1")));
        assert!(!v6.contains(ip("fe80::1")));
        assert!(!v6.contains(ip("10.0.0.1")));
    }

    #[test]
    fn an_ipv4_mapped_ipv6_peer_matches_its_ipv4_block() {
        let block: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(block.contains(ip("::ffff:10.1.2.3")));
        assert!(!block.contains(ip("::ffff:11.1.2.3")));
        // 映射地址写成 v6 网段：规范化成 v4 之后前缀长度对不上，必须是明确的错误，不能悄悄放宽成别的网段
        assert!(matches!(
            "::ffff:10.0.0.0/104".parse::<Cidr>(),
            Err(CidrError::PrefixOutOfRange { .. })
        ));
        // 单个映射地址（/128 等价于 v4 的 /32）也按 v4 地址存
        let mapped_host: Cidr = "::ffff:10.0.0.7".parse().unwrap();
        assert!(mapped_host.contains(ip("10.0.0.7")));
        assert!(!mapped_host.contains(ip("10.0.0.8")));
    }

    #[test]
    fn malformed_and_ambiguous_cidrs_are_refused() {
        for bad in [
            "",
            "10.0.0.0/",
            "10.0.0.0/33",
            "10.0.0.0/-1",
            "10.0.0.0/8/8",
            "10.0.0.0/ 8x",
            "10.0.0.5/8",
            "256.0.0.1",
            "localhost",
            "::1/129",
            "fd00::1/8",
            "*",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad:?}");
        }
        assert_eq!(
            "10.0.0.5/8".parse::<Cidr>().unwrap_err(),
            CidrError::HostBitsSet {
                text: "10.0.0.5/8".into(),
                network: ip("10.0.0.0"),
                prefix: 8
            }
        );
    }

    #[test]
    fn cidr_display_round_trips() {
        for text in ["10.0.0.0/8", "127.0.0.1/32", "fd00::/8", "::1/128"] {
            assert_eq!(text.parse::<Cidr>().unwrap().to_string(), text);
        }
        // 单个地址写成 /32
        assert_eq!(
            "127.0.0.1".parse::<Cidr>().unwrap().to_string(),
            "127.0.0.1/32"
        );
    }

    // ---------- 客户端来源 ----------

    #[test]
    fn an_untrusted_peer_is_the_client_and_its_forwarded_header_is_ignored() {
        let trusted = proxies(&["10.0.0.0/8"]);
        let forged = headers(&["203.0.113.9, 198.51.100.1"]);
        let client = resolve_client(&trusted, Some(ip("192.0.2.50")), &forged);
        assert_eq!(client.ip, ip("192.0.2.50"));
        assert!(!client.forwarded);
        // 名单是空的：对任何对端都一样
        let none = TrustedProxies::default();
        let client = resolve_client(&none, Some(ip("10.0.0.1")), &forged);
        assert_eq!(client.ip, ip("10.0.0.1"));
        assert!(!client.forwarded);
        // 没有对端信息（直接驱动路由）：同样不读
        let client = resolve_client(&trusted, None, &forged);
        assert_eq!(client.peer, None);
        assert!(!client.forwarded);
        assert_eq!(client.ip, ip("0.0.0.0"));
    }

    #[test]
    fn a_trusted_proxy_reports_the_real_client_and_leftmost_forgeries_are_never_reached() {
        let trusted = proxies(&["10.0.0.0/8", "127.0.0.1"]);
        // 代理把它看到的对端地址追加在最右边；客户端伪造的在更左边
        let chain = headers(&["1.1.1.1, 2.2.2.2, 203.0.113.9"]);
        let client = resolve_client(&trusted, Some(ip("10.0.0.2")), &chain);
        assert_eq!(client.ip, ip("203.0.113.9"));
        assert!(client.forwarded);
        assert_eq!(client.peer, Some(ip("10.0.0.2")));
        // 链里有多跳受信代理：跳过它们
        let chain = headers(&["198.51.100.7, 10.0.0.9, 10.0.0.2"]);
        let client = resolve_client(&trusted, Some(ip("127.0.0.1")), &chain);
        assert_eq!(client.ip, ip("198.51.100.7"));
        // 多个头按出现顺序拼接
        let chain = headers(&["1.1.1.1", "203.0.113.9, 10.0.0.9"]);
        let client = resolve_client(&trusted, Some(ip("10.0.0.2")), &chain);
        assert_eq!(client.ip, ip("203.0.113.9"));
    }

    #[test]
    fn a_trusted_proxy_without_a_usable_chain_falls_back_to_the_peer() {
        let trusted = proxies(&["10.0.0.0/8"]);
        let peer = Some(ip("10.0.0.2"));
        for chain in [
            headers(&[]),
            headers(&["10.0.0.9, 10.0.0.3"]), // 全是受信代理
            headers(&["unknown"]),
            headers(&["1.2.3.4, unknown"]), // 最靠近我们的一跳读不懂：不猜
            headers(&["1.2.3.4,"]),
            headers(&["not-an-ip"]),
        ] {
            let client = resolve_client(&trusted, peer, &chain);
            assert_eq!(client.ip, ip("10.0.0.2"), "{chain:?}");
            assert!(!client.forwarded, "{chain:?}");
        }
    }

    #[test]
    fn forwarded_entries_with_ports_brackets_and_mapped_addresses_are_understood() {
        let trusted = proxies(&["10.0.0.0/8"]);
        for (chain, expected) in [
            ("203.0.113.9:4711", "203.0.113.9"),
            ("[2001:db8::1]:4711", "2001:db8::1"),
            ("[2001:db8::1]", "2001:db8::1"),
            ("2001:db8::1", "2001:db8::1"),
            ("::ffff:203.0.113.9", "203.0.113.9"),
            ("  203.0.113.9  ", "203.0.113.9"),
        ] {
            let client = resolve_client(&trusted, Some(ip("10.0.0.2")), &headers(&[chain]));
            assert_eq!(client.ip, ip(expected), "{chain}");
            assert!(client.forwarded, "{chain}");
        }
        for bad in [
            "203.0.113.9:",
            "203.0.113.9:99999x",
            "[2001:db8::1",
            "[2001:db8::1]x",
        ] {
            let client = resolve_client(&trusted, Some(ip("10.0.0.2")), &headers(&[bad]));
            assert!(!client.forwarded, "{bad}");
        }
    }

    #[test]
    fn oversized_chains_are_not_parsed_at_all() {
        let trusted = proxies(&["10.0.0.0/8"]);
        let long_chain = std::iter::repeat_n("1.2.3.4", MAX_FORWARDED_ENTRIES + 1)
            .collect::<Vec<_>>()
            .join(",");
        let client = resolve_client(&trusted, Some(ip("10.0.0.2")), &headers(&[&long_chain]));
        assert_eq!(client.ip, ip("10.0.0.2"));
        assert!(!client.forwarded);
        let fat = format!("{}203.0.113.9", " ".repeat(MAX_FORWARDED_BYTES));
        let client = resolve_client(&trusted, Some(ip("10.0.0.2")), &headers(&[&fat]));
        assert!(!client.forwarded);
    }

    #[test]
    fn a_non_utf8_forwarded_header_is_not_trusted() {
        let trusted = proxies(&["10.0.0.0/8"]);
        let mut map = HeaderMap::new();
        map.append(
            "x-forwarded-for",
            axum::http::HeaderValue::from_bytes(b"203.0.113.9\xff").unwrap(),
        );
        let client = resolve_client(&trusted, Some(ip("10.0.0.2")), &map);
        assert_eq!(client.ip, ip("10.0.0.2"));
        assert!(!client.forwarded);
    }
}

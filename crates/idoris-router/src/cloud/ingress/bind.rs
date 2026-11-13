//! 云形态的监听（计算服务 S5）：**只有云形态**允许非 loopback，并且要有明确的公网配置。
//!
//! # 谁能在哪里监听
//!
//! | 形态 | 数据面监听 | 备注 |
//! |---|---|---|
//! | 个人 | 只能 loopback | 契约不变（规范 §2.7） |
//! | 存量 tenant 部署 / 组织 | 和改动前完全一样（`parse_bind_host` + `validate_bind_authority`：loopback 或 Tailscale） | 这两个函数一个字符没动，非 loopback 的 tenant 部署照旧被拒绝 |
//! | **云** | loopback，或任何**明确的**单播 IP（通配 / 组播 / 广播永远不行） | 非 loopback 要满足下面的「公网配置」 |
//!
//! 非 loopback 的公网配置（缺一个就启动失败）：
//!
//! 1. 必须是云形态（`IDORIS_FORM=cloud`）；
//! 2. 云入口配置里的 `public.trusted_proxies` 不能为空：**TLS 由反向代理终止，iDoris 自己不做 TLS**。没有声明前置的
//!    反向代理却直接在非 loopback 地址上监听，意味着明文 HTTP 上跑 bearer 凭证；
//! 3. 至少有一张有效的虚拟 key（和既有的非 loopback 规则一致：空库不对外开门）；
//! 4. 不能开 `IDORIS_DEV_NO_KEY`（云形态一律不允许，见 `bin/idoris.rs`）。
//!
//! 通配地址（`0.0.0.0`、`::`）继续被拒绝，和既有的 `IDORIS_BIND_HOST` 规则一致：要绑就绑到明确的接口地址，
//! 不要在不知道自己暴露在哪些网卡上的情况下监听。
//!
//! # Admin v0 永远只听 loopback
//!
//! Admin v0（只读状态接口）有自己的端口和自己的监听器，地址是固定的 IPv4 loopback，和数据面监听的地址毫无关系。
//! 云形态的数据面开了公网，Admin v0 **不会跟着暴露**：[`ListenPlan`] 是数据面和 Admin 监听地址的唯一来源，
//! Admin 地址取自 `AdminBindConfig`（固定 loopback），而且构造时再断言一次。云入口的路由表里也没有任何 `/admin/*`。

use std::net::{IpAddr, SocketAddr};

use super::config::IngressConfig;
use crate::admin::AdminBindConfig;

/// 数据面和 Admin 要监听的地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenPlan {
    pub data: SocketAddr,
    pub admin: SocketAddr,
}

impl ListenPlan {
    /// 数据面监听 `bind_host:port`；Admin 监听 `AdminBindConfig` 给的地址，必须是 loopback，否则不构造。
    pub fn new(bind_host: IpAddr, port: u16, admin: AdminBindConfig) -> Result<Self, String> {
        let admin_addr = admin.addr();
        if !admin_addr.ip().is_loopback() {
            return Err(format!(
                "Admin v0 只能监听 loopback，拒绝监听 {admin_addr}（数据面监听 {bind_host} 不影响它）"
            ));
        }
        Ok(Self {
            data: SocketAddr::from((bind_host, port)),
            admin: admin_addr,
        })
    }
}

/// 解析云形态的 `IDORIS_BIND_HOST`：未设 = loopback；否则必须是明确的单播 IP 字面量（不能是主机名，不能是通配 / 组播 / 广播）。
/// 和既有 `parse_bind_host` 的区别只有一点：不再限制必须是 loopback 或 Tailscale 地址。
pub fn parse_cloud_bind_host(raw: Option<&str>, env_name: &str) -> Result<IpAddr, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(crate::BIND_HOST);
    };
    let host = raw
        .parse::<IpAddr>()
        .map_err(|_| format!("{env_name} 必须是明确的 IP 地址，不能使用主机名：'{raw}'"))?;
    if host.is_unspecified()
        || host.is_multicast()
        || matches!(host, IpAddr::V4(ip) if ip == std::net::Ipv4Addr::BROADCAST)
    {
        return Err(format!(
            "{env_name} 禁止通配/组播/广播地址：'{host}'；请绑定明确接口地址"
        ));
    }
    Ok(host)
}

/// 云形态的监听检查。loopback 永远允许；非 loopback 要求已经声明前置的反向代理，并且库里有有效的虚拟 key。
pub fn validate_public_bind(
    bind_host: IpAddr,
    config: &IngressConfig,
    has_active_key: bool,
    env_name: &str,
) -> Result<(), String> {
    if bind_host.is_loopback() {
        return Ok(());
    }
    if config.trusted_proxies.is_empty() {
        return Err(format!(
            "{env_name}={bind_host} 是非 loopback，但云入口配置的 public.trusted_proxies 是空的：\
             TLS 由反向代理终止，iDoris 自己不做 TLS，必须声明前置的反向代理才能在非 loopback 地址上监听"
        ));
    }
    if !has_active_key {
        return Err(format!(
            "{env_name}={bind_host} 是非 loopback，但当前没有有效 virtual key；拒绝启动"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const MINIMAL: &str = "schema_version: 1
tenants:
  - account: agent24
    data_classes: [adult]
    max_retention: zdr
    billing: paid
";

    fn config(trusted: &[&str]) -> IngressConfig {
        let list = trusted
            .iter()
            .map(|entry| format!("\"{entry}\""))
            .collect::<Vec<_>>()
            .join(", ");
        IngressConfig::parse(&format!("{MINIMAL}public:\n  trusted_proxies: [{list}]\n")).unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_cloud_bind_host_accepts_any_explicit_unicast_address_but_never_a_wildcard() {
        assert_eq!(parse_cloud_bind_host(None, "X").unwrap(), crate::BIND_HOST);
        assert_eq!(
            parse_cloud_bind_host(Some("  "), "X").unwrap(),
            crate::BIND_HOST
        );
        for ok in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.20",
            "203.0.113.7",
            "100.64.0.5",
            "fd00::1",
            "2001:db8::7",
        ] {
            assert_eq!(
                parse_cloud_bind_host(Some(ok), "X").unwrap(),
                ip(ok),
                "{ok}"
            );
        }
        for bad in [
            "0.0.0.0",
            "::",
            "255.255.255.255",
            "224.0.0.1",
            "ff02::1",
            "localhost",
            "idoris.ai",
            "10.0.0.5/8",
        ] {
            assert!(parse_cloud_bind_host(Some(bad), "X").is_err(), "{bad}");
        }
        // 报错信息带上变量名，运维一眼看得出是哪个变量
        let error = parse_cloud_bind_host(Some("0.0.0.0"), "IDORIS_BIND_HOST").unwrap_err();
        assert!(error.contains("IDORIS_BIND_HOST"), "{error}");
    }

    #[test]
    fn loopback_needs_nothing_and_a_public_bind_needs_a_declared_proxy_and_a_key() {
        // loopback：什么都不要（反向代理在同一台机器上，常见做法）
        assert!(validate_public_bind(ip("127.0.0.1"), &config(&[]), false, "X").is_ok());
        assert!(validate_public_bind(ip("::1"), &config(&[]), false, "X").is_ok());
        // 非 loopback、没有声明反向代理：拒绝（明文 HTTP 上跑 bearer 凭证）
        let error = validate_public_bind(ip("10.0.0.5"), &config(&[]), true, "IDORIS_BIND_HOST")
            .unwrap_err();
        assert!(
            error.contains("trusted_proxies") && error.contains("TLS"),
            "{error}"
        );
        // 声明了反向代理但没有有效 key：拒绝（空库不对外开门）
        let error = validate_public_bind(
            ip("10.0.0.5"),
            &config(&["10.0.0.1"]),
            false,
            "IDORIS_BIND_HOST",
        )
        .unwrap_err();
        assert!(error.contains("virtual key"), "{error}");
        // 都满足：允许
        assert!(validate_public_bind(ip("10.0.0.5"), &config(&["10.0.0.1"]), true, "X").is_ok());
        assert!(
            validate_public_bind(ip("203.0.113.7"), &config(&["198.51.100.0/24"]), true, "X")
                .is_ok()
        );
    }

    #[test]
    fn admin_stays_on_loopback_whatever_the_data_plane_binds() {
        let admin = AdminBindConfig::parse(Some("9741")).unwrap();
        for host in [
            "127.0.0.1",
            "10.0.0.5",
            "203.0.113.7",
            "100.64.0.5",
            "::1",
            "fd00::1",
            "2001:db8::7",
        ] {
            let plan = ListenPlan::new(ip(host), 8740, admin).unwrap();
            assert_eq!(plan.data, SocketAddr::from((ip(host), 8740)), "{host}");
            assert!(
                plan.admin.ip().is_loopback(),
                "Admin v0 must stay on loopback when the data plane binds {host}, got {}",
                plan.admin
            );
            assert_eq!(plan.admin.port(), 9741);
            assert_eq!(plan.admin.ip(), IpAddr::V4(crate::admin::ADMIN_BIND_HOST));
        }
    }
}

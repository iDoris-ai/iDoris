use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionSourceError {
    MissingPeer,
    NonLoopback,
}

impl SubscriptionSourceError {
    pub const fn http_status(self) -> u16 {
        403
    }

    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::MissingPeer => "SUBSCRIPTION_SOURCE_MISSING",
            Self::NonLoopback => "SUBSCRIPTION_SOURCE_FORBIDDEN",
        }
    }
}

impl std::fmt::Display for SubscriptionSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason_code())
    }
}

impl std::error::Error for SubscriptionSourceError {}

/// Accept subscription relay requests only from the transport's real peer.
///
/// The API intentionally has no forwarded-header input: X-Forwarded-For /
/// Forwarded can never upgrade a remote socket into a trusted source. Task17
/// wires the production connection layer so only its SocketAddr reaches here.
/// Tailscale is deliberately rejected in the first release.
pub fn check_subscription_source(
    peer: Option<SocketAddr>,
) -> Result<SocketAddr, SubscriptionSourceError> {
    let peer = peer.ok_or(SubscriptionSourceError::MissingPeer)?;
    if is_loopback(peer.ip()) {
        Ok(peer)
    } else {
        Err(SubscriptionSourceError::NonLoopback)
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    }
}

#![allow(clippy::expect_used)]

use std::net::SocketAddr;

use idoris_router::subscription::source::{SubscriptionSourceError, check_subscription_source};

fn addr(value: &str) -> SocketAddr {
    value.parse().expect("test address must parse")
}

#[test]
fn loopback_ipv4_ipv6_and_mapped_ipv4_are_allowed() {
    for peer in [
        "127.0.0.1:8740",
        "127.42.9.7:1234",
        "[::1]:8740",
        "[::ffff:127.0.0.1]:8740",
        "[::ffff:127.255.255.254]:8740",
    ] {
        let peer = addr(peer);
        assert_eq!(check_subscription_source(Some(peer)), Ok(peer));
    }
}

#[test]
fn missing_lan_public_mapped_remote_and_tailscale_are_forbidden() {
    assert_eq!(
        check_subscription_source(None),
        Err(SubscriptionSourceError::MissingPeer)
    );

    for peer in [
        "192.168.1.10:8740",
        "10.0.0.2:8740",
        "8.8.8.8:443",
        "100.64.0.1:8740",
        "[2001:4860:4860::8888]:443",
        "[::ffff:192.168.1.10]:8740",
        "[::ffff:100.64.0.1]:8740",
    ] {
        assert_eq!(
            check_subscription_source(Some(addr(peer))),
            Err(SubscriptionSourceError::NonLoopback),
            "{peer}",
        );
    }
}

#[test]
fn parser_rejects_invalid_octets_instead_of_loose_string_matching() {
    for bad in ["127.0.0.999:8740", "999.1.1.1:8740", "256.0.0.1:80"] {
        assert!(bad.parse::<SocketAddr>().is_err(), "{bad}");
    }
}

#[test]
fn source_errors_map_to_fixed_forbidden_http_contract() {
    for error in [
        SubscriptionSourceError::MissingPeer,
        SubscriptionSourceError::NonLoopback,
    ] {
        assert_eq!(error.http_status(), 403);
        assert!(error.reason_code().starts_with("SUBSCRIPTION_SOURCE_"));
    }
}

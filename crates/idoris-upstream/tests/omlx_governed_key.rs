//! 引擎管理器接管的端点上的 key 口径（IDORIS-CORE S5，接口规范 §2.7.7；[#619](https://github.com/iDoris-ai/iDoris/issues/619)）：
//! iDoris 自己拉起的引擎只用私有 key；外部引擎的 `settings.json` key 要等端口属主校验通过；没有登记的端点与以前一样。
//! 全部使用临时假 HOME，不碰真实 `~/.omlx`。
#![allow(clippy::unwrap_used)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use idoris_upstream::omlx::key::{
    EndpointKey, endpoint_identity, govern_endpoint, governed_endpoint, release_endpoint,
    resolve_for_endpoint, resolve_from_for, resolve_governed_os, resolve_omlx_api_key_for,
    resolve_omlx_api_key_os_for,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "idoris-omlx-governed-key-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(".omlx")).unwrap();
        Self(path)
    }

    fn with_settings_key(key: &str) -> Self {
        let home = Self::new();
        fs::write(
            home.0.join(".omlx/settings.json"),
            format!(r#"{{"auth":{{"api_key":"{key}"}}}}"#),
        )
        .unwrap();
        home
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn os(value: &str) -> Option<OsString> {
    Some(OsString::from(value))
}

#[test]
fn an_endpoint_is_identified_by_scheme_host_and_port_only() {
    for (a, b) in [
        ("http://127.0.0.1:19731", "http://127.0.0.1:19731/"),
        ("http://127.0.0.1:19731", "http://127.0.0.1:19731/v1/models"),
        (
            "http://127.0.0.1:19731",
            "http://user:pw@127.0.0.1:19731?q=1#f",
        ),
        ("http://LOCALHOST:8088", "http://localhost:8088"),
    ] {
        assert_eq!(endpoint_identity(a), endpoint_identity(b), "{a} / {b}");
        assert!(endpoint_identity(a).is_some());
    }
    assert_eq!(
        endpoint_identity("http://127.0.0.1:19731").as_deref(),
        Some("http://127.0.0.1:19731")
    );
    // Another port, another host, another scheme: another endpoint.
    for other in [
        "http://127.0.0.1:19732",
        "http://127.0.0.2:19731",
        "https://127.0.0.1:19731",
    ] {
        assert_ne!(
            endpoint_identity("http://127.0.0.1:19731"),
            endpoint_identity(other),
            "{other}"
        );
    }
    for junk in ["", "not a url", "ftp://127.0.0.1:1", "file:///tmp/x"] {
        assert_eq!(endpoint_identity(junk), None, "{junk:?}");
    }
}

#[test]
fn a_private_key_is_the_only_key_an_owned_engine_gets() {
    let home = Home::with_settings_key("from-file");
    let private = EndpointKey::Private("private-key".to_owned());
    // Neither the env key nor the settings key is ever offered to an engine iDoris owns.
    for env in [None, os("from-env")] {
        for loopback in [true, false] {
            assert_eq!(
                resolve_for_endpoint(env.clone(), Some(home.path()), loopback, Some(&private)),
                os("private-key"),
                "{env:?} loopback={loopback}"
            );
        }
    }
    // No home, no env: still the private key.
    assert_eq!(
        resolve_for_endpoint(None, None, true, Some(&private)),
        os("private-key")
    );
}

#[test]
fn an_external_engines_settings_key_waits_for_the_owner_check_but_the_env_key_does_not() {
    let home = Home::with_settings_key("from-file");
    let unverified = EndpointKey::External {
        settings_key_verified: false,
    };
    let verified = EndpointKey::External {
        settings_key_verified: true,
    };
    // Issue 619: the settings key goes to whoever listens on the port, so it needs the check first.
    assert_eq!(
        resolve_for_endpoint(None, Some(home.path()), true, Some(&unverified)),
        None
    );
    assert_eq!(
        resolve_for_endpoint(None, Some(home.path()), true, Some(&verified)),
        os("from-file")
    );
    // The env key was given explicitly by the caller: unchanged either way.
    for policy in [&unverified, &verified] {
        assert_eq!(
            resolve_for_endpoint(os("from-env"), Some(home.path()), true, Some(policy)),
            os("from-env")
        );
    }
    // Verified or not, the settings key still never leaves loopback.
    assert_eq!(
        resolve_for_endpoint(None, Some(home.path()), false, Some(&verified)),
        None
    );
}

#[test]
fn no_registration_means_the_old_order_unchanged() {
    let home = Home::with_settings_key("from-file");
    for (env, loopback) in [
        (None, true),
        (None, false),
        (os("e"), true),
        (os("e"), false),
    ] {
        assert_eq!(
            resolve_for_endpoint(env.clone(), Some(home.path()), loopback, None),
            resolve_from_for(env, Some(home.path()), loopback)
        );
    }
}

#[test]
fn the_registry_hands_the_private_key_to_exactly_the_registered_endpoint_until_it_is_released() {
    let endpoint = "http://127.0.0.1:59101";
    let other = "http://127.0.0.1:59102";
    assert!(governed_endpoint(endpoint).is_none());
    assert!(govern_endpoint(
        endpoint,
        EndpointKey::Private("registry-key".to_owned())
    ));
    // Whatever the process environment says, the governed endpoint answers with the private key,
    // and the same endpoint written with a path or a trailing slash is the same endpoint.
    for spelling in [
        endpoint,
        "http://127.0.0.1:59101/",
        "http://127.0.0.1:59101/v1",
    ] {
        assert_eq!(
            resolve_omlx_api_key_for(spelling).as_deref(),
            Some("registry-key"),
            "{spelling}"
        );
        assert_eq!(
            resolve_omlx_api_key_os_for(spelling),
            os("registry-key"),
            "{spelling}"
        );
    }
    assert_eq!(
        resolve_governed_os(endpoint),
        Some(os("registry-key")),
        "the governed lookup says so"
    );
    // Another port does not get it, and is not governed.
    assert_ne!(
        resolve_omlx_api_key_for(other).as_deref(),
        Some("registry-key")
    );
    assert_eq!(resolve_governed_os(other), None);
    // Released: the key is gone from the resolver (no stale private key outlives its engine).
    release_endpoint(endpoint);
    assert!(governed_endpoint(endpoint).is_none());
    assert_ne!(
        resolve_omlx_api_key_for(endpoint).as_deref(),
        Some("registry-key")
    );
    assert_eq!(resolve_governed_os(endpoint), None);
    // Releasing twice, or something that was never registered, is harmless.
    release_endpoint(endpoint);
    release_endpoint("not a url");
}

#[test]
fn registering_replaces_the_previous_policy_and_a_non_url_is_refused() {
    let endpoint = "http://127.0.0.1:59103";
    assert!(govern_endpoint(
        endpoint,
        EndpointKey::Private("first".to_owned())
    ));
    assert!(govern_endpoint(
        endpoint,
        EndpointKey::External {
            settings_key_verified: false
        }
    ));
    assert_eq!(
        governed_endpoint(endpoint),
        Some(EndpointKey::External {
            settings_key_verified: false
        })
    );
    assert_ne!(resolve_omlx_api_key_for(endpoint).as_deref(), Some("first"));
    release_endpoint(endpoint);
    assert!(!govern_endpoint(
        "not a url",
        EndpointKey::Private("x".to_owned())
    ));
    assert!(!govern_endpoint(
        "ftp://127.0.0.1:1",
        EndpointKey::Private("x".to_owned())
    ));
}

#[test]
fn a_private_key_is_never_printed() {
    let shown = format!(
        "{:?} {:?}",
        EndpointKey::Private("canary-private-key-q1w2".to_owned()),
        Some(EndpointKey::Private("canary-private-key-q1w2".to_owned()))
    );
    assert!(!shown.contains("canary-private-key-q1w2"), "{shown}");
    assert!(shown.contains("REDACTED"));
    assert!(
        format!(
            "{:?}",
            EndpointKey::External {
                settings_key_verified: true
            }
        )
        .contains("true")
    );
}

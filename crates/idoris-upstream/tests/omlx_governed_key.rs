//! 引擎管理器接管的端点上的 key 口径（IDORIS-CORE S5，接口规范 §2.7.7；[#619](https://github.com/iDoris-ai/iDoris/issues/619)）：
//! iDoris 自己拉起的引擎只用私有 key；外部引擎的 `settings.json` key 要等端口属主校验通过；没有登记的端点与以前一样。
//! 全部使用临时假 HOME，不碰真实 `~/.omlx`。
#![allow(clippy::unwrap_used)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use idoris_upstream::omlx::key::{
    EndpointKey, KeyRefusal, block_endpoint, endpoint_generation, endpoint_identity,
    forget_endpoint, govern_endpoint, governed_endpoint, release_endpoint, resolve_for_endpoint,
    resolve_from_for, resolve_governed_os, resolve_omlx_api_key_checked, resolve_omlx_api_key_for,
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
        ("http://203.0.113.7:8088", "http://203.0.113.7:8088/v1"),
    ] {
        assert_eq!(endpoint_identity(a), endpoint_identity(b), "{a} / {b}");
        assert!(endpoint_identity(a).is_some());
    }
    // Another port, another (non-loopback) host, another scheme on a non-loopback host: another endpoint.
    assert_eq!(
        endpoint_identity("http://203.0.113.7:8088").as_deref(),
        Some("http://203.0.113.7:8088")
    );
    for other in [
        "http://203.0.113.7:8089",
        "http://203.0.113.8:8088",
        "https://203.0.113.7:8088",
        "http://example.com:8088",
    ] {
        assert_ne!(
            endpoint_identity("http://203.0.113.7:8088"),
            endpoint_identity(other),
            "{other}"
        );
    }
    for junk in ["", "not a url", "ftp://127.0.0.1:1", "file:///tmp/x"] {
        assert_eq!(endpoint_identity(junk), None, "{junk:?}");
    }
}

/// IDORIS-CORE S6, Codex final review H5: every loopback spelling of a port is the same endpoint. The engine manager
/// registers `http://127.0.0.1:<port>`; a companion card that says `localhost:<port>` or `[::1]:<port>` must land on the same
/// registration (its block, its generation), not escape it by the spelling of the host and fall into the §2.9 fallback chain.
#[test]
fn every_loopback_spelling_of_a_port_is_one_endpoint_and_another_port_is_another() {
    let main = endpoint_identity("http://127.0.0.1:19731").unwrap();
    assert_eq!(main, "loopback:19731");
    for alias in [
        "http://localhost:19731",
        "http://LOCALHOST:19731/v1",
        "https://localhost:19731",
        "http://[::1]:19731",
        "http://[0:0:0:0:0:0:0:1]:19731",
        "http://127.0.0.2:19731",
        "http://127.1.2.3:19731/",
        "https://127.0.0.1:19731",
        // IPv4-mapped IPv6 loopback.
        "http://[::ffff:127.0.0.1]:19731",
    ] {
        assert_eq!(
            endpoint_identity(alias).as_deref(),
            Some(main.as_str()),
            "{alias}"
        );
    }
    for other in [
        "http://127.0.0.1:19732",
        "http://localhost:19732",
        "http://[::1]:19732",
    ] {
        assert_ne!(
            endpoint_identity(other).as_deref(),
            Some(main.as_str()),
            "{other}"
        );
    }
    // The default ports are part of the identity of a port-less URL.
    assert_eq!(
        endpoint_identity("http://localhost").as_deref(),
        Some("loopback:80")
    );
    assert_eq!(
        endpoint_identity("https://[::1]").as_deref(),
        Some("loopback:443")
    );
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
    forget_endpoint(endpoint);
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

// ---------------------------------------------------------------------------------------------
// IDORIS-CORE S6 (Codex 预审 3 与 1)：封上的端点、代际
// ---------------------------------------------------------------------------------------------

#[test]
fn a_blocked_endpoint_is_sent_no_credential_whatever_the_environment_and_the_settings_hold() {
    let home = Home::with_settings_key("from-file");
    // The fallback chain of §2.9 (env key, then the settings key) does not apply to a blocked endpoint, loopback or not,
    // and an unregistered endpoint still gets it (the contrast is what the blocking is about).
    for env in [None, os("from-env")] {
        for loopback in [true, false] {
            assert_eq!(
                resolve_for_endpoint(
                    env.clone(),
                    Some(home.path()),
                    loopback,
                    Some(&EndpointKey::Blocked)
                ),
                None,
                "{env:?} loopback={loopback}"
            );
        }
    }
    assert_eq!(
        resolve_for_endpoint(None, Some(home.path()), true, None),
        os("from-file"),
        "an endpoint nobody governs still falls back"
    );
    assert_eq!(format!("{:?}", EndpointKey::Blocked), "Blocked");
}

#[test]
fn blocking_an_endpoint_is_not_releasing_it() {
    let endpoint = "http://127.0.0.1:19831";
    govern_endpoint(endpoint, EndpointKey::Private("old-engine".to_owned()));
    // Blocked, not released: the endpoint is still governed, so no fallback can apply to it.
    assert!(block_endpoint(endpoint));
    assert_eq!(governed_endpoint(endpoint), Some(EndpointKey::Blocked));
    assert_eq!(
        resolve_omlx_api_key_checked(endpoint, None),
        Err(KeyRefusal::Blocked)
    );
    // Only a verified listener's registration lifts it.
    govern_endpoint(endpoint, EndpointKey::Private("new-engine".to_owned()));
    assert_eq!(
        resolve_omlx_api_key_checked(endpoint, None),
        Ok(Some("new-engine".to_owned()))
    );
    forget_endpoint(endpoint);
    assert!(!block_endpoint("not a url"));
}

#[test]
fn the_generation_moves_on_every_change_of_what_is_registered_and_only_then() {
    let endpoint = "http://127.0.0.1:19832";
    assert_eq!(endpoint_generation(endpoint), 0, "never registered");
    let private = |key: &str| EndpointKey::Private(key.to_owned());
    govern_endpoint(endpoint, private("a"));
    let first = endpoint_generation(endpoint);
    assert!(first > 0);
    // The same registration again is not a change.
    govern_endpoint(endpoint, private("a"));
    assert_eq!(endpoint_generation(endpoint), first);
    // A different key, a block, a lift, a changed verification, a release: each is one step.
    let mut expected = first;
    for step in [
        private("b"),
        EndpointKey::Blocked,
        private("b"),
        EndpointKey::External {
            settings_key_verified: false,
        },
        EndpointKey::External {
            settings_key_verified: true,
        },
    ] {
        govern_endpoint(endpoint, step);
        expected += 1;
        assert_eq!(endpoint_generation(endpoint), expected);
    }
    release_endpoint(endpoint);
    expected += 1;
    assert_eq!(endpoint_generation(endpoint), expected);
    // Releasing what is already released is not a change; and a generation is never reused.
    release_endpoint(endpoint);
    assert_eq!(endpoint_generation(endpoint), expected);
    govern_endpoint(endpoint, private("c"));
    assert_eq!(endpoint_generation(endpoint), expected + 1);
    release_endpoint(endpoint);
    // An endpoint that is not a URL has no generation.
    assert_eq!(endpoint_generation("not a url"), 0);
}

#[test]
fn a_request_admitted_for_an_older_generation_is_refused_by_the_checked_resolver() {
    let endpoint = "http://127.0.0.1:19833";
    govern_endpoint(endpoint, EndpointKey::Private("first-engine".to_owned()));
    let admitted = endpoint_generation(endpoint);
    // Admitted under the current generation: the key of the engine that is there.
    assert_eq!(
        resolve_omlx_api_key_checked(endpoint, Some(admitted)),
        Ok(Some("first-engine".to_owned()))
    );
    // The engine is replaced: the work admitted before must not get the new engine's key.
    govern_endpoint(endpoint, EndpointKey::Private("second-engine".to_owned()));
    let refusal = resolve_omlx_api_key_checked(endpoint, Some(admitted)).unwrap_err();
    assert_eq!(
        refusal,
        KeyRefusal::StaleGeneration {
            admitted,
            current: endpoint_generation(endpoint)
        }
    );
    // The refusal says what happened and nothing else: no key, old or new.
    let text = refusal.to_string();
    assert!(
        !text.contains("first-engine") && !text.contains("second-engine"),
        "{text}"
    );
    // Work admitted afterwards (and work that was never bound) gets the current one.
    assert_eq!(
        resolve_omlx_api_key_checked(endpoint, Some(endpoint_generation(endpoint))),
        Ok(Some("second-engine".to_owned()))
    );
    assert_eq!(
        resolve_omlx_api_key_checked(endpoint, None),
        Ok(Some("second-engine".to_owned()))
    );
    forget_endpoint(endpoint);
}

#[test]
fn the_checked_resolver_leaves_an_ungoverned_endpoint_as_it_was() {
    // Not managed: generation 0, no registration, an unbound request passes straight to the ordinary order.
    let endpoint = "http://127.0.0.1:19834";
    assert_eq!(endpoint_generation(endpoint), 0);
    assert_eq!(
        resolve_omlx_api_key_checked(endpoint, Some(0)),
        resolve_omlx_api_key_checked(endpoint, None)
    );
}

/// Test cleanup (S8's note, via the coordinator): the registry is process-global and the OS hands a freed port to another test of
/// the same process. A **revocation** keeps counting (a generation is never reused: that is a production property, asserted
/// above); **forgetting** an endpoint drops the whole entry, so the next test that gets this port sees an endpoint that was
/// never registered.
#[test]
fn forgetting_an_endpoint_resets_it_completely_unlike_a_revocation() {
    let endpoint = "http://127.0.0.1:19835";
    assert_eq!(endpoint_generation(endpoint), 0, "never registered");
    govern_endpoint(endpoint, EndpointKey::Private("a".to_owned()));
    block_endpoint(endpoint);
    assert_eq!(endpoint_generation(endpoint), 2);
    release_endpoint(endpoint);
    assert_eq!(
        endpoint_generation(endpoint),
        3,
        "a revocation keeps counting"
    );
    assert!(governed_endpoint(endpoint).is_none());

    forget_endpoint(endpoint);
    assert_eq!(endpoint_generation(endpoint), 0, "forgetting starts over");
    assert!(governed_endpoint(endpoint).is_none());
    // The next owner of the port counts from the beginning.
    govern_endpoint(endpoint, EndpointKey::Private("b".to_owned()));
    assert_eq!(endpoint_generation(endpoint), 1);
    // Forgetting a registered loopback spelling forgets the same entry; forgetting what is not a URL or was never there is harmless.
    forget_endpoint("http://localhost:19835");
    assert_eq!(endpoint_generation(endpoint), 0);
    assert!(governed_endpoint(endpoint).is_none());
    forget_endpoint(endpoint);
    forget_endpoint("not a url");
}

//! 接口规范 §2.9「oMLX 密钥」：环境变量 → `~/.omlx/settings.json` 的 `auth.api_key` → 无鉴权。
//! 全部使用临时假 HOME，不碰真实 `~/.omlx`。
#![allow(clippy::unwrap_used)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use idoris_upstream::omlx::key::{
    OMLX_KEY_REMEDIATION, SETTINGS_MAX_BYTES, resolve_from, settings_api_key,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "idoris-omlx-key-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(".omlx")).unwrap();
        Self(path)
    }
    fn settings(&self, content: impl AsRef<[u8]>) -> &Self {
        fs::write(self.0.join(".omlx/settings.json"), content).unwrap();
        self
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
fn priority_env_then_settings_then_none() {
    let home = Home::new();
    home.settings(r#"{"auth":{"api_key":"from-file"}}"#);
    // 1. 环境变量优先于文件。
    assert_eq!(
        resolve_from(os("from-env"), Some(home.path())),
        os("from-env")
    );
    // 2. 没设环境变量：文件。
    assert_eq!(resolve_from(None, Some(home.path())), os("from-file"));
    // 3. 两处都没有：无鉴权。
    let empty = Home::new();
    assert_eq!(resolve_from(None, Some(empty.path())), None);
    assert_eq!(resolve_from(None, None), None);
}

#[test]
fn empty_env_counts_as_unset_and_falls_through() {
    let home = Home::new();
    home.settings(r#"{"auth":{"api_key":"from-file"}}"#);
    assert_eq!(resolve_from(os(""), Some(home.path())), os("from-file"));
    assert_eq!(resolve_from(os(""), None), None);
}

#[cfg(unix)]
#[test]
fn a_non_utf8_env_value_is_kept_so_callers_can_reject_it() {
    use std::os::unix::ffi::OsStringExt;
    let home = Home::new();
    home.settings(r#"{"auth":{"api_key":"from-file"}}"#);
    let bad = OsString::from_vec(vec![b'k', 0xff]);
    assert_eq!(
        resolve_from(Some(bad.clone()), Some(home.path())),
        Some(bad)
    );
}

#[test]
fn bad_settings_files_are_just_absent() {
    let big_pad = "x".repeat(SETTINGS_MAX_BYTES as usize);
    let oversize = format!(r#"{{"pad":"{big_pad}","auth":{{"api_key":"k"}}}}"#);
    let cases: Vec<(&str, String)> = vec![
        ("empty file", String::new()),
        ("not json", "auth: api_key".into()),
        ("truncated", r#"{"auth":{"api_key":"k"#.into()),
        ("array root", r#"["auth"]"#.into()),
        ("no auth", r#"{"other":1}"#.into()),
        ("auth not object", r#"{"auth":"k"}"#.into()),
        ("no api_key", r#"{"auth":{}}"#.into()),
        ("null key", r#"{"auth":{"api_key":null}}"#.into()),
        ("number key", r#"{"auth":{"api_key":123}}"#.into()),
        ("object key", r#"{"auth":{"api_key":{"v":"k"}}}"#.into()),
        ("empty key", r#"{"auth":{"api_key":""}}"#.into()),
        ("blank key", r#"{"auth":{"api_key":"   "}}"#.into()),
        ("newline in key", r#"{"auth":{"api_key":"a\nb"}}"#.into()),
        ("control char", r#"{"auth":{"api_key":"a\u0000b"}}"#.into()),
        ("oversize file", oversize),
    ];
    for (name, content) in cases {
        let home = Home::new();
        home.settings(&content);
        assert_eq!(settings_api_key(home.path()), None, "{name}");
        assert_eq!(resolve_from(None, Some(home.path())), None, "{name}");
    }
    // 目录、缺文件、缺 .omlx。
    let home = Home::new();
    fs::create_dir_all(home.path().join(".omlx/settings.json")).unwrap();
    assert_eq!(settings_api_key(home.path()), None);
    assert_eq!(settings_api_key(&home.path().join("nope")), None);
    // 64 KiB 以内仍可读。
    let ok_pad = "x".repeat(SETTINGS_MAX_BYTES as usize - 100);
    let home = Home::new();
    home.settings(format!(r#"{{"pad":"{ok_pad}","auth":{{"api_key":"k"}}}}"#));
    assert_eq!(settings_api_key(home.path()).as_deref(), Some("k"));
}

#[test]
fn extra_fields_and_whitespace_around_json_are_fine() {
    let home = Home::new();
    home.settings("\n  {\"server\":{\"port\":8088},\"auth\":{\"api_key\":\"abc-123\",\"x\":1}}\n");
    assert_eq!(settings_api_key(home.path()).as_deref(), Some("abc-123"));
}

#[cfg(unix)]
#[test]
fn a_fifo_settings_file_is_ignored_without_blocking() {
    let home = Home::new();
    let status = std::process::Command::new("mkfifo")
        .arg(home.path().join(".omlx/settings.json"))
        .status()
        .unwrap();
    assert!(status.success());
    // 若实现去打开 FIFO，这里会一直阻塞。
    assert_eq!(settings_api_key(home.path()), None);
}

#[test]
fn remediation_names_both_sources() {
    assert!(OMLX_KEY_REMEDIATION.contains("IDORIS_OMLX_API_KEY"));
    assert!(OMLX_KEY_REMEDIATION.contains("settings.json"));
    assert!(OMLX_KEY_REMEDIATION.contains("auth.api_key"));
}

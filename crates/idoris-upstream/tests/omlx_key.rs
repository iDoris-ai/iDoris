//! 接口规范 §2.9「oMLX 密钥」：环境变量 → `~/.omlx/settings.json` 的 `auth.api_key` → 无鉴权。
//! 全部使用临时假 HOME，不碰真实 `~/.omlx`。
#![allow(clippy::unwrap_used)]

#[path = "../../idoris-router/tests/support/state_isolation.rs"]
mod state_isolation;
use state_isolation::IsolateState;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use idoris_upstream::omlx::key::{
    KeySource, OMLX_KEY_REMEDIATION, SETTINGS_MAX_BYTES, is_loopback_base_url, resolve_from,
    resolve_from_for, resolve_with_source, settings_api_key,
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
        ("inner space", r#"{"auth":{"api_key":"a b"}}"#.into()),
        ("inner tab", r#"{"auth":{"api_key":"a\tb"}}"#.into()),
        ("inner nbsp", "{\"auth\":{\"api_key\":\"a\u{a0}b\"}}".into()),
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

#[test]
fn settings_key_is_returned_trimmed() {
    let home = Home::new();
    home.settings("{\"auth\":{\"api_key\":\"  \\tkey-1\\n \"}}");
    assert_eq!(settings_api_key(home.path()).as_deref(), Some("key-1"));
    assert_eq!(resolve_from(None, Some(home.path())), os("key-1"));
}

#[test]
fn env_value_is_not_trimmed() {
    let home = Home::new();
    assert_eq!(resolve_from(os(" k "), Some(home.path())), os(" k "));
}

#[cfg(unix)]
#[test]
fn a_symlinked_settings_file_is_ignored() {
    let home = Home::new();
    let target = home.path().join("real.json");
    fs::write(&target, r#"{"auth":{"api_key":"via-link"}}"#).unwrap();
    std::os::unix::fs::symlink(&target, home.path().join(".omlx/settings.json")).unwrap();
    assert_eq!(settings_api_key(home.path()), None);
    assert_eq!(resolve_from(None, Some(home.path())), None);
    // 同一内容放成常规文件就能读到，说明被忽略的原因是符号链接。
    fs::remove_file(home.path().join(".omlx/settings.json")).unwrap();
    fs::copy(&target, home.path().join(".omlx/settings.json")).unwrap();
    assert_eq!(settings_api_key(home.path()).as_deref(), Some("via-link"));
}

/// #609：检查与打开之间没有窗口——类型由已打开的句柄说了算。测试自己先开着一个读端（让管道里的数据留得住），
/// 再开写端写入合法的、带 key 的 JSON 并关掉写端：这时 FIFO 里躺着完整内容、也没有写端，实现若不核对句柄类型，
/// `O_NONBLOCK` 打开后能读到内容并遇到 EOF，就会返回 key。正确的实现因为不是常规文件而返回 `None`。
#[cfg(unix)]
#[test]
fn a_fifo_with_valid_content_is_rejected_by_the_opened_handles_type() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let home = Home::new();
    let path = home.path().join(".omlx/settings.json");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let nonblock = nix::fcntl::OFlag::O_NONBLOCK.bits();
    let _keeper = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nonblock)
        .open(&path)
        .unwrap();
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .custom_flags(nonblock)
        .open(&path)
        .unwrap();
    writer
        .write_all(br#"{"auth":{"api_key":"from-fifo"}}"#)
        .unwrap();
    drop(writer);
    assert_eq!(settings_api_key(home.path()), None);
    assert_eq!(resolve_from(None, Some(home.path())), None);
}

/// 「不跟随符号链接」只对最后一段成立（规范与注释已收窄）：`~/.omlx` 本身是符号链接（常见：模型与配置放在别的盘）
/// 时，里面的常规文件照常读取。这条钉住的是已知口径，不是承诺的安全属性。
#[cfg(unix)]
#[test]
fn only_the_final_path_component_is_not_followed() {
    let home = Home::new();
    let real = home.path().join("elsewhere");
    fs::create_dir_all(&real).unwrap();
    fs::write(
        real.join("settings.json"),
        r#"{"auth":{"api_key":"k-dir"}}"#,
    )
    .unwrap();
    fs::remove_dir_all(home.path().join(".omlx")).unwrap();
    std::os::unix::fs::symlink(&real, home.path().join(".omlx")).unwrap();
    assert_eq!(settings_api_key(home.path()).as_deref(), Some("k-dir"));
}

#[test]
fn settings_key_is_only_for_loopback_but_env_key_is_unchanged() {
    let home = Home::new();
    home.settings(r#"{"auth":{"api_key":"from-file"}}"#);
    // 文件 key：loopback 有，非 loopback 没有（也不落到别的来源）。
    assert_eq!(
        resolve_from_for(None, Some(home.path()), true),
        os("from-file")
    );
    assert_eq!(resolve_from_for(None, Some(home.path()), false), None);
    assert_eq!(resolve_from_for(os(""), Some(home.path()), false), None);
    // 环境变量：不论是否 loopback 都照旧。
    assert_eq!(
        resolve_from_for(os("from-env"), Some(home.path()), false),
        os("from-env")
    );
    assert_eq!(
        resolve_from_for(os("from-env"), Some(home.path()), true),
        os("from-env")
    );
    // 来源标记。
    assert_eq!(
        resolve_with_source(os("e"), Some(home.path())),
        Some((OsString::from("e"), KeySource::Env))
    );
    assert_eq!(
        resolve_with_source(None, Some(home.path())),
        Some((OsString::from("from-file"), KeySource::Settings))
    );
}

#[test]
fn loopback_base_url_classification() {
    for url in [
        "http://127.0.0.1:8088",
        "http://localhost:8088/",
        "http://LOCALHOST",
        "http://[::1]:8088",
        "https://127.0.0.5",
    ] {
        assert!(is_loopback_base_url(url), "{url}");
    }
    for url in [
        "http://203.0.113.7:8088",
        "http://10.0.0.2",
        "http://omlx.example.com",
        "http://127.0.0.1.evil.example",
        "http://user@127.0.0.1",
        "ftp://127.0.0.1",
        "not a url",
        "",
    ] {
        assert!(!is_loopback_base_url(url), "{url}");
    }
}

/// 生命周期适配器的配置：文件 key 只给 loopback 的 base URL；环境变量 key 照旧。
/// key 取自进程环境，所以在子进程里跑。
#[test]
fn adapter_config_follows_the_same_loopback_rule() {
    const CASE: &str = "IDORIS_UPSTREAM_KEY_CASE";
    const FILE_KEY: &str = "canary-adapter-file-key-z9x8";
    const ENV_KEY: &str = "canary-adapter-env-key-c7v6";
    use idoris_upstream::OmlxAdapterConfig;
    if let Ok(case) = std::env::var(CASE) {
        let remote = OmlxAdapterConfig::for_base_url("http://203.0.113.7:8088");
        let local = OmlxAdapterConfig::for_base_url("http://127.0.0.1:8088");
        match case.as_str() {
            "file" => {
                assert_eq!(remote.api_key, None);
                assert_eq!(local.api_key.as_deref(), Some(FILE_KEY));
                assert_eq!(
                    OmlxAdapterConfig::default().api_key.as_deref(),
                    Some(FILE_KEY)
                );
            }
            _ => {
                assert_eq!(remote.api_key.as_deref(), Some(ENV_KEY));
                assert_eq!(local.api_key.as_deref(), Some(ENV_KEY));
            }
        }
        return;
    }
    for (case, env_key) in [("file", None), ("env", Some(ENV_KEY))] {
        let home = Home::new();
        home.settings(format!(r#"{{"auth":{{"api_key":"{FILE_KEY}"}}}}"#));
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "adapter_config_follows_the_same_loopback_rule"])
            .env(CASE, case)
            .env("HOME", home.path())
            .env_remove("IDORIS_OMLX_API_KEY");
        if let Some(key) = env_key {
            cmd.env("IDORIS_OMLX_API_KEY", key);
        }
        let output = cmd.isolate_state().output().unwrap();
        assert!(output.status.success(), "case {case}");
    }
}

//! Process-level start-up behaviour of the compute-service deployment form (`IDORIS_FORM`, S1).
//!
//! These spawn the real `idoris` binary. The library-level equivalents (and the byte-for-byte
//! legacy controls) are in `src/storage.rs` and `src/startup_form.rs`; this file proves the
//! environment variable is actually wired into `main`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use rusqlite::Connection;
use tempfile::TempDir;

static PROCESS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Running(Option<Child>);

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Running {
    /// Stops the process and returns everything it wrote to stderr.
    fn stop(mut self) -> String {
        let mut child = self.0.take().unwrap();
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_port_except(other: u16) -> u16 {
    loop {
        let port = free_port();
        if port != other {
            return port;
        }
    }
}

struct Fixture {
    _root: TempDir,
    cwd: TempDir,
    exe: PathBuf,
}

fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir_all(bin.join("config/components")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_idoris"), bin.join("idoris")).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    for file in [
        "components/omlx.yaml",
        "routing-policy.yaml",
        "catalog.yaml",
    ] {
        let mut raw = fs::read_to_string(repo.join(file)).unwrap();
        if file == "components/omlx.yaml" {
            raw.push_str("\nextensions:\n  _degradation: Removing bindings disables aliases.\n  idoris_role_models: { daily: unavailable-chat }\n");
        }
        fs::write(bin.join("config").join(file), raw.replace(":8000", ":0")).unwrap();
    }
    let cwd = TempDir::new().unwrap();
    fs::create_dir_all(cwd.path().join("config/components")).unwrap();
    Fixture {
        exe: bin.join("idoris"),
        _root: root,
        cwd,
    }
}

impl Fixture {
    fn db(&self) -> PathBuf {
        self.cwd.path().join("idoris-test.sqlite3")
    }

    fn tenants(&self, entries: &[(&str, i64)]) -> PathBuf {
        let path = self.cwd.path().join("tenants.yaml");
        let mut raw = String::from("tenants:\n");
        for (tenant, limit) in entries {
            raw.push_str(&format!(
                "  - tenant_id: {tenant}\n    limit_minor: {limit}\n    billing_timezone: UTC\n    scope: all\n"
            ));
        }
        fs::write(&path, raw).unwrap();
        path
    }

    fn spawn(&self, port: u16, env: &[(&str, &str)]) -> Running {
        let admin_port = free_port_except(port);
        let mut cmd = Command::new(&self.exe);
        for (key, _) in
            std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
        {
            cmd.env_remove(key);
        }
        cmd.current_dir(self.cwd.path())
            .env("IDORIS_PORT", port.to_string())
            .env("IDORIS_ADMIN_PORT", admin_port.to_string())
            .env("IDORIS_DB_PATH", self.db())
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match cmd.spawn() {
                Ok(child) => return Running(Some(child)),
                Err(err)
                    if err.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to spawn copied idoris binary: {err}"),
            }
        }
    }
}

/// The process must exit with code 1 and a stderr containing `expected`, without ever listening.
fn failed(mut child: Running, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            let output = child.0.take().unwrap().wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty(), "must not have started listening");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(expected), "{stderr}");
            return;
        }
        assert!(Instant::now() < deadline, "start-up did not fail");
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn wait_health(child: &mut Running, port: u16) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            && response.status().is_success()
        {
            return;
        }
        assert!(
            child.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "the process exited before it became healthy"
        );
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn tenant_ids(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT tenant_id FROM tenant_config ORDER BY tenant_id")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn an_unknown_form_stops_startup_and_names_the_variable() {
    let f = fixture();
    let port = free_port();
    failed(
        f.spawn(port, &[("IDORIS_FORM", "clound")]),
        "IDORIS_FORM 无效",
    );
    // the same misspelling in IDORIS_DEPLOY_MODE would have been swallowed as `tenant`; here it must not be
    let tenants = f.tenants(&[("acme", 1)]);
    failed(
        f.spawn(
            port,
            &[
                ("IDORIS_DEPLOY_MODE", "tenant"),
                ("IDORIS_FORM", "organisation"),
                ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
            ],
        ),
        "unknown form",
    );
    assert!(
        !f.db().exists(),
        "a bad IDORIS_FORM must stop before storage is touched"
    );
}

#[test]
fn a_form_that_conflicts_with_the_deploy_mode_stops_startup() {
    let f = fixture();
    let port = free_port();
    // deploy mode defaults to personal: an organization or cloud form cannot be personal
    for form in ["organization", "cloud"] {
        failed(
            f.spawn(port, &[("IDORIS_FORM", form)]),
            "conflicts with deploy_mode",
        );
    }
    let tenants = f.tenants(&[("acme", 1)]);
    failed(
        f.spawn(
            port,
            &[
                ("IDORIS_DEPLOY_MODE", "tenant"),
                ("IDORIS_FORM", "personal"),
                ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
            ],
        ),
        "conflicts with deploy_mode",
    );
    assert!(!f.db().exists());
}

#[tokio::test]
async fn the_organization_form_keeps_a_removed_tenant_across_real_process_restarts() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants(&[("acme", 100), ("beta", 100)]);
    let env = [
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_FORM", "organization"),
        ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
    ];

    let port = free_port();
    let mut first = f.spawn(port, &env);
    wait_health(&mut first, port).await;
    let stderr = first.stop();
    assert!(
        stderr.contains("租户初始化：从配置文件新写入 2 个"),
        "first start seeds both tenants: {stderr}"
    );
    assert_eq!(tenant_ids(&f.db()), ["acme", "beta"]);

    // beta disappears from the file; two more starts must keep it and stay quiet about pruning
    f.tenants(&[("acme", 100)]);
    for round in 0..2 {
        let port = free_port();
        let mut again = f.spawn(port, &env);
        wait_health(&mut again, port).await;
        let stderr = again.stop();
        assert!(!stderr.contains("已撤销"), "round {round}: {stderr}");
        assert_eq!(tenant_ids(&f.db()), ["acme", "beta"], "round {round}");
    }
    // the compute-service tables exist and the markers survived
    let conn = Connection::open(f.db()).unwrap();
    let markers: i64 = conn
        .query_row("SELECT count(*) FROM tenant_bootstrap", [], |r| r.get(0))
        .unwrap();
    let accounts: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='accounts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((markers, accounts), (2, 1));
}

#[tokio::test]
async fn without_the_form_a_removed_tenant_is_still_revoked_as_before() {
    // Control for the test above: the very same sequence with no IDORIS_FORM is the pre-S1 behaviour.
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants(&[("acme", 100), ("beta", 100)]);
    let env = [
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
    ];
    let port = free_port();
    let mut first = f.spawn(port, &env);
    wait_health(&mut first, port).await;
    let stderr = first.stop();
    assert!(
        !stderr.contains("租户初始化"),
        "legacy start prints nothing new: {stderr}"
    );
    assert_eq!(tenant_ids(&f.db()), ["acme", "beta"]);

    f.tenants(&[("acme", 100)]);
    let port = free_port();
    let mut again = f.spawn(port, &env);
    wait_health(&mut again, port).await;
    let stderr = again.stop();
    assert!(
        stderr.contains("已撤销 1 个不再存在于可信配置中的租户"),
        "{stderr}"
    );
    assert_eq!(tenant_ids(&f.db()), ["acme"]);
    // and a legacy database never gets the compute-service tables
    let conn = Connection::open(f.db()).unwrap();
    let accounts: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('accounts','principals')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(accounts, 0);
}

#[tokio::test]
async fn an_explicit_personal_form_starts_like_the_default() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let port = free_port();
    let mut child = f.spawn(port, &[("IDORIS_FORM", "personal")]);
    wait_health(&mut child, port).await;
    let stderr = child.stop();
    assert!(!stderr.contains("租户初始化"), "{stderr}");
    assert_eq!(tenant_ids(&f.db()), ["personal"]);
}

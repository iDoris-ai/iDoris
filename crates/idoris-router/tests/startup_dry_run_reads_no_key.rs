//! L10：启动时对每张要 Supervisor 的卡做的适配器干跑（`RuntimeRegistry::resolve_adapter_params`）**不解析任何凭证**：不读 `IDORIS_OMLX_API_KEY`，
//! 不读用户的 `~/.omlx/settings.json`。凭证只在真正建适配器时才解析（托管引擎的私有 key 要等引擎拉起之后才登记；引擎最后降级的话，
//! 根本不会建这张卡的适配器）。此前干跑走的是和真正建适配器同一条路，等于每次启动都在引擎还没起来的时候就去读了一遍用户的密钥文件。
//!
//! 怎么数「读了几次」：`idoris_upstream::omlx::key::resolutions()` 是进程里按环境与 `settings.json` 解析 oMLX key 的次数，所有读环境的入口都经过它。
//! 它是进程全局的计数，所以这个文件只有这一条测试，而且在**自己的子进程**里数（本测试再起一次自己，环境是空的 `HOME`、没有
//! `IDORIS_OMLX_API_KEY`）：既没有别的测试的读取来干扰计数，对照用的「真正建适配器」也读不到这台机器上真实的 `~/.omlx`。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;

use std::{collections::BTreeMap, process::Command, time::Duration};

use child_guard::SpawnGuarded;
use idoris_router::{components, runtime::RuntimeRegistry};
use idoris_upstream::factory::create_adapter_with;

const CHILD: &str = "IDORIS_TEST_DRY_RUN_KEY_CHILD";
const TEST: &str = "the_start_up_adapter_dry_run_resolves_no_credential_and_a_real_build_does";

/// 一张托管的 oMLX 卡（要 Supervisor：非 resident 的 `http_service`）。
const CARD: &str = r#"provider:
  id: omlx
  family: local
  tier: local
  capabilities: [chat, embedding]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:18765"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: on_demand, keepalive: { idle_ttl_s: 300 }, admission: requires_eviction }
extensions:
  _degradation: "未托管时退回外部 oMLX：iDoris 只探测，不拉起、不停止"
  idoris_managed: { engine: omlx, mode: auto }
"#;

#[test]
fn the_start_up_adapter_dry_run_resolves_no_credential_and_a_real_build_does() {
    if std::env::var_os(CHILD).is_none() {
        // 父进程：在空的环境里再起一次自己，只跑这一条。
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env_remove("IDORIS_OMLX_API_KEY")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = command.spawn_guarded().unwrap().wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "the child failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "the child really ran the test:\n{stdout}"
        );
        return;
    }

    // 子进程：干跑之前、之后各数一次。
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("omlx.yaml"), CARD).unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let cards = components::load_components_locked(&canonical, false, Duration::ZERO).unwrap();
    assert_eq!(cards.len(), 1);

    let before = idoris_upstream::omlx::key::resolutions();
    let params = RuntimeRegistry::resolve_adapter_params(&cards, &BTreeMap::new()).unwrap();
    assert_eq!(
        idoris_upstream::omlx::key::resolutions(),
        before,
        "the start-up dry run resolved a credential: it read the environment or the user's settings.json"
    );

    // 对照（让上面那条「零次」不是因为计数器根本不动）：真正建适配器，凭证在这时解析。
    let adapter = create_adapter_with(&cards[0], &params).unwrap();
    drop(adapter);
    assert!(
        idoris_upstream::omlx::key::resolutions() > before,
        "building the adapter for real resolves the credential, and the counter sees it"
    );
}

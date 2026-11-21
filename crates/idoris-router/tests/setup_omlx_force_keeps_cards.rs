//! #626 项 2：`setup omlx --force` 只替换 oMLX 自己的文件（`omlx.yaml`、伴生卡 `omlx-embed.yaml`、固定文件
//! `subscription.yaml`），目录里别的卡（例如 `setup upstream` 生成的）原样保留。保留下来的卡与新卡一起过加载器；有冲突就
//! 整体失败，目录一个字节不变。不带 `--force` 且目录非空仍报 `out_dir_not_empty`。库级测试：不起进程、不连网。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use idoris_router::cli::{SetupOptions, SetupTarget};
use idoris_router::components::load_components;
use idoris_router::setup_engine::{SetupError, run_setup, write_validated_with};
use idoris_router::setup_omlx::OmlxSetup;
use serde_json::Value;
use tempfile::TempDir;

use idoris_contracts::load_policy::LoadMode;

const ENDPOINT: &str = "http://127.0.0.1:8088";

struct Env {
    home: TempDir,
    work: TempDir,
}

impl Env {
    fn new(models: &[&str]) -> Self {
        let env = Self {
            home: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
        };
        for id in models {
            let dir = env.home.path().join(".omlx/models").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("config.json"), "{}").unwrap();
            fs::write(dir.join("model.safetensors"), b"w").unwrap();
        }
        env
    }

    fn out(&self) -> PathBuf {
        self.work.path().join("comp")
    }

    async fn run(
        &self,
        endpoint: &str,
        binds: &[(&str, &str)],
        force: bool,
    ) -> Result<Value, SetupError> {
        let setup = OmlxSetup::new(Some(self.home.path().to_path_buf()));
        let options = SetupOptions {
            target: SetupTarget::Endpoint {
                url: endpoint.to_string(),
                probe: false,
            },
            out_dir: self.out(),
            bindings: binds
                .iter()
                .map(|(r, m)| (r.to_string(), m.to_string()))
                .collect(),
            load_mode: LoadMode::OnDemand,
            api_key_stdin: false,
            force,
        };
        run_setup(&setup, &options, None).await
    }

    /// 目录里全部文件的名字加字节；用来断言「一个字节不变」。
    fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        snapshot(&self.out())
    }

    /// 暂存目录、备份文件都不该留下：工作目录里只有 `comp`，`comp` 里没有 `.idoris-*`。
    fn assert_no_leftovers(&self) {
        let parent: Vec<_> = fs::read_dir(self.work.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(parent, ["comp"], "暂存目录没有清理");
        assert!(
            self.snapshot()
                .keys()
                .all(|name| !name.contains(".idoris-")),
            "{:?}",
            self.snapshot().keys().collect::<Vec<_>>()
        );
    }
}

fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().into_string().unwrap(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

/// 与 `setup upstream` 生成的卡同形状：loopback resident、`idoris_upstream` 标注、`openai_compat` 种类。
fn upstream_card(id: &str, port: u16, extra_ext: &str) -> String {
    format!(
        r#"provider:
  id: {id}
  family: local
  tier: local
  capabilities:
  - chat
  privacy_class: local_only
  cost:
    input_per_m: 0
    output_per_m: 0
  locality: loopback
form: http_service
endpoint: http://127.0.0.1:{port}
version_pin: openai_compat@unknown
privacy_class: local_only
allowed_egress:
- loopback
fallback_policy: fail_closed
fail_closed: true
load_policy:
  mode: resident
  keepalive:
    pinned: true
  admission: coexist
extensions:
  _degradation: d
  idoris_upstream:
    kind: openai_compat
    flavor: openai_compat
{extra_ext}"#
    )
}

async fn seed_with_upstream(env: &Env, binds: &[(&str, &str)]) -> Vec<u8> {
    env.run(ENDPOINT, binds, false).await.unwrap();
    let card = upstream_card("lmstudio", 1234, "").into_bytes();
    fs::write(env.out().join("lmstudio.yaml"), &card).unwrap();
    card
}

/// 主流程：目录里有上游卡，`--force` 重新生成 oMLX 的卡（端点换了）、上游卡逐字节保留、固定文件仍在、加载器放行整个目录。
#[tokio::test]
async fn force_replaces_the_omlx_cards_and_keeps_an_upstream_card_byte_for_byte() {
    let env = Env::new(&["bge-m3-mlx-fp16"]);
    env.run(ENDPOINT, &[], false).await.unwrap();
    let upstream = upstream_card("lmstudio", 1234, "").into_bytes();
    fs::write(env.out().join("lmstudio.yaml"), &upstream).unwrap();
    let before = env.snapshot();

    let output = env
        .run(
            "http://127.0.0.1:8001",
            &[("embed", "bge-m3-mlx-fp16")],
            true,
        )
        .await
        .unwrap();
    assert_eq!(output["companion_card_file"], "omlx-embed.yaml");

    let after = env.snapshot();
    assert_eq!(after["lmstudio.yaml"], upstream, "上游卡必须逐字节保留");
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        [
            "lmstudio.yaml",
            "omlx-embed.yaml",
            "omlx.yaml",
            "subscription.yaml"
        ]
    );
    assert_ne!(after["omlx.yaml"], before["omlx.yaml"], "oMLX 卡被替换");
    assert!(
        String::from_utf8_lossy(&after["omlx.yaml"]).contains("127.0.0.1:8001"),
        "新端点写进了 oMLX 卡"
    );
    let cards = load_components(&env.out(), false).unwrap();
    let mut ids: Vec<_> = cards.iter().map(|c| c.provider.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["lmstudio", "omlx", "omlx-embed", "subscription"]);
    env.assert_no_leftovers();
}

/// 上次生成过伴生卡、这次没给 `embed`：伴生卡被清掉，上游卡与不是卡的文件仍在。
#[tokio::test]
async fn force_without_embed_removes_the_old_companion_but_keeps_everything_else() {
    let env = Env::new(&["bge-m3-mlx-fp16"]);
    let upstream = seed_with_upstream(&env, &[("embed", "bge-m3-mlx-fp16")]).await;
    fs::write(env.out().join("notes.txt"), b"hand written").unwrap();
    assert!(env.out().join("omlx-embed.yaml").exists());

    env.run(ENDPOINT, &[], true).await.unwrap();
    let after = env.snapshot();
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        [
            "lmstudio.yaml",
            "notes.txt",
            "omlx.yaml",
            "subscription.yaml"
        ]
    );
    assert_eq!(after["lmstudio.yaml"], upstream);
    assert_eq!(after["notes.txt"], b"hand written");
    env.assert_no_leftovers();
}

/// 属于 oMLX 自己的旧文件不算「保留的卡」：上次留下的伴生卡已经坏了（或和别的卡冲突），这次没给 `embed`，它被清掉，
/// 整体校验不看它；坏掉的旧 `omlx.yaml` 同样直接被替换。
#[tokio::test]
async fn stale_files_the_engine_owns_never_block_a_forced_regeneration() {
    let env = Env::new(&[]);
    seed_with_upstream(&env, &[]).await;
    fs::write(env.out().join("omlx-embed.yaml"), "not: [valid").unwrap();
    fs::write(env.out().join("omlx.yaml"), "also broken: [").unwrap();
    env.run(ENDPOINT, &[], true).await.unwrap();
    assert_eq!(
        env.snapshot().keys().collect::<Vec<_>>(),
        ["lmstudio.yaml", "omlx.yaml", "subscription.yaml"]
    );
    load_components(&env.out(), false).unwrap();
    env.assert_no_leftovers();
}

/// 反例一：不带 `--force`，目录里有上游卡（非空）：照旧 `out_dir_not_empty`，什么也不变。
#[tokio::test]
async fn without_force_a_directory_holding_an_upstream_card_is_still_refused() {
    let env = Env::new(&[]);
    seed_with_upstream(&env, &[]).await;
    let before = env.snapshot();
    let error = env.run(ENDPOINT, &[], false).await.unwrap_err();
    assert_eq!(error.code, "out_dir_not_empty");
    assert!(error.message.contains("--force"), "{}", error.message);
    assert_eq!(env.snapshot(), before);
    env.assert_no_leftovers();
}

/// 反例二：不带 `--force`，目录里只有一张别的卡（没有 oMLX 的文件）：同样拒绝。
#[tokio::test]
async fn without_force_a_directory_with_only_a_foreign_card_is_refused() {
    let env = Env::new(&[]);
    fs::create_dir_all(env.out()).unwrap();
    fs::write(
        env.out().join("lmstudio.yaml"),
        upstream_card("lmstudio", 1234, ""),
    )
    .unwrap();
    let before = env.snapshot();
    let error = env.run(ENDPOINT, &[], false).await.unwrap_err();
    assert_eq!(error.code, "out_dir_not_empty");
    assert_eq!(env.snapshot(), before);
}

/// 冲突：保留的卡与新 oMLX 卡同一个 `provider.id`（手写了一张 `omlx`，或占了伴生卡的 `omlx-embed`）：整体校验失败，
/// 目录一个字节不变（包括旧的 oMLX 卡与上游卡），没有暂存目录或备份文件残留。
#[tokio::test]
async fn a_kept_card_that_collides_on_provider_id_fails_the_whole_run_and_changes_nothing() {
    for (colliding_id, binds) in [
        ("omlx", &[][..]),
        ("omlx-embed", &[("embed", "bge-m3-mlx-fp16")][..]),
    ] {
        let env = Env::new(&["bge-m3-mlx-fp16"]);
        let first: &[(&str, &str)] = &[];
        env.run(ENDPOINT, first, false).await.unwrap();
        fs::write(
            env.out().join("hand-written.yaml"),
            upstream_card(colliding_id, 1234, ""),
        )
        .unwrap();
        let before = env.snapshot();
        let error = env
            .run("http://127.0.0.1:8001", binds, true)
            .await
            .unwrap_err();
        assert_eq!(error.code, "validation_failed", "{colliding_id}");
        assert!(
            error.message.contains(colliding_id),
            "{colliding_id}: {}",
            error.message
        );
        assert_eq!(env.snapshot(), before, "{colliding_id}: 目录必须不变");
        env.assert_no_leftovers();
    }
}

/// 冲突：保留的卡本身加载不了（不是合法 YAML，或 `idoris_upstream.flavor` 写错）：整体失败，说明里点名那个文件，目录不变。
#[tokio::test]
async fn a_kept_card_that_does_not_load_fails_the_whole_run_and_names_the_file() {
    for (file, content, expect) in [
        ("broken.yaml", "not: [valid".to_string(), "broken.yaml"),
        (
            "wrong-flavor.yaml",
            upstream_card("lmstudio", 1234, "").replace("flavor: openai_compat", "flavor: vllm"),
            "upstream_flavor_mismatch",
        ),
    ] {
        let env = Env::new(&[]);
        env.run(ENDPOINT, &[], false).await.unwrap();
        fs::write(env.out().join(file), &content).unwrap();
        let before = env.snapshot();
        let error = env
            .run("http://127.0.0.1:8001", &[], true)
            .await
            .unwrap_err();
        assert_eq!(error.code, "validation_failed", "{file}");
        assert!(error.message.contains(expect), "{file}: {}", error.message);
        assert!(error.message.contains(file), "{file}: {}", error.message);
        assert_eq!(env.snapshot(), before, "{file}: 目录必须不变");
        env.assert_no_leftovers();
    }
}

/// 多张默认卡：新写的卡要当默认，而保留的卡里已经有默认卡：整体失败，不落盘。对照：保留的卡没有默认，放行。
/// （oMLX 自己的卡不带 `idoris_default`，所以直接用通用的 `write_validated_with` 造这个场景。）
#[test]
fn a_second_default_card_among_the_kept_ones_fails_and_a_non_default_one_passes() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    let kept = upstream_card("kept", 1234, "  idoris_default: true\n");
    fs::write(out.join("kept.yaml"), &kept).unwrap();
    let new_default = upstream_card("fresh", 1235, "  idoris_default: true\n");
    let before = snapshot(&out);
    let error =
        write_validated_with(&out, true, &[("fresh.yaml", new_default.as_str())], &[]).unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(error.message.contains("kept"), "{}", error.message);
    assert_eq!(snapshot(&out), before);

    // 对照：新卡不当默认，放行。
    let plain = upstream_card("fresh", 1235, "");
    write_validated_with(&out, true, &[("fresh.yaml", plain.as_str())], &[]).unwrap();
    assert_eq!(snapshot(&out)["kept.yaml"], kept.as_bytes());
    assert!(out.join("fresh.yaml").exists());
}

/// 不带 `--force`，目录不存在或为空：与以前一样直接生成（保留语义不影响首次 setup）。
#[tokio::test]
async fn an_empty_or_missing_directory_works_without_force() {
    let env = Env::new(&[]);
    env.run(ENDPOINT, &[], false).await.unwrap();
    assert_eq!(
        env.snapshot().keys().collect::<Vec<_>>(),
        ["omlx.yaml", "subscription.yaml"]
    );
    let env = Env::new(&[]);
    fs::create_dir_all(env.out()).unwrap();
    env.run(ENDPOINT, &[], false).await.unwrap();
}

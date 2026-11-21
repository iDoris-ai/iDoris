//! IDORIS-CORE S7（接口规范 §2.3、§2.7.8；设计稿 §2.8.1）：`setup omlx --bind embed=<模型>` 额外生成伴生 resident 卡
//! `omlx-embed.yaml`。库级测试，不起进程、不连真实 oMLX：用 `--no-probe`（离线对照临时家目录里的模型）与假 oMLX（wiremock）。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use idoris_contracts::ComponentCard;
use idoris_contracts::common::Capability;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_router::cli::{SetupOptions, SetupTarget};
use idoris_router::components::load_components;
use idoris_router::engine_kind::engine_identity;
use idoris_router::setup_engine::{SetupError, run_setup};
use idoris_router::setup_omlx::OmlxSetup;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

    fn setup(&self) -> OmlxSetup {
        OmlxSetup::new(Some(self.home.path().to_path_buf()))
    }

    fn options(&self, binds: &[(&str, &str)], force: bool) -> SetupOptions {
        SetupOptions {
            target: SetupTarget::Endpoint {
                url: ENDPOINT.to_string(),
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
        }
    }

    async fn run(&self, binds: &[(&str, &str)], force: bool) -> Result<Value, SetupError> {
        run_setup(&self.setup(), &self.options(binds, force), None).await
    }
}

fn card_at(dir: &Path, file: &str) -> ComponentCard {
    serde_yaml::from_str(&fs::read_to_string(dir.join(file)).unwrap()).unwrap()
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn role_models(card: &ComponentCard) -> Option<Value> {
    card.extensions
        .as_ref()
        .and_then(|e| e.get("idoris_role_models").cloned())
}

#[tokio::test]
async fn an_embed_binding_adds_the_companion_resident_card_on_the_same_endpoint() {
    let env = Env::new(&["bge-m3-mlx-fp16", "Qwen3-8B"]);
    let out = env
        .run(
            &[("embed", "bge-m3-mlx-fp16"), ("daily", "Qwen3-8B")],
            false,
        )
        .await
        .unwrap();
    assert_eq!(out["companion_card_file"], "omlx-embed.yaml");
    let dir = env.out();
    assert_eq!(
        files(&dir),
        ["omlx-embed.yaml", "omlx.yaml", "subscription.yaml"]
    );

    let companion = card_at(&dir, "omlx-embed.yaml");
    assert_eq!(companion.provider.id, "omlx-embed");
    assert_eq!(companion.provider.capabilities, [Capability::Embedding]);
    assert_eq!(companion.endpoint, ENDPOINT);
    assert_eq!(
        companion.version_pin,
        card_at(&dir, "omlx.yaml").version_pin
    );
    let policy = companion.load_policy.as_ref().unwrap();
    assert_eq!(policy.mode, LoadMode::Resident);
    assert!(matches!(
        policy.keepalive,
        Keepalive::Pinned { pinned: true }
    ));
    assert_eq!(policy.admission, Admission::Coexist);
    assert_eq!(
        role_models(&companion),
        Some(json!({"embed": "bge-m3-mlx-fp16"}))
    );
    assert!(
        companion.extensions.as_ref().unwrap()["_degradation"].is_string(),
        "idoris_role_models 必须带 _degradation"
    );
    // 伴生卡没有自己的生命周期：不带托管声明，引擎种类仍是 omlx。
    assert!(
        !companion
            .extensions
            .as_ref()
            .unwrap()
            .contains_key("idoris_managed")
    );
    let identity = engine_identity(&companion).unwrap();
    assert_eq!(
        (identity.engine_kind.as_str(), identity.managed),
        ("omlx", false)
    );

    // 主卡保持 on_demand，embed 绑定只在伴生卡上；别的绑定留在主卡。
    let main = card_at(&dir, "omlx.yaml");
    assert_eq!(main.provider.id, "omlx");
    assert_eq!(main.load_policy.as_ref().unwrap().mode, LoadMode::OnDemand);
    assert_eq!(role_models(&main), Some(json!({"daily": "Qwen3-8B"})));

    // 输出里的 bindings 仍是命令行给的全部绑定（只增不改）。
    assert_eq!(
        out["bindings"],
        json!({"embed": "bge-m3-mlx-fp16", "daily": "Qwen3-8B"})
    );
    // 两张卡一起过服务启动时的加载器。
    let cards = load_components(&dir, false).unwrap();
    let mut ids: Vec<_> = cards.iter().map(|c| c.provider.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["omlx", "omlx-embed", "subscription"]);
}

#[tokio::test]
async fn only_an_embed_binding_leaves_the_main_card_without_role_models() {
    let env = Env::new(&["bge-m3-mlx-fp16"]);
    env.run(&[("embed", "bge-m3-mlx-fp16")], false)
        .await
        .unwrap();
    let main = card_at(&env.out(), "omlx.yaml");
    assert_eq!(role_models(&main), None);
    assert!(
        !fs::read_to_string(env.out().join("omlx.yaml"))
            .unwrap()
            .contains("Removing these bindings"),
        "没有绑定就不该有绑定的降级说明"
    );
}

#[tokio::test]
async fn without_an_embed_binding_no_companion_card_and_the_output_is_unchanged() {
    let env = Env::new(&["Qwen3-8B"]);
    for binds in [vec![], vec![("daily", "Qwen3-8B")]] {
        let _ = fs::remove_dir_all(env.out());
        let out = env.run(&binds, false).await.unwrap();
        assert!(out.get("companion_card_file").is_none(), "{out}");
        assert_eq!(files(&env.out()), ["omlx.yaml", "subscription.yaml"]);
    }
}

#[tokio::test]
async fn an_uninstalled_embed_model_is_refused_and_nothing_is_written() {
    let env = Env::new(&["Qwen3-8B"]);
    let error = env
        .run(&[("embed", "bge-m3-mlx-fp16")], false)
        .await
        .unwrap_err();
    assert_eq!(error.code, "model_not_installed");
    assert!(!env.out().exists());
}

#[tokio::test]
async fn force_regenerates_both_cards_byte_for_byte() {
    let env = Env::new(&["bge-m3-mlx-fp16", "Qwen3-8B"]);
    let binds = [("embed", "bge-m3-mlx-fp16"), ("daily", "Qwen3-8B")];
    env.run(&binds, false).await.unwrap();
    let read = |name: &str| fs::read(env.out().join(name)).unwrap();
    let before = (read("omlx.yaml"), read("omlx-embed.yaml"));
    // 不带 --force 重跑：目录非空，拒绝。
    let refused = env.run(&binds, false).await.unwrap_err();
    assert_eq!(refused.code, "out_dir_not_empty");
    env.run(&binds, true).await.unwrap();
    assert_eq!(before, (read("omlx.yaml"), read("omlx-embed.yaml")));
    assert_eq!(
        files(&env.out()),
        ["omlx-embed.yaml", "omlx.yaml", "subscription.yaml"]
    );
}

#[tokio::test]
async fn force_without_an_embed_binding_removes_the_previous_companion_card() {
    let env = Env::new(&["bge-m3-mlx-fp16"]);
    env.run(&[("embed", "bge-m3-mlx-fp16")], false)
        .await
        .unwrap();
    assert!(env.out().join("omlx-embed.yaml").exists());
    let out = env.run(&[], true).await.unwrap();
    assert!(out.get("companion_card_file").is_none());
    assert_eq!(files(&env.out()), ["omlx.yaml", "subscription.yaml"]);
    // 没有备份文件残留。
    assert!(
        files(&env.out())
            .iter()
            .all(|name| !name.contains(".idoris-bak"))
    );
}

#[tokio::test]
async fn force_never_touches_foreign_files_even_next_to_a_companion_card() {
    let env = Env::new(&["bge-m3-mlx-fp16"]);
    env.run(&[("embed", "bge-m3-mlx-fp16")], false)
        .await
        .unwrap();
    fs::write(env.out().join("mine.yaml"), "keep").unwrap();
    let error = env
        .run(&[("embed", "bge-m3-mlx-fp16")], true)
        .await
        .unwrap_err();
    assert_eq!(error.code, "out_dir_not_empty");
    assert_eq!(fs::read(env.out().join("mine.yaml")).unwrap(), b"keep");
    assert!(env.out().join("omlx-embed.yaml").exists());
}

#[tokio::test]
async fn the_companion_card_is_written_in_probed_adopt_mode_too() {
    let server = MockServer::start().await;
    let list = json!({"data": [{"id": "bge-m3-mlx-fp16"}]});
    for (p, body) in [
        ("/v1/models", list),
        ("/v1/models/status", json!({"models": []})),
        ("/api/status", json!({})),
    ] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    let env = Env::new(&[]);
    let mut options = env.options(&[("embed", "bge-m3-mlx-fp16")], false);
    options.target = SetupTarget::Endpoint {
        url: server.uri(),
        probe: true,
    };
    let out = run_setup(&env.setup(), &options, None).await.unwrap();
    assert_eq!(out["companion_card_file"], "omlx-embed.yaml");
    let companion = card_at(&env.out(), "omlx-embed.yaml");
    assert_eq!(companion.endpoint, server.uri());
    // 探测过的绑定对照引擎的模型列表：不在列表里就拒绝，也不写伴生卡。
    let _ = fs::remove_dir_all(env.out());
    options.bindings = vec![("embed".into(), "absent".into())];
    let error = run_setup(&env.setup(), &options, None).await.unwrap_err();
    assert_eq!(error.code, "model_not_found");
    assert!(!env.out().exists());
}

/// 托管模式：主卡带 `idoris_managed`，伴生卡不带（引擎的启停只经主卡），两张卡指向同一个私有端口。
#[tokio::test]
async fn in_managed_mode_only_the_main_card_is_managed_and_both_share_the_private_port() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(&["bge-m3-mlx-fp16"]);
    let exe = env.work.path().join("omlx-cli");
    fs::write(
        &exe,
        "#!/bin/sh\n[ \"$1\" = --version ] && echo 0.6.4-fake || exit 9\n",
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    let setup = env.setup().with_explicit_executable(&exe);
    let mut options = env.options(&[("embed", "bge-m3-mlx-fp16")], false);
    options.target = SetupTarget::Managed(idoris_router::cli::ManagedOptions { port: None });
    let out = run_setup(&setup, &options, None).await.unwrap();
    assert_eq!(out["mode"], "managed");
    assert_eq!(out["companion_card_file"], "omlx-embed.yaml");
    let main = card_at(&env.out(), "omlx.yaml");
    let companion = card_at(&env.out(), "omlx-embed.yaml");
    assert!(
        main.extensions
            .as_ref()
            .unwrap()
            .contains_key("idoris_managed")
    );
    assert!(
        !companion
            .extensions
            .as_ref()
            .unwrap()
            .contains_key("idoris_managed")
    );
    assert_eq!(main.endpoint, companion.endpoint);
    assert_eq!(main.endpoint, out["endpoint"]);
}

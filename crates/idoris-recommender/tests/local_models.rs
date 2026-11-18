#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use idoris_recommender::catalog::{ModelKind, OmlxBinding};
use idoris_recommender::local_models::{
    LocalModel, LocalModelsResponse, default_models_dir, infer_kind, scan_local_models,
};
use serde_json::json;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "idoris-local-models-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn model_dir(root: &Path, name: &str, config: &str, weights: &[(&str, usize)]) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.json"), config).unwrap();
    for (file, len) in weights {
        fs::write(dir.join(file), vec![0u8; *len]).unwrap();
    }
    dir
}

fn causal_config() -> String {
    json!({"architectures": ["Qwen3ForCausalLM"], "model_type": "qwen3"}).to_string()
}

fn binding(omlx_id: &str, catalog_id: &str, kind: Option<ModelKind>) -> OmlxBinding {
    OmlxBinding {
        catalog_id: catalog_id.into(),
        omlx_id: omlx_id.into(),
        kind,
    }
}

fn ids(models: &[LocalModel]) -> Vec<&str> {
    models.iter().map(|m| m.omlx_id.as_str()).collect()
}

#[test]
fn default_dir_is_dot_omlx_models_under_the_given_home() {
    assert_eq!(
        default_models_dir(Path::new("/h")),
        PathBuf::from("/h/.omlx/models")
    );
}

#[test]
fn lists_complete_models_sorted_with_size_and_catalog_id() {
    let tmp = TempDir::new();
    model_dir(
        tmp.path(),
        "Qwen3-8B-4bit",
        &causal_config(),
        &[("model.safetensors", 1000), ("tokenizer.json", 7)],
    );
    model_dir(
        tmp.path(),
        "Another-4bit",
        &causal_config(),
        &[("a.safetensors", 10), ("b.safetensors", 20)],
    );
    let bindings = [binding("Qwen3-8B-4bit", "qwen3-8b", Some(ModelKind::Chat))];
    let models = scan_local_models(tmp.path(), &bindings);
    assert_eq!(ids(&models), ["Another-4bit", "Qwen3-8B-4bit"]);
    let qwen = &models[1];
    assert_eq!(qwen.kind, ModelKind::Chat);
    assert_eq!(qwen.catalog_id.as_deref(), Some("qwen3-8b"));
    // config.json 的体积也计入目录体积。
    let config_len = causal_config().len() as u64;
    assert_eq!(qwen.size_bytes, 1000 + 7 + config_len);
    assert_eq!(models[0].catalog_id, None);
    assert_eq!(models[0].kind, ModelKind::Chat);
}

#[test]
fn response_serializes_with_the_spec_field_names() {
    let response = LocalModelsResponse {
        models: vec![LocalModel {
            omlx_id: "Qwen3-8B-4bit".into(),
            kind: ModelKind::Chat,
            size_bytes: 4_620_000_000,
            catalog_id: Some("qwen3-8b".into()),
        }],
    };
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        json!({"models": [{"omlx_id": "Qwen3-8B-4bit", "kind": "chat", "size_bytes": 4620000000u64, "catalog_id": "qwen3-8b"}]})
    );
    let unknown = LocalModel {
        omlx_id: "x".into(),
        kind: ModelKind::Unknown,
        size_bytes: 1,
        catalog_id: None,
    };
    let value = serde_json::to_value(&unknown).unwrap();
    assert_eq!(value["kind"], "unknown");
    assert!(
        value["catalog_id"].is_null(),
        "catalog_id 缺省是 null，不是省略"
    );
}

#[test]
fn catalog_kind_wins_over_config_and_config_wins_over_unknown() {
    let tmp = TempDir::new();
    let embedding_config = json!({"architectures": ["XLMRobertaModel"]}).to_string();
    model_dir(
        tmp.path(),
        "from-catalog",
        &embedding_config,
        &[("w.safetensors", 1)],
    );
    model_dir(
        tmp.path(),
        "from-config",
        &causal_config(),
        &[("w.safetensors", 1)],
    );
    model_dir(tmp.path(), "nothing", "{}", &[("w.safetensors", 1)]);
    model_dir(
        tmp.path(),
        "catalog-no-kind",
        &causal_config(),
        &[("w.safetensors", 1)],
    );
    let bindings = [
        binding("from-catalog", "c1", Some(ModelKind::Chat)),
        binding("catalog-no-kind", "c2", None),
    ];
    let models = scan_local_models(tmp.path(), &bindings);
    let kind = |id: &str| models.iter().find(|m| m.omlx_id == id).unwrap().kind;
    assert_eq!(kind("from-catalog"), ModelKind::Chat);
    assert_eq!(kind("from-config"), ModelKind::Chat);
    assert_eq!(kind("nothing"), ModelKind::Unknown);
    // catalog 里只有 omlx_id 没有 kind：退到 config.json，同时 catalog_id 仍然带上。
    assert_eq!(kind("catalog-no-kind"), ModelKind::Chat);
    let entry = models
        .iter()
        .find(|m| m.omlx_id == "catalog-no-kind")
        .unwrap();
    assert_eq!(entry.catalog_id.as_deref(), Some("c2"));
}

#[test]
fn invalid_json_config_is_unknown_but_the_model_is_still_listed() {
    let tmp = TempDir::new();
    model_dir(
        tmp.path(),
        "broken-config",
        "{ not json",
        &[("w.safetensors", 5)],
    );
    model_dir(
        tmp.path(),
        "good",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["broken-config", "good"]);
    assert_eq!(models[0].kind, ModelKind::Unknown);
    assert_eq!(models[1].kind, ModelKind::Chat);
    // 目录里写了 kind 时，config.json 坏了也不影响。
    let models = scan_local_models(
        tmp.path(),
        &[binding("broken-config", "c", Some(ModelKind::Vision))],
    );
    assert_eq!(models[0].kind, ModelKind::Vision);
}

#[test]
fn incomplete_directories_are_skipped_and_never_fail_the_list() {
    let tmp = TempDir::new();
    model_dir(
        tmp.path(),
        "good",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    // 没有权重。
    model_dir(
        tmp.path(),
        "no-weights",
        &causal_config(),
        &[("tokenizer.json", 5)],
    );
    // 没有 config.json。
    let dir = tmp.path().join("no-config");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("w.safetensors"), b"x").unwrap();
    // config.json 是个目录。
    let dir = tmp.path().join("config-is-dir");
    fs::create_dir_all(dir.join("config.json")).unwrap();
    fs::write(dir.join("w.safetensors"), b"x").unwrap();
    // 下载残留。
    model_dir(
        tmp.path(),
        "downloading",
        &causal_config(),
        &[("w.safetensors", 5), ("w2.safetensors.incomplete", 5)],
    );
    // 分片缺失。
    let dir = model_dir(
        tmp.path(),
        "missing-shard",
        &causal_config(),
        &[("model-00001.safetensors", 5)],
    );
    fs::write(
        dir.join("model.safetensors.index.json"),
        json!({"weight_map": {"a": "model-00001.safetensors", "b": "model-00002.safetensors"}})
            .to_string(),
    )
    .unwrap();
    // 分片齐全。
    let dir = model_dir(
        tmp.path(),
        "sharded-ok",
        &causal_config(),
        &[
            ("model-00001.safetensors", 5),
            ("model-00002.safetensors", 6),
        ],
    );
    fs::write(
        dir.join("model.safetensors.index.json"),
        json!({"weight_map": {"a": "model-00001.safetensors", "b": "model-00002.safetensors"}})
            .to_string(),
    )
    .unwrap();
    // 索引里的分片名企图逃出目录。
    let dir = model_dir(
        tmp.path(),
        "escaping-shard",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    fs::write(
        dir.join("model.safetensors.index.json"),
        json!({"weight_map": {"a": "../good/w.safetensors"}}).to_string(),
    )
    .unwrap();
    // 目录下的普通文件、隐藏暂存目录、空目录。
    fs::write(tmp.path().join("README.txt"), b"hi").unwrap();
    model_dir(
        tmp.path(),
        ".cache",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    fs::create_dir_all(tmp.path().join("empty")).unwrap();

    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["good", "sharded-ok"]);
}

#[test]
fn missing_or_unreadable_models_dir_is_an_empty_list() {
    let tmp = TempDir::new();
    assert!(scan_local_models(&tmp.path().join("absent"), &[]).is_empty());
    // 路径是个文件。
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();
    assert!(scan_local_models(&file, &[]).is_empty());
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_followed() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new();
    let outside = TempDir::new();
    model_dir(
        outside.path(),
        "Escaped",
        &causal_config(),
        &[("w.safetensors", 999)],
    );
    model_dir(
        tmp.path(),
        "good",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    // 模型目录本身是符号链接：整项跳过。
    symlink(
        outside.path().join("Escaped"),
        tmp.path().join("linked-dir"),
    )
    .unwrap();
    // 悬空链接、指向文件的链接：跳过，不报错。
    symlink(tmp.path().join("nope"), tmp.path().join("dangling")).unwrap();
    symlink(
        tmp.path().join("good").join("config.json"),
        tmp.path().join("linked-file"),
    )
    .unwrap();
    // 目录内的符号链接文件：既不计体积，也不算权重。
    let only_link = model_dir(tmp.path(), "only-symlinked-weights", &causal_config(), &[]);
    symlink(
        outside.path().join("Escaped").join("w.safetensors"),
        only_link.join("w.safetensors"),
    )
    .unwrap();
    let with_link = model_dir(
        tmp.path(),
        "with-extra-link",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    symlink(
        outside.path().join("Escaped").join("w.safetensors"),
        with_link.join("extra.safetensors"),
    )
    .unwrap();
    // 符号链接形式的 config.json 不算数。
    let link_config = tmp.path().join("linked-config");
    fs::create_dir_all(&link_config).unwrap();
    fs::write(link_config.join("w.safetensors"), b"x").unwrap();
    symlink(
        outside.path().join("Escaped").join("config.json"),
        link_config.join("config.json"),
    )
    .unwrap();
    // 目录内的符号链接子目录不会被走进去（避免环与逃逸）。
    let loop_dir = model_dir(
        tmp.path(),
        "loop",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    symlink(&loop_dir, loop_dir.join("self")).unwrap();

    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["good", "loop", "with-extra-link"]);
    let config_len = causal_config().len() as u64;
    let with_extra = models
        .iter()
        .find(|m| m.omlx_id == "with-extra-link")
        .unwrap();
    assert_eq!(with_extra.size_bytes, 5 + config_len);
}

#[cfg(unix)]
#[test]
fn unreadable_config_json_skips_that_model_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new();
    model_dir(
        tmp.path(),
        "good",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    let locked = model_dir(
        tmp.path(),
        "locked",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    let config = locked.join("config.json");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
    let models = scan_local_models(tmp.path(), &[]);
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    // 以 root 身份读得到文件时两项都在；普通用户读不了，只剩 good。两种情形下 good 都必须在。
    assert!(ids(&models).contains(&"good"));
    assert!(ids(&models).len() <= 2);
}

#[test]
fn infer_kind_table() {
    let cases = [
        (
            json!({"architectures": ["Qwen3ForCausalLM"], "model_type": "qwen3"}),
            ModelKind::Chat,
        ),
        (
            json!({"architectures": ["LlamaForCausalLM"]}),
            ModelKind::Chat,
        ),
        (
            json!({"architectures": ["GPT2LMHeadModel"]}),
            ModelKind::Chat,
        ),
        (
            json!({"architectures": ["Qwen3_5ForConditionalGeneration"], "vision_config": {"depth": 1}}),
            ModelKind::Vision,
        ),
        (
            json!({"architectures": ["Qwen2VLForConditionalGeneration"]}),
            ModelKind::Vision,
        ),
        (
            json!({"architectures": ["LlavaForCausalLM"], "vision_config": {}}),
            ModelKind::Vision,
        ),
        // 没有 vision_config 的 ConditionalGeneration（Whisper / T5 一类）不当 chat。
        (
            json!({"architectures": ["WhisperForConditionalGeneration"]}),
            ModelKind::Unknown,
        ),
        (
            json!({"architectures": ["T5ForConditionalGeneration"]}),
            ModelKind::Unknown,
        ),
        (
            json!({"architectures": ["XLMRobertaModel"]}),
            ModelKind::Embedding,
        ),
        (
            json!({"architectures": ["BertModel"]}),
            ModelKind::Embedding,
        ),
        (
            json!({"architectures": ["Qwen3EmbeddingModel"]}),
            ModelKind::Embedding,
        ),
        (
            json!({"architectures": ["XLMRobertaForSequenceClassification"]}),
            ModelKind::Rerank,
        ),
        (
            json!({"architectures": ["Qwen3ForRerank"]}),
            ModelKind::Rerank,
        ),
        (
            json!({"architectures": ["DeepseekOCRForCausalLM"]}),
            ModelKind::Ocr,
        ),
        (json!({"model_type": "got_ocr2"}), ModelKind::Ocr),
        (json!({"architectures": ["CosyVoiceModel"]}), ModelKind::Tts),
        (json!({"model_type": "kokoro_tts"}), ModelKind::Tts),
        (
            json!({"architectures": ["FluxTransformer2DModel"]}),
            ModelKind::Image,
        ),
        (
            json!({"_class_name": "StableDiffusionPipeline", "model_type": "diffusion"}),
            ModelKind::Image,
        ),
        (json!({}), ModelKind::Unknown),
        (json!({"architectures": []}), ModelKind::Unknown),
        (
            json!({"architectures": "Qwen3ForCausalLM"}),
            ModelKind::Unknown,
        ),
        (json!([1, 2]), ModelKind::Unknown),
        (
            json!({"architectures": ["SomethingElse"]}),
            ModelKind::Unknown,
        ),
    ];
    for (config, expected) in cases {
        assert_eq!(infer_kind(&config), expected, "{config}");
    }
}

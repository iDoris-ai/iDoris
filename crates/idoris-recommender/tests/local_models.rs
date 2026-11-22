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

/// #609：损坏或截断的 `config.json` 说明下载没收完，首次加载必失败，不能报已安装（推荐会因此跳过下载）。
#[test]
fn a_corrupt_config_json_means_not_installed() {
    let tmp = TempDir::new();
    let cases: [(&str, &str); 6] = [
        ("truncated", r#"{"architectures":["Qwen3ForCausal"#),
        ("not-json", "{ not json"),
        ("empty-config", ""),
        ("array-root", r#"["architectures"]"#),
        ("string-root", r#""hello""#),
        ("null-root", "null"),
    ];
    for (name, config) in cases {
        // 权重是非空的：拦下这个目录的只有损坏的 config。
        model_dir(tmp.path(), name, config, &[("w.safetensors", 5)]);
    }
    model_dir(
        tmp.path(),
        "good",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["good"]);
    // 目录里写了 kind 也救不了：不完整就是不完整。
    let bound: Vec<_> = cases
        .iter()
        .map(|(name, _)| binding(name, "c", Some(ModelKind::Vision)))
        .collect();
    assert_eq!(
        ids(&scan_local_models(tmp.path(), &bound)),
        ["good"],
        "a catalog binding must not resurrect a corrupt directory"
    );
    // `{}` 是合法的 JSON 对象：照旧列出，`kind` 是 unknown。
    model_dir(tmp.path(), "empty-object", "{}", &[("w.safetensors", 5)]);
    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["empty-object", "good"]);
    assert_eq!(models[0].kind, ModelKind::Unknown);
}

/// #609 的原始复现：截断的 config + 一个零字节填充的权重文件。
#[test]
fn truncated_config_with_a_zero_byte_weight_is_not_installed() {
    let tmp = TempDir::new();
    model_dir(
        tmp.path(),
        "half-downloaded",
        r#"{"architectures":["Qwen3ForCausalLM"],"model_ty"#,
        &[("model.safetensors", 0)],
    );
    assert!(scan_local_models(tmp.path(), &[]).is_empty());
}

#[test]
fn zero_byte_weights_and_zero_byte_shards_do_not_count() {
    let tmp = TempDir::new();
    // 只有零字节权重：不算权重。
    model_dir(
        tmp.path(),
        "only-empty-weight",
        &causal_config(),
        &[("model.safetensors", 0)],
    );
    // 一个非空、一个零字节：非空的算数，目录完整。
    model_dir(
        tmp.path(),
        "one-real-weight",
        &causal_config(),
        &[("a.safetensors", 7), ("b.safetensors", 0)],
    );
    // 索引列出的分片里有零字节的：不完整。
    let dir = model_dir(
        tmp.path(),
        "empty-shard",
        &causal_config(),
        &[
            ("model-00001.safetensors", 5),
            ("model-00002.safetensors", 0),
        ],
    );
    fs::write(
        dir.join("model.safetensors.index.json"),
        json!({"weight_map": {"a": "model-00001.safetensors", "b": "model-00002.safetensors"}})
            .to_string(),
    )
    .unwrap();
    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["one-real-weight"]);
}

/// #609：隐藏目录（暂存）里的权重不计体积、不算权重；文件名以 `.` 开头同理。
#[test]
fn hidden_components_inside_a_model_dir_are_excluded_from_size_and_weights() {
    let tmp = TempDir::new();
    let config_len = causal_config().len() as u64;
    let dir = model_dir(
        tmp.path(),
        "with-staging",
        &causal_config(),
        &[("w.safetensors", 5), (".dotfile.safetensors", 1000)],
    );
    fs::create_dir_all(dir.join(".cache").join("deep")).unwrap();
    fs::write(dir.join(".cache").join("big.safetensors"), vec![0u8; 2000]).unwrap();
    fs::write(
        dir.join(".cache").join("deep").join("bigger.gguf"),
        vec![0u8; 3000],
    )
    .unwrap();
    // 只有隐藏处有权重：不完整。
    let only_hidden = model_dir(tmp.path(), "only-hidden-weights", &causal_config(), &[]);
    fs::create_dir_all(only_hidden.join(".staging")).unwrap();
    fs::write(only_hidden.join(".staging").join("w.safetensors"), b"x").unwrap();
    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["with-staging"]);
    assert_eq!(models[0].size_bytes, 5 + config_len);
}

/// 隐藏目录里的下载残留标记仍然让目录不完整（残留检查在隐藏排除之前）。
#[test]
fn a_partial_marker_inside_a_hidden_directory_still_blocks_installed() {
    let tmp = TempDir::new();
    let dir = model_dir(tmp.path(), "m", &causal_config(), &[("w.safetensors", 5)]);
    fs::create_dir_all(dir.join(".cache")).unwrap();
    fs::write(dir.join(".cache").join("x.safetensors.incomplete"), b"x").unwrap();
    assert!(scan_local_models(tmp.path(), &[]).is_empty());
}

/// #609：符号链接模型目录（含 HF snapshot 风格）本版不支持，按未安装；规范 §2.9 勘误十一写明了取舍。
#[cfg(unix)]
#[test]
fn hf_snapshot_style_symlinked_model_dirs_are_documented_as_unsupported() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new();
    let cache = TempDir::new();
    let snapshot = cache.path().join("snapshots").join("abc123");
    fs::create_dir_all(&snapshot).unwrap();
    let blobs = cache.path().join("blobs");
    fs::create_dir_all(&blobs).unwrap();
    fs::write(blobs.join("w"), b"weights").unwrap();
    fs::write(blobs.join("c"), causal_config()).unwrap();
    symlink(blobs.join("w"), snapshot.join("model.safetensors")).unwrap();
    symlink(blobs.join("c"), snapshot.join("config.json")).unwrap();
    // 目录级链接指向 snapshot；snapshot 里的文件本身也是链接。
    symlink(&snapshot, tmp.path().join("Linked-Model")).unwrap();
    // 在 models 根内的别名也一样不支持。
    model_dir(
        tmp.path(),
        "Real-Model",
        &causal_config(),
        &[("w.safetensors", 5)],
    );
    symlink(tmp.path().join("Real-Model"), tmp.path().join("Alias")).unwrap();
    let models = scan_local_models(tmp.path(), &[]);
    assert_eq!(ids(&models), ["Real-Model"]);
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

/// 把 `prefix` + 填充 + `suffix` 凑成恰好 `total` 字节的合法 JSON（填充放在一个字符串字段里）。
fn json_of_exact_len(prefix: &str, suffix: &str, total: usize) -> String {
    let overhead = prefix.len() + suffix.len() + r#","pad":"""#.len();
    assert!(total > overhead);
    format!(
        r#"{prefix},"pad":"{}"{suffix}"#,
        "x".repeat(total - overhead)
    )
}

const READ_LIMIT: usize = 4 * 1024 * 1024;

/// #609（Codex）：`config.json` 与索引按句柄读，上限是 4 MiB：恰好 4 MiB 可以，多 1 字节拒绝。
#[test]
fn config_json_size_boundary_is_exactly_four_mib() {
    let tmp = TempDir::new();
    let prefix = r#"{"architectures":["Qwen3ForCausalLM"]"#;
    for (name, total) in [
        ("at-limit", READ_LIMIT),
        ("one-over", READ_LIMIT + 1),
        ("one-under", READ_LIMIT - 1),
    ] {
        let config = json_of_exact_len(prefix, "}", total);
        assert_eq!(config.len(), total);
        model_dir(tmp.path(), name, &config, &[("w.safetensors", 5)]);
    }
    assert_eq!(
        ids(&scan_local_models(tmp.path(), &[])),
        ["at-limit", "one-under"]
    );
}

#[test]
fn index_json_size_boundary_is_exactly_four_mib_and_is_never_read_unbounded() {
    let tmp = TempDir::new();
    let prefix = r#"{"weight_map":{"a":"w.safetensors"}"#;
    for (name, total) in [
        ("at-limit", READ_LIMIT),
        ("one-over", READ_LIMIT + 1),
        ("far-over", READ_LIMIT * 3),
    ] {
        let dir = model_dir(tmp.path(), name, &causal_config(), &[("w.safetensors", 5)]);
        fs::write(
            dir.join("model.safetensors.index.json"),
            json_of_exact_len(prefix, "}", total),
        )
        .unwrap();
    }
    assert_eq!(ids(&scan_local_models(tmp.path(), &[])), ["at-limit"]);
}

fn with_index(root: &Path, name: &str, files: &[(&str, usize)], index: serde_json::Value) {
    let dir = model_dir(root, name, &causal_config(), files);
    fs::write(dir.join("model.safetensors.index.json"), index.to_string()).unwrap();
}

/// #609（Codex）：索引必须有非空 `weight_map`；分片（去重后）必须是遍历时认定的「顶层、可见、非空权重」。
#[test]
fn a_shard_index_must_be_non_empty_and_every_shard_must_be_a_visible_non_empty_weight() {
    let tmp = TempDir::new();
    let good = &[("model-1.safetensors", 5), ("model-2.safetensors", 6)];
    // 合格：两个分片齐全，重复引用同一个分片不影响。
    with_index(
        tmp.path(),
        "ok",
        good,
        json!({"weight_map": {"a": "model-1.safetensors", "b": "model-2.safetensors",
                              "c": "model-1.safetensors"}}),
    );
    // 空 weight_map。
    with_index(tmp.path(), "empty-map", good, json!({"weight_map": {}}));
    // 分片是空文件（索引之外还有别的非空权重，目录本身有权重）。
    with_index(
        tmp.path(),
        "empty-shard",
        &[("model-1.safetensors", 5), ("model-2.safetensors", 0)],
        json!({"weight_map": {"a": "model-1.safetensors", "b": "model-2.safetensors"}}),
    );
    // 分片是隐藏文件。
    with_index(
        tmp.path(),
        "hidden-shard",
        &[("model-1.safetensors", 5), (".hidden.safetensors", 6)],
        json!({"weight_map": {"a": "model-1.safetensors", "b": ".hidden.safetensors"}}),
    );
    // 分片不是权重文件（普通的非空文件）。
    with_index(
        tmp.path(),
        "not-a-weight",
        &[("model-1.safetensors", 5), ("notes.txt", 6)],
        json!({"weight_map": {"a": "model-1.safetensors", "b": "notes.txt"}}),
    );
    // 分片是目录。
    let dir = model_dir(
        tmp.path(),
        "dir-shard",
        &causal_config(),
        &[("model-1.safetensors", 5)],
    );
    fs::create_dir_all(dir.join("model-2.safetensors")).unwrap();
    fs::write(
        dir.join("model.safetensors.index.json"),
        json!({"weight_map": {"a": "model-1.safetensors", "b": "model-2.safetensors"}}).to_string(),
    )
    .unwrap();
    // 分片在子目录里（名字里带路径）。
    let dir = model_dir(
        tmp.path(),
        "nested-shard",
        &causal_config(),
        &[("model-1.safetensors", 5)],
    );
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub").join("model-2.safetensors"), b"x").unwrap();
    fs::write(
        dir.join("model.safetensors.index.json"),
        json!({"weight_map": {"a": "model-1.safetensors", "b": "sub/model-2.safetensors"}})
            .to_string(),
    )
    .unwrap();
    // 值不是字符串、空字符串。
    with_index(
        tmp.path(),
        "non-string",
        good,
        json!({"weight_map": {"a": 1}}),
    );
    with_index(
        tmp.path(),
        "empty-name",
        good,
        json!({"weight_map": {"a": ""}}),
    );
    // 索引本身是目录。
    let dir = model_dir(
        tmp.path(),
        "index-is-dir",
        &causal_config(),
        &[("model-1.safetensors", 5)],
    );
    fs::create_dir_all(dir.join("model.safetensors.index.json")).unwrap();
    assert_eq!(ids(&scan_local_models(tmp.path(), &[])), ["ok"]);
}

//! owner 的硬规则：契约里不出现任何云厂商专属的枚举值、示例或默认值（尤其是 AWS / Bedrock / S3 一类）。
//! 示例夹具里的供应商和区域一律用明显的假名（`example-*`、`sg-1`）。
//!
//! 这条测试扫描本 crate 的全部源码、测试、示例、夹具和快照，只要出现被禁的词就失败。它故意不扫描自己：
//! 被禁的词在这里是用拼接的方式写出来的，避免文件本身命中。
//!
//! 区域是**标签**，不是枚举：`permit::Region` 只检查格式，不收录、不认识任何厂商的区域命名。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// 被禁的整词（按 ASCII 字母数字切词后，忽略大小写比较）。用拼接写出，避免本文件自己命中。
fn forbidden_words() -> Vec<String> {
    vec![
        ["a", "ws"].concat(),
        ["bed", "rock"].concat(),
        ["ama", "zon"].concat(),
        ["ama", "zonaws"].concat(),
        ["sage", "maker"].concat(),
        ["cloud", "front"].concat(),
        ["e", "c2"].concat(),
    ]
}

/// 形如 `us-east-1`、`ap-southeast-2`、`eu-central-1` 的厂商区域名：
/// 两位字母 + 方位词（可带一个修饰段）+ 数字。
fn looks_like_a_cloud_vendor_region(token: &str) -> bool {
    let parts: Vec<&str> = token.split('-').collect();
    if parts.len() < 3 {
        return false;
    }
    let directions = [
        "north",
        "south",
        "east",
        "west",
        "central",
        "northeast",
        "northwest",
        "southeast",
        "southwest",
    ];
    let first = parts[0];
    let last = parts[parts.len() - 1];
    first.len() == 2
        && first.bytes().all(|b| b.is_ascii_lowercase())
        && directions.contains(&parts[1])
        && !last.is_empty()
        && last.bytes().all(|b| b.is_ascii_digit())
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn crate_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("Cargo.toml")];
    for sub in ["src", "tests", "examples", "testdata"] {
        let dir = root.join(sub);
        if dir.is_dir() {
            files_under(&dir, &mut files);
        }
    }
    files
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n != "no_vendor_specific_names.rs")
        })
        .collect()
}

/// 把文本按「不是 ASCII 字母数字」切开，得到纯词（用来查被禁的整词）。
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
}

/// 把文本按「不是小写字母、数字、连字符」切开，得到可能带连字符的标记（用来查区域名的形状）。
fn hyphenated_tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| w.contains('-'))
        .map(str::to_ascii_lowercase)
}

#[test]
fn no_cloud_vendor_specific_names_anywhere_in_the_crate() {
    let forbidden = forbidden_words();
    let mut violations = Vec::new();
    let files = crate_files();
    assert!(
        files.len() > 20,
        "scan found suspiciously few files: {}",
        files.len()
    );
    for path in &files {
        let Ok(text) = fs::read_to_string(path) else {
            continue; // 非 UTF-8 的文件不是文本，不会含有这些词
        };
        for word in words(&text) {
            let hit = forbidden.iter().any(|bad| {
                word == *bad || (bad == &forbidden[0] && word.starts_with(bad.as_str()))
            });
            if hit {
                violations.push(format!("{}: forbidden word {word:?}", path.display()));
            }
        }
        for token in hyphenated_tokens(&text) {
            if looks_like_a_cloud_vendor_region(&token) {
                violations.push(format!(
                    "{}: vendor-style region name {token:?}",
                    path.display()
                ));
            }
        }
        if text.to_ascii_lowercase().contains(&["s", "3://"].concat()) {
            violations.push(format!("{}: object-store URL", path.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "cloud-vendor-specific content is forbidden in the contracts:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_detector_itself_works() {
    // 正控：检测器能认出被禁的东西，否则上面那条「没命中」没有意义。
    let bad_word = ["a", "ws"].concat();
    assert!(words(&format!("use {bad_word}-sdk here")).any(|w| w == bad_word));
    assert!(words("laws and draws").all(|w| w != bad_word));
    for region in [
        "us-east-1",
        "ap-southeast-2",
        "eu-central-1",
        "ap-northeast-1",
    ] {
        assert!(looks_like_a_cloud_vendor_region(region), "{region}");
    }
    // 合法的假标签不会被误伤
    for label in [
        "sg-1",
        "eu-1",
        "th-1",
        "cn-1",
        "th-central",
        "region-a-2",
        "example-provider",
    ] {
        assert!(!looks_like_a_cloud_vendor_region(label), "{label}");
    }
}

#[test]
fn region_is_a_plain_label_the_contract_knows_no_vendor_naming() {
    use idoris_compute_contracts::permit::Region;
    // 例子与夹具里用的假标签
    for label in ["sg-1", "eu-1", "th-1", "cn-1"] {
        assert!(Region::new(label).is_ok(), "{label}");
    }
    // 契约里没有任何区域枚举或默认值：两个空白的、无关的标签同样有效，不存在「已知区域表」
    assert!(Region::new("zz-9").is_ok());
    assert!(Region::new("region-without-meaning").is_ok());
}

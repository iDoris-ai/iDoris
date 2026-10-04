#![allow(dead_code)]

pub fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../idoris-upstream/tests/fixtures/subscription_fake_cli.sh")
}

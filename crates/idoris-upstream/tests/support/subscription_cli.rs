#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

pub struct FakeSubscriptionCli {
    root: tempfile::TempDir,
    bin_dir: PathBuf,
}

impl FakeSubscriptionCli {
    pub fn install() -> Self {
        let root = tempfile::TempDir::new().expect("fake CLI tempdir");
        let bin_dir = root.path().join("bin");
        fs::create_dir(&bin_dir).expect("fake CLI bin dir");
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/subscription_fake_cli.sh");
        for name in ["codex", "claude"] {
            let target = bin_dir.join(name);
            // Link to the stable, already-executable fixture instead of
            // copying a just-written executable and immediately exec'ing it.
            // Linux overlay filesystems can otherwise transiently return
            // ETXTBSY ("Text file busy") under parallel tests.
            symlink(&fixture, &target).expect("link fake CLI fixture");
        }
        Self { root, bin_dir }
    }

    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }
    pub fn program(&self, name: &str) -> PathBuf {
        self.bin_dir.join(name)
    }
    pub fn marker(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
}

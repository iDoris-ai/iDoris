#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use idoris_upstream::subscription::workspace::SubscriptionWorkspace;

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn workspace_has_private_control_and_restricted_request_cwd() {
    let workspace = SubscriptionWorkspace::create().unwrap();
    let root = workspace.cwd().to_path_buf();
    assert_eq!(mode(&root), 0o555);
    assert_eq!(mode(workspace.control_dir()), 0o700);
    assert_eq!(mode(workspace.result_file()), 0o600);

    let direct = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(root.join("ordinary-write"));
    assert!(
        direct.is_err(),
        "0555 cwd must reject an ordinary direct write"
    );

    // Mutation/positive control: same-UID code can chmod the directory. This
    // proves the 0555 check is real, and also why it must not be described as
    // a strong same-UID sandbox boundary.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("writable-control"), b"ok").unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();

    workspace.cleanup_after_reap().unwrap();
    assert!(!root.exists());
}

#[test]
fn drop_without_reap_confirmation_leaves_the_restricted_workspace() {
    let workspace = SubscriptionWorkspace::create().unwrap();
    let root = workspace.cwd().to_path_buf();
    drop(workspace);

    assert!(root.exists());
    assert_eq!(mode(&root), 0o555);

    // Test-only recovery of our own leaked fixture. Production lifecycle code
    // must use cleanup_after_reap after proving the process group is gone.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(root).unwrap();
}

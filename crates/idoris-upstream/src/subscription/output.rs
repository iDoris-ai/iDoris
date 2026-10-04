use std::fs::File;
use std::io::Read;

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use super::error::{SubscriptionErrorCode, SubscriptionRelayError};
use super::workspace::SubscriptionWorkspace;

pub const MAX_RESULT_BYTES: usize = 256 * 1024;

pub fn select_output(
    workspace: &SubscriptionWorkspace,
    stdout: &str,
    exit_code: Option<i32>,
) -> Result<String, SubscriptionRelayError> {
    if exit_code != Some(0) {
        return Err(SubscriptionRelayError::new(
            SubscriptionErrorCode::CliFailed,
        ));
    }

    let file = match open_result(workspace) {
        Ok(Some(file)) => file,
        Ok(None) => return non_empty(stdout),
        Err(error) => return Err(error),
    };
    let bytes = read_open_result(file)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
    if text.trim().is_empty() {
        non_empty(stdout)
    } else {
        Ok(text.trim().to_string())
    }
}

#[cfg(unix)]
fn open_result(workspace: &SubscriptionWorkspace) -> Result<Option<File>, SubscriptionRelayError> {
    let path = workspace.result_file();
    if path.parent() != Some(workspace.control_dir()) {
        return Err(SubscriptionRelayError::new(
            SubscriptionErrorCode::CliFailed,
        ));
    }
    match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => {
            let metadata = file
                .metadata()
                .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
            if !metadata.file_type().is_file() {
                return Err(SubscriptionRelayError::new(
                    SubscriptionErrorCode::CliFailed,
                ));
            }
            Ok(Some(file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(SubscriptionRelayError::new(
            SubscriptionErrorCode::CliFailed,
        )),
    }
}

#[cfg(not(unix))]
fn open_result(_workspace: &SubscriptionWorkspace) -> Result<Option<File>, SubscriptionRelayError> {
    Err(SubscriptionRelayError::new(
        SubscriptionErrorCode::CliFailed,
    ))
}

fn read_open_result(file: File) -> Result<Vec<u8>, SubscriptionRelayError> {
    let mut bytes = Vec::new();
    file.take((MAX_RESULT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
    if bytes.len() > MAX_RESULT_BYTES {
        return Err(SubscriptionRelayError::new(
            SubscriptionErrorCode::OutputLimit,
        ));
    }
    Ok(bytes)
}

fn non_empty(value: &str) -> Result<String, SubscriptionRelayError> {
    let value = value.trim();
    if value.is_empty() {
        Err(SubscriptionRelayError::new(
            SubscriptionErrorCode::EmptyOutput,
        ))
    } else {
        Ok(value.to_string())
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn nonzero_exit_cannot_be_overridden_by_result_file() {
        let workspace = SubscriptionWorkspace::create().unwrap();
        fs::write(workspace.result_file(), b"looks-successful").unwrap();
        let error = select_output(&workspace, "stdout", Some(23)).unwrap_err();
        assert_eq!(error.reason_code(), "RELAY_CLI_FAILED");
        workspace.cleanup_after_reap().unwrap();
    }

    #[test]
    fn nonempty_result_wins_and_missing_or_empty_falls_back_to_stdout() {
        let workspace = SubscriptionWorkspace::create().unwrap();
        fs::write(workspace.result_file(), b"  from-file  \n").unwrap();
        assert_eq!(
            select_output(&workspace, "from-stdout", Some(0)).unwrap(),
            "from-file"
        );
        fs::write(workspace.result_file(), b" \n\t ").unwrap();
        assert_eq!(
            select_output(&workspace, "  fallback  ", Some(0)).unwrap(),
            "fallback"
        );
        fs::remove_file(workspace.result_file()).unwrap();
        assert_eq!(
            select_output(&workspace, "stdout-only", Some(0)).unwrap(),
            "stdout-only"
        );
        workspace.cleanup_after_reap().unwrap();
    }

    #[test]
    fn symlink_directory_and_oversized_result_are_rejected() {
        let workspace = SubscriptionWorkspace::create().unwrap();
        let outside = workspace.cwd().join("outside.txt");
        fs::write(&outside, b"SECRET").unwrap_err();

        fs::remove_file(workspace.result_file()).unwrap();
        symlink("/etc/hosts", workspace.result_file()).unwrap();
        assert_eq!(
            select_output(&workspace, "fallback", Some(0))
                .unwrap_err()
                .reason_code(),
            "RELAY_CLI_FAILED"
        );
        fs::remove_file(workspace.result_file()).unwrap();
        fs::create_dir(workspace.result_file()).unwrap();
        assert_eq!(
            select_output(&workspace, "fallback", Some(0))
                .unwrap_err()
                .reason_code(),
            "RELAY_CLI_FAILED"
        );
        fs::remove_dir(workspace.result_file()).unwrap();
        fs::write(workspace.result_file(), vec![b'x'; MAX_RESULT_BYTES + 1]).unwrap();
        assert_eq!(
            select_output(&workspace, "fallback", Some(0))
                .unwrap_err()
                .reason_code(),
            "RELAY_OUTPUT_LIMIT"
        );
        workspace.cleanup_after_reap().unwrap();
    }

    #[test]
    fn open_handle_is_stable_across_path_replacement() {
        let workspace = SubscriptionWorkspace::create().unwrap();
        fs::write(workspace.result_file(), b"original").unwrap();
        let file = open_result(&workspace).unwrap().unwrap();
        let replacement = workspace.control_dir().join("replacement.txt");
        fs::write(&replacement, b"replacement").unwrap();
        fs::rename(&replacement, workspace.result_file()).unwrap();

        assert_eq!(read_open_result(file).unwrap(), b"original");
        assert_eq!(
            fs::read(workspace.result_file()).unwrap(),
            b"replacement".to_vec()
        );
        workspace.cleanup_after_reap().unwrap();
    }
}

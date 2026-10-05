use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const ROOT_MODE: u32 = 0o555;
const CONTROL_MODE: u32 = 0o700;
const RESULT_MODE: u32 = 0o600;

#[derive(Debug)]
pub enum WorkspaceError {
    UnsupportedPlatform,
    Io(std::io::Error),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                f.write_str("subscription workspace isolation is unsupported on this platform")
            }
            Self::Io(error) => write!(f, "subscription workspace setup failed: {error}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<std::io::Error> for WorkspaceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Per-request CLI workspace owned entirely by iDoris.
///
/// The root is mode 0555 to stop ordinary cooperative writes, while the
/// private control directory/result file remain 0700/0600. This is not a
/// same-UID security sandbox: a child running as the same OS user may chmod
/// paths it owns. Strong filesystem isolation, if required, is a separate OS
/// sandbox concern.
///
/// Dropping this value deliberately does not delete the directory. The future
/// process-group owner must first confirm the whole group is reaped, then call
/// [`Self::cleanup_after_reap`]. If cleanup is never confirmed, leaving the
/// restricted workspace behind is safer than deleting files a surviving child
/// might still be using.
#[derive(Debug)]
pub struct SubscriptionWorkspace {
    root: PathBuf,
    control: PathBuf,
    result: PathBuf,
}

impl SubscriptionWorkspace {
    pub fn create() -> Result<Self, WorkspaceError> {
        #[cfg(unix)]
        {
            Self::create_under(None)
        }
        #[cfg(not(unix))]
        {
            Err(WorkspaceError::UnsupportedPlatform)
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.root
    }

    pub fn control_dir(&self) -> &Path {
        &self.control
    }

    pub fn result_file(&self) -> &Path {
        &self.result
    }

    /// Call only after the lifecycle owner has confirmed the child process
    /// group is fully reaped.
    pub fn cleanup_after_reap(self) -> Result<(), WorkspaceError> {
        #[cfg(unix)]
        {
            set_mode(&self.root, CONTROL_MODE)?;
            set_mode(&self.control, CONTROL_MODE)?;
            if self.result.exists() {
                set_mode(&self.result, RESULT_MODE)?;
            }
            fs::remove_dir_all(&self.root)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(WorkspaceError::UnsupportedPlatform)
        }
    }

    #[cfg(unix)]
    fn create_under(base: Option<&Path>) -> Result<Self, WorkspaceError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("idoris-subscription-");
        let temp = match base {
            Some(base) => builder.tempdir_in(base)?,
            None => builder.tempdir()?,
        };
        let root = temp.path().to_path_buf();
        let control = root.join("control");
        fs::create_dir(&control)?;
        set_mode(&control, CONTROL_MODE)?;

        let result = control.join("result.txt");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(RESULT_MODE)
            .open(&result)?;
        set_mode(&result, RESULT_MODE)?;
        set_mode(&root, ROOT_MODE)?;
        let persisted = temp.keep();
        debug_assert_eq!(persisted, root);

        Ok(Self {
            root,
            control,
            result,
        })
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), WorkspaceError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn tempdir_failure_cannot_reach_the_spawn_step() {
        let outer = tempfile::TempDir::new().unwrap();
        let blocker = outer.path().join("not-a-directory");
        fs::write(&blocker, b"x").unwrap();
        let spawned = AtomicUsize::new(0);

        let result = SubscriptionWorkspace::create_under(Some(&blocker));
        if result.is_ok() {
            spawned.fetch_add(1, Ordering::SeqCst);
        }

        assert!(result.is_err());
        assert_eq!(spawned.load(Ordering::SeqCst), 0);
    }
}

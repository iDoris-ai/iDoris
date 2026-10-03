//! Durable marker that prevents loads after a possibly interrupted load.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::BackendError;

/// A durable, fail-closed marker associated with one runtime engine.
pub struct LoadFence {
    path: PathBuf,
}

impl LoadFence {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Returns an error whenever the marker exists or its state cannot be
    /// determined (including permission and other filesystem errors).
    pub fn check_clear(&self) -> Result<(), BackendError> {
        match File::open(&self.path) {
            Ok(_) => Err(BackendError::internal(format!(
                "durable load fence is present at {}",
                self.path.display()
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("checking", &self.path, error)),
        }
    }

    /// Persists the marker before a load can be issued. It is never removed
    /// implicitly, including when this value is dropped.
    pub fn begin(&self) -> Result<(), BackendError> {
        let parent = parent_dir(&self.path);
        ensure_parent_durable(parent)?;
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|error| io_error("creating", &self.path, error))?;
        marker
            .write_all(b"idoris load in progress\n")
            .and_then(|()| marker.sync_all())
            .map_err(|error| io_error("syncing", &self.path, error))?;
        sync_directory(parent).map_err(|error| io_error("syncing directory for", parent, error))
    }

    /// Removes the marker and durably records the directory update.
    pub fn clear(&self) -> Result<(), BackendError> {
        match fs::remove_file(&self.path) {
            Ok(()) => sync_directory(parent_dir(&self.path))
                .map_err(|error| io_error("syncing directory for", parent_dir(&self.path), error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("removing", &self.path, error)),
        }
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn ensure_parent_durable(path: &Path) -> Result<(), BackendError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(BackendError::internal(format!(
                "load fence parent is not a directory: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("checking directory", path, error)),
    }

    if let Some(parent) = path.parent().filter(|parent| *parent != path) {
        ensure_parent_durable(parent)?;
    }
    match fs::create_dir(path) {
        Ok(()) => {
            if let Some(parent) = path.parent().filter(|parent| *parent != path) {
                sync_directory(parent)
                    .map_err(|error| io_error("syncing directory for", parent, error))?;
            }
            sync_directory(path).map_err(|error| io_error("syncing directory", path, error))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata =
                fs::metadata(path).map_err(|error| io_error("checking directory", path, error))?;
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(io_error("creating directory", path, error))
            }
        }
        Err(error) => Err(io_error("creating directory", path, error)),
    }
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> BackendError {
    BackendError::internal(format!(
        "{action} load fence at {}: {error}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "idoris-load-fence-test-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                label
            ))
            .join("nested")
            .join("marker")
    }

    #[test]
    fn marker_survives_reconstructing_the_fence() {
        let path = test_path("restart");
        let fence = LoadFence::new(path.clone());
        fence.begin().expect("begin should persist marker");
        assert!(LoadFence::new(path.clone()).check_clear().is_err());
        fs::remove_dir_all(
            path.parent()
                .and_then(Path::parent)
                .expect("nested parent exists"),
        )
        .expect("test directory should be removable");
    }

    #[test]
    fn clear_allows_a_new_begin() {
        let path = test_path("clear");
        let fence = LoadFence::new(path.clone());
        fence.begin().expect("begin should persist marker");
        fence.clear().expect("clear should remove marker durably");
        fence.check_clear().expect("marker should be absent");
        fence.begin().expect("new load should be allowed");
        fs::remove_dir_all(
            path.parent()
                .and_then(Path::parent)
                .expect("nested parent exists"),
        )
        .expect("test directory should be removable");
    }

    #[test]
    fn begin_rejects_an_existing_marker() {
        let path = test_path("exists");
        let fence = LoadFence::new(path.clone());
        fence.begin().expect("first begin should succeed");
        assert!(fence.begin().is_err());
        fs::remove_dir_all(
            path.parent()
                .and_then(Path::parent)
                .expect("nested parent exists"),
        )
        .expect("test directory should be removable");
    }

    #[test]
    fn abnormal_paths_fail_closed() {
        let path = std::env::temp_dir().join(format!(
            "idoris-fence-file-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"occupied").expect("create file used as parent");
        let fence = LoadFence::new(path.join("marker"));
        assert!(fence.check_clear().is_err());
        assert!(fence.begin().is_err());
        fs::remove_file(path).expect("remove test fixture");
    }
}

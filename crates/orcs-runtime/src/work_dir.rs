//! Lifecycle-managed work directory.
//!
//! [`WorkDir`] owns a directory and guarantees its existence for the
//! duration of the value's lifetime.
//!
//! # Motivation
//!
//! Temporary directories require explicit lifecycle management:
//! **who creates, who owns, and who cleans up**.
//! Without a dedicated owner, temp directories leak on disk
//! (no RAII guard) or use predictable names (security risk).
//!
//! `WorkDir` centralises this concern so that callers never
//! interact with `std::env::temp_dir()` or `tempfile::TempDir` directly.
//!
//! # Variants
//!
//! | Variant | Created by | Drop behaviour |
//! |---------|-----------|----------------|
//! | [`Persistent`](WorkDir::Persistent) | User-specified path | Directory is **kept** |
//! | [`Temporary`](WorkDir::Temporary) | Auto-generated | Directory is **deleted** |

use std::path::{Path, PathBuf};

/// A directory whose existence is guaranteed while this value is alive.
///
/// Use [`WorkDir::persistent`] for user-specified paths (no cleanup)
/// and [`WorkDir::temporary`] for auto-generated directories (cleanup on drop).
#[derive(Debug)]
pub enum WorkDir {
    /// User-specified directory. Not deleted on drop.
    Persistent(PathBuf),
    /// Auto-generated temporary directory. Deleted on drop via [`tempfile::TempDir`].
    Temporary(tempfile::TempDir),
}

impl WorkDir {
    /// Create a `WorkDir` backed by a user-specified path.
    ///
    /// Creates the directory (and parents) if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if directory creation fails.
    pub fn persistent(path: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&path)?;
        Ok(Self::Persistent(path))
    }

    /// Create a `WorkDir` backed by a temporary directory.
    ///
    /// The directory is created with a cryptographically random name
    /// (via [`tempfile::tempdir`]) and is automatically deleted when
    /// the `WorkDir` is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if temporary directory creation fails.
    pub fn temporary() -> std::io::Result<Self> {
        let td = tempfile::tempdir()?;
        Ok(Self::Temporary(td))
    }

    /// Returns the path to the work directory.
    ///
    /// The path is valid for as long as this `WorkDir` value is alive.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Persistent(p) => p,
            Self::Temporary(td) => td.path(),
        }
    }

    /// Returns `true` if this is a temporary (auto-cleanup) directory.
    #[must_use]
    pub fn is_temporary(&self) -> bool {
        matches!(self, Self::Temporary(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_creates_and_cleans_up() {
        let path;
        {
            let wd = WorkDir::temporary().expect("should create temporary WorkDir");
            path = wd.path().to_path_buf();
            assert!(
                path.exists(),
                "directory should exist while WorkDir is alive"
            );
            assert!(wd.is_temporary());
        }
        // After drop, directory should be gone
        assert!(
            !path.exists(),
            "directory should be removed after WorkDir is dropped"
        );
    }

    #[test]
    fn persistent_creates_directory() {
        let td = tempfile::tempdir().expect("should create outer temp dir");
        let target = td.path().join("my-persistent-dir");

        let wd = WorkDir::persistent(target.clone()).expect("should create persistent WorkDir");

        assert!(target.exists(), "persistent directory should be created");
        assert!(!wd.is_temporary());
        assert_eq!(wd.path(), target);

        drop(wd);
        // Persistent directories are NOT deleted on drop
        assert!(
            target.exists(),
            "persistent directory should survive WorkDir drop"
        );
    }

    #[test]
    fn temporary_path_is_accessible() {
        let wd = WorkDir::temporary().expect("should create temporary WorkDir");
        let path = wd.path();

        // Should be able to create files inside the work dir
        let file = path.join("test.txt");
        std::fs::write(&file, "hello").expect("should write file inside WorkDir");
        assert!(file.exists());
    }

    #[test]
    fn persistent_nested_creation() {
        let td = tempfile::tempdir().expect("should create outer temp dir");
        let deep = td.path().join("a").join("b").join("c");

        let wd =
            WorkDir::persistent(deep.clone()).expect("should create nested persistent WorkDir");

        assert!(deep.exists(), "nested directories should be created");
        assert_eq!(wd.path(), deep);
    }
}

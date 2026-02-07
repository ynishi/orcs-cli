//! File system sandbox implementation.
//!
//! The [`SandboxPolicy`] trait and [`SandboxError`] are defined in `orcs-auth`.
//! This module provides the concrete filesystem implementation:
//!
//! - [`ProjectSandbox`] — default sandbox rooted at the project directory
//!
//! # Architecture
//!
//! ```text
//! SandboxPolicy trait (orcs-auth)
//!          │
//!          ├── ProjectSandbox (THIS MODULE)
//!          │     project_root: /home/user/myproject (.git detected)
//!          │     permissive_root: /home/user/myproject (= project_root)
//!          │
//!          └── scoped() → Virtual Sandbox (narrower boundary)
//!                permissive_root: /home/user/myproject/components/x
//! ```
//!
//! # Security
//!
//! All paths are canonicalized to prevent symlink/traversal escapes.
//! Writes validate the deepest existing ancestor for new-file creation.
//!
//! ## Known Limitations
//!
//! `validate_write` performs a check-then-use sequence on the filesystem.
//! Between the boundary check and the actual I/O, an attacker with local
//! access could swap a directory for a symlink (TOCTOU race). In practice
//! this is mitigated by:
//!
//! 1. Running inside a Docker/OS-level sandbox (primary security boundary)
//! 2. Returning a canonicalized path so callers never follow un-resolved symlinks
//!
//! For environments without OS-level sandboxing, consider `openat(2)`-based
//! path resolution for stronger guarantees.

// Re-export trait and error from orcs-auth for backward compatibility
pub use orcs_auth::{SandboxError, SandboxPolicy};

use std::path::{Path, PathBuf};

// ─── Concrete Implementation ────────────────────────────────────────

/// Default sandbox rooted at the project directory.
///
/// - `project_root` — where `.git`/`.orcs` was detected (immutable after creation)
/// - `permissive_root` — effective boundary (defaults to `project_root`, narrowed by `scoped()`)
///
/// # Example
///
/// ```no_run
/// use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
///
/// let sandbox = ProjectSandbox::new("/home/user/myproject").unwrap();
/// assert!(sandbox.validate_read("src/main.rs").is_ok());
/// assert!(sandbox.validate_read("/etc/passwd").is_err());
/// ```
#[derive(Debug, Clone)]
pub struct ProjectSandbox {
    project_root: PathBuf,
    permissive_root: PathBuf,
}

impl ProjectSandbox {
    /// Creates a new sandbox rooted at the given project directory.
    ///
    /// The path is canonicalized to resolve symlinks.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Init`] if the path cannot be canonicalized.
    pub fn new(project_root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let root = project_root.as_ref().canonicalize().map_err(|e| {
            SandboxError::Init(format!(
                "cannot canonicalize '{}': {e}",
                project_root.as_ref().display()
            ))
        })?;

        Ok(Self {
            project_root: root.clone(),
            permissive_root: root,
        })
    }

    /// Creates a scoped (virtual) sandbox within this one.
    ///
    /// The effective boundary is narrowed to `sub_path` (relative to
    /// current `root()`). The `project_root` remains unchanged.
    ///
    /// # Errors
    ///
    /// Returns error if `sub_path` resolves outside the current boundary.
    pub fn scoped(&self, sub_path: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let absolute = if sub_path.as_ref().is_absolute() {
            sub_path.as_ref().to_path_buf()
        } else {
            self.permissive_root.join(sub_path.as_ref())
        };

        let canonical = absolute.canonicalize().map_err(|e| {
            SandboxError::Init(format!(
                "cannot canonicalize scoped path '{}': {e}",
                absolute.display()
            ))
        })?;

        if !canonical.starts_with(&self.permissive_root) {
            return Err(SandboxError::OutsideBoundary {
                path: absolute.display().to_string(),
                root: self.permissive_root.display().to_string(),
            });
        }

        Ok(Self {
            project_root: self.project_root.clone(),
            permissive_root: canonical,
        })
    }
}

impl SandboxPolicy for ProjectSandbox {
    fn project_root(&self) -> &Path {
        &self.project_root
    }

    fn root(&self) -> &Path {
        &self.permissive_root
    }

    fn validate_read(&self, path: &str) -> Result<PathBuf, SandboxError> {
        let absolute = resolve_absolute(path, &self.permissive_root);
        let canonical = absolute
            .canonicalize()
            .map_err(|e| SandboxError::NotFound {
                path: path.to_string(),
                source: e,
            })?;

        if !canonical.starts_with(&self.permissive_root) {
            return Err(SandboxError::OutsideBoundary {
                path: path.to_string(),
                root: self.permissive_root.display().to_string(),
            });
        }

        Ok(canonical)
    }

    fn validate_write(&self, path: &str) -> Result<PathBuf, SandboxError> {
        let absolute = resolve_absolute(path, &self.permissive_root);

        let mut ancestor = absolute.as_path();
        loop {
            if ancestor.exists() {
                let canonical_ancestor = ancestor.canonicalize().map_err(|e| {
                    SandboxError::Init(format!("path resolution failed: {path} ({e})"))
                })?;
                if !canonical_ancestor.starts_with(&self.permissive_root) {
                    return Err(SandboxError::OutsideBoundary {
                        path: path.to_string(),
                        root: self.permissive_root.display().to_string(),
                    });
                }
                // Return canonicalized ancestor + remaining non-existent suffix.
                // This prevents callers from following symlinks in the
                // un-resolved portion of the path.
                let suffix = absolute.strip_prefix(ancestor).unwrap_or(Path::new(""));
                if suffix.as_os_str().is_empty() {
                    return Ok(canonical_ancestor);
                }
                return Ok(canonical_ancestor.join(suffix));
            }
            match ancestor.parent() {
                Some(p) if !p.as_os_str().is_empty() => ancestor = p,
                _ => {
                    return Err(SandboxError::OutsideBoundary {
                        path: path.to_string(),
                        root: self.permissive_root.display().to_string(),
                    });
                }
            }
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Resolves a path to absolute, relative to the given root.
fn resolve_absolute(path: &str, root: &Path) -> PathBuf {
    let requested = Path::new(path);
    if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_sandbox() -> (tempfile::TempDir, ProjectSandbox) {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = ProjectSandbox::new(tmp.path()).unwrap();
        (tmp, sandbox)
    }

    // ─── ProjectSandbox Construction ────────────────────────────────

    #[test]
    fn new_sandbox_canonicalizes_root() {
        let (tmp, sandbox) = test_sandbox();
        let expected = tmp.path().canonicalize().unwrap();
        assert_eq!(sandbox.root(), expected);
        assert_eq!(sandbox.project_root(), expected);
    }

    #[test]
    fn new_sandbox_nonexistent_path_fails() {
        let result = ProjectSandbox::new("/nonexistent/path/xyz");
        assert!(result.is_err());
    }

    // ─── validate_read ──────────────────────────────────────────────

    #[test]
    fn read_accepts_file_under_root() {
        let (tmp, sandbox) = test_sandbox();
        fs::write(tmp.path().join("ok.txt"), "data").unwrap();

        let result = sandbox.validate_read("ok.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn read_accepts_absolute_under_root() {
        let (tmp, sandbox) = test_sandbox();
        let file = tmp.path().join("abs.txt");
        fs::write(&file, "data").unwrap();

        let result = sandbox.validate_read(file.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn read_rejects_outside_root() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.validate_read("/etc/hosts");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("access denied"), "got: {err}");
    }

    #[test]
    fn read_rejects_traversal_via_dotdot() {
        let (tmp, sandbox) = test_sandbox();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(tmp.path().join("secret.txt"), "secret").unwrap();

        let scoped = sandbox.scoped("sub").unwrap();
        let result = scoped.validate_read("../secret.txt");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("access denied"), "got: {err}");
    }

    #[test]
    fn read_rejects_nonexistent() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.validate_read("nonexistent.txt");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("path not found"), "got: {err}");
    }

    // ─── validate_write ─────────────────────────────────────────────

    #[test]
    fn write_accepts_new_file_under_root() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.validate_write("new_file.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn write_accepts_nested_new_file() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.validate_write("sub/deep/new.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn write_rejects_outside_root() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.validate_write("/etc/evil.txt");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("access denied"), "got: {err}");
    }

    #[test]
    fn write_rejects_traversal_via_dotdot() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.validate_write("../escape.txt");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("access denied"), "got: {err}");
    }

    // ─── scoped (Virtual Sandbox) ───────────────────────────────────

    #[test]
    fn scoped_narrows_boundary() {
        let (tmp, sandbox) = test_sandbox();
        let sub = tmp.path().join("components");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("comp.lua"), "-- lua").unwrap();
        fs::write(tmp.path().join("top.txt"), "top").unwrap();

        let scoped = sandbox.scoped("components").unwrap();

        // Can read within scoped boundary
        assert!(scoped.validate_read("comp.lua").is_ok());

        // Cannot read parent's files
        assert!(scoped.validate_read("../top.txt").is_err());
    }

    #[test]
    fn scoped_preserves_project_root() {
        let (tmp, sandbox) = test_sandbox();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();

        let scoped = sandbox.scoped("sub").unwrap();
        assert_eq!(scoped.project_root(), sandbox.project_root());
        assert_ne!(scoped.root(), sandbox.root());
    }

    #[test]
    fn scoped_rejects_outside_parent() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.scoped("/etc");
        assert!(result.is_err());
    }

    #[test]
    fn scoped_nonexistent_subdir_fails() {
        let (_tmp, sandbox) = test_sandbox();
        let result = sandbox.scoped("nonexistent_sub");
        assert!(result.is_err());
    }

    // ─── SandboxPolicy trait object ─────────────────────────────────

    #[test]
    fn trait_object_works() {
        let (tmp, sandbox) = test_sandbox();
        fs::write(tmp.path().join("trait_test.txt"), "ok").unwrap();

        let policy: Box<dyn SandboxPolicy> = Box::new(sandbox);
        assert!(policy.validate_read("trait_test.txt").is_ok());
        assert!(policy.validate_read("/etc/hosts").is_err());
    }

    #[test]
    fn arc_trait_object_works() {
        use std::sync::Arc;

        let (tmp, sandbox) = test_sandbox();
        fs::write(tmp.path().join("arc_test.txt"), "ok").unwrap();

        let policy: Arc<dyn SandboxPolicy> = Arc::new(sandbox);
        let clone = Arc::clone(&policy);

        assert!(policy.validate_read("arc_test.txt").is_ok());
        assert!(clone.validate_write("new.txt").is_ok());
    }

    // ─── Symlink Attack Tests ───────────────────────────────────────

    #[cfg(unix)]
    mod symlink_tests {
        use super::*;
        use std::os::unix::fs::symlink;

        #[test]
        fn read_rejects_symlink_escape() {
            let (tmp, sandbox) = test_sandbox();
            symlink("/etc/hosts", tmp.path().join("evil_link")).unwrap();

            let result = sandbox.validate_read("evil_link");
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("access denied"),
                "symlink to /etc/hosts should be rejected"
            );
        }

        #[test]
        fn write_rejects_symlink_parent_escape() {
            let (tmp, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), tmp.path().join("escape_dir")).unwrap();

            let result = sandbox.validate_write("escape_dir/evil.txt");
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("access denied"),
                "symlink directory escape should be rejected"
            );
        }

        #[test]
        fn read_allows_symlink_within_sandbox() {
            let (tmp, sandbox) = test_sandbox();
            let real = tmp.path().join("real.txt");
            fs::write(&real, "ok").unwrap();
            symlink(&real, tmp.path().join("good_link")).unwrap();

            let result = sandbox.validate_read("good_link");
            assert!(result.is_ok(), "symlink within sandbox should be allowed");
        }

        #[test]
        fn scoped_read_rejects_symlink_to_parent() {
            let (tmp, sandbox) = test_sandbox();
            let sub = tmp.path().join("sub");
            fs::create_dir_all(&sub).unwrap();
            fs::write(tmp.path().join("secret.txt"), "secret").unwrap();
            symlink(tmp.path().join("secret.txt"), sub.join("link_to_parent")).unwrap();

            let scoped = sandbox.scoped("sub").unwrap();
            let result = scoped.validate_read("link_to_parent");
            assert!(
                result.is_err(),
                "symlink escaping scoped sandbox should be rejected"
            );
        }
    }
}

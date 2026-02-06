//! Resource boundary policy.
//!
//! Controls *where* operations can target. Today this abstracts
//! filesystem boundaries; the same trait model extends to Docker
//! volumes, network scopes, pod mounts, etc.
//!
//! # Architecture
//!
//! ```text
//! SandboxPolicy trait (orcs-auth)   ← trait definition
//!          │
//!          ├── ProjectSandbox (orcs-runtime)   ← filesystem impl
//!          │     project_root: /home/user/myproject
//!          │     permissive_root: /home/user/myproject
//!          │
//!          └── (future) DockerVolumePolicy, NetworkPolicy, ...
//! ```
//!
//! # Security
//!
//! Implementations must canonicalize all paths to prevent
//! symlink/traversal escapes.

use std::path::{Path, PathBuf};
use thiserror::Error;

// ─── Error ──────────────────────────────────────────────────────────

/// Errors from sandbox path validation.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// Path resolves outside the sandbox boundary.
    #[error("access denied: '{path}' is outside sandbox root '{root}'")]
    OutsideBoundary { path: String, root: String },

    /// Path does not exist (for read operations).
    #[error("path not found: {path} ({source})")]
    NotFound {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Sandbox initialization failed.
    #[error("sandbox init failed: {0}")]
    Init(String),
}

// ─── Trait ───────────────────────────────────────────────────────────

/// Resource boundary policy for file operations.
///
/// Controls which paths are accessible. All file I/O in tools
/// and shell executors goes through this trait.
///
/// # Implementors
///
/// - `ProjectSandbox` (in `orcs-runtime`) — default filesystem implementation
/// - Custom impls for testing or restricted environments
pub trait SandboxPolicy: Send + Sync + std::fmt::Debug {
    /// The project root (where `.git`/`.orcs` was detected).
    fn project_root(&self) -> &Path;

    /// The effective sandbox boundary.
    ///
    /// All file operations must resolve to paths under this root.
    /// May be narrower than `project_root()` for scoped sandboxes.
    fn root(&self) -> &Path;

    /// Validates an existing path for reading.
    ///
    /// Resolves relative paths against `root()`, canonicalizes,
    /// and verifies the result is under `root()`.
    fn validate_read(&self, path: &str) -> Result<PathBuf, SandboxError>;

    /// Validates a (potentially new) path for writing.
    ///
    /// For paths that don't exist yet, walks up to the deepest
    /// existing ancestor and validates that.
    fn validate_write(&self, path: &str) -> Result<PathBuf, SandboxError>;
}

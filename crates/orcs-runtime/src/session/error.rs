//! Storage error types.

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during session storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Session not found.
    #[error("session not found: {0}")]
    NotFound(String),

    /// I/O error during file operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Invalid path.
    #[error("invalid path: {0}")]
    InvalidPath(PathBuf),

    /// Storage directory creation failed.
    #[error("failed to create storage directory: {path}")]
    DirectoryCreation {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Checksum mismatch (data corruption).
    #[error("checksum mismatch for session {session_id}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        session_id: String,
        expected: String,
        actual: String,
    },

    /// Version incompatibility.
    #[error("version incompatible: file version {file_version}, supported {supported_version}")]
    VersionIncompatible {
        file_version: u32,
        supported_version: u32,
    },

    /// Permission denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Snapshot restore failed for an Enabled component.
    #[error("snapshot restore failed: {0}")]
    Snapshot(#[from] orcs_component::SnapshotError),
}

impl StorageError {
    /// Creates a NotFound error.
    pub fn not_found(id: impl Into<String>) -> Self {
        Self::NotFound(id.into())
    }

    /// Creates an InvalidPath error.
    pub fn invalid_path(path: impl Into<PathBuf>) -> Self {
        Self::InvalidPath(path.into())
    }

    /// Creates a DirectoryCreation error.
    pub fn directory_creation(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::DirectoryCreation {
            path: path.into(),
            source,
        }
    }

    /// Creates a ChecksumMismatch error.
    pub fn checksum_mismatch(
        session_id: impl Into<String>,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        Self::ChecksumMismatch {
            session_id: session_id.into(),
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Returns `true` if this is a recoverable error.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::NotFound(_) | Self::Io(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_error() {
        let err = StorageError::not_found("session-123");
        assert!(matches!(err, StorageError::NotFound(_)));
        assert!(err.to_string().contains("session-123"));
    }

    #[test]
    fn invalid_path_error() {
        let err = StorageError::invalid_path("/invalid/path");
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[test]
    fn is_recoverable() {
        assert!(StorageError::not_found("x").is_recoverable());
        assert!(!StorageError::VersionIncompatible {
            file_version: 1,
            supported_version: 2
        }
        .is_recoverable());
    }
}

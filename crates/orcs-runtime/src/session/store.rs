//! Session storage abstraction.
//!
//! The [`SessionStore`] trait defines the interface for session persistence.
//! This allows pluggable storage backends (local file, remote API, hybrid).

use super::{SessionAsset, StorageError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;

/// Session storage abstraction.
///
/// Implementations must be thread-safe (`Send + Sync`) for use across async tasks.
///
/// # Design Principles
///
/// - **Async**: All operations are async for I/O efficiency
/// - **Error Handling**: Returns `StorageError` for consistent error handling
/// - **Metadata Efficiency**: `list()` returns lightweight metadata, not full assets
///
/// # Example
///
/// ```no_run
/// use orcs_runtime::session::{SessionStore, SessionAsset, StorageError};
///
/// async fn save_session(store: &impl SessionStore, asset: &SessionAsset) -> Result<(), StorageError> {
///     store.save(asset).await?;
///     println!("Saved session: {}", asset.id);
///     Ok(())
/// }
/// ```
pub trait SessionStore: Send + Sync {
    /// Saves a session asset.
    ///
    /// If a session with the same ID exists, it will be overwritten.
    fn save(&self, asset: &SessionAsset) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Loads a session asset by ID.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::NotFound` if the session does not exist.
    fn load(&self, id: &str) -> impl Future<Output = Result<SessionAsset, StorageError>> + Send;

    /// Lists all session metadata.
    ///
    /// Returns lightweight metadata for each session, sorted by `updated_at` descending.
    fn list(&self) -> impl Future<Output = Result<Vec<SessionMeta>, StorageError>> + Send;

    /// Deletes a session by ID.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::NotFound` if the session does not exist.
    fn delete(&self, id: &str) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Checks if a session exists.
    fn exists(&self, id: &str) -> impl Future<Output = Result<bool, StorageError>> + Send;

    /// Returns the current sync status.
    ///
    /// For local-only stores, this returns `SyncStatus::offline()`.
    fn sync_status(&self) -> SyncStatus {
        SyncStatus::offline()
    }
}

/// Session metadata for listing.
///
/// This is a lightweight representation of a session for display purposes.
/// Use [`SessionStore::load`] to get the full [`SessionAsset`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Session ID.
    pub id: String,

    /// Optional human-readable name.
    pub name: Option<String>,

    /// Creation timestamp.
    pub created_at: DateTime<Utc>,

    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,

    /// Associated project path.
    pub project_path: Option<PathBuf>,

    /// Number of conversation turns.
    pub turn_count: usize,

    /// Sync state for this session.
    pub sync_state: SyncState,
}

impl SessionMeta {
    /// Creates metadata from a full session asset.
    pub fn from_asset(asset: &SessionAsset) -> Self {
        Self {
            id: asset.id.clone(),
            name: asset.project_context.name.clone(),
            created_at: DateTime::from_timestamp_millis(asset.created_at_ms as i64)
                .unwrap_or_else(Utc::now),
            updated_at: DateTime::from_timestamp_millis(asset.updated_at_ms as i64)
                .unwrap_or_else(Utc::now),
            project_path: asset.project_context.root_path.clone(),
            turn_count: asset.history.len(),
            sync_state: SyncState::LocalOnly,
        }
    }
}

/// Sync state for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SyncState {
    /// Local only (not synced).
    #[default]
    LocalOnly,

    /// Fully synced with remote.
    Synced,

    /// Local has unsynced changes.
    LocalAhead,

    /// Remote has unsynced changes.
    RemoteAhead,

    /// Conflict between local and remote.
    Conflict,
}

impl SyncState {
    /// Returns `true` if fully synced.
    pub fn is_synced(&self) -> bool {
        matches!(self, Self::Synced)
    }

    /// Returns `true` if there are pending changes.
    pub fn has_pending_changes(&self) -> bool {
        matches!(self, Self::LocalAhead | Self::RemoteAhead)
    }

    /// Returns `true` if there is a conflict.
    pub fn has_conflict(&self) -> bool {
        matches!(self, Self::Conflict)
    }
}

/// Overall sync status for the store.
#[derive(Debug, Clone, Default)]
pub struct SyncStatus {
    /// Current sync mode.
    pub mode: SyncMode,

    /// Last successful sync time.
    pub last_sync: Option<DateTime<Utc>>,

    /// Number of sessions pending upload.
    pub pending_uploads: usize,

    /// Number of sessions pending download.
    pub pending_downloads: usize,

    /// Sessions with conflicts.
    pub conflicts: Vec<String>,
}

impl SyncStatus {
    /// Creates an offline status (local-only mode).
    pub fn offline() -> Self {
        Self {
            mode: SyncMode::Offline,
            ..Default::default()
        }
    }

    /// Creates an online status.
    pub fn online() -> Self {
        Self {
            mode: SyncMode::Online,
            last_sync: Some(Utc::now()),
            ..Default::default()
        }
    }

    /// Returns `true` if online and synced.
    pub fn is_fully_synced(&self) -> bool {
        self.mode == SyncMode::Online
            && self.pending_uploads == 0
            && self.pending_downloads == 0
            && self.conflicts.is_empty()
    }
}

/// Sync mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Offline mode (local only).
    #[default]
    Offline,

    /// Online and connected.
    Online,

    /// Currently syncing.
    Syncing,

    /// Error state.
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_state_predicates() {
        assert!(SyncState::Synced.is_synced());
        assert!(!SyncState::LocalOnly.is_synced());

        assert!(SyncState::LocalAhead.has_pending_changes());
        assert!(!SyncState::Synced.has_pending_changes());

        assert!(SyncState::Conflict.has_conflict());
        assert!(!SyncState::LocalOnly.has_conflict());
    }

    #[test]
    fn sync_status_offline() {
        let status = SyncStatus::offline();
        assert_eq!(status.mode, SyncMode::Offline);
        assert!(status.last_sync.is_none());
    }

    #[test]
    fn sync_status_fully_synced() {
        let status = SyncStatus::online();
        assert!(status.is_fully_synced());

        let status_with_pending = SyncStatus {
            mode: SyncMode::Online,
            pending_uploads: 1,
            ..Default::default()
        };
        assert!(!status_with_pending.is_fully_synced());
    }
}

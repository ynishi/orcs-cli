//! Local file-based session storage.
//!
//! Sessions are stored as JSON files in a configurable directory:
//!
//! ```text
//! ~/.orcs/sessions/
//! ├── 550e8400-e29b-41d4-a716-446655440000.json
//! ├── 6ba7b810-9dad-11d1-80b4-00c04fd430c8.json
//! └── ...
//! ```

use super::{SessionAsset, SessionMeta, SessionStore, StorageError, SyncStatus};
use std::path::PathBuf;
use tokio::fs;

/// Local file-based session store.
///
/// This is the default storage backend, suitable for single-machine use.
///
/// # Features
///
/// - Sessions stored as pretty-printed JSON
/// - Atomic writes (write to temp, then rename)
/// - Automatic directory creation
///
/// # Example
///
/// ```no_run
/// use orcs_runtime::session::{LocalFileStore, SessionStore, SessionAsset};
/// use std::path::PathBuf;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let store = LocalFileStore::new(PathBuf::from("~/.orcs/sessions"))?;
///
/// // Save a session
/// let asset = SessionAsset::new();
/// store.save(&asset).await?;
///
/// // List all sessions
/// let sessions = store.list().await?;
/// println!("Found {} sessions", sessions.len());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct LocalFileStore {
    /// Base directory for session files.
    base_path: PathBuf,
}

impl LocalFileStore {
    /// Creates a new local file store.
    ///
    /// The directory will be created if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::DirectoryCreation` if the directory cannot be created.
    pub fn new(base_path: PathBuf) -> Result<Self, StorageError> {
        // Expand ~ to home directory
        let expanded = expand_tilde(&base_path);

        // Create directory if needed (synchronously for constructor)
        if !expanded.exists() {
            std::fs::create_dir_all(&expanded)
                .map_err(|e| StorageError::directory_creation(&expanded, e))?;
        }

        Ok(Self {
            base_path: expanded,
        })
    }

    /// Returns the base path.
    #[must_use]
    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }

    /// Returns the file path for a session ID.
    fn session_path(&self, id: &str) -> PathBuf {
        self.base_path.join(format!("{id}.json"))
    }

    /// Returns a temporary file path for atomic writes.
    fn temp_path(&self, id: &str) -> PathBuf {
        self.base_path.join(format!(".{id}.json.tmp"))
    }
}

impl SessionStore for LocalFileStore {
    async fn save(&self, asset: &SessionAsset) -> Result<(), StorageError> {
        let json = asset.to_json()?;
        let path = self.session_path(&asset.id);
        let temp_path = self.temp_path(&asset.id);

        // Write to temp file first (atomic write pattern)
        fs::write(&temp_path, &json).await?;

        // Rename to final path (atomic on most filesystems)
        fs::rename(&temp_path, &path).await?;

        Ok(())
    }

    async fn load(&self, id: &str) -> Result<SessionAsset, StorageError> {
        let path = self.session_path(id);

        if !path.exists() {
            return Err(StorageError::not_found(id));
        }

        let json = fs::read_to_string(&path).await?;
        let asset = SessionAsset::from_json(&json)?;

        Ok(asset)
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, StorageError> {
        let mut sessions = Vec::new();
        let mut entries = fs::read_dir(&self.base_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            // Skip non-JSON files and temp files
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }

            // Try to load the session
            if let Ok(json) = fs::read_to_string(&path).await {
                if let Ok(asset) = SessionAsset::from_json(&json) {
                    sessions.push(SessionMeta::from_asset(&asset));
                }
            }
        }

        // Sort by updated_at descending (most recent first)
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        Ok(sessions)
    }

    async fn delete(&self, id: &str) -> Result<(), StorageError> {
        let path = self.session_path(id);

        if !path.exists() {
            return Err(StorageError::not_found(id));
        }

        fs::remove_file(&path).await?;
        Ok(())
    }

    async fn exists(&self, id: &str) -> Result<bool, StorageError> {
        let path = self.session_path(id);
        Ok(path.exists())
    }

    fn sync_status(&self) -> SyncStatus {
        SyncStatus::offline()
    }
}

/// Expands `~` to the user's home directory.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    if let Some(path_str) = path.to_str() {
        if let Some(rest) = path_str.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
    }
    path.to_path_buf()
}

/// Returns the default session storage path.
#[must_use]
pub fn default_session_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".orcs")
        .join("sessions")
}

#[cfg(test)]
mod tests {
    use super::super::SyncMode;
    use super::*;
    use tempfile::TempDir;

    async fn test_store() -> (LocalFileStore, TempDir) {
        let temp = TempDir::new().unwrap();
        let store = LocalFileStore::new(temp.path().to_path_buf()).unwrap();
        (store, temp)
    }

    #[tokio::test]
    async fn save_and_load() {
        let (store, _temp) = test_store().await;

        let mut asset = SessionAsset::new();
        asset.add_turn(super::super::ConversationTurn::user("test"));

        // Save
        store.save(&asset).await.unwrap();

        // Load
        let loaded = store.load(&asset.id).await.unwrap();
        assert_eq!(loaded.id, asset.id);
        assert_eq!(loaded.history.len(), 1);
    }

    #[tokio::test]
    async fn load_not_found() {
        let (store, _temp) = test_store().await;

        let result = store.load("nonexistent").await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn list_sessions() {
        let (store, _temp) = test_store().await;

        // Create multiple sessions
        for i in 0..3 {
            let mut asset = SessionAsset::new();
            asset.project_context.name = Some(format!("project-{i}"));
            store.save(&asset).await.unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        let sessions = store.list().await.unwrap();
        assert_eq!(sessions.len(), 3);

        // Should be sorted by updated_at descending
        assert!(sessions[0].updated_at >= sessions[1].updated_at);
    }

    #[tokio::test]
    async fn delete_session() {
        let (store, _temp) = test_store().await;

        let asset = SessionAsset::new();
        store.save(&asset).await.unwrap();

        assert!(store.exists(&asset.id).await.unwrap());

        store.delete(&asset.id).await.unwrap();

        assert!(!store.exists(&asset.id).await.unwrap());
    }

    #[tokio::test]
    async fn delete_not_found() {
        let (store, _temp) = test_store().await;

        let result = store.delete("nonexistent").await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn exists() {
        let (store, _temp) = test_store().await;

        let asset = SessionAsset::new();
        assert!(!store.exists(&asset.id).await.unwrap());

        store.save(&asset).await.unwrap();
        assert!(store.exists(&asset.id).await.unwrap());
    }

    #[tokio::test]
    async fn sync_status_is_offline() {
        let (store, _temp) = test_store().await;
        let status = store.sync_status();
        assert_eq!(status.mode, SyncMode::Offline);
    }

    #[test]
    fn expand_tilde_with_home() {
        let path = PathBuf::from("~/test/path");
        let expanded = expand_tilde(&path);

        if dirs::home_dir().is_some() {
            assert!(!expanded.to_str().unwrap().starts_with("~/"));
        }
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let path = PathBuf::from("/absolute/path");
        let expanded = expand_tilde(&path);
        assert_eq!(expanded, path);
    }
}

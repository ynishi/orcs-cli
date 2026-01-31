//! Session persistence and storage.
//!
//! This module provides session management with the following design principles:
//!
//! - **Abstraction**: [`SessionStore`] trait for pluggable storage backends
//! - **Local First**: Works fully offline with [`LocalFileStore`]
//! - **Cloud Ready**: Sync can be added via trait implementation
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Application Layer                        │
//! │  SessionAsset, ConversationHistory, ProjectContext          │
//! └─────────────────────────────────────────────────────────────┘
//!                            │
//!                            ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Storage Abstraction                      │
//! │  SessionStore trait                                         │
//! └─────────────────────────────────────────────────────────────┘
//!                            │
//!           ┌────────────────┴────────────────┐
//!           ▼                                 ▼
//!     ┌──────────┐                     ┌──────────┐
//!     │  Local   │                     │  Remote  │
//!     │  Store   │                     │  Store   │
//!     └──────────┘                     └──────────┘
//! ```
//!
//! # Example
//!
//! ```no_run
//! use orcs_runtime::session::{SessionStore, LocalFileStore, SessionAsset};
//! use std::path::PathBuf;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create a local store
//! let store = LocalFileStore::new(PathBuf::from("~/.orcs/sessions"))?;
//!
//! // Create and save a session
//! let mut asset = SessionAsset::new();
//! store.save(&asset).await?;
//!
//! // List all sessions
//! let sessions = store.list().await?;
//!
//! // Load a specific session
//! let loaded = store.load(&asset.id).await?;
//! # Ok(())
//! # }
//! ```

mod asset;
mod error;
mod local;
mod store;

pub use asset::{
    AutoTriggerConfig, CodingStylePrefs, CommunicationPrefs, CompactedTurn, ConversationHistory,
    ConversationTurn, LearnedFact, ProjectContext, SessionAsset, SkillConfig, ToolCallRecord,
    TriggerCondition, UserPreferences,
};
pub use error::StorageError;
pub use local::{default_session_path, LocalFileStore};
pub use store::{SessionMeta, SessionStore, SyncMode, SyncState, SyncStatus};

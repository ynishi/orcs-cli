//! SharedBoard - Shared event log for cross-component visibility.
//!
//! The Board provides a rolling buffer of recent events (Output and Extension)
//! that any Component can query. Unlike the event system which is fire-and-forget,
//! the Board retains a configurable number of recent entries for retrospective queries.
//!
//! # Architecture
//!
//! ```text
//! EventEmitter (auto-write on emit_output/emit_event)
//!     |
//!     v
//! SharedBoard (Arc<RwLock<Board>>)
//!     |
//!     v read (query)
//! +-------------------------+
//! | orcs.board_recent(n)    |  <- Lua API
//! | emitter.board_recent(n) |  <- Native Component API
//! +-------------------------+
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use orcs_runtime::board::{shared_board, BoardEntry, BoardEntryKind};
//!
//! let board = shared_board();
//!
//! // Auto-written by EventEmitter; manual append for illustration:
//! {
//!     let mut b = board.write().unwrap();
//!     b.append(BoardEntry {
//!         timestamp: chrono::Utc::now(),
//!         source: ComponentId::builtin("tool"),
//!         kind: BoardEntryKind::Output { level: "info".into() },
//!         operation: "display".into(),
//!         payload: serde_json::json!({"message": "hello"}),
//!     });
//! }
//!
//! // Query recent entries
//! {
//!     let b = board.read().unwrap();
//!     let recent = b.recent(10);
//!     assert_eq!(recent.len(), 1);
//! }
//! ```

use chrono::{DateTime, Utc};
use orcs_types::ComponentId;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

/// Default maximum entries in the Board.
const DEFAULT_MAX_ENTRIES: usize = 1000;

/// A single entry in the Board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardEntry {
    /// When the entry was recorded.
    pub timestamp: DateTime<Utc>,
    /// Which component produced this entry.
    pub source: ComponentId,
    /// What kind of entry (Output or Event).
    pub kind: BoardEntryKind,
    /// Operation name (e.g., "display", "complete").
    pub operation: String,
    /// Entry payload data.
    pub payload: serde_json::Value,
}

/// Classification of a Board entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum BoardEntryKind {
    /// From `emit_output` / `emit_output_with_level`.
    Output {
        /// Log level ("info", "warn", "error").
        level: String,
    },
    /// From `emit_event` (Extension events).
    Event {
        /// Extension category (e.g., "tool:result").
        category: String,
    },
}

/// Rolling buffer of recent events for cross-component visibility.
///
/// Backed by a [`VecDeque`] with a configurable maximum size.
/// When full, the oldest entry is evicted on each append.
pub struct Board {
    entries: VecDeque<BoardEntry>,
    max_entries: usize,
}

impl Board {
    /// Creates a new Board with default capacity (1000 entries).
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(DEFAULT_MAX_ENTRIES),
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Creates a new Board with specified maximum entries.
    ///
    /// A `max_entries` of 0 is treated as 1 (at least one entry is always stored).
    #[must_use]
    pub fn with_capacity(max_entries: usize) -> Self {
        let max_entries = max_entries.max(1);
        Self {
            entries: VecDeque::with_capacity(max_entries),
            max_entries,
        }
    }

    /// Appends an entry, evicting the oldest if at capacity.
    pub fn append(&mut self, entry: BoardEntry) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Returns the most recent `n` entries (oldest first, newest last).
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<&BoardEntry> {
        let len = self.entries.len();
        let skip = len.saturating_sub(n);
        self.entries.iter().skip(skip).collect()
    }

    /// Returns the most recent `n` entries from a specific source (oldest first).
    #[must_use]
    pub fn query_by_source(&self, source: &ComponentId, n: usize) -> Vec<&BoardEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|e| e.source.fqn_eq(source))
            .take(n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    /// Returns the most recent `n` entries as JSON values.
    ///
    /// Each entry is serialized via serde. Entries that fail
    /// serialization are silently skipped.
    #[must_use]
    pub fn recent_as_json(&self, n: usize) -> Vec<serde_json::Value> {
        self.recent(n)
            .into_iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect()
    }

    /// Returns the total number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the board has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe shared Board handle.
pub type SharedBoard = Arc<RwLock<Board>>;

/// Creates a new [`SharedBoard`] with default capacity.
#[must_use]
pub fn shared_board() -> SharedBoard {
    Arc::new(RwLock::new(Board::new()))
}

/// Creates a new [`SharedBoard`] with specified capacity.
#[must_use]
pub fn shared_board_with_capacity(max_entries: usize) -> SharedBoard {
    Arc::new(RwLock::new(Board::with_capacity(max_entries)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(source_name: &str, kind: BoardEntryKind) -> BoardEntry {
        BoardEntry {
            timestamp: Utc::now(),
            source: ComponentId::builtin(source_name),
            kind,
            operation: "display".to_string(),
            payload: serde_json::json!({"message": "hello"}),
        }
    }

    fn output_kind(level: &str) -> BoardEntryKind {
        BoardEntryKind::Output {
            level: level.to_string(),
        }
    }

    fn event_kind(category: &str) -> BoardEntryKind {
        BoardEntryKind::Event {
            category: category.to_string(),
        }
    }

    #[test]
    fn new_board_is_empty() {
        let board = Board::new();
        assert!(board.is_empty());
        assert_eq!(board.len(), 0);
    }

    #[test]
    fn append_and_recent() {
        let mut board = Board::new();
        board.append(sample_entry("tool", output_kind("info")));
        board.append(sample_entry("agent", output_kind("warn")));
        board.append(sample_entry("shell", event_kind("tool:result")));

        assert_eq!(board.len(), 3);

        let recent = board.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].source.name, "agent");
        assert_eq!(recent[1].source.name, "shell");
    }

    #[test]
    fn recent_more_than_available() {
        let mut board = Board::new();
        board.append(sample_entry("tool", output_kind("info")));

        let recent = board.recent(100);
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn eviction_at_capacity() {
        let mut board = Board::with_capacity(3);
        board.append(sample_entry("a", output_kind("info")));
        board.append(sample_entry("b", output_kind("info")));
        board.append(sample_entry("c", output_kind("info")));
        assert_eq!(board.len(), 3);

        // This should evict "a"
        board.append(sample_entry("d", output_kind("info")));
        assert_eq!(board.len(), 3);

        let recent = board.recent(10);
        assert_eq!(recent[0].source.name, "b");
        assert_eq!(recent[1].source.name, "c");
        assert_eq!(recent[2].source.name, "d");
    }

    #[test]
    fn query_by_source() {
        let mut board = Board::new();
        board.append(sample_entry("tool", output_kind("info")));
        board.append(sample_entry("agent", output_kind("info")));
        board.append(sample_entry("tool", output_kind("warn")));
        board.append(sample_entry("shell", output_kind("info")));
        board.append(sample_entry("tool", event_kind("result")));

        let tool_source = ComponentId::builtin("tool");
        let results = board.query_by_source(&tool_source, 10);
        assert_eq!(results.len(), 3);
        // Oldest first
        assert_eq!(
            results[0].kind,
            BoardEntryKind::Output {
                level: "info".into()
            }
        );
        assert_eq!(
            results[2].kind,
            BoardEntryKind::Event {
                category: "result".into()
            }
        );
    }

    #[test]
    fn query_by_source_limited() {
        let mut board = Board::new();
        board.append(sample_entry("tool", output_kind("info")));
        board.append(sample_entry("tool", output_kind("warn")));
        board.append(sample_entry("tool", output_kind("error")));

        let tool_source = ComponentId::builtin("tool");
        let results = board.query_by_source(&tool_source, 2);
        assert_eq!(results.len(), 2);
        // Most recent 2, oldest first
        assert_eq!(
            results[0].kind,
            BoardEntryKind::Output {
                level: "warn".into()
            }
        );
        assert_eq!(
            results[1].kind,
            BoardEntryKind::Output {
                level: "error".into()
            }
        );
    }

    #[test]
    fn recent_as_json() {
        let mut board = Board::new();
        board.append(sample_entry("tool", output_kind("info")));
        board.append(sample_entry("agent", event_kind("tool:result")));

        let json = board.recent_as_json(10);
        assert_eq!(json.len(), 2);
        assert_eq!(json[0]["source"]["name"], "tool");
        assert_eq!(json[1]["kind"]["type"], "Event");
        assert_eq!(json[1]["kind"]["category"], "tool:result");
    }

    #[test]
    fn shared_board_thread_safe() {
        let board = shared_board();

        {
            let mut b = board.write().expect("lock");
            b.append(sample_entry("tool", output_kind("info")));
        }

        {
            let b = board.read().expect("lock");
            assert_eq!(b.len(), 1);
        }
    }

    #[test]
    fn shared_board_with_custom_capacity() {
        let board = shared_board_with_capacity(5);
        let b = board.read().expect("lock");
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn default_board() {
        let board = Board::default();
        assert!(board.is_empty());
    }
}

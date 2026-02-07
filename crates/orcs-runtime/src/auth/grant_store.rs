//! Default implementation of [`GrantPolicy`].
//!
//! Provides [`DefaultGrantStore`] — a thread-safe, in-memory grant store
//! that manages dynamic command permissions.
//!
//! # Architecture
//!
//! ```text
//! GrantPolicy trait (orcs-auth)       ← abstract definition
//!          │
//!          └── DefaultGrantStore (THIS MODULE)  ← concrete impl
//! ```
//!
//! # Grant Matching
//!
//! Uses **prefix matching**: a command is granted if it starts with
//! any stored pattern. One-time grants are consumed on first match.

use orcs_auth::{CommandGrant, GrantKind, GrantPolicy};
use std::collections::HashSet;
use std::sync::RwLock;

/// Thread-safe, in-memory command grant store.
///
/// Implements [`GrantPolicy`] using `RwLock<HashSet<String>>` for
/// concurrent read/write access.
///
/// # Grant Types
///
/// | Type | Behavior |
/// |------|----------|
/// | Persistent | Valid until revoked or session ends |
/// | OneTime | Consumed on first successful match |
///
/// # Thread Safety
///
/// All operations are thread-safe via `RwLock`. Read-heavy workloads
/// (frequent `is_granted` checks) benefit from concurrent read access.
///
/// # Example
///
/// ```
/// use orcs_auth::{CommandGrant, GrantPolicy};
/// use orcs_runtime::DefaultGrantStore;
///
/// let store = DefaultGrantStore::new();
///
/// // Grant a persistent pattern
/// store.grant(CommandGrant::persistent("rm -rf"));
/// assert!(store.is_granted("rm -rf ./temp"));
/// assert!(store.is_granted("rm -rf ./temp")); // Still valid
///
/// // Grant a one-time pattern
/// store.grant(CommandGrant::one_time("git push --force"));
/// assert!(store.is_granted("git push --force origin main")); // Consumed
/// assert!(!store.is_granted("git push --force origin main")); // Gone
/// ```
#[derive(Debug, Default)]
pub struct DefaultGrantStore {
    /// Persistent grants (survive until revoked).
    persistent: RwLock<HashSet<String>>,
    /// One-time grants (consumed on first match).
    one_time: RwLock<HashSet<String>>,
}

impl DefaultGrantStore {
    /// Creates a new empty grant store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl GrantPolicy for DefaultGrantStore {
    fn grant(&self, grant: CommandGrant) {
        match grant.kind {
            GrantKind::Persistent => match self.persistent.write() {
                Ok(mut set) => {
                    set.insert(grant.pattern);
                }
                Err(e) => {
                    tracing::error!("grant_store: persistent lock poisoned on grant: {e}");
                }
            },
            GrantKind::OneTime => match self.one_time.write() {
                Ok(mut set) => {
                    set.insert(grant.pattern);
                }
                Err(e) => {
                    tracing::error!("grant_store: one_time lock poisoned on grant: {e}");
                }
            },
        }
    }

    fn revoke(&self, pattern: &str) {
        if let Err(e) = self.persistent.write().map(|mut s| {
            s.remove(pattern);
        }) {
            tracing::error!("grant_store: persistent lock poisoned on revoke: {e}");
        }
        if let Err(e) = self.one_time.write().map(|mut s| {
            s.remove(pattern);
        }) {
            tracing::error!("grant_store: one_time lock poisoned on revoke: {e}");
        }
    }

    fn is_granted(&self, command: &str) -> bool {
        // Check persistent grants first (read lock — concurrent)
        match self.persistent.read() {
            Ok(set) => {
                if set
                    .iter()
                    .any(|p| command == p.as_str() || command.starts_with(&format!("{} ", p)))
                {
                    return true;
                }
            }
            Err(e) => {
                tracing::error!("grant_store: persistent lock poisoned on is_granted: {e}");
                return false;
            }
        }

        // Check one-time grants (write lock — consume on match)
        match self.one_time.write() {
            Ok(mut set) => {
                if let Some(pattern) = set
                    .iter()
                    .find(|p| command == p.as_str() || command.starts_with(&format!("{} ", p)))
                    .cloned()
                {
                    set.remove(&pattern);
                    return true;
                }
            }
            Err(e) => {
                tracing::error!("grant_store: one_time lock poisoned on is_granted: {e}");
            }
        }

        false
    }

    fn clear(&self) {
        if let Err(e) = self.persistent.write().map(|mut s| s.clear()) {
            tracing::error!("grant_store: persistent lock poisoned on clear: {e}");
        }
        if let Err(e) = self.one_time.write().map(|mut s| s.clear()) {
            tracing::error!("grant_store: one_time lock poisoned on clear: {e}");
        }
    }

    fn grant_count(&self) -> usize {
        let persistent = self.persistent.read().map(|s| s.len()).unwrap_or(0);
        let one_time = self.one_time.read().map(|s| s.len()).unwrap_or(0);
        persistent.saturating_add(one_time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_store_is_empty() {
        let store = DefaultGrantStore::new();
        assert_eq!(store.grant_count(), 0);
        assert!(!store.is_granted("anything"));
    }

    #[test]
    fn persistent_grant_survives_multiple_checks() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("rm -rf"));

        assert!(store.is_granted("rm -rf ./temp"));
        assert!(store.is_granted("rm -rf ./other"));
        assert!(store.is_granted("rm -rf ./temp")); // Still valid
        assert_eq!(store.grant_count(), 1);
    }

    #[test]
    fn one_time_grant_consumed_on_match() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::one_time("git push --force"));

        assert_eq!(store.grant_count(), 1);
        assert!(store.is_granted("git push --force origin main")); // Consumed
        assert_eq!(store.grant_count(), 0);
        assert!(!store.is_granted("git push --force origin main")); // Gone
    }

    #[test]
    fn prefix_matching() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("rm -rf"));

        assert!(store.is_granted("rm -rf ./temp"));
        assert!(store.is_granted("rm -rf /home/user/stuff"));
        assert!(!store.is_granted("rm ./temp")); // Not a prefix match
        assert!(!store.is_granted("ls -la"));
    }

    #[test]
    fn revoke_persistent() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("rm -rf"));
        assert!(store.is_granted("rm -rf ./temp"));

        store.revoke("rm -rf");
        assert!(!store.is_granted("rm -rf ./temp"));
        assert_eq!(store.grant_count(), 0);
    }

    #[test]
    fn revoke_one_time() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::one_time("git push --force"));

        store.revoke("git push --force");
        assert!(!store.is_granted("git push --force origin"));
        assert_eq!(store.grant_count(), 0);
    }

    #[test]
    fn clear_removes_all() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("rm -rf"));
        store.grant(CommandGrant::one_time("git push --force"));
        assert_eq!(store.grant_count(), 2);

        store.clear();
        assert_eq!(store.grant_count(), 0);
        assert!(!store.is_granted("rm -rf ./temp"));
        assert!(!store.is_granted("git push --force origin"));
    }

    #[test]
    fn mixed_grant_types() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("ls"));
        store.grant(CommandGrant::one_time("rm -rf"));
        assert_eq!(store.grant_count(), 2);

        // Both match
        assert!(store.is_granted("ls -la"));
        assert!(store.is_granted("rm -rf ./temp")); // Consumed

        // One-time gone, persistent remains
        assert_eq!(store.grant_count(), 1);
        assert!(store.is_granted("ls -la"));
        assert!(!store.is_granted("rm -rf ./temp"));
    }

    #[test]
    fn word_boundary_pattern_match() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("ls"));

        // "ls" matches exact and "ls <args>" but NOT "lsblk"
        assert!(store.is_granted("ls"));
        assert!(store.is_granted("ls -la"));
        assert!(!store.is_granted("lsblk")); // Word boundary: "ls" != "lsblk"
    }

    #[test]
    fn revoke_nonexistent_is_noop() {
        let store = DefaultGrantStore::new();
        store.revoke("nonexistent"); // Should not panic
        assert_eq!(store.grant_count(), 0);
    }

    #[test]
    fn thread_safety_basic() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(DefaultGrantStore::new());

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    let pattern = format!("cmd-{i}");
                    store.grant(CommandGrant::persistent(&pattern));
                    assert!(store.is_granted(&format!("{pattern} arg")));
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert_eq!(store.grant_count(), 4);
    }
}

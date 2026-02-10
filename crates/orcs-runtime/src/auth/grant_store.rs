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

    /// Restores grants from a previously saved list.
    ///
    /// Typically used to restore session state:
    /// ```ignore
    /// let grants = old_store.list_grants();
    /// // ... serialize grants to SessionAsset, persist, reload ...
    /// let new_store = DefaultGrantStore::new();
    /// new_store.restore_grants(&grants);
    /// ```
    ///
    /// Existing grants are preserved (additive merge).
    pub fn restore_grants(&self, grants: &[CommandGrant]) {
        for grant in grants {
            self.grant(grant.clone());
        }
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

    fn list_grants(&self) -> Vec<CommandGrant> {
        let mut grants = Vec::new();

        if let Ok(set) = self.persistent.read() {
            grants.extend(set.iter().map(|p| CommandGrant::persistent(p.as_str())));
        } else {
            tracing::error!("grant_store: persistent lock poisoned on list_grants");
        }

        if let Ok(set) = self.one_time.read() {
            grants.extend(set.iter().map(|p| CommandGrant::one_time(p.as_str())));
        } else {
            tracing::error!("grant_store: one_time lock poisoned on list_grants");
        }

        grants
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
    fn list_grants_empty() {
        let store = DefaultGrantStore::new();
        assert!(store.list_grants().is_empty());
    }

    #[test]
    fn list_grants_persistent_only() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("ls"));
        store.grant(CommandGrant::persistent("cargo"));

        let grants = store.list_grants();
        assert_eq!(grants.len(), 2);
        assert!(grants.iter().all(|g| g.kind == GrantKind::Persistent));

        let patterns: HashSet<_> = grants.iter().map(|g| g.pattern.as_str()).collect();
        assert!(patterns.contains("ls"));
        assert!(patterns.contains("cargo"));
    }

    #[test]
    fn list_grants_mixed() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("ls"));
        store.grant(CommandGrant::one_time("rm -rf"));

        let grants = store.list_grants();
        assert_eq!(grants.len(), 2);

        let persistent: Vec<_> = grants
            .iter()
            .filter(|g| g.kind == GrantKind::Persistent)
            .collect();
        let one_time: Vec<_> = grants
            .iter()
            .filter(|g| g.kind == GrantKind::OneTime)
            .collect();
        assert_eq!(persistent.len(), 1);
        assert_eq!(one_time.len(), 1);
        assert_eq!(persistent[0].pattern, "ls");
        assert_eq!(one_time[0].pattern, "rm -rf");
    }

    #[test]
    fn list_grants_roundtrip() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("ls"));
        store.grant(CommandGrant::persistent("cargo"));
        store.grant(CommandGrant::one_time("rm -rf"));

        let grants = store.list_grants();

        // Restore into a new store via restore_grants
        let store2 = DefaultGrantStore::new();
        store2.restore_grants(&grants);

        assert_eq!(store2.grant_count(), 3);
        assert!(store2.is_granted("ls -la"));
        assert!(store2.is_granted("cargo test"));
        assert!(store2.is_granted("rm -rf ./temp")); // consumes one-time
        assert!(!store2.is_granted("rm -rf ./temp")); // gone
    }

    #[test]
    fn restore_grants_additive() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("ls"));

        let additional = vec![
            CommandGrant::persistent("cargo"),
            CommandGrant::one_time("git push"),
        ];
        store.restore_grants(&additional);

        assert_eq!(store.grant_count(), 3);
        assert!(store.is_granted("ls -la"));
        assert!(store.is_granted("cargo test"));
        assert!(store.is_granted("git push origin main"));
    }

    #[test]
    fn restore_grants_serde_roundtrip() {
        let store = DefaultGrantStore::new();
        store.grant(CommandGrant::persistent("ls"));
        store.grant(CommandGrant::persistent("cargo"));
        store.grant(CommandGrant::one_time("rm -rf"));

        let grants = store.list_grants();
        let json = serde_json::to_string(&grants).expect("serialize grants");
        let restored: Vec<CommandGrant> = serde_json::from_str(&json).expect("deserialize grants");

        let store2 = DefaultGrantStore::new();
        store2.restore_grants(&restored);

        assert_eq!(store2.grant_count(), 3);
        assert!(store2.is_granted("ls -la"));
        assert!(store2.is_granted("cargo test"));
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

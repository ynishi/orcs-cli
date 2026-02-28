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

use orcs_auth::{CommandGrant, GrantError, GrantKind, GrantPolicy};
use std::collections::HashSet;
use std::sync::RwLock;

/// Thread-safe, in-memory command grant store.
///
/// Implements [`GrantPolicy`] using a single `RwLock` over both grant sets
/// for atomic snapshot consistency across all operations.
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
/// All operations are thread-safe via a single `RwLock`.
/// Because one-time grants are consumed on match, [`is_granted`](GrantPolicy::is_granted)
/// acquires a **write lock** to ensure atomicity. Read-only accessors
/// (`grant_count`, `list_grants`) use a read lock for concurrency.
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
/// store.grant(CommandGrant::persistent("rm -rf")).expect("grant persistent");
/// assert!(store.is_granted("rm -rf ./temp").expect("check grant"));
/// assert!(store.is_granted("rm -rf ./temp").expect("check grant")); // Still valid
///
/// // Grant a one-time pattern
/// store.grant(CommandGrant::one_time("git push --force")).expect("grant one-time");
/// assert!(store.is_granted("git push --force origin main").expect("check grant")); // Consumed
/// assert!(!store.is_granted("git push --force origin main").expect("check grant")); // Gone
/// ```
#[derive(Debug, Default)]
pub struct DefaultGrantStore {
    /// Both grant sets behind a single lock for atomic access.
    inner: RwLock<GrantSets>,
}

/// Internal storage for grant patterns.
#[derive(Debug, Default)]
struct GrantSets {
    /// Persistent grants (survive until revoked).
    persistent: HashSet<String>,
    /// One-time grants (consumed on first match).
    one_time: HashSet<String>,
}

impl DefaultGrantStore {
    /// Creates a new empty grant store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restores grants from a previously saved list (batch, single lock).
    ///
    /// Typically used to restore session state:
    /// ```ignore
    /// let grants = old_store.list_grants().expect("list grants");
    /// // ... serialize grants to SessionAsset, persist, reload ...
    /// let new_store = DefaultGrantStore::new();
    /// new_store.restore_grants(&grants).expect("restore grants");
    /// ```
    ///
    /// Existing grants are preserved (additive merge).
    ///
    /// # Errors
    ///
    /// Returns [`GrantError`] if internal state is inaccessible.
    pub fn restore_grants(&self, grants: &[CommandGrant]) -> Result<(), GrantError> {
        let mut inner = self.inner.write().map_err(|_| GrantError::LockPoisoned {
            context: "grant store".into(),
        })?;
        for grant in grants {
            match grant.kind {
                GrantKind::Persistent => {
                    inner.persistent.insert(grant.pattern.clone());
                }
                GrantKind::OneTime => {
                    inner.one_time.insert(grant.pattern.clone());
                }
            }
        }
        Ok(())
    }

    /// Helper: acquire write lock.
    fn write_inner(&self) -> Result<std::sync::RwLockWriteGuard<'_, GrantSets>, GrantError> {
        self.inner.write().map_err(|_| GrantError::LockPoisoned {
            context: "grant store".into(),
        })
    }

    /// Helper: acquire read lock.
    fn read_inner(&self) -> Result<std::sync::RwLockReadGuard<'_, GrantSets>, GrantError> {
        self.inner.read().map_err(|_| GrantError::LockPoisoned {
            context: "grant store".into(),
        })
    }
}

impl GrantPolicy for DefaultGrantStore {
    fn grant(&self, grant: CommandGrant) -> Result<(), GrantError> {
        let mut inner = self.write_inner()?;
        match grant.kind {
            GrantKind::Persistent => {
                inner.persistent.insert(grant.pattern);
            }
            GrantKind::OneTime => {
                inner.one_time.insert(grant.pattern);
            }
        }
        Ok(())
    }

    fn revoke(&self, pattern: &str) -> Result<(), GrantError> {
        let mut inner = self.write_inner()?;
        inner.persistent.remove(pattern);
        inner.one_time.remove(pattern);
        Ok(())
    }

    fn is_granted(&self, command: &str) -> Result<bool, GrantError> {
        let mut inner = self.write_inner()?;

        // Check persistent grants (not consumed)
        if inner
            .persistent
            .iter()
            .any(|p| command == p.as_str() || prefix_matches(command, p))
        {
            return Ok(true);
        }

        // Check one-time grants (consume on match)
        if let Some(pattern) = inner
            .one_time
            .iter()
            .find(|p| command == p.as_str() || prefix_matches(command, p))
            .cloned()
        {
            inner.one_time.remove(&pattern);
            return Ok(true);
        }

        Ok(false)
    }

    fn clear(&self) -> Result<(), GrantError> {
        let mut inner = self.write_inner()?;
        inner.persistent.clear();
        inner.one_time.clear();
        Ok(())
    }

    fn grant_count(&self) -> usize {
        self.read_inner()
            .map(|inner| inner.persistent.len().saturating_add(inner.one_time.len()))
            .unwrap_or(0)
    }

    fn list_grants(&self) -> Result<Vec<CommandGrant>, GrantError> {
        let inner = self.read_inner()?;
        let mut grants = Vec::with_capacity(inner.persistent.len() + inner.one_time.len());
        grants.extend(
            inner
                .persistent
                .iter()
                .map(|p| CommandGrant::persistent(p.as_str())),
        );
        grants.extend(
            inner
                .one_time
                .iter()
                .map(|p| CommandGrant::one_time(p.as_str())),
        );
        Ok(grants)
    }
}

/// Checks if `command` starts with `pattern` followed by a space.
///
/// Avoids `format!` allocation on every comparison.
fn prefix_matches(command: &str, pattern: &str) -> bool {
    command.starts_with(pattern) && command.as_bytes().get(pattern.len()) == Some(&b' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_store_is_empty() {
        let store = DefaultGrantStore::new();
        assert_eq!(store.grant_count(), 0);
        assert!(!store
            .is_granted("anything")
            .expect("is_granted on empty store"));
    }

    #[test]
    fn persistent_grant_survives_multiple_checks() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("rm -rf"))
            .expect("grant persistent");

        assert!(store.is_granted("rm -rf ./temp").expect("check 1"));
        assert!(store.is_granted("rm -rf ./other").expect("check 2"));
        assert!(store
            .is_granted("rm -rf ./temp")
            .expect("check 3 still valid"));
        assert_eq!(store.grant_count(), 1);
    }

    #[test]
    fn one_time_grant_consumed_on_match() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::one_time("git push --force"))
            .expect("grant one-time");

        assert_eq!(store.grant_count(), 1);
        assert!(store
            .is_granted("git push --force origin main")
            .expect("first check consumes"));
        assert_eq!(store.grant_count(), 0);
        assert!(!store
            .is_granted("git push --force origin main")
            .expect("second check after consumed"));
    }

    #[test]
    fn prefix_matching() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("rm -rf"))
            .expect("grant persistent");

        assert!(store.is_granted("rm -rf ./temp").expect("prefix match"));
        assert!(store
            .is_granted("rm -rf /home/user/stuff")
            .expect("prefix match long"));
        assert!(!store.is_granted("rm ./temp").expect("no prefix match"));
        assert!(!store.is_granted("ls -la").expect("unrelated cmd"));
    }

    #[test]
    fn revoke_persistent() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("rm -rf"))
            .expect("grant persistent");
        assert!(store.is_granted("rm -rf ./temp").expect("before revoke"));

        store.revoke("rm -rf").expect("revoke persistent");
        assert!(!store.is_granted("rm -rf ./temp").expect("after revoke"));
        assert_eq!(store.grant_count(), 0);
    }

    #[test]
    fn revoke_one_time() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::one_time("git push --force"))
            .expect("grant one-time");

        store.revoke("git push --force").expect("revoke one-time");
        assert!(!store
            .is_granted("git push --force origin")
            .expect("after revoke"));
        assert_eq!(store.grant_count(), 0);
    }

    #[test]
    fn clear_removes_all() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("rm -rf"))
            .expect("grant persistent");
        store
            .grant(CommandGrant::one_time("git push --force"))
            .expect("grant one-time");
        assert_eq!(store.grant_count(), 2);

        store.clear().expect("clear all grants");
        assert_eq!(store.grant_count(), 0);
        assert!(!store
            .is_granted("rm -rf ./temp")
            .expect("after clear persistent"));
        assert!(!store
            .is_granted("git push --force origin")
            .expect("after clear one-time"));
    }

    #[test]
    fn mixed_grant_types() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("ls"))
            .expect("grant persistent");
        store
            .grant(CommandGrant::one_time("rm -rf"))
            .expect("grant one-time");
        assert_eq!(store.grant_count(), 2);

        // Both match
        assert!(store.is_granted("ls -la").expect("persistent match"));
        assert!(store
            .is_granted("rm -rf ./temp")
            .expect("one-time consumed"));

        // One-time gone, persistent remains
        assert_eq!(store.grant_count(), 1);
        assert!(store.is_granted("ls -la").expect("persistent still valid"));
        assert!(!store.is_granted("rm -rf ./temp").expect("one-time gone"));
    }

    #[test]
    fn word_boundary_pattern_match() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("ls"))
            .expect("grant persistent");

        // "ls" matches exact and "ls <args>" but NOT "lsblk"
        assert!(store.is_granted("ls").expect("exact match"));
        assert!(store.is_granted("ls -la").expect("prefix+space match"));
        assert!(!store.is_granted("lsblk").expect("word boundary rejection"));
    }

    #[test]
    fn revoke_nonexistent_is_noop() {
        let store = DefaultGrantStore::new();
        store
            .revoke("nonexistent")
            .expect("revoke nonexistent noop");
        assert_eq!(store.grant_count(), 0);
    }

    #[test]
    fn list_grants_empty() {
        let store = DefaultGrantStore::new();
        assert!(store.list_grants().expect("list empty store").is_empty());
    }

    #[test]
    fn list_grants_persistent_only() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("ls"))
            .expect("grant ls");
        store
            .grant(CommandGrant::persistent("cargo"))
            .expect("grant cargo");

        let grants = store.list_grants().expect("list grants");
        assert_eq!(grants.len(), 2);
        assert!(grants.iter().all(|g| g.kind == GrantKind::Persistent));

        let patterns: HashSet<_> = grants.iter().map(|g| g.pattern.as_str()).collect();
        assert!(patterns.contains("ls"));
        assert!(patterns.contains("cargo"));
    }

    #[test]
    fn list_grants_mixed() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("ls"))
            .expect("grant persistent");
        store
            .grant(CommandGrant::one_time("rm -rf"))
            .expect("grant one-time");

        let grants = store.list_grants().expect("list mixed grants");
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
        store
            .grant(CommandGrant::persistent("ls"))
            .expect("grant ls");
        store
            .grant(CommandGrant::persistent("cargo"))
            .expect("grant cargo");
        store
            .grant(CommandGrant::one_time("rm -rf"))
            .expect("grant rm");

        let grants = store.list_grants().expect("list grants");

        // Restore into a new store via restore_grants
        let store2 = DefaultGrantStore::new();
        store2.restore_grants(&grants).expect("restore grants");

        assert_eq!(store2.grant_count(), 3);
        assert!(store2.is_granted("ls -la").expect("restored ls"));
        assert!(store2.is_granted("cargo test").expect("restored cargo"));
        assert!(store2
            .is_granted("rm -rf ./temp")
            .expect("restored rm consumes"));
        assert!(!store2
            .is_granted("rm -rf ./temp")
            .expect("consumed rm gone"));
    }

    #[test]
    fn restore_grants_additive() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("ls"))
            .expect("grant ls");

        let additional = vec![
            CommandGrant::persistent("cargo"),
            CommandGrant::one_time("git push"),
        ];
        store.restore_grants(&additional).expect("restore additive");

        assert_eq!(store.grant_count(), 3);
        assert!(store.is_granted("ls -la").expect("original ls"));
        assert!(store.is_granted("cargo test").expect("added cargo"));
        assert!(store
            .is_granted("git push origin main")
            .expect("added git push"));
    }

    #[test]
    fn restore_grants_serde_roundtrip() {
        let store = DefaultGrantStore::new();
        store
            .grant(CommandGrant::persistent("ls"))
            .expect("grant ls");
        store
            .grant(CommandGrant::persistent("cargo"))
            .expect("grant cargo");
        store
            .grant(CommandGrant::one_time("rm -rf"))
            .expect("grant rm");

        let grants = store.list_grants().expect("list grants");
        let json = serde_json::to_string(&grants).expect("serialize grants");
        let restored: Vec<CommandGrant> = serde_json::from_str(&json).expect("deserialize grants");

        let store2 = DefaultGrantStore::new();
        store2.restore_grants(&restored).expect("restore from json");

        assert_eq!(store2.grant_count(), 3);
        assert!(store2.is_granted("ls -la").expect("serde restored ls"));
        assert!(store2
            .is_granted("cargo test")
            .expect("serde restored cargo"));
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
                    store
                        .grant(CommandGrant::persistent(&pattern))
                        .expect("concurrent grant");
                    assert!(store
                        .is_granted(&format!("{pattern} arg"))
                        .expect("concurrent is_granted"));
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert_eq!(store.grant_count(), 4);
    }
}

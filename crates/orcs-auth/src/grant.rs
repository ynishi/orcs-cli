//! Dynamic command permission grants.
//!
//! Provides types for granting and revoking command execution permissions
//! at runtime. This is the **dynamic** counterpart to [`Capability`] (static WHAT).
//!
//! # Auth Model
//!
//! ```text
//! Effective Permission =
//!     Capability(static WHAT)
//!   ∩ SandboxPolicy(WHERE)
//!   ∩ Session(WHO + WHEN)
//!   ∩ GrantPolicy(dynamic WHAT — modified by Grant/Revoke operations)
//! ```
//!
//! # Grant vs Capability
//!
//! | Aspect | Capability | Grant |
//! |--------|-----------|-------|
//! | Defined | At component creation | At runtime (e.g., HIL approval) |
//! | Scope | Logical operations (READ, WRITE, ...) | Command patterns ("rm -rf", "git push") |
//! | Mutability | Immutable (inherited, narrowing only) | Mutable (grant/revoke) |
//! | Lifetime | Component lifetime | Session lifetime or one-time |
//!
//! # Architecture
//!
//! ```text
//! GrantPolicy trait (orcs-auth)   ← trait definition (THIS MODULE)
//!          │
//!          └── DefaultGrantStore (orcs-runtime)   ← concrete impl
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error returned by grant operations that access internal state.
#[derive(Debug, Error)]
pub enum GrantError {
    /// Internal lock was poisoned (a thread panicked while holding it).
    #[error("grant store lock poisoned: {context}")]
    LockPoisoned {
        /// Which lock was poisoned.
        context: String,
    },
}

/// A dynamic permission grant for a command pattern.
///
/// Represents the result of a permission-changing operation (e.g., HIL approval).
///
/// # Example
///
/// ```
/// use orcs_auth::{CommandGrant, GrantKind};
///
/// // Persistent grant (lasts until session ends or revoked)
/// let grant = CommandGrant::persistent("rm -rf");
/// assert_eq!(grant.pattern, "rm -rf");
/// assert_eq!(grant.kind, GrantKind::Persistent);
///
/// // One-time grant (consumed on first use)
/// let grant = CommandGrant::one_time("git push --force");
/// assert_eq!(grant.kind, GrantKind::OneTime);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandGrant {
    /// The command pattern to match (prefix match).
    pub pattern: String,
    /// How long this grant is valid.
    pub kind: GrantKind,
}

impl CommandGrant {
    /// Creates a persistent grant (valid until session ends or revoked).
    #[must_use]
    pub fn persistent(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            kind: GrantKind::Persistent,
        }
    }

    /// Creates a one-time grant (consumed on first use).
    #[must_use]
    pub fn one_time(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            kind: GrantKind::OneTime,
        }
    }
}

/// The lifetime of a [`CommandGrant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GrantKind {
    /// Valid until the session ends or the grant is explicitly revoked.
    Persistent,
    /// Consumed after a single use.
    OneTime,
}

/// Dynamic command permission management.
///
/// Abstracts the "dynamic WHAT" layer of the permission model.
/// Implementations store granted patterns and answer queries.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` for use across async boundaries.
///
/// # Example
///
/// ```
/// use orcs_auth::{GrantPolicy, CommandGrant};
///
/// fn check_with_grants(grants: &dyn GrantPolicy, cmd: &str) -> bool {
///     grants.is_granted(cmd).unwrap_or(false)
/// }
/// ```
pub trait GrantPolicy: Send + Sync + std::fmt::Debug {
    /// Grants a command pattern.
    ///
    /// After granting, commands matching this pattern will be allowed
    /// by [`is_granted`](Self::is_granted).
    ///
    /// # Errors
    ///
    /// Returns [`GrantError`] if internal state is inaccessible.
    fn grant(&self, grant: CommandGrant) -> Result<(), GrantError>;

    /// Revokes a previously granted pattern.
    ///
    /// The pattern must match exactly (not prefix match).
    ///
    /// # Errors
    ///
    /// Returns [`GrantError`] if internal state is inaccessible.
    fn revoke(&self, pattern: &str) -> Result<(), GrantError>;

    /// Checks if a command is allowed by any granted pattern.
    ///
    /// Uses **prefix matching**: returns `true` if the command starts
    /// with any granted pattern. One-time grants are consumed on match.
    ///
    /// # Errors
    ///
    /// Returns [`GrantError`] if internal state is inaccessible.
    fn is_granted(&self, command: &str) -> Result<bool, GrantError>;

    /// Clears all grants.
    ///
    /// # Errors
    ///
    /// Returns [`GrantError`] if internal state is inaccessible.
    fn clear(&self) -> Result<(), GrantError>;

    /// Returns the number of active grants.
    fn grant_count(&self) -> usize;

    /// Returns all currently active grants.
    ///
    /// This is a **trait-level operation** (not an impl-specific convenience).
    /// Any `GrantPolicy` implementation — whether backed by local memory,
    /// a remote store, or a database — must be able to enumerate its grants
    /// so that callers (e.g., session persistence) can work through
    /// `dyn GrantPolicy` without knowing the concrete type (OCP).
    ///
    /// # Errors
    ///
    /// Returns [`GrantError`] if internal state is inaccessible.
    ///
    /// # Notes
    ///
    /// - OneTime grants are included (they haven't been consumed yet)
    /// - The order of returned grants is unspecified
    fn list_grants(&self) -> Result<Vec<CommandGrant>, GrantError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_grant_persistent() {
        let grant = CommandGrant::persistent("rm -rf");
        assert_eq!(grant.pattern, "rm -rf");
        assert_eq!(grant.kind, GrantKind::Persistent);
    }

    #[test]
    fn command_grant_one_time() {
        let grant = CommandGrant::one_time("git push --force");
        assert_eq!(grant.pattern, "git push --force");
        assert_eq!(grant.kind, GrantKind::OneTime);
    }

    #[test]
    fn command_grant_equality() {
        let a = CommandGrant::persistent("rm -rf");
        let b = CommandGrant::persistent("rm -rf");
        assert_eq!(a, b);

        let c = CommandGrant::one_time("rm -rf");
        assert_ne!(a, c); // Different kind
    }

    #[test]
    fn serde_roundtrip() {
        let grant = CommandGrant::persistent("rm -rf");
        let json = serde_json::to_string(&grant).expect("serialize");
        let parsed: CommandGrant = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, grant);
    }

    #[test]
    fn grant_kind_serde() {
        let kind = GrantKind::OneTime;
        let json = serde_json::to_string(&kind).expect("serialize");
        let parsed: GrantKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, kind);
    }
}

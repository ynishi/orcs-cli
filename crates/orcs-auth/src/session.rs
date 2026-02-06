//! Session types (Principal + Privilege).

use crate::PrivilegeLevel;
use orcs_types::Principal;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// An active security context combining identity and privilege.
///
/// A Session represents the current state of an actor in the system:
///
/// - **Who**: The [`Principal`] (user, component, or system)
/// - **What level**: The [`PrivilegeLevel`] (standard or elevated)
///
/// # Immutability
///
/// Sessions are immutable value types. Methods like [`elevate`](Self::elevate)
/// and [`drop_privilege`](Self::drop_privilege) return new sessions rather
/// than modifying the existing one. This enables:
///
/// - Safe sharing across threads
/// - Clear audit trails (old session vs new session)
/// - Simple `Clone`, `Serialize`, `Deserialize`
///
/// # Dynamic Permissions
///
/// Dynamic command permissions (grant/revoke) are managed separately
/// via [`GrantPolicy`](crate::GrantPolicy), not by Session.
/// Session only carries identity and privilege level.
///
/// # Why No Default?
///
/// **DO NOT implement `Default` for Session.**
///
/// A session requires a valid [`Principal`]. There is no sensible
/// default identity. Always construct with [`Session::new`].
///
/// # Example
///
/// ```
/// use orcs_auth::{Session, PrivilegeLevel};
/// use orcs_types::{Principal, PrincipalId};
/// use std::time::Duration;
///
/// // Create a session for a user
/// let user = Principal::User(PrincipalId::new());
/// let session = Session::new(user);
///
/// // Check current state
/// assert!(!session.is_elevated());
///
/// // Elevate for privileged operations
/// let elevated = session.elevate(Duration::from_secs(300));
/// assert!(elevated.is_elevated());
///
/// // Drop back to standard when done
/// let standard = elevated.drop_privilege();
/// assert!(!standard.is_elevated());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// The actor performing operations.
    principal: Principal,
    /// Current privilege level.
    privilege: PrivilegeLevel,
}

impl Session {
    /// Creates a new session with Standard privilege level.
    ///
    /// All sessions start in Standard mode. Use [`elevate`](Self::elevate)
    /// to gain elevated privileges.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Session;
    /// use orcs_types::{Principal, PrincipalId};
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    /// assert!(!session.is_elevated());
    /// ```
    #[must_use]
    pub fn new(principal: Principal) -> Self {
        Self {
            principal,
            privilege: PrivilegeLevel::Standard,
        }
    }

    /// Returns a reference to the principal.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns a reference to the current privilege level.
    #[must_use]
    pub fn privilege(&self) -> &PrivilegeLevel {
        &self.privilege
    }

    /// Returns `true` if currently elevated (and not expired).
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Session;
    /// use orcs_types::{Principal, PrincipalId};
    /// use std::time::Duration;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    /// assert!(!session.is_elevated());
    ///
    /// let elevated = session.elevate(Duration::from_secs(60));
    /// assert!(elevated.is_elevated());
    /// ```
    #[must_use]
    pub fn is_elevated(&self) -> bool {
        self.privilege.is_elevated()
    }

    /// Returns a new session with elevated privileges.
    ///
    /// The elevation lasts for the specified duration, after which
    /// the session automatically behaves as Standard.
    ///
    /// # Arguments
    ///
    /// * `duration` - How long the elevation should last
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Session;
    /// use orcs_types::{Principal, PrincipalId};
    /// use std::time::Duration;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    ///
    /// // Elevate for 5 minutes
    /// let elevated = session.elevate(Duration::from_secs(300));
    /// assert!(elevated.is_elevated());
    ///
    /// // Original session is unchanged
    /// assert!(!session.is_elevated());
    /// ```
    #[must_use]
    pub fn elevate(&self, duration: Duration) -> Self {
        Self {
            principal: self.principal.clone(),
            privilege: PrivilegeLevel::elevated_for(duration),
        }
    }

    /// Returns a new session with Standard privilege level.
    ///
    /// Use this to explicitly drop elevated privileges before
    /// the automatic expiration.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Session;
    /// use orcs_types::{Principal, PrincipalId};
    /// use std::time::Duration;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    /// let elevated = session.elevate(Duration::from_secs(300));
    ///
    /// // Explicitly drop privileges
    /// let standard = elevated.drop_privilege();
    /// assert!(!standard.is_elevated());
    /// ```
    #[must_use]
    pub fn drop_privilege(&self) -> Self {
        Self {
            principal: self.principal.clone(),
            privilege: PrivilegeLevel::Standard,
        }
    }

    /// Returns the remaining elevation time, or `None` if not elevated.
    #[must_use]
    pub fn remaining_elevation(&self) -> Option<Duration> {
        self.privilege.remaining()
    }
}

impl std::fmt::Display for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = if self.is_elevated() {
            "elevated"
        } else {
            "standard"
        };
        write!(f, "{}@{}", self.principal, level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::PrincipalId;

    #[test]
    fn new_session_is_standard() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        assert!(!session.is_elevated());
        assert!(session.remaining_elevation().is_none());
    }

    #[test]
    fn elevate_creates_new_session() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        let elevated = session.elevate(Duration::from_secs(60));

        // Original unchanged
        assert!(!session.is_elevated());
        // New session is elevated
        assert!(elevated.is_elevated());
    }

    #[test]
    fn drop_privilege_creates_new_session() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        let elevated = session.elevate(Duration::from_secs(60));
        let dropped = elevated.drop_privilege();

        // Elevated session unchanged
        assert!(elevated.is_elevated());
        // Dropped session is standard
        assert!(!dropped.is_elevated());
    }

    #[test]
    fn principal_preserved_after_elevate() {
        let id = PrincipalId::new();
        let session = Session::new(Principal::User(id));
        let elevated = session.elevate(Duration::from_secs(60));

        assert_eq!(
            session.principal().user_id(),
            elevated.principal().user_id()
        );
    }

    #[test]
    fn display_shows_level() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        let display = format!("{session}");
        assert!(display.contains("standard"));

        let elevated = session.elevate(Duration::from_secs(60));
        let display = format!("{elevated}");
        assert!(display.contains("elevated"));
    }

    #[test]
    fn system_session() {
        let session = Session::new(Principal::System);
        assert!(session.principal().is_system());
        assert!(!session.is_elevated());
    }

    #[test]
    fn clone_preserves_all_fields() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        let elevated = session.elevate(Duration::from_secs(60));
        let cloned = elevated.clone();

        assert!(cloned.is_elevated());
        assert_eq!(elevated.principal().user_id(), cloned.principal().user_id());
    }
}

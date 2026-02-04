//! Session types (Principal + Privilege).

use super::PrivilegeLevel;
use orcs_types::Principal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::RwLock;
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
/// Sessions are immutable. Methods like [`elevate`](Self::elevate) and
/// [`drop_privilege`](Self::drop_privilege) return new sessions rather
/// than modifying the existing one. This enables:
///
/// - Safe sharing across threads
/// - Clear audit trails (old session vs new session)
/// - No hidden state mutations
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
/// use orcs_runtime::{Principal, PrivilegeLevel, Session};
/// use orcs_types::PrincipalId;
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
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    /// The actor performing operations.
    principal: Principal,
    /// Current privilege level.
    privilege: PrivilegeLevel,
    /// Dynamically granted command patterns (runtime only, not serialized).
    ///
    /// When a user approves a command via HIL, the pattern is added here
    /// so subsequent identical commands don't require re-approval.
    #[serde(skip)]
    granted_commands: RwLock<HashSet<String>>,
}

impl Clone for Session {
    /// Clones the session.
    ///
    /// Note: `granted_commands` is **not** cloned. The new session starts
    /// with an empty grant list. This is intentional:
    ///
    /// - Grants are tied to a specific session instance
    /// - Elevate/drop operations create new sessions without inheriting grants
    /// - Use `Arc<Session>` for shared grant state
    fn clone(&self) -> Self {
        Self {
            principal: self.principal.clone(),
            privilege: self.privilege.clone(),
            granted_commands: RwLock::new(HashSet::new()),
        }
    }
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
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    /// assert!(!session.is_elevated());
    /// ```
    #[must_use]
    pub fn new(principal: Principal) -> Self {
        Self {
            principal,
            privilege: PrivilegeLevel::Standard,
            granted_commands: RwLock::new(HashSet::new()),
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
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
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
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
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
            granted_commands: RwLock::new(HashSet::new()),
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
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
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
            granted_commands: RwLock::new(HashSet::new()),
        }
    }

    /// Returns the remaining elevation time, or `None` if not elevated.
    #[must_use]
    pub fn remaining_elevation(&self) -> Option<Duration> {
        self.privilege.remaining()
    }

    // =========================================================================
    // Dynamic Command Grants (HIL integration)
    // =========================================================================

    /// Grants permission for a command pattern.
    ///
    /// Once granted, commands matching this pattern will be allowed without
    /// requiring further HIL approval (until the session is dropped or
    /// [`clear_grants`](Self::clear_grants) is called).
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called from multiple threads.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    ///
    /// session.grant_command("rm -rf");
    /// assert!(session.is_command_granted("rm -rf ./temp"));
    /// ```
    pub fn grant_command(&self, pattern: &str) {
        if let Ok(mut grants) = self.granted_commands.write() {
            grants.insert(pattern.to_string());
        }
    }

    /// Checks if a command is allowed by a previously granted pattern.
    ///
    /// Returns `true` if any granted pattern is contained within the command.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called from multiple threads.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    ///
    /// assert!(!session.is_command_granted("rm -rf ./temp"));
    ///
    /// session.grant_command("rm -rf");
    /// assert!(session.is_command_granted("rm -rf ./temp"));
    /// assert!(session.is_command_granted("rm -rf /var/log"));
    /// assert!(!session.is_command_granted("ls -la"));
    /// ```
    #[must_use]
    pub fn is_command_granted(&self, command: &str) -> bool {
        if let Ok(grants) = self.granted_commands.read() {
            grants.iter().any(|pattern| command.contains(pattern))
        } else {
            false
        }
    }

    /// Revokes a previously granted command pattern.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    ///
    /// session.grant_command("rm -rf");
    /// assert!(session.is_command_granted("rm -rf ./temp"));
    ///
    /// session.revoke_command("rm -rf");
    /// assert!(!session.is_command_granted("rm -rf ./temp"));
    /// ```
    pub fn revoke_command(&self, pattern: &str) {
        if let Ok(mut grants) = self.granted_commands.write() {
            grants.remove(pattern);
        }
    }

    /// Clears all granted command patterns.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{Principal, Session};
    /// use orcs_types::PrincipalId;
    ///
    /// let session = Session::new(Principal::User(PrincipalId::new()));
    ///
    /// session.grant_command("rm -rf");
    /// session.grant_command("git reset");
    ///
    /// session.clear_grants();
    /// assert!(!session.is_command_granted("rm -rf ./temp"));
    /// assert!(!session.is_command_granted("git reset --hard"));
    /// ```
    pub fn clear_grants(&self) {
        if let Ok(mut grants) = self.granted_commands.write() {
            grants.clear();
        }
    }

    /// Returns the number of granted command patterns.
    #[must_use]
    pub fn grant_count(&self) -> usize {
        self.granted_commands.read().map(|g| g.len()).unwrap_or(0)
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

    // =========================================================================
    // Dynamic Command Grants Tests
    // =========================================================================

    #[test]
    fn grant_command_allows_matching() {
        let session = Session::new(Principal::User(PrincipalId::new()));

        assert!(!session.is_command_granted("rm -rf ./temp"));

        session.grant_command("rm -rf");
        assert!(session.is_command_granted("rm -rf ./temp"));
        assert!(session.is_command_granted("rm -rf /var/log"));
    }

    #[test]
    fn grant_does_not_match_unrelated_commands() {
        let session = Session::new(Principal::User(PrincipalId::new()));

        session.grant_command("rm -rf");
        assert!(!session.is_command_granted("ls -la"));
        assert!(!session.is_command_granted("git status"));
    }

    #[test]
    fn revoke_command_removes_grant() {
        let session = Session::new(Principal::User(PrincipalId::new()));

        session.grant_command("rm -rf");
        assert!(session.is_command_granted("rm -rf ./temp"));

        session.revoke_command("rm -rf");
        assert!(!session.is_command_granted("rm -rf ./temp"));
    }

    #[test]
    fn clear_grants_removes_all() {
        let session = Session::new(Principal::User(PrincipalId::new()));

        session.grant_command("rm -rf");
        session.grant_command("git reset");
        session.grant_command("sudo");
        assert_eq!(session.grant_count(), 3);

        session.clear_grants();
        assert_eq!(session.grant_count(), 0);
        assert!(!session.is_command_granted("rm -rf ./temp"));
    }

    #[test]
    fn clone_does_not_inherit_grants() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        session.grant_command("rm -rf");

        let cloned = session.clone();
        assert!(!cloned.is_command_granted("rm -rf ./temp"));
        assert_eq!(cloned.grant_count(), 0);
    }

    #[test]
    fn elevate_does_not_inherit_grants() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        session.grant_command("rm -rf");

        let elevated = session.elevate(Duration::from_secs(60));
        assert!(!elevated.is_command_granted("rm -rf ./temp"));
    }

    #[test]
    fn multiple_patterns_can_match() {
        let session = Session::new(Principal::User(PrincipalId::new()));

        session.grant_command("rm");
        session.grant_command("git");

        assert!(session.is_command_granted("rm -rf ./temp"));
        assert!(session.is_command_granted("git push --force"));
        assert!(!session.is_command_granted("ls -la"));
    }

    #[test]
    fn grant_count_tracks_patterns() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        assert_eq!(session.grant_count(), 0);

        session.grant_command("rm -rf");
        assert_eq!(session.grant_count(), 1);

        session.grant_command("git reset");
        assert_eq!(session.grant_count(), 2);

        // Duplicate grant doesn't increase count
        session.grant_command("rm -rf");
        assert_eq!(session.grant_count(), 2);
    }
}

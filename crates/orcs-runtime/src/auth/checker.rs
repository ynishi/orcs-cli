//! Permission checking types and policies.
//!
//! This module provides the [`PermissionChecker`] trait for policy-based
//! permission decisions, and [`DefaultPolicy`] as a standard implementation.
//!
//! # Design
//!
//! Permission checking is separated from the types themselves:
//!
//! - [`Session`](crate::Session): Holds identity and privilege level
//! - [`SignalScope`](orcs_types::SignalScope): Defines affected scope
//! - [`PermissionChecker`]: Decides if session can act on scope
//!
//! This separation allows:
//!
//! - Different policies for different environments
//! - Testing with mock policies
//! - Policy changes without modifying core types
//!
//! # Example
//!
//! ```
//! use orcs_runtime::{Principal, Session, PermissionChecker, DefaultPolicy};
//! use orcs_types::{PrincipalId, SignalScope, ChannelId};
//! use std::time::Duration;
//!
//! let policy = DefaultPolicy;
//! let session = Session::new(Principal::User(PrincipalId::new()));
//!
//! // Standard session cannot signal Global scope
//! assert!(!policy.can_signal(&session, &SignalScope::Global));
//!
//! // But can signal Channel scope
//! let channel = ChannelId::new();
//! assert!(policy.can_signal(&session, &SignalScope::Channel(channel)));
//!
//! // Elevated session can signal Global scope
//! let elevated = session.elevate(Duration::from_secs(60));
//! assert!(policy.can_signal(&elevated, &SignalScope::Global));
//! ```

use super::Session;
use orcs_types::SignalScope;

/// Policy for permission checking.
///
/// Implement this trait to define custom permission policies.
/// The default implementation is [`DefaultPolicy`].
///
/// # Why a Trait?
///
/// Using a trait allows:
///
/// - **Testing**: Mock implementations for unit tests
/// - **Flexibility**: Different policies for different contexts
/// - **Extension**: Custom policies without modifying core code
///
/// # Example Implementation
///
/// ```
/// use orcs_runtime::{PermissionChecker, Session};
/// use orcs_types::SignalScope;
///
/// struct StrictPolicy;
///
/// impl PermissionChecker for StrictPolicy {
///     fn can_signal(&self, session: &Session, scope: &SignalScope) -> bool {
///         // Only elevated sessions can signal anything
///         session.is_elevated()
///     }
///
///     fn can_destructive(&self, session: &Session, _action: &str) -> bool {
///         session.is_elevated()
///     }
/// }
/// ```
pub trait PermissionChecker {
    /// Check if session can send a signal with the given scope.
    ///
    /// # Arguments
    ///
    /// * `session` - The session attempting the action
    /// * `scope` - The scope of the signal
    ///
    /// # Returns
    ///
    /// `true` if the session is allowed to signal the given scope.
    fn can_signal(&self, session: &Session, scope: &SignalScope) -> bool;

    /// Check if session can perform a destructive operation.
    ///
    /// Destructive operations include:
    ///
    /// - `git reset --hard`
    /// - `git push --force`
    /// - `rm -rf`
    /// - File overwrite without backup
    ///
    /// # Arguments
    ///
    /// * `session` - The session attempting the action
    /// * `action` - Description of the action (for logging/audit)
    ///
    /// # Returns
    ///
    /// `true` if the session is allowed to perform the action.
    fn can_destructive(&self, session: &Session, action: &str) -> bool;
}

/// Default permission policy.
///
/// # Rules
///
/// | Scope/Action | Standard | Elevated |
/// |--------------|----------|----------|
/// | Global signal | Denied | Allowed |
/// | Channel signal | Allowed | Allowed |
/// | Destructive ops | Denied | Allowed |
///
/// # Future Extensions
///
/// This policy may be extended to support:
///
/// - Channel ownership checks
/// - Component-specific permissions
/// - Time-based restrictions
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultPolicy;

impl PermissionChecker for DefaultPolicy {
    fn can_signal(&self, session: &Session, scope: &SignalScope) -> bool {
        match scope {
            SignalScope::Global => session.is_elevated(),
            SignalScope::Channel(_) => {
                // M1: Allow all channel signals
                // Future: Check channel ownership
                true
            }
        }
    }

    fn can_destructive(&self, session: &Session, _action: &str) -> bool {
        session.is_elevated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{ChannelId, Principal, PrincipalId};
    use std::time::Duration;

    fn standard_session() -> Session {
        Session::new(Principal::User(PrincipalId::new()))
    }

    fn elevated_session() -> Session {
        standard_session().elevate(Duration::from_secs(60))
    }

    #[test]
    fn standard_cannot_signal_global() {
        let policy = DefaultPolicy;
        let session = standard_session();

        assert!(!policy.can_signal(&session, &SignalScope::Global));
    }

    #[test]
    fn standard_can_signal_channel() {
        let policy = DefaultPolicy;
        let session = standard_session();
        let channel = ChannelId::new();

        assert!(policy.can_signal(&session, &SignalScope::Channel(channel)));
    }

    #[test]
    fn elevated_can_signal_global() {
        let policy = DefaultPolicy;
        let session = elevated_session();

        assert!(policy.can_signal(&session, &SignalScope::Global));
    }

    #[test]
    fn elevated_can_signal_channel() {
        let policy = DefaultPolicy;
        let session = elevated_session();
        let channel = ChannelId::new();

        assert!(policy.can_signal(&session, &SignalScope::Channel(channel)));
    }

    #[test]
    fn standard_cannot_destructive() {
        let policy = DefaultPolicy;
        let session = standard_session();

        assert!(!policy.can_destructive(&session, "git reset --hard"));
        assert!(!policy.can_destructive(&session, "rm -rf"));
    }

    #[test]
    fn elevated_can_destructive() {
        let policy = DefaultPolicy;
        let session = elevated_session();

        assert!(policy.can_destructive(&session, "git reset --hard"));
        assert!(policy.can_destructive(&session, "rm -rf"));
    }

    #[test]
    fn system_session_standard() {
        let policy = DefaultPolicy;
        let session = Session::new(Principal::System);

        // System is also Standard by default
        assert!(!policy.can_signal(&session, &SignalScope::Global));
        assert!(policy.can_signal(&session, &SignalScope::Channel(ChannelId::new())));
    }

    #[test]
    fn dropped_privilege_cannot_global() {
        let policy = DefaultPolicy;
        let session = elevated_session().drop_privilege();

        assert!(!policy.can_signal(&session, &SignalScope::Global));
    }
}

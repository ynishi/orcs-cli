//! Permission policy trait.
//!
//! Defines [`PermissionPolicy`] — the abstract policy for permission checking.
//! This trait lives in `orcs-auth` so that both `orcs-component` and `orcs-runtime`
//! can reference it without circular dependencies.
//!
//! # Architecture
//!
//! ```text
//! PermissionPolicy trait (orcs-auth)   <- abstract, no runtime deps
//!          │
//!          ├── PermissionChecker (orcs-runtime) <- extends with check_command(CommandCheckResult)
//!          │         │
//!          │         └── DefaultPolicy (orcs-runtime) <- concrete impl
//!          │
//!          └── (future) WasmPolicy, DockerPolicy, ...
//! ```
//!
//! # Three-Layer Model
//!
//! ```text
//! Effective Permission = Capability(WHAT) ∩ SandboxPolicy(WHERE) ∩ PermissionPolicy(WHO+WHEN)
//! ```
//!
//! | Layer | Type | Controls |
//! |-------|------|----------|
//! | [`crate::Capability`] | Bitflags | What operations are allowed |
//! | [`crate::SandboxPolicy`] | Trait | Where operations can target |
//! | [`PermissionPolicy`] | Trait (THIS) | Who can act, with what privilege |

use crate::{CommandPermission, Session};
use orcs_types::SignalScope;

/// Abstract permission policy for session-based access control.
///
/// Implement this trait to define custom permission policies.
/// The trait is runtime-independent — it doesn't depend on
/// `ApprovalRequest` or other runtime-specific types.
///
/// # Implementors
///
/// - `DefaultPolicy` (in `orcs-runtime`) — standard policy with blocked/elevated patterns
/// - Custom impls for testing or restricted environments
///
/// # Example
///
/// ```
/// use orcs_auth::{PermissionPolicy, Session, CommandPermission};
/// use orcs_types::{SignalScope, Principal, PrincipalId};
///
/// struct PermissivePolicy;
///
/// impl PermissionPolicy for PermissivePolicy {
///     fn can_signal(&self, _session: &Session, _scope: &SignalScope) -> bool {
///         true
///     }
///
///     fn can_destructive(&self, _session: &Session, _action: &str) -> bool {
///         true
///     }
///
///     fn can_execute_command(&self, _session: &Session, _cmd: &str) -> bool {
///         true
///     }
///
///     fn can_spawn_child(&self, _session: &Session) -> bool {
///         true
///     }
///
///     fn can_spawn_runner(&self, _session: &Session) -> bool {
///         true
///     }
/// }
///
/// let policy = PermissivePolicy;
/// let session = Session::new(Principal::User(PrincipalId::new()));
/// assert!(policy.can_execute_command(&session, "ls -la"));
/// assert!(policy.check_command_permission(&session, "ls -la").is_allowed());
/// ```
pub trait PermissionPolicy: Send + Sync {
    /// Check if session can send a signal with the given scope.
    fn can_signal(&self, session: &Session, scope: &SignalScope) -> bool;

    /// Check if session can perform a destructive operation.
    ///
    /// Destructive operations include `git reset --hard`, `git push --force`,
    /// `rm -rf`, file overwrite without backup, etc.
    fn can_destructive(&self, session: &Session, action: &str) -> bool;

    /// Check if session can execute a shell command.
    fn can_execute_command(&self, session: &Session, cmd: &str) -> bool;

    /// Check if session can spawn a child entity.
    fn can_spawn_child(&self, session: &Session) -> bool;

    /// Check if session can spawn a runner (parallel execution).
    fn can_spawn_runner(&self, session: &Session) -> bool;

    /// Check command with granular permission result.
    ///
    /// Returns [`CommandPermission`] with three possible states:
    /// - `Allowed`: Execute immediately
    /// - `Denied`: Block with reason
    /// - `RequiresApproval`: Needs user approval
    ///
    /// # Default Implementation
    ///
    /// Wraps `can_execute_command`:
    /// - Returns `Allowed` if `can_execute_command` returns true
    /// - Returns `Denied` otherwise
    ///
    /// Override this for more granular control (e.g., HIL integration).
    fn check_command_permission(&self, session: &Session, cmd: &str) -> CommandPermission {
        if self.can_execute_command(session, cmd) {
            CommandPermission::Allowed
        } else {
            CommandPermission::Denied("permission denied".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{ChannelId, Principal, PrincipalId};

    struct PermissivePolicy;

    impl PermissionPolicy for PermissivePolicy {
        fn can_signal(&self, _session: &Session, _scope: &SignalScope) -> bool {
            true
        }
        fn can_destructive(&self, _session: &Session, _action: &str) -> bool {
            true
        }
        fn can_execute_command(&self, _session: &Session, _cmd: &str) -> bool {
            true
        }
        fn can_spawn_child(&self, _session: &Session) -> bool {
            true
        }
        fn can_spawn_runner(&self, _session: &Session) -> bool {
            true
        }
    }

    struct StrictPolicy;

    impl PermissionPolicy for StrictPolicy {
        fn can_signal(&self, session: &Session, scope: &SignalScope) -> bool {
            match scope {
                SignalScope::Global => session.is_elevated(),
                _ => true,
            }
        }
        fn can_destructive(&self, session: &Session, _action: &str) -> bool {
            session.is_elevated()
        }
        fn can_execute_command(&self, session: &Session, _cmd: &str) -> bool {
            session.is_elevated()
        }
        fn can_spawn_child(&self, session: &Session) -> bool {
            session.is_elevated()
        }
        fn can_spawn_runner(&self, session: &Session) -> bool {
            session.is_elevated()
        }
    }

    fn standard_session() -> Session {
        Session::new(Principal::User(PrincipalId::new()))
    }

    fn elevated_session() -> Session {
        standard_session().elevate(std::time::Duration::from_secs(60))
    }

    #[test]
    fn permissive_allows_everything() {
        let policy = PermissivePolicy;
        let session = standard_session();

        assert!(policy.can_signal(&session, &SignalScope::Global));
        assert!(policy.can_destructive(&session, "rm -rf"));
        assert!(policy.can_execute_command(&session, "ls"));
        assert!(policy.can_spawn_child(&session));
        assert!(policy.can_spawn_runner(&session));
    }

    #[test]
    fn strict_denies_standard_session() {
        let policy = StrictPolicy;
        let session = standard_session();

        assert!(!policy.can_signal(&session, &SignalScope::Global));
        assert!(!policy.can_destructive(&session, "rm -rf"));
        assert!(!policy.can_execute_command(&session, "ls"));
        assert!(!policy.can_spawn_child(&session));
        assert!(!policy.can_spawn_runner(&session));
    }

    #[test]
    fn strict_allows_elevated_session() {
        let policy = StrictPolicy;
        let session = elevated_session();

        assert!(policy.can_signal(&session, &SignalScope::Global));
        assert!(policy.can_destructive(&session, "rm -rf"));
        assert!(policy.can_execute_command(&session, "ls"));
        assert!(policy.can_spawn_child(&session));
        assert!(policy.can_spawn_runner(&session));
    }

    #[test]
    fn strict_allows_channel_signal_for_standard() {
        let policy = StrictPolicy;
        let session = standard_session();
        let channel = ChannelId::new();

        assert!(policy.can_signal(&session, &SignalScope::Channel(channel)));
    }

    #[test]
    fn default_check_command_permission_wraps_can_execute() {
        let policy = PermissivePolicy;
        let session = standard_session();

        let result = policy.check_command_permission(&session, "ls -la");
        assert!(result.is_allowed());
    }

    #[test]
    fn default_check_command_permission_denied_when_not_allowed() {
        let policy = StrictPolicy;
        let session = standard_session();

        let result = policy.check_command_permission(&session, "ls -la");
        assert!(result.is_denied());
    }

    #[test]
    fn trait_object_works() {
        let policy: Box<dyn PermissionPolicy> = Box::new(PermissivePolicy);
        let session = standard_session();

        assert!(policy.can_execute_command(&session, "ls"));
    }
}

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

use super::blocked_patterns::{is_blocked_command, matching_elevation_pattern, requires_elevation};
use super::command_check::CommandCheckResult;
use super::Session;
use crate::components::ApprovalRequest;
use orcs_types::SignalScope;
use std::sync::Arc;

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
///
///     fn can_execute_command(&self, session: &Session, _cmd: &str) -> bool {
///         session.is_elevated()
///     }
///
///     fn can_spawn_child(&self, session: &Session) -> bool {
///         session.is_elevated()
///     }
///
///     fn can_spawn_runner(&self, session: &Session) -> bool {
///         session.is_elevated()
///     }
/// }
/// ```
pub trait PermissionChecker: Send + Sync {
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

    /// Check if session can execute a shell command.
    ///
    /// Shell commands are potentially dangerous as they can:
    /// - Read/write arbitrary files
    /// - Spawn network connections
    /// - Modify system state
    ///
    /// # Arguments
    ///
    /// * `session` - The session attempting the action
    /// * `cmd` - The shell command to execute
    ///
    /// # Returns
    ///
    /// `true` if the session is allowed to execute the command.
    fn can_execute_command(&self, session: &Session, cmd: &str) -> bool;

    /// Check if session can spawn a child entity.
    ///
    /// Child spawning allows code execution within the parent's context.
    ///
    /// # Arguments
    ///
    /// * `session` - The session attempting the action
    ///
    /// # Returns
    ///
    /// `true` if the session is allowed to spawn children.
    fn can_spawn_child(&self, session: &Session) -> bool;

    /// Check if session can spawn a runner (parallel execution).
    ///
    /// Runner spawning creates a new ChannelRunner for parallel execution.
    ///
    /// # Arguments
    ///
    /// * `session` - The session attempting the action
    ///
    /// # Returns
    ///
    /// `true` if the session is allowed to spawn runners.
    fn can_spawn_runner(&self, session: &Session) -> bool;

    /// Check command with granular result including HIL approval flow.
    ///
    /// This is the preferred method for command checking as it supports:
    ///
    /// - Dynamic grants (previously approved commands)
    /// - HIL approval flow (RequiresApproval)
    /// - Detailed denial reasons
    ///
    /// # Default Implementation
    ///
    /// The default implementation wraps `can_execute_command`:
    /// - Returns `Allowed` if `can_execute_command` returns true
    /// - Returns `Denied` otherwise
    ///
    /// Override this method to implement HIL integration.
    ///
    /// # Arguments
    ///
    /// * `session` - The session (Arc for grant_command access)
    /// * `cmd` - The shell command to check
    ///
    /// # Returns
    ///
    /// [`CommandCheckResult`] with granular permission information.
    fn check_command(&self, session: &Arc<Session>, cmd: &str) -> CommandCheckResult {
        if self.can_execute_command(session, cmd) {
            CommandCheckResult::Allowed
        } else {
            CommandCheckResult::Denied("permission denied".to_string())
        }
    }
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
/// | Blocked commands | Denied | Denied |
/// | Elevated commands | Denied | Allowed |
///
/// # Command Filtering
///
/// Commands are checked against two lists:
///
/// 1. **Blocked patterns**: Always denied (even for elevated)
/// 2. **Elevated patterns**: Require elevated session
///
/// # Audit Logging
///
/// All permission checks are logged for audit:
/// - Allowed operations: debug level
/// - Denied operations: warn level
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultPolicy;

impl PermissionChecker for DefaultPolicy {
    fn can_signal(&self, session: &Session, scope: &SignalScope) -> bool {
        let allowed = match scope {
            SignalScope::Global => session.is_elevated(),
            SignalScope::Channel(_) | SignalScope::WithChildren(_) => true,
        };

        // Audit logging
        if allowed {
            tracing::debug!(
                principal = ?session.principal(),
                elevated = session.is_elevated(),
                scope = ?scope,
                "signal allowed"
            );
        } else {
            tracing::warn!(
                principal = ?session.principal(),
                elevated = session.is_elevated(),
                scope = ?scope,
                "signal denied: requires elevation"
            );
        }

        allowed
    }

    fn can_destructive(&self, session: &Session, action: &str) -> bool {
        let allowed = session.is_elevated();

        // Audit logging
        if allowed {
            tracing::info!(
                principal = ?session.principal(),
                action = action,
                "destructive operation allowed"
            );
        } else {
            tracing::warn!(
                principal = ?session.principal(),
                action = action,
                "destructive operation denied: requires elevation"
            );
        }

        allowed
    }

    fn can_execute_command(&self, session: &Session, cmd: &str) -> bool {
        // Check blocked patterns first (even for elevated)
        if is_blocked_command(cmd) {
            tracing::error!(
                principal = ?session.principal(),
                cmd = cmd,
                "command BLOCKED: matches dangerous pattern"
            );
            return false;
        }

        // Check if command requires elevation
        let elevation_required = requires_elevation(cmd);
        let allowed = if elevation_required {
            session.is_elevated()
        } else {
            // Non-dangerous commands still require elevation in default policy
            session.is_elevated()
        };

        // Audit logging
        if allowed {
            if elevation_required {
                tracing::info!(
                    principal = ?session.principal(),
                    cmd = cmd,
                    "elevated command allowed"
                );
            } else {
                tracing::debug!(
                    principal = ?session.principal(),
                    cmd = cmd,
                    "command allowed"
                );
            }
        } else {
            tracing::warn!(
                principal = ?session.principal(),
                cmd = cmd,
                requires_elevation = elevation_required,
                "command denied: requires elevation"
            );
        }

        allowed
    }

    fn can_spawn_child(&self, session: &Session) -> bool {
        let allowed = session.is_elevated();

        // Audit logging
        if allowed {
            tracing::debug!(
                principal = ?session.principal(),
                "spawn_child allowed"
            );
        } else {
            tracing::warn!(
                principal = ?session.principal(),
                "spawn_child denied: requires elevation"
            );
        }

        allowed
    }

    fn can_spawn_runner(&self, session: &Session) -> bool {
        let allowed = session.is_elevated();

        // Audit logging
        if allowed {
            tracing::debug!(
                principal = ?session.principal(),
                "spawn_runner allowed"
            );
        } else {
            tracing::warn!(
                principal = ?session.principal(),
                "spawn_runner denied: requires elevation"
            );
        }

        allowed
    }

    /// Check command with dynamic grants and HIL approval support.
    ///
    /// Flow:
    /// 1. Check blocked patterns → Denied
    /// 2. Check dynamic grants → Allowed (if previously approved)
    /// 3. Check elevated session → Allowed
    /// 4. Check elevation required → RequiresApproval
    /// 5. Otherwise → Allowed (safe commands for standard session)
    fn check_command(&self, session: &Arc<Session>, cmd: &str) -> CommandCheckResult {
        // Step 1: Always block dangerous patterns
        if is_blocked_command(cmd) {
            tracing::error!(
                principal = ?session.principal(),
                cmd = cmd,
                "command BLOCKED: matches dangerous pattern"
            );
            return CommandCheckResult::Denied(
                "command blocked: matches dangerous pattern".to_string(),
            );
        }

        // Step 2: Check dynamic grants (previously approved via HIL)
        if session.is_command_granted(cmd) {
            tracing::debug!(
                principal = ?session.principal(),
                cmd = cmd,
                "command allowed: previously granted"
            );
            return CommandCheckResult::Allowed;
        }

        // Step 3: Elevated sessions can execute anything (except blocked)
        if session.is_elevated() {
            tracing::debug!(
                principal = ?session.principal(),
                cmd = cmd,
                "command allowed: elevated session"
            );
            return CommandCheckResult::Allowed;
        }

        // Step 4: Check if command requires elevation
        if let Some(pattern) = matching_elevation_pattern(cmd) {
            tracing::info!(
                principal = ?session.principal(),
                cmd = cmd,
                pattern = pattern,
                "command requires approval"
            );

            let request = ApprovalRequest::new(
                "bash",
                format!("Execute command: {}", cmd),
                serde_json::json!({
                    "command": cmd,
                    "pattern": pattern,
                }),
            );

            return CommandCheckResult::RequiresApproval {
                request,
                grant_pattern: pattern.to_string(),
            };
        }

        // Step 5: Safe commands are allowed for standard sessions
        tracing::debug!(
            principal = ?session.principal(),
            cmd = cmd,
            "command allowed: safe command"
        );
        CommandCheckResult::Allowed
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

    #[test]
    fn standard_cannot_execute_command() {
        let policy = DefaultPolicy;
        let session = standard_session();

        assert!(!policy.can_execute_command(&session, "ls -la"));
        assert!(!policy.can_execute_command(&session, "rm -rf /"));
    }

    #[test]
    fn elevated_can_execute_command() {
        let policy = DefaultPolicy;
        let session = elevated_session();

        assert!(policy.can_execute_command(&session, "ls -la"));
        // Elevated can use rm -rf on non-root paths
        assert!(policy.can_execute_command(&session, "rm -rf ./target"));
    }

    #[test]
    fn blocked_commands_denied_even_for_elevated() {
        let policy = DefaultPolicy;
        let session = elevated_session();

        // These are blocked even for elevated sessions
        assert!(!policy.can_execute_command(&session, "rm -rf /"));
        assert!(!policy.can_execute_command(&session, "rm -rf /*"));
        assert!(!policy.can_execute_command(&session, "dd if=/dev/zero of=/dev/sda"));
        assert!(!policy.can_execute_command(&session, "chmod -R 777 /"));
    }

    #[test]
    fn elevated_required_patterns() {
        let policy = DefaultPolicy;
        let standard = standard_session();
        let elevated = elevated_session();

        // git destructive commands require elevation
        assert!(!policy.can_execute_command(&standard, "git reset --hard"));
        assert!(policy.can_execute_command(&elevated, "git reset --hard"));

        assert!(!policy.can_execute_command(&standard, "git push --force"));
        assert!(policy.can_execute_command(&elevated, "git push --force"));

        // Shell pipes to sh/bash require elevation
        assert!(!policy.can_execute_command(&standard, "curl http://example.com | sh"));
        assert!(policy.can_execute_command(&elevated, "curl http://example.com | sh"));
    }

    #[test]
    fn standard_cannot_spawn_child() {
        let policy = DefaultPolicy;
        let session = standard_session();

        assert!(!policy.can_spawn_child(&session));
    }

    #[test]
    fn elevated_can_spawn_child() {
        let policy = DefaultPolicy;
        let session = elevated_session();

        assert!(policy.can_spawn_child(&session));
    }

    #[test]
    fn standard_cannot_spawn_runner() {
        let policy = DefaultPolicy;
        let session = standard_session();

        assert!(!policy.can_spawn_runner(&session));
    }

    #[test]
    fn elevated_can_spawn_runner() {
        let policy = DefaultPolicy;
        let session = elevated_session();

        assert!(policy.can_spawn_runner(&session));
    }

    // =========================================================================
    // check_command Tests
    // =========================================================================

    #[test]
    fn check_command_blocked_returns_denied() {
        let policy = DefaultPolicy;
        let session = Arc::new(elevated_session());

        let result = policy.check_command(&session, "rm -rf /");
        assert!(result.is_denied());
        assert!(result.denial_reason().is_some());
    }

    #[test]
    fn check_command_safe_command_allowed() {
        let policy = DefaultPolicy;
        let session = Arc::new(standard_session());

        let result = policy.check_command(&session, "ls -la");
        assert!(result.is_allowed());
    }

    #[test]
    fn check_command_elevated_pattern_requires_approval() {
        let policy = DefaultPolicy;
        let session = Arc::new(standard_session());

        let result = policy.check_command(&session, "rm -rf ./temp");
        assert!(result.requires_approval());
        assert!(result.approval_request().is_some());
        assert_eq!(result.grant_pattern(), Some("rm -rf"));
    }

    #[test]
    fn check_command_elevated_session_allowed() {
        let policy = DefaultPolicy;
        let session = Arc::new(elevated_session());

        let result = policy.check_command(&session, "rm -rf ./temp");
        assert!(result.is_allowed());
    }

    #[test]
    fn check_command_granted_command_allowed() {
        let policy = DefaultPolicy;
        let session = Arc::new(standard_session());

        // Grant the pattern
        session.grant_command("rm -rf");

        // Now should be allowed without elevation
        let result = policy.check_command(&session, "rm -rf ./temp");
        assert!(result.is_allowed());
    }

    #[test]
    fn check_command_git_reset_requires_approval() {
        let policy = DefaultPolicy;
        let session = Arc::new(standard_session());

        let result = policy.check_command(&session, "git reset --hard HEAD~1");
        assert!(result.requires_approval());
        assert_eq!(result.grant_pattern(), Some("git reset --hard"));
    }

    #[test]
    fn check_command_git_push_force_requires_approval() {
        let policy = DefaultPolicy;
        let session = Arc::new(standard_session());

        let result = policy.check_command(&session, "git push --force origin main");
        assert!(result.requires_approval());
        assert_eq!(result.grant_pattern(), Some("git push --force"));
    }
}

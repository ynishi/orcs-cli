//! Command permission types.
//!
//! Provides [`CommandPermission`] for trait-level permission results
//! that don't depend on runtime-specific types like `ApprovalRequest`.
//!
//! # Architecture
//!
//! ```text
//! CommandPermission (orcs-auth)   <- trait-level, runtime-independent
//!         │
//!         └── CommandCheckResult (orcs-runtime)  <- runtime, includes ApprovalRequest
//! ```
//!
//! `CommandPermission` is used by:
//! - `ChildContext::check_command_permission()` (in `orcs-component`) — trait boundary
//! - [`crate::PermissionPolicy::check_command_permission()`] — abstract policy
//!
//! `CommandCheckResult` extends this with HIL-specific fields and stays in `orcs-runtime`.

/// Result of a command permission check (trait-level type).
///
/// This is a simplified, runtime-independent version suitable for trait boundaries.
/// Runtime implementations that need HIL integration use `CommandCheckResult`
/// (in `orcs-runtime`) which includes `ApprovalRequest`.
///
/// # Variants
///
/// - `Allowed`: Command can execute immediately
/// - `Denied`: Command is permanently blocked (e.g., denylist)
/// - `RequiresApproval`: Command needs user approval before execution
///
/// # Example
///
/// ```
/// use orcs_auth::CommandPermission;
///
/// let perm = CommandPermission::Allowed;
/// assert!(perm.is_allowed());
///
/// let perm = CommandPermission::Denied("blocked pattern".to_string());
/// assert!(perm.is_denied());
/// assert_eq!(perm.status_str(), "denied");
///
/// let perm = CommandPermission::RequiresApproval {
///     grant_pattern: "rm -rf".to_string(),
///     description: "destructive operation".to_string(),
/// };
/// assert!(perm.requires_approval());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPermission {
    /// Command is allowed to execute.
    Allowed,
    /// Command is denied with a reason.
    Denied(String),
    /// Command requires user approval via HIL.
    RequiresApproval {
        /// The pattern to grant if approved.
        grant_pattern: String,
        /// Human-readable description of why approval is needed.
        description: String,
    },
}

impl CommandPermission {
    /// Returns `true` if the command is allowed.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Returns `true` if the command is denied.
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied(_))
    }

    /// Returns `true` if the command requires approval.
    #[must_use]
    pub fn requires_approval(&self) -> bool {
        matches!(self, Self::RequiresApproval { .. })
    }

    /// Returns the status as a string ("allowed", "denied", "requires_approval").
    #[must_use]
    pub fn status_str(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied(_) => "denied",
            Self::RequiresApproval { .. } => "requires_approval",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_helpers() {
        let p = CommandPermission::Allowed;
        assert!(p.is_allowed());
        assert!(!p.is_denied());
        assert!(!p.requires_approval());
        assert_eq!(p.status_str(), "allowed");
    }

    #[test]
    fn denied_helpers() {
        let p = CommandPermission::Denied("blocked".to_string());
        assert!(!p.is_allowed());
        assert!(p.is_denied());
        assert!(!p.requires_approval());
        assert_eq!(p.status_str(), "denied");
    }

    #[test]
    fn requires_approval_helpers() {
        let p = CommandPermission::RequiresApproval {
            grant_pattern: "rm -rf".to_string(),
            description: "destructive operation".to_string(),
        };
        assert!(!p.is_allowed());
        assert!(!p.is_denied());
        assert!(p.requires_approval());
        assert_eq!(p.status_str(), "requires_approval");
    }

    #[test]
    fn equality() {
        assert_eq!(CommandPermission::Allowed, CommandPermission::Allowed);
        assert_eq!(
            CommandPermission::Denied("x".into()),
            CommandPermission::Denied("x".into())
        );
        assert_ne!(
            CommandPermission::Allowed,
            CommandPermission::Denied("x".into())
        );
    }
}

//! Command check result types.
//!
//! Provides [`CommandCheckResult`] for granular permission decisions
//! that support HIL (Human-in-the-Loop) approval workflows.

use crate::components::ApprovalRequest;

/// Result of a command permission check.
///
/// Unlike `can_execute_command` which returns a simple bool, this enum
/// provides more granular control:
///
/// - `Allowed`: Command can execute immediately
/// - `Denied`: Command is permanently blocked
/// - `RequiresApproval`: Command needs HIL approval before execution
///
/// # HIL Integration
///
/// When `RequiresApproval` is returned, the caller should:
///
/// 1. Submit the `ApprovalRequest` to `HilComponent`
/// 2. Wait for user approval/rejection
/// 3. If approved, call `grants.grant(CommandGrant::persistent(grant_pattern))`
/// 4. Retry the command (which will now return `Allowed`)
///
/// # Example
///
/// ```ignore
/// match checker.check_command(&session, "rm -rf ./temp") {
///     CommandCheckResult::Allowed => execute(cmd),
///     CommandCheckResult::Denied(reason) => error!("{}", reason),
///     CommandCheckResult::RequiresApproval { request, grant_pattern } => {
///         let id = hil.submit(request);
///         if await_approval(id) {
///             session.grant_command(&grant_pattern);
///             execute(cmd);
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub enum CommandCheckResult {
    /// Command is allowed to execute.
    Allowed,
    /// Command is denied with a reason.
    Denied(String),
    /// Command requires user approval via HIL.
    RequiresApproval {
        /// The approval request to submit to HilComponent.
        request: ApprovalRequest,
        /// The pattern to grant if approved (for future commands).
        grant_pattern: String,
    },
}

impl CommandCheckResult {
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

    /// Returns the denial reason if denied.
    #[must_use]
    pub fn denial_reason(&self) -> Option<&str> {
        match self {
            Self::Denied(reason) => Some(reason),
            _ => None,
        }
    }

    /// Returns the approval request if requires approval.
    #[must_use]
    pub fn approval_request(&self) -> Option<&ApprovalRequest> {
        match self {
            Self::RequiresApproval { request, .. } => Some(request),
            _ => None,
        }
    }

    /// Returns the grant pattern if requires approval.
    #[must_use]
    pub fn grant_pattern(&self) -> Option<&str> {
        match self {
            Self::RequiresApproval { grant_pattern, .. } => Some(grant_pattern),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_helpers() {
        let result = CommandCheckResult::Allowed;
        assert!(result.is_allowed());
        assert!(!result.is_denied());
        assert!(!result.requires_approval());
        assert!(result.denial_reason().is_none());
        assert!(result.approval_request().is_none());
        assert!(result.grant_pattern().is_none());
    }

    #[test]
    fn denied_helpers() {
        let result = CommandCheckResult::Denied("test reason".to_string());
        assert!(!result.is_allowed());
        assert!(result.is_denied());
        assert!(!result.requires_approval());
        assert_eq!(result.denial_reason(), Some("test reason"));
        assert!(result.approval_request().is_none());
        assert!(result.grant_pattern().is_none());
    }

    #[test]
    fn requires_approval_helpers() {
        let request = ApprovalRequest::new("bash", "test", serde_json::json!({}));
        let result = CommandCheckResult::RequiresApproval {
            request: request.clone(),
            grant_pattern: "rm -rf".to_string(),
        };
        assert!(!result.is_allowed());
        assert!(!result.is_denied());
        assert!(result.requires_approval());
        assert!(result.denial_reason().is_none());
        assert!(result.approval_request().is_some());
        assert_eq!(result.grant_pattern(), Some("rm -rf"));
    }
}

//! Unified access denied error type.
//!
//! [`AccessDenied`] unifies the three permission layers into a single error:
//!
//! ```text
//! Effective Permission = Capability(WHAT) ∩ SandboxPolicy(WHERE) ∩ Session(WHO+WHEN)
//!                            │                    │                      │
//!                   CapabilityDenied      ResourceDenied         SessionDenied
//! ```

use crate::{Capability, SandboxError};
use thiserror::Error;

/// Unified error for access denied across all permission layers.
///
/// Callers can match on the variant to determine which layer denied access
/// and provide appropriate user feedback.
///
/// # Example
///
/// ```
/// use orcs_auth::{AccessDenied, Capability};
///
/// let err = AccessDenied::CapabilityDenied {
///     operation: "write".to_string(),
///     required: Capability::WRITE,
///     available: Capability::READ,
/// };
///
/// assert!(err.to_string().contains("write"));
/// ```
#[derive(Debug, Error)]
pub enum AccessDenied {
    /// Operation requires a capability the entity does not have.
    #[error("capability denied: '{operation}' requires {required}, available: {available}")]
    CapabilityDenied {
        /// The operation that was attempted.
        operation: String,
        /// The capability required for the operation.
        required: Capability,
        /// The capabilities actually available to the entity.
        available: Capability,
    },

    /// Resource access denied by sandbox policy.
    #[error(transparent)]
    ResourceDenied(#[from] SandboxError),

    /// Session does not have sufficient privilege for the operation.
    #[error("session denied: {0}")]
    SessionDenied(String),
}

impl AccessDenied {
    /// Returns the permission layer that denied access.
    #[must_use]
    pub fn layer(&self) -> &'static str {
        match self {
            Self::CapabilityDenied { .. } => "capability",
            Self::ResourceDenied(_) => "resource",
            Self::SessionDenied(_) => "session",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_denied_display() {
        let err = AccessDenied::CapabilityDenied {
            operation: "write_file".to_string(),
            required: Capability::WRITE,
            available: Capability::READ,
        };

        let msg = err.to_string();
        assert!(msg.contains("write_file"), "got: {msg}");
        assert!(msg.contains("capability denied"), "got: {msg}");
        assert_eq!(err.layer(), "capability");
    }

    #[test]
    fn resource_denied_from_sandbox_error() {
        let sandbox_err = SandboxError::OutsideBoundary {
            path: "/etc/passwd".to_string(),
            root: "/home/user/project".to_string(),
        };
        let err = AccessDenied::from(sandbox_err);

        let msg = err.to_string();
        assert!(msg.contains("access denied"), "got: {msg}");
        assert_eq!(err.layer(), "resource");
    }

    #[test]
    fn session_denied_display() {
        let err = AccessDenied::SessionDenied("requires elevation".to_string());

        let msg = err.to_string();
        assert!(msg.contains("requires elevation"), "got: {msg}");
        assert_eq!(err.layer(), "session");
    }
}

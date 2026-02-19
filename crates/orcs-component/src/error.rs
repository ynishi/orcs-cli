//! Component layer errors.
//!
//! Errors that can occur during component operations.
//! All errors implement [`ErrorCode`] for unified handling.
//!
//! # Error Code Convention
//!
//! All component errors use the `COMPONENT_` prefix:
//!
//! | Error | Code | Recoverable |
//! |-------|------|-------------|
//! | [`NotSupported`](ComponentError::NotSupported) | `COMPONENT_NOT_SUPPORTED` | No |
//! | [`ExecutionFailed`](ComponentError::ExecutionFailed) | `COMPONENT_EXECUTION_FAILED` | Yes |
//! | [`InvalidPayload`](ComponentError::InvalidPayload) | `COMPONENT_INVALID_PAYLOAD` | No |
//! | [`Aborted`](ComponentError::Aborted) | `COMPONENT_ABORTED` | No |
//! | [`Suspended`](ComponentError::Suspended) | `COMPONENT_SUSPENDED` | Yes |
//!
//! # Recoverability
//!
//! - **Recoverable**: Retry may succeed (transient failures)
//! - **Not Recoverable**: Retry won't help (logic errors)
//!
//! # Example
//!
//! ```
//! use orcs_component::ComponentError;
//! use orcs_types::ErrorCode;
//!
//! let err = ComponentError::NotSupported("unknown".into());
//! assert_eq!(err.code(), "COMPONENT_NOT_SUPPORTED");
//! assert!(!err.is_recoverable());
//!
//! let err = ComponentError::ExecutionFailed("timeout".into());
//! assert_eq!(err.code(), "COMPONENT_EXECUTION_FAILED");
//! assert!(err.is_recoverable());
//! ```

use orcs_types::ErrorCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Component layer error.
///
/// # Variants
///
/// | Variant | When | Recovery |
/// |---------|------|----------|
/// | `NotSupported` | Unknown operation | Fix request |
/// | `ExecutionFailed` | Operation failed | May retry |
/// | `InvalidPayload` | Bad request data | Fix payload |
/// | `Aborted` | Signal-triggered abort | Intentional |
/// | `Suspended` | Awaiting approval | Approval flow |
///
/// # Example
///
/// ```
/// use orcs_component::ComponentError;
/// use orcs_types::ErrorCode;
///
/// fn handle_error(err: ComponentError) {
///     eprintln!("[{}] {}", err.code(), err);
///     if err.is_recoverable() {
///         eprintln!("May retry");
///     }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum ComponentError {
    /// Operation not supported by this component.
    ///
    /// The requested operation is not recognized.
    /// Check the component's supported operations.
    ///
    /// **Not recoverable** - the operation will never work.
    #[error("operation not supported: {0}")]
    NotSupported(String),

    /// Execution failed during operation.
    ///
    /// The operation was recognized but failed during execution.
    /// Common causes: timeout, resource unavailable, external service failure.
    ///
    /// **Recoverable** - retry may succeed.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    /// Invalid payload in request.
    ///
    /// The request payload doesn't match expected format.
    /// Check payload structure and required fields.
    ///
    /// **Not recoverable** - fix the payload.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),

    /// Component was aborted.
    ///
    /// The component received an abort signal (Veto/Cancel).
    /// This is intentional termination, not an error condition.
    ///
    /// **Not recoverable** - intentional stop.
    #[error("component aborted")]
    Aborted,

    /// Initialization failed.
    ///
    /// The component failed to initialize.
    /// Check configuration and dependencies.
    ///
    /// **Recoverable** - may succeed with different config.
    #[error("initialization failed: {0}")]
    InitFailed(String),

    /// Execution suspended pending human approval.
    ///
    /// Returned by `on_request()` when a command requires permission
    /// that hasn't been granted yet. The ChannelRunner intercepts this
    /// error and transitions to `AwaitingApproval` state, allowing the
    /// event loop to continue processing signals (Approve/Reject).
    ///
    /// On approval, the ChannelRunner grants the pattern and re-dispatches
    /// the pending request. On rejection, the request is discarded.
    ///
    /// **Recoverable** - approval resolves this.
    ///
    /// # Fields
    ///
    /// - `approval_id`: Unique ID for this approval request (matches Signal routing).
    /// - `grant_pattern`: Permission pattern to grant on approval (e.g. `"shell:*"`).
    /// - `pending_request`: Serialized original request for re-dispatch after approval.
    #[error("suspended pending approval: {approval_id}")]
    Suspended {
        /// Unique ID for this approval request.
        approval_id: String,
        /// Permission pattern to grant on approval.
        grant_pattern: String,
        /// Serialized original request for re-dispatch.
        pending_request: serde_json::Value,
    },
}

impl ErrorCode for ComponentError {
    /// Returns a machine-readable error code.
    ///
    /// All component errors use the `COMPONENT_` prefix.
    fn code(&self) -> &'static str {
        match self {
            Self::NotSupported(_) => "COMPONENT_NOT_SUPPORTED",
            Self::ExecutionFailed(_) => "COMPONENT_EXECUTION_FAILED",
            Self::InvalidPayload(_) => "COMPONENT_INVALID_PAYLOAD",
            Self::Aborted => "COMPONENT_ABORTED",
            Self::InitFailed(_) => "COMPONENT_INIT_FAILED",
            Self::Suspended { .. } => "COMPONENT_SUSPENDED",
        }
    }

    /// Returns whether the error is recoverable.
    ///
    /// # Returns
    ///
    /// - `true`: Retry may succeed
    /// - `false`: Retry will not help
    fn is_recoverable(&self) -> bool {
        match self {
            Self::ExecutionFailed(_) => true,
            Self::InitFailed(_) => true,
            Self::Suspended { .. } => true,
            Self::NotSupported(_) => false,
            Self::InvalidPayload(_) => false,
            Self::Aborted => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::assert_error_codes;

    /// All variants for exhaustive testing
    fn all_variants() -> Vec<ComponentError> {
        vec![
            ComponentError::NotSupported("x".into()),
            ComponentError::ExecutionFailed("x".into()),
            ComponentError::InvalidPayload("x".into()),
            ComponentError::Aborted,
            ComponentError::InitFailed("x".into()),
            ComponentError::Suspended {
                approval_id: "test-001".into(),
                grant_pattern: "shell:*".into(),
                pending_request: serde_json::Value::Null,
            },
        ]
    }

    #[test]
    fn all_error_codes_valid() {
        // This test ensures ALL variants have correct prefix and format
        assert_error_codes(&all_variants(), "COMPONENT_");
    }

    #[test]
    fn not_supported_error() {
        let err = ComponentError::NotSupported("unknown".into());
        assert_eq!(err.code(), "COMPONENT_NOT_SUPPORTED");
        assert!(!err.is_recoverable());
        assert!(err.to_string().contains("not supported"));
    }

    #[test]
    fn execution_failed_error() {
        let err = ComponentError::ExecutionFailed("timeout".into());
        assert_eq!(err.code(), "COMPONENT_EXECUTION_FAILED");
        assert!(err.is_recoverable());
        assert!(err.to_string().contains("execution failed"));
    }

    #[test]
    fn invalid_payload_error() {
        let err = ComponentError::InvalidPayload("missing field".into());
        assert_eq!(err.code(), "COMPONENT_INVALID_PAYLOAD");
        assert!(!err.is_recoverable());
        assert!(err.to_string().contains("invalid payload"));
    }

    #[test]
    fn aborted_error() {
        let err = ComponentError::Aborted;
        assert_eq!(err.code(), "COMPONENT_ABORTED");
        assert!(!err.is_recoverable());
        assert!(err.to_string().contains("aborted"));
    }

    #[test]
    fn init_failed_error() {
        let err = ComponentError::InitFailed("missing config".into());
        assert_eq!(err.code(), "COMPONENT_INIT_FAILED");
        assert!(err.is_recoverable());
        assert!(err.to_string().contains("initialization failed"));
    }

    #[test]
    fn suspended_error() {
        let err = ComponentError::Suspended {
            approval_id: "ap-42".into(),
            grant_pattern: "shell:ls".into(),
            pending_request: serde_json::json!({"op": "exec", "cmd": "ls"}),
        };
        assert_eq!(err.code(), "COMPONENT_SUSPENDED");
        assert!(err.is_recoverable());
        assert!(err.to_string().contains("suspended pending approval"));
        assert!(err.to_string().contains("ap-42"));
    }

    #[test]
    fn suspended_preserves_fields() {
        let payload = serde_json::json!({"command": "rm -rf /", "args": []});
        let err = ComponentError::Suspended {
            approval_id: "ap-99".into(),
            grant_pattern: "shell:rm".into(),
            pending_request: payload.clone(),
        };
        match err {
            ComponentError::Suspended {
                approval_id,
                grant_pattern,
                pending_request,
            } => {
                assert_eq!(approval_id, "ap-99");
                assert_eq!(grant_pattern, "shell:rm");
                assert_eq!(pending_request, payload);
            }
            _ => panic!("Expected Suspended variant"),
        }
    }

    #[test]
    fn suspended_serialization_roundtrip() {
        let err = ComponentError::Suspended {
            approval_id: "ap-rt".into(),
            grant_pattern: "tool:*".into(),
            pending_request: serde_json::json!({"test": true}),
        };
        let serialized = serde_json::to_string(&err).expect("Suspended should serialize to JSON");
        let deserialized: ComponentError =
            serde_json::from_str(&serialized).expect("Suspended should deserialize from JSON");
        assert_eq!(deserialized.code(), "COMPONENT_SUSPENDED");
        match deserialized {
            ComponentError::Suspended {
                approval_id,
                grant_pattern,
                pending_request,
            } => {
                assert_eq!(approval_id, "ap-rt");
                assert_eq!(grant_pattern, "tool:*");
                assert_eq!(pending_request, serde_json::json!({"test": true}));
            }
            _ => panic!("Expected Suspended after roundtrip"),
        }
    }

    #[test]
    fn error_code_prefix() {
        // All component errors should have COMPONENT_ prefix
        let errors: Vec<ComponentError> = vec![
            ComponentError::NotSupported("x".into()),
            ComponentError::ExecutionFailed("x".into()),
            ComponentError::InvalidPayload("x".into()),
            ComponentError::Aborted,
            ComponentError::InitFailed("x".into()),
            ComponentError::Suspended {
                approval_id: "test-001".into(),
                grant_pattern: "shell:*".into(),
                pending_request: serde_json::Value::Null,
            },
        ];

        for err in errors {
            assert!(err.code().starts_with("COMPONENT_"));
        }
    }
}

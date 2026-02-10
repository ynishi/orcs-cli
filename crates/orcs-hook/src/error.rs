//! Error types for the hook system.

use thiserror::Error;

/// Errors that can occur in the hook system.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HookError {
    /// Invalid FQL pattern syntax.
    #[error("invalid FQL pattern: {0}")]
    InvalidFql(String),

    /// Unknown hook point string.
    #[error("unknown hook point: {0}")]
    UnknownHookPoint(String),

    /// Hook execution failed.
    #[error("hook execution failed [{hook_id}]: {message}")]
    ExecutionFailed {
        /// ID of the hook that failed.
        hook_id: String,
        /// Error message.
        message: String,
    },

    /// Hook with the given ID was not found.
    #[error("hook not found: {0}")]
    NotFound(String),

    /// Hook chain depth limit exceeded.
    #[error("depth limit exceeded (depth={depth}, max={max_depth})")]
    DepthExceeded {
        /// Current depth.
        depth: u8,
        /// Maximum allowed depth.
        max_depth: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_fql() {
        let err = HookError::InvalidFql("missing ::".into());
        assert_eq!(err.to_string(), "invalid FQL pattern: missing ::");
    }

    #[test]
    fn display_unknown_hook_point() {
        let err = HookError::UnknownHookPoint("foo.bar".into());
        assert_eq!(err.to_string(), "unknown hook point: foo.bar");
    }

    #[test]
    fn display_execution_failed() {
        let err = HookError::ExecutionFailed {
            hook_id: "audit-hook".into(),
            message: "lua error".into(),
        };
        assert_eq!(
            err.to_string(),
            "hook execution failed [audit-hook]: lua error"
        );
    }

    #[test]
    fn display_not_found() {
        let err = HookError::NotFound("my-hook".into());
        assert_eq!(err.to_string(), "hook not found: my-hook");
    }

    #[test]
    fn display_depth_exceeded() {
        let err = HookError::DepthExceeded {
            depth: 5,
            max_depth: 4,
        };
        assert_eq!(err.to_string(), "depth limit exceeded (depth=5, max=4)");
    }

    #[test]
    fn error_is_clone_and_eq() {
        let a = HookError::NotFound("x".into());
        let b = a.clone();
        assert_eq!(a, b);
    }
}

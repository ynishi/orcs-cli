//! Channel layer errors.
//!
//! This module defines errors for the Channel layer.
//! All errors implement [`ErrorCode`] for standardized handling.
//!
//! # Error Codes
//!
//! | Variant | Code | Recoverable |
//! |---------|------|-------------|
//! | [`ChannelError::NotFound`] | `CHANNEL_NOT_FOUND` | No |
//! | [`ChannelError::InvalidState`] | `CHANNEL_INVALID_STATE` | No |
//! | [`ChannelError::ParentNotFound`] | `CHANNEL_PARENT_NOT_FOUND` | No |
//! | [`ChannelError::AlreadyExists`] | `CHANNEL_ALREADY_EXISTS` | No |
//!
//! # Example
//!
//! ```
//! use orcs_runtime::ChannelError;
//! use orcs_types::{ChannelId, ErrorCode};
//!
//! let err = ChannelError::NotFound(ChannelId::new());
//!
//! // Machine-readable code for programmatic handling
//! assert_eq!(err.code(), "CHANNEL_NOT_FOUND");
//!
//! // Recoverability info for retry logic
//! assert!(!err.is_recoverable());
//! ```

use orcs_types::{ChannelId, ErrorCode};
use thiserror::Error;

/// Channel layer error.
///
/// Represents failures within the Channel management layer.
/// Implements [`ErrorCode`] for standardized error handling.
///
/// # Example
///
/// ```
/// use orcs_runtime::ChannelError;
/// use orcs_types::{ChannelId, ErrorCode};
///
/// fn handle_error(err: ChannelError) {
///     match err.code() {
///         "CHANNEL_NOT_FOUND" => eprintln!("Channel missing"),
///         "CHANNEL_INVALID_STATE" => eprintln!("State error"),
///         _ => eprintln!("Other error: {err}"),
///     }
/// }
/// ```
#[derive(Debug, Clone, Error)]
pub enum ChannelError {
    /// The specified channel does not exist.
    ///
    /// This typically occurs when:
    /// - Querying a channel that was never created
    /// - Accessing a channel that was killed
    #[error("channel not found: {0}")]
    NotFound(ChannelId),

    /// Attempted an invalid state transition.
    ///
    /// Channel state transitions are strict:
    /// - Only `Running` → `Completed` is allowed via `complete()`
    /// - Only `Running` → `Aborted` is allowed via `abort()`
    /// - Terminal states cannot transition further
    #[error("invalid state transition: {from} -> {to}")]
    InvalidState {
        /// The current state name.
        from: String,
        /// The attempted target state name.
        to: String,
    },

    /// The parent channel for a spawn operation does not exist.
    ///
    /// When spawning a child channel, the specified parent must exist.
    #[error("parent channel not found: {0}")]
    ParentNotFound(ChannelId),

    /// A channel with this ID already exists.
    ///
    /// Channel IDs are UUIDs and should be globally unique.
    /// This error indicates a potential bug or UUID collision.
    #[error("channel already exists: {0}")]
    AlreadyExists(ChannelId),
}

impl ErrorCode for ChannelError {
    fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "CHANNEL_NOT_FOUND",
            Self::InvalidState { .. } => "CHANNEL_INVALID_STATE",
            Self::ParentNotFound(_) => "CHANNEL_PARENT_NOT_FOUND",
            Self::AlreadyExists(_) => "CHANNEL_ALREADY_EXISTS",
        }
    }

    fn is_recoverable(&self) -> bool {
        // All channel errors are non-recoverable
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::assert_error_codes;

    fn all_variants() -> Vec<ChannelError> {
        vec![
            ChannelError::NotFound(ChannelId::new()),
            ChannelError::InvalidState {
                from: "x".into(),
                to: "y".into(),
            },
            ChannelError::ParentNotFound(ChannelId::new()),
            ChannelError::AlreadyExists(ChannelId::new()),
        ]
    }

    #[test]
    fn all_error_codes_valid() {
        assert_error_codes(&all_variants(), "CHANNEL_");
    }

    #[test]
    fn not_found_error() {
        let id = ChannelId::new();
        let err = ChannelError::NotFound(id);
        assert_eq!(err.code(), "CHANNEL_NOT_FOUND");
        assert!(!err.is_recoverable());
    }

    #[test]
    fn invalid_state_error() {
        let err = ChannelError::InvalidState {
            from: "Running".into(),
            to: "Running".into(),
        };
        assert_eq!(err.code(), "CHANNEL_INVALID_STATE");
        assert!(!err.is_recoverable());
    }
}

//! Event layer errors.
//!
//! This module defines error types for the event system,
//! implementing the [`ErrorCode`] trait for unified error handling
//! across the ORCS system.
//!
//! # Error Code Convention
//!
//! All event errors use the `EVENT_` prefix for their codes:
//!
//! | Error | Code | Recoverable |
//! |-------|------|-------------|
//! | [`EventError::Timeout`] | `EVENT_TIMEOUT` | Yes |
//! | [`EventError::InvalidSignal`] | `EVENT_INVALID_SIGNAL` | No |
//! | [`EventError::InvalidRequest`] | `EVENT_INVALID_REQUEST` | No |
//! | [`EventError::PermissionDenied`] | `EVENT_PERMISSION_DENIED` | No |
//!
//! # Recoverability
//!
//! An error is **recoverable** if retrying the operation may succeed:
//!
//! - **Timeout**: Target may become available (network, busy component)
//! - **Invalid Signal**: Won't change on retry (bug in caller)
//! - **Permission Denied**: Requires elevation, not retry
//!
//! # Integration with ErrorCode
//!
//! All errors implement [`ErrorCode`] for:
//!
//! - **Machine-readable codes**: For programmatic error handling
//! - **Recoverability info**: For retry logic and user feedback
//! - **Unified logging**: Consistent format across crates
//!
//! # Usage
//!
//! ```
//! use orcs_event::EventError;
//! use orcs_types::{ErrorCode, RequestId};
//!
//! fn handle_error(err: EventError) {
//!     // Log with error code
//!     eprintln!("[{}] {}", err.code(), err);
//!
//!     // Decide on retry
//!     if err.is_recoverable() {
//!         eprintln!("Retrying...");
//!     } else {
//!         eprintln!("Fatal error, cannot retry");
//!     }
//! }
//!
//! let err = EventError::Timeout(RequestId::new());
//! handle_error(err);
//! ```
//!
//! # Error Handling Patterns
//!
//! ## Retry with Backoff
//!
//! ```
//! use orcs_event::EventError;
//! use orcs_types::{ErrorCode, RequestId};
//!
//! fn retry_on_timeout<T, F>(mut operation: F, max_retries: usize) -> Result<T, EventError>
//! where
//!     F: FnMut() -> Result<T, EventError>,
//! {
//!     let mut attempts = 0;
//!     loop {
//!         match operation() {
//!             Ok(result) => return Ok(result),
//!             Err(err) if err.is_recoverable() && attempts < max_retries => {
//!                 attempts += 1;
//!                 // In real code: add exponential backoff
//!                 continue;
//!             }
//!             Err(err) => return Err(err),
//!         }
//!     }
//! }
//! ```
//!
//! ## Error Code Matching
//!
//! ```
//! use orcs_event::EventError;
//! use orcs_types::{ErrorCode, RequestId};
//!
//! fn handle_by_code(err: &EventError) {
//!     match err.code() {
//!         "EVENT_TIMEOUT" => {
//!             // Handle timeout specifically
//!         }
//!         "EVENT_PERMISSION_DENIED" => {
//!             // Prompt for elevation
//!         }
//!         code if code.starts_with("EVENT_") => {
//!             // Generic event error handling
//!         }
//!         _ => unreachable!("Unknown error code"),
//!     }
//! }
//! ```

use orcs_types::{ErrorCode, RequestId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Event layer error.
///
/// All variants implement [`ErrorCode`] for standardized
/// error handling across the ORCS system.
///
/// # Variants
///
/// | Variant | When | Recovery |
/// |---------|------|----------|
/// | [`Timeout`](Self::Timeout) | Request exceeded timeout | Retry may help |
/// | [`InvalidSignal`](Self::InvalidSignal) | Malformed signal | Fix the caller |
/// | [`InvalidRequest`](Self::InvalidRequest) | Malformed request | Fix the caller |
/// | [`PermissionDenied`](Self::PermissionDenied) | Insufficient privilege | Elevate session |
///
/// # Example
///
/// ```
/// use orcs_event::EventError;
/// use orcs_types::{ErrorCode, RequestId};
///
/// let err = EventError::Timeout(RequestId::new());
/// assert_eq!(err.code(), "EVENT_TIMEOUT");
/// assert!(err.is_recoverable());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum EventError {
    /// Request timed out waiting for response.
    ///
    /// This is **recoverable** - retry may succeed if the target
    /// component becomes available or finishes current work.
    ///
    /// # Common Causes
    ///
    /// - Target component is busy processing other requests
    /// - Network latency (in distributed scenarios)
    /// - Target component has crashed (requires restart)
    ///
    /// # Recovery
    ///
    /// - Retry with exponential backoff
    /// - Increase timeout for slow operations
    /// - Check target component health
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_event::EventError;
    /// use orcs_types::{ErrorCode, RequestId};
    ///
    /// let req_id = RequestId::new();
    /// let err = EventError::Timeout(req_id);
    ///
    /// assert!(err.is_recoverable());
    /// assert!(err.to_string().contains("timed out"));
    /// ```
    #[error("request timed out: {0}")]
    Timeout(RequestId),

    /// Invalid signal was received.
    ///
    /// This is **not recoverable** - the signal format or content
    /// is invalid and won't change on retry.
    ///
    /// # Common Causes
    ///
    /// - Malformed signal payload
    /// - Invalid scope specification
    /// - Deserialization failure
    ///
    /// # Recovery
    ///
    /// - Fix the signal construction in the caller
    /// - Validate signal before sending
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_event::EventError;
    /// use orcs_types::ErrorCode;
    ///
    /// let err = EventError::InvalidSignal("missing scope".into());
    ///
    /// assert!(!err.is_recoverable());
    /// assert!(err.to_string().contains("invalid signal"));
    /// ```
    #[error("invalid signal: {0}")]
    InvalidSignal(String),

    /// Invalid request was constructed.
    ///
    /// This is **not recoverable** - the request format or content
    /// is invalid and won't change on retry.
    ///
    /// # Common Causes
    ///
    /// - Empty operation string
    /// - Invalid payload format
    ///
    /// # Recovery
    ///
    /// - Fix the request construction in the caller
    /// - Validate parameters before constructing
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_event::EventError;
    /// use orcs_types::ErrorCode;
    ///
    /// let err = EventError::InvalidRequest("operation cannot be empty".into());
    ///
    /// assert!(!err.is_recoverable());
    /// assert!(err.to_string().contains("invalid request"));
    /// ```
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Permission denied for the requested operation.
    ///
    /// This is **not recoverable by retry** - requires privilege
    /// elevation or different credentials.
    ///
    /// # Common Causes
    ///
    /// - Attempting Global signal without elevated privilege
    /// - Component trying to signal outside its scope
    /// - Session has expired elevation
    ///
    /// # Recovery
    ///
    /// - Elevate the session: `session.elevate(duration)`
    /// - Use a more limited scope
    /// - Request elevation from Human
    ///
    /// # Permission Model
    ///
    /// ```text
    /// | Scope           | Standard | Elevated |
    /// |-----------------|----------|----------|
    /// | Global          | Denied   | Allowed  |
    /// | Channel (own)   | Allowed  | Allowed  |
    /// | Channel (other) | Denied   | Allowed  |
    /// ```
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_event::EventError;
    /// use orcs_types::ErrorCode;
    ///
    /// let err = EventError::PermissionDenied(
    ///     "Global scope requires elevated privilege".into()
    /// );
    ///
    /// assert!(!err.is_recoverable());
    /// assert_eq!(err.code(), "EVENT_PERMISSION_DENIED");
    /// ```
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}

impl ErrorCode for EventError {
    /// Returns a machine-readable error code.
    ///
    /// All event errors use the `EVENT_` prefix.
    ///
    /// # Codes
    ///
    /// - `EVENT_TIMEOUT` - Request timed out
    /// - `EVENT_INVALID_SIGNAL` - Malformed signal
    /// - `EVENT_INVALID_REQUEST` - Malformed request
    /// - `EVENT_PERMISSION_DENIED` - Insufficient privilege
    fn code(&self) -> &'static str {
        match self {
            Self::Timeout(_) => "EVENT_TIMEOUT",
            Self::InvalidSignal(_) => "EVENT_INVALID_SIGNAL",
            Self::InvalidRequest(_) => "EVENT_INVALID_REQUEST",
            Self::PermissionDenied(_) => "EVENT_PERMISSION_DENIED",
        }
    }

    /// Returns whether the error is recoverable.
    ///
    /// # Returns
    ///
    /// - `true`: Retry may succeed ([`Timeout`](Self::Timeout))
    /// - `false`: Retry will not help, requires different action
    fn is_recoverable(&self) -> bool {
        match self {
            Self::Timeout(_) => true,
            Self::InvalidSignal(_) => false,
            Self::InvalidRequest(_) => false,
            Self::PermissionDenied(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::assert_error_codes;

    /// All variants for exhaustive testing
    fn all_variants() -> Vec<EventError> {
        vec![
            EventError::Timeout(RequestId::new()),
            EventError::InvalidSignal("x".into()),
            EventError::InvalidRequest("x".into()),
            EventError::PermissionDenied("x".into()),
        ]
    }

    #[test]
    fn all_error_codes_valid() {
        // This test ensures ALL variants have correct prefix and format
        assert_error_codes(&all_variants(), "EVENT_");
    }

    #[test]
    fn timeout_error() {
        let req_id = RequestId::new();
        let err = EventError::Timeout(req_id);

        assert_eq!(err.code(), "EVENT_TIMEOUT");
        assert!(err.is_recoverable());
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn invalid_signal_error() {
        let err = EventError::InvalidSignal("bad format".into());

        assert_eq!(err.code(), "EVENT_INVALID_SIGNAL");
        assert!(!err.is_recoverable());
        assert!(err.to_string().contains("invalid signal"));
    }

    #[test]
    fn invalid_request_error() {
        let err = EventError::InvalidRequest("operation cannot be empty".into());

        assert_eq!(err.code(), "EVENT_INVALID_REQUEST");
        assert!(!err.is_recoverable());
        assert!(err.to_string().contains("invalid request"));
    }

    #[test]
    fn permission_denied_error() {
        let err = EventError::PermissionDenied("Global signal requires elevation".into());

        assert_eq!(err.code(), "EVENT_PERMISSION_DENIED");
        assert!(!err.is_recoverable());
        assert!(err.to_string().contains("permission denied"));
    }
}

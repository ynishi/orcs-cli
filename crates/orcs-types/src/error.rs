//! Unified error interface for ORCS.
//!
//! This module provides the [`ErrorCode`] trait for standardized
//! error handling across all ORCS crates.
//!
//! # Design
//!
//! All ORCS error types should implement [`ErrorCode`] to provide:
//!
//! - **Machine-readable codes**: For programmatic error handling
//! - **Recoverability info**: For retry logic and user feedback
//!
//! # Example
//!
//! ```
//! use orcs_types::ErrorCode;
//!
//! #[derive(Debug)]
//! enum MyError {
//!     NotFound(String),
//!     Timeout,
//! }
//!
//! impl ErrorCode for MyError {
//!     fn code(&self) -> &'static str {
//!         match self {
//!             Self::NotFound(_) => "NOT_FOUND",
//!             Self::Timeout => "TIMEOUT",
//!         }
//!     }
//!
//!     fn is_recoverable(&self) -> bool {
//!         matches!(self, Self::Timeout)
//!     }
//! }
//!
//! let err = MyError::Timeout;
//! assert_eq!(err.code(), "TIMEOUT");
//! assert!(err.is_recoverable());
//! ```

/// Unified error code interface for ORCS errors.
///
/// Implement this trait for all error types to enable:
///
/// - Consistent error code format across crates
/// - Unified error handling in EventBus and components
/// - Standardized logging and monitoring
///
/// # Code Format
///
/// Error codes should be:
///
/// - **UPPER_SNAKE_CASE**: e.g., `"TIMEOUT"`, `"PERMISSION_DENIED"`
/// - **Namespace-prefixed for specificity**: e.g., `"AUTH_EXPIRED"`, `"EVENT_INVALID_SIGNAL"`
/// - **Stable**: Codes should not change once defined (API contract)
///
/// # Recoverability
///
/// An error is recoverable if:
///
/// - Retrying the operation may succeed
/// - The user can take action to fix it
/// - It's a transient condition (network, timeout)
///
/// Non-recoverable errors:
///
/// - Invalid input (won't change on retry)
/// - Permission denied (requires elevation, not retry)
/// - Internal errors (bugs)
///
/// # Example Implementation
///
/// ```
/// use orcs_types::ErrorCode;
///
/// enum AuthError {
///     SessionExpired,
///     PermissionDenied { action: String },
///     InvalidToken,
/// }
///
/// impl ErrorCode for AuthError {
///     fn code(&self) -> &'static str {
///         match self {
///             Self::SessionExpired => "AUTH_SESSION_EXPIRED",
///             Self::PermissionDenied { .. } => "AUTH_PERMISSION_DENIED",
///             Self::InvalidToken => "AUTH_INVALID_TOKEN",
///         }
///     }
///
///     fn is_recoverable(&self) -> bool {
///         match self {
///             // Session can be refreshed
///             Self::SessionExpired => true,
///             // Need elevation, not retry
///             Self::PermissionDenied { .. } => false,
///             // Invalid token won't become valid
///             Self::InvalidToken => false,
///         }
///     }
/// }
/// ```
pub trait ErrorCode {
    /// Returns a machine-readable error code.
    ///
    /// # Format
    ///
    /// - UPPER_SNAKE_CASE
    /// - Optionally prefixed with domain (e.g., `"AUTH_"`, `"EVENT_"`)
    /// - Stable across versions (breaking change if modified)
    ///
    /// # Examples
    ///
    /// - `"TIMEOUT"`
    /// - `"PERMISSION_DENIED"`
    /// - `"AUTH_SESSION_EXPIRED"`
    /// - `"EVENT_INVALID_SIGNAL"`
    fn code(&self) -> &'static str;

    /// Returns whether the error is recoverable.
    ///
    /// # Returns
    ///
    /// - `true`: Retry may succeed, or user can take corrective action
    /// - `false`: Retry will not help, requires code/config change
    fn is_recoverable(&self) -> bool;
}

/// Validates that an error code follows ORCS conventions.
///
/// # Checks
///
/// 1. Code is UPPER_SNAKE_CASE
/// 2. Code starts with expected prefix
/// 3. Code is not empty
///
/// # Panics
///
/// Panics with descriptive message if validation fails.
///
/// # Example
///
/// ```
/// use orcs_types::{ErrorCode, assert_error_code};
///
/// #[derive(Debug)]
/// enum MyError { Timeout }
///
/// impl ErrorCode for MyError {
///     fn code(&self) -> &'static str { "MY_TIMEOUT" }
///     fn is_recoverable(&self) -> bool { true }
/// }
///
/// let err = MyError::Timeout;
/// assert_error_code(&err, "MY_");
/// ```
pub fn assert_error_code<E: ErrorCode>(err: &E, expected_prefix: &str) {
    let code = err.code();

    // Check not empty
    assert!(!code.is_empty(), "Error code must not be empty");

    // Check prefix
    assert!(
        code.starts_with(expected_prefix),
        "Error code '{}' must start with prefix '{}'",
        code,
        expected_prefix
    );

    // Check UPPER_SNAKE_CASE
    assert!(
        is_upper_snake_case(code),
        "Error code '{}' must be UPPER_SNAKE_CASE",
        code
    );
}

/// Validates multiple error codes at once.
///
/// Use this to verify all variants of an error enum.
///
/// # Example
///
/// ```
/// use orcs_types::{ErrorCode, assert_error_codes};
///
/// #[derive(Debug)]
/// enum MyError { A, B }
///
/// impl ErrorCode for MyError {
///     fn code(&self) -> &'static str {
///         match self {
///             Self::A => "MY_A",
///             Self::B => "MY_B",
///         }
///     }
///     fn is_recoverable(&self) -> bool { false }
/// }
///
/// assert_error_codes(&[MyError::A, MyError::B], "MY_");
/// ```
pub fn assert_error_codes<E: ErrorCode>(errors: &[E], expected_prefix: &str) {
    for err in errors {
        assert_error_code(err, expected_prefix);
    }
}

/// Checks if a string is UPPER_SNAKE_CASE.
fn is_upper_snake_case(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    // Must not start or end with underscore
    if s.starts_with('_') || s.ends_with('_') {
        return false;
    }

    // Must not have consecutive underscores
    if s.contains("__") {
        return false;
    }

    // All chars must be uppercase letters, digits, or underscore
    s.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    enum TestError {
        Transient,
        Permanent,
    }

    impl ErrorCode for TestError {
        fn code(&self) -> &'static str {
            match self {
                Self::Transient => "TEST_TRANSIENT",
                Self::Permanent => "TEST_PERMANENT",
            }
        }

        fn is_recoverable(&self) -> bool {
            matches!(self, Self::Transient)
        }
    }

    #[test]
    fn error_code_trait() {
        let transient = TestError::Transient;
        assert_eq!(transient.code(), "TEST_TRANSIENT");
        assert!(transient.is_recoverable());

        let permanent = TestError::Permanent;
        assert_eq!(permanent.code(), "TEST_PERMANENT");
        assert!(!permanent.is_recoverable());
    }

    #[test]
    fn assert_error_code_valid() {
        let err = TestError::Transient;
        assert_error_code(&err, "TEST_");
    }

    #[test]
    fn assert_error_codes_all_variants() {
        assert_error_codes(&[TestError::Transient, TestError::Permanent], "TEST_");
    }

    #[test]
    #[should_panic(expected = "must start with prefix")]
    fn assert_error_code_wrong_prefix() {
        let err = TestError::Transient;
        assert_error_code(&err, "WRONG_");
    }

    #[test]
    fn is_upper_snake_case_valid() {
        assert!(is_upper_snake_case("HELLO"));
        assert!(is_upper_snake_case("HELLO_WORLD"));
        assert!(is_upper_snake_case("A_B_C"));
        assert!(is_upper_snake_case("ERROR_123"));
    }

    #[test]
    fn is_upper_snake_case_invalid() {
        assert!(!is_upper_snake_case(""));
        assert!(!is_upper_snake_case("hello"));
        assert!(!is_upper_snake_case("Hello_World"));
        assert!(!is_upper_snake_case("_HELLO"));
        assert!(!is_upper_snake_case("HELLO_"));
        assert!(!is_upper_snake_case("HELLO__WORLD"));
    }
}

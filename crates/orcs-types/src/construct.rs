//! Fallible construction traits for ORCS types.
//!
//! This module provides the [`TryNew`] trait for types that require
//! validation during construction and may fail.
//!
//! # When to Use Which Pattern
//!
//! | Pattern | Use When |
//! |---------|----------|
//! | `new()` | Construction always succeeds (infallible) |
//! | [`TryNew`] | Construction requires validation (fallible) |
//! | `TryFrom<T>` | Converting from another type (fallible) |
//! | `Default` | Sensible default value exists |
//! | Builder | Complex multi-field initialization |
//! | Dependency Injection | External dependencies required |
//!
//! # Design Rationale
//!
//! Following Rust's naming conventions:
//!
//! - `new()` - Infallible, always returns `Self`
//! - `try_new()` - Fallible, returns `Result<Self, Error>`
//! - `TryFrom::try_from()` - Fallible conversion from another type
//!
//! This mirrors the standard library's `TryFrom`/`TryInto` pattern
//! but for constructors that don't convert from another type.
//!
//! # Example
//!
//! ```
//! use orcs_types::TryNew;
//!
//! /// Non-empty string wrapper.
//! #[derive(Debug)]
//! struct NonEmptyString(String);
//!
//! #[derive(Debug, PartialEq)]
//! struct EmptyStringError;
//!
//! impl TryNew for NonEmptyString {
//!     type Error = EmptyStringError;
//!     type Args = String;
//!
//!     fn try_new(value: String) -> Result<Self, Self::Error> {
//!         if value.is_empty() {
//!             return Err(EmptyStringError);
//!         }
//!         Ok(NonEmptyString(value))
//!     }
//! }
//!
//! // Valid construction
//! let valid = NonEmptyString::try_new("hello".to_string());
//! assert!(valid.is_ok());
//!
//! // Invalid construction
//! let invalid = NonEmptyString::try_new(String::new());
//! assert_eq!(invalid.unwrap_err(), EmptyStringError);
//! ```

/// Trait for fallible construction with validation.
///
/// Implement this trait when:
///
/// - Construction requires validation that may fail
/// - You are NOT converting from another type (use `TryFrom` instead)
/// - A plain `new()` cannot guarantee success
///
/// # Naming Convention
///
/// Types implementing `TryNew` should NOT have a plain `new()` method
/// that performs the same validation. The `try_` prefix makes fallibility
/// explicit at the call site.
///
/// # Associated Types
///
/// - `Error`: The error type returned when validation fails
/// - `Args`: The arguments required for construction (can be a tuple)
///
/// # Implementation Guidelines
///
/// 1. **Document invariants**: Explain what validation is performed
/// 2. **Use specific errors**: Return meaningful error types, not `String`
/// 3. **Keep validation pure**: Don't perform side effects in `try_new`
/// 4. **Consider `Args` type**: Use tuples for multiple arguments
///
/// # Example: Single Argument
///
/// ```
/// use orcs_types::TryNew;
///
/// struct PositiveInt(i32);
///
/// #[derive(Debug)]
/// struct NotPositiveError;
///
/// impl TryNew for PositiveInt {
///     type Error = NotPositiveError;
///     type Args = i32;
///
///     fn try_new(value: i32) -> Result<Self, Self::Error> {
///         if value <= 0 {
///             return Err(NotPositiveError);
///         }
///         Ok(PositiveInt(value))
///     }
/// }
/// ```
///
/// # Example: Multiple Arguments (Tuple)
///
/// ```
/// use orcs_types::TryNew;
///
/// struct Range {
///     start: u32,
///     end: u32,
/// }
///
/// #[derive(Debug)]
/// struct InvalidRangeError;
///
/// impl TryNew for Range {
///     type Error = InvalidRangeError;
///     type Args = (u32, u32);
///
///     fn try_new((start, end): (u32, u32)) -> Result<Self, Self::Error> {
///         if start >= end {
///             return Err(InvalidRangeError);
///         }
///         Ok(Range { start, end })
///     }
/// }
///
/// // Usage
/// let range = Range::try_new((1, 10));
/// assert!(range.is_ok());
/// ```
///
/// # Example: Config Struct Argument
///
/// ```
/// use orcs_types::TryNew;
///
/// struct Server {
///     host: String,
///     port: u16,
/// }
///
/// struct ServerConfig {
///     host: String,
///     port: u16,
/// }
///
/// #[derive(Debug)]
/// enum ServerError {
///     EmptyHost,
///     InvalidPort,
/// }
///
/// impl TryNew for Server {
///     type Error = ServerError;
///     type Args = ServerConfig;
///
///     fn try_new(config: ServerConfig) -> Result<Self, Self::Error> {
///         if config.host.is_empty() {
///             return Err(ServerError::EmptyHost);
///         }
///         if config.port == 0 {
///             return Err(ServerError::InvalidPort);
///         }
///         Ok(Server {
///             host: config.host,
///             port: config.port,
///         })
///     }
/// }
/// ```
pub trait TryNew {
    /// The error type returned when construction fails.
    ///
    /// Should be a specific error type that describes why validation failed.
    /// Avoid using `String` or generic error types.
    type Error;

    /// Arguments required for construction.
    ///
    /// Can be:
    /// - A single value: `type Args = String;`
    /// - A tuple: `type Args = (String, u32);`
    /// - A config struct: `type Args = MyConfig;`
    /// - Unit for no args: `type Args = ();` (rare, consider `Default`)
    type Args;

    /// Attempts to create a new instance.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if validation fails. The error should
    /// contain enough information to understand why construction failed.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::TryNew;
    ///
    /// struct Bounded(u8);
    ///
    /// #[derive(Debug)]
    /// struct OutOfBounds;
    ///
    /// impl TryNew for Bounded {
    ///     type Error = OutOfBounds;
    ///     type Args = u8;
    ///
    ///     fn try_new(value: u8) -> Result<Self, Self::Error> {
    ///         if value > 100 {
    ///             return Err(OutOfBounds);
    ///         }
    ///         Ok(Bounded(value))
    ///     }
    /// }
    ///
    /// assert!(Bounded::try_new(50).is_ok());
    /// assert!(Bounded::try_new(150).is_err());
    /// ```
    fn try_new(args: Self::Args) -> Result<Self, Self::Error>
    where
        Self: Sized;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test: Single argument
    #[derive(Debug)]
    struct NonEmptyString(String);

    #[derive(Debug, PartialEq)]
    struct EmptyStringError;

    impl TryNew for NonEmptyString {
        type Error = EmptyStringError;
        type Args = String;

        fn try_new(value: String) -> Result<Self, Self::Error> {
            if value.is_empty() {
                return Err(EmptyStringError);
            }
            Ok(NonEmptyString(value))
        }
    }

    #[test]
    fn try_new_single_arg_valid() {
        let result = NonEmptyString::try_new("hello".to_string());
        assert!(result.is_ok());
        assert_eq!(result.unwrap().0, "hello");
    }

    #[test]
    fn try_new_single_arg_invalid() {
        let result = NonEmptyString::try_new(String::new());
        assert_eq!(result.unwrap_err(), EmptyStringError);
    }

    // Test: Tuple arguments
    #[derive(Debug)]
    struct Range {
        start: u32,
        end: u32,
    }

    #[derive(Debug, PartialEq)]
    struct InvalidRangeError;

    impl TryNew for Range {
        type Error = InvalidRangeError;
        type Args = (u32, u32);

        fn try_new((start, end): (u32, u32)) -> Result<Self, Self::Error> {
            if start >= end {
                return Err(InvalidRangeError);
            }
            Ok(Range { start, end })
        }
    }

    #[test]
    fn try_new_tuple_args_valid() {
        let result = Range::try_new((1, 10));
        assert!(result.is_ok());
        let range =
            result.expect("Range::try_new((1, 10)) should succeed for valid ascending range");
        assert_eq!(range.start, 1);
        assert_eq!(range.end, 10);
    }

    #[test]
    fn try_new_tuple_args_invalid_same() {
        let result = Range::try_new((5, 5));
        assert_eq!(result.unwrap_err(), InvalidRangeError);
    }

    #[test]
    fn try_new_tuple_args_invalid_reversed() {
        let result = Range::try_new((10, 1));
        assert_eq!(result.unwrap_err(), InvalidRangeError);
    }

    // Test: Config struct argument
    struct Config {
        name: String,
        max_retries: u32,
    }

    #[derive(Debug)]
    #[allow(dead_code)]
    struct Service {
        name: String,
        max_retries: u32,
    }

    #[derive(Debug, PartialEq)]
    enum ServiceError {
        EmptyName,
        ZeroRetries,
    }

    impl TryNew for Service {
        type Error = ServiceError;
        type Args = Config;

        fn try_new(config: Config) -> Result<Self, Self::Error> {
            if config.name.is_empty() {
                return Err(ServiceError::EmptyName);
            }
            if config.max_retries == 0 {
                return Err(ServiceError::ZeroRetries);
            }
            Ok(Service {
                name: config.name,
                max_retries: config.max_retries,
            })
        }
    }

    #[test]
    fn try_new_config_struct_valid() {
        let config = Config {
            name: "test-service".to_string(),
            max_retries: 3,
        };
        let result = Service::try_new(config);
        assert!(result.is_ok());
    }

    #[test]
    fn try_new_config_struct_empty_name() {
        let config = Config {
            name: String::new(),
            max_retries: 3,
        };
        let result = Service::try_new(config);
        assert_eq!(result.unwrap_err(), ServiceError::EmptyName);
    }

    #[test]
    fn try_new_config_struct_zero_retries() {
        let config = Config {
            name: "test".to_string(),
            max_retries: 0,
        };
        let result = Service::try_new(config);
        assert_eq!(result.unwrap_err(), ServiceError::ZeroRetries);
    }

    // Test: Boundary values
    #[derive(Debug)]
    #[allow(dead_code)]
    struct BoundedValue(u8);

    #[derive(Debug, PartialEq)]
    struct OutOfBoundsError {
        value: u8,
        max: u8,
    }

    impl TryNew for BoundedValue {
        type Error = OutOfBoundsError;
        type Args = u8;

        fn try_new(value: u8) -> Result<Self, Self::Error> {
            const MAX: u8 = 100;
            if value > MAX {
                return Err(OutOfBoundsError { value, max: MAX });
            }
            Ok(BoundedValue(value))
        }
    }

    #[test]
    fn try_new_boundary_at_max() {
        let result = BoundedValue::try_new(100);
        assert!(result.is_ok());
    }

    #[test]
    fn try_new_boundary_over_max() {
        let result = BoundedValue::try_new(101);
        let err = result.unwrap_err();
        assert_eq!(err.value, 101);
        assert_eq!(err.max, 100);
    }

    #[test]
    fn try_new_boundary_zero() {
        let result = BoundedValue::try_new(0);
        assert!(result.is_ok());
    }
}

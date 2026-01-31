//! Application-level error type.
//!
//! [`AppError`] unifies all internal errors for the application layer.

use orcs_runtime::{ChannelError, EngineError};
use orcs_types::ErrorCode;
use thiserror::Error;

/// Unified application error.
///
/// Collects all internal errors into a single type for CLI/GUI handling.
///
/// # Example
///
/// ```
/// use orcs_app::{AppError, ChannelError};
/// use orcs_types::ChannelId;
///
/// // Internal error automatically converts to AppError
/// let channel_err = ChannelError::NotFound(ChannelId::new());
/// let app_err: AppError = channel_err.into();
///
/// // CLI can use Display for user-friendly message
/// eprintln!("Error: {}", app_err);
/// ```
#[derive(Debug, Error)]
pub enum AppError {
    /// Channel operation failed
    #[error("Channel error: {0}")]
    Channel(#[from] ChannelError),

    /// Engine operation failed
    #[error("Engine error: {0}")]
    Engine(#[from] EngineError),

    /// Configuration error
    #[error("Config error: {0}")]
    Config(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl ErrorCode for AppError {
    fn code(&self) -> &'static str {
        match self {
            Self::Channel(e) => e.code(),
            Self::Engine(e) => e.code(),
            Self::Config(_) => "APP_CONFIG_ERROR",
            Self::Io(_) => "APP_IO_ERROR",
        }
    }

    fn is_recoverable(&self) -> bool {
        match self {
            Self::Channel(e) => e.is_recoverable(),
            Self::Engine(e) => e.is_recoverable(),
            Self::Config(_) => false,
            Self::Io(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::ChannelId;

    #[test]
    fn channel_error_converts() {
        let err = ChannelError::NotFound(ChannelId::new());
        let app_err: AppError = err.into();
        assert!(matches!(app_err, AppError::Channel(_)));
    }

    #[test]
    fn error_codes() {
        let err = AppError::Config("test".into());
        assert_eq!(err.code(), "APP_CONFIG_ERROR");
        assert!(!err.is_recoverable());
    }
}

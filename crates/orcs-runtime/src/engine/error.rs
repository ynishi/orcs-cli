//! Engine Layer Errors.
//!
//! This module defines errors for the Engine layer.
//! All errors implement [`ErrorCode`] for standardized handling.
//!
//! # Error Codes
//!
//! | Variant | Code | Recoverable |
//! |---------|------|-------------|
//! | [`EngineError::ComponentNotFound`] | `ENGINE_COMPONENT_NOT_FOUND` | No |
//! | [`EngineError::NoTarget`] | `ENGINE_NO_TARGET` | No |
//! | [`EngineError::SendFailed`] | `ENGINE_SEND_FAILED` | Yes |
//! | [`EngineError::ChannelClosed`] | `ENGINE_CHANNEL_CLOSED` | No |
//! | [`EngineError::NotRunning`] | `ENGINE_NOT_RUNNING` | No |
//! | [`EngineError::Timeout`] | `ENGINE_TIMEOUT` | Yes |
//! | [`EngineError::ComponentFailed`] | `ENGINE_COMPONENT_FAILED` | No |
//! | [`EngineError::NoSubscriber`] | `ENGINE_NO_SUBSCRIBER` | No |
//!
//! # Recoverability
//!
//! Recoverable errors may succeed on retry:
//! - `SendFailed`: Transient channel issue
//! - `Timeout`: Slow response, may complete later
//!
//! Non-recoverable errors require code/config changes.

use orcs_event::EventCategory;
use orcs_types::{ComponentId, ErrorCode, RequestId};
use thiserror::Error;

/// Engine layer error.
///
/// Represents failures within the Engine runtime and EventBus.
/// Implements [`ErrorCode`] for standardized error handling.
///
/// # Example
///
/// ```
/// use orcs_runtime::EngineError;
/// use orcs_types::ErrorCode;
///
/// let err = EngineError::NoTarget;
/// assert_eq!(err.code(), "ENGINE_NO_TARGET");
/// assert!(!err.is_recoverable());
/// ```
#[derive(Debug, Clone, Error)]
pub enum EngineError {
    /// Component not found in EventBus registry.
    #[error("component not found: {0}")]
    ComponentNotFound(ComponentId),

    /// Request target is required but not specified.
    #[error("request target is required")]
    NoTarget,

    /// Failed to send message to component.
    #[error("send failed: {0}")]
    SendFailed(String),

    /// Component channel was closed unexpectedly.
    #[error("channel closed")]
    ChannelClosed,

    /// Engine is not running.
    #[error("engine not running")]
    NotRunning,

    /// Request timed out waiting for response.
    #[error("request timed out: {0}")]
    Timeout(RequestId),

    /// Component returned an error.
    #[error("component error: {0}")]
    ComponentFailed(String),

    /// No subscriber for the requested category.
    #[error("no subscriber for category: {0}")]
    NoSubscriber(EventCategory),
}

impl ErrorCode for EngineError {
    fn code(&self) -> &'static str {
        match self {
            Self::ComponentNotFound(_) => "ENGINE_COMPONENT_NOT_FOUND",
            Self::NoTarget => "ENGINE_NO_TARGET",
            Self::SendFailed(_) => "ENGINE_SEND_FAILED",
            Self::ChannelClosed => "ENGINE_CHANNEL_CLOSED",
            Self::NotRunning => "ENGINE_NOT_RUNNING",
            Self::Timeout(_) => "ENGINE_TIMEOUT",
            Self::ComponentFailed(_) => "ENGINE_COMPONENT_FAILED",
            Self::NoSubscriber(_) => "ENGINE_NO_SUBSCRIBER",
        }
    }

    fn is_recoverable(&self) -> bool {
        matches!(self, Self::SendFailed(_) | Self::Timeout(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::assert_error_codes;

    fn all_variants() -> Vec<EngineError> {
        vec![
            EngineError::ComponentNotFound(ComponentId::builtin("x")),
            EngineError::NoTarget,
            EngineError::SendFailed("x".into()),
            EngineError::ChannelClosed,
            EngineError::NotRunning,
            EngineError::Timeout(RequestId::new()),
            EngineError::ComponentFailed("x".into()),
            EngineError::NoSubscriber(EventCategory::Echo),
        ]
    }

    #[test]
    fn all_error_codes_valid() {
        assert_error_codes(&all_variants(), "ENGINE_");
    }

    #[test]
    fn engine_error_codes() {
        let err = EngineError::NoTarget;
        assert_eq!(err.code(), "ENGINE_NO_TARGET");

        let err = EngineError::ComponentNotFound(ComponentId::builtin("x"));
        assert_eq!(err.code(), "ENGINE_COMPONENT_NOT_FOUND");

        let err = EngineError::Timeout(RequestId::new());
        assert_eq!(err.code(), "ENGINE_TIMEOUT");

        let err = EngineError::ComponentFailed("test".into());
        assert_eq!(err.code(), "ENGINE_COMPONENT_FAILED");
    }

    #[test]
    fn engine_error_recoverable() {
        assert!(!EngineError::NoTarget.is_recoverable());
        assert!(EngineError::SendFailed("x".into()).is_recoverable());
        assert!(EngineError::Timeout(RequestId::new()).is_recoverable());
        assert!(!EngineError::ComponentFailed("x".into()).is_recoverable());
    }
}

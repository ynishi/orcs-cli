//! Error types for Lua component operations.

use thiserror::Error;

/// Errors that can occur in Lua component operations.
#[derive(Debug, Error)]
pub enum LuaError {
    /// Lua runtime error.
    #[error("lua error: {0}")]
    Runtime(#[from] mlua::Error),

    /// Script file not found.
    #[error("script not found: {0}")]
    ScriptNotFound(String),

    /// Invalid script format.
    #[error("invalid script: {0}")]
    InvalidScript(String),

    /// Missing required callback.
    #[error("missing callback: {0}")]
    MissingCallback(String),

    /// Type conversion error.
    #[error("type error: {0}")]
    TypeError(String),

    /// Component initialization failed.
    #[error("init failed: {0}")]
    InitFailed(String),
}

impl From<LuaError> for orcs_component::ComponentError {
    fn from(err: LuaError) -> Self {
        orcs_component::ComponentError::ExecutionFailed(err.to_string())
    }
}

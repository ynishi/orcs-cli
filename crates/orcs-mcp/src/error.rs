//! MCP client error types.

use thiserror::Error;

/// Errors from MCP client operations.
#[derive(Debug, Error)]
pub enum McpError {
    /// Server not found in configuration.
    #[error("MCP server not found: {name}")]
    ServerNotFound { name: String },

    /// Server already connected.
    #[error("MCP server already connected: {name}")]
    AlreadyConnected { name: String },

    /// Server not connected.
    #[error("MCP server not connected: {name}")]
    NotConnected { name: String },

    /// Failed to spawn server process.
    #[error("failed to spawn MCP server '{name}': {source}")]
    SpawnFailed {
        name: String,
        source: std::io::Error,
    },

    /// MCP protocol error (initialization, RPC, etc.).
    #[error("MCP protocol error for '{server}': {message}")]
    Protocol { server: String, message: String },

    /// Tool not found on any connected server.
    #[error("MCP tool not found: {tool}")]
    ToolNotFound { tool: String },

    /// Tool invocation returned an error.
    #[error("MCP tool '{tool}' on '{server}' returned error: {message}")]
    ToolError {
        server: String,
        tool: String,
        message: String,
    },
}

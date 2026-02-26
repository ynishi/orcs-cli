//! MCP (Model Context Protocol) client integration for ORCS.
//!
//! Provides connection management, tool discovery, and tool invocation
//! for MCP servers, bridging them into the ORCS IntentRegistry.
//!
//! # Architecture
//!
//! ```text
//! config.toml [mcp.servers.<name>]
//!   → McpServerConfig (command, args, env)
//!   → McpClientManager.connect(name)
//!   → rmcp::RunningService (stdio child process)
//!   → tools/list → IntentDef (IntentResolver::Mcp)
//!   → tools/call → IntentResult
//! ```

mod config;
mod error;
mod manager;

pub use config::{McpConfig, McpServerConfig};
pub use error::McpError;
pub use manager::McpClientManager;

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
pub use rmcp::model::CallToolResult;

/// Extracts text from MCP Content items.
///
/// Concatenates all `Text` variant content. Non-text content
/// (images, resources, audio) is represented as `[<type>]` placeholder.
pub fn content_to_text(content: &[rmcp::model::Content]) -> String {
    use rmcp::model::RawContent;
    use std::ops::Deref;

    let mut parts: Vec<String> = Vec::with_capacity(content.len());
    for item in content {
        match item.deref() {
            RawContent::Text(text) => parts.push(text.text.clone()),
            RawContent::Image(_) => parts.push("[image]".into()),
            RawContent::Resource(res) => match &res.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    if text.is_empty() {
                        parts.push("[resource]".into());
                    } else {
                        parts.push(text.clone());
                    }
                }
                _ => parts.push("[resource]".into()),
            },
            RawContent::Audio(_) => parts.push("[audio]".into()),
            RawContent::ResourceLink(link) => {
                parts.push(format!("[resource_link: {}]", link.uri));
            }
        }
    }
    parts.join("\n")
}

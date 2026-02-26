//! MCP client connection manager.
//!
//! Manages lifecycle of MCP server connections:
//! - Connect to configured servers (stdio child processes)
//! - Discover tools via `tools/list`
//! - Invoke tools via `tools/call`
//! - Track which tools belong to which server
//! - Graceful shutdown

use crate::config::{McpConfig, McpServerConfig};
use crate::error::McpError;
use orcs_types::intent::IntentDef;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use std::collections::HashMap;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// A connected MCP server with its discovered tools.
struct ConnectedServer {
    /// The rmcp client service handle.
    service: RunningService<RoleClient, ()>,
}

/// Maps a tool name to the server that provides it.
struct ToolRoute {
    /// Server name (key in `McpConfig.servers`).
    server_name: String,
    /// IntentDef generated from the MCP tool definition.
    intent_def: IntentDef,
}

/// Manages connections to multiple MCP servers.
///
/// Thread-safe via interior `RwLock`. Designed to be shared as
/// `Arc<McpClientManager>` across the runtime.
pub struct McpClientManager {
    config: McpConfig,
    servers: RwLock<HashMap<String, ConnectedServer>>,
    tool_routes: RwLock<HashMap<String, ToolRoute>>,
}

impl McpClientManager {
    /// Create a new manager from MCP configuration.
    pub fn new(config: McpConfig) -> Self {
        Self {
            config,
            servers: RwLock::new(HashMap::new()),
            tool_routes: RwLock::new(HashMap::new()),
        }
    }

    /// Connect to a single configured MCP server by name.
    ///
    /// Spawns the server process, completes the MCP initialization
    /// handshake, discovers tools, and registers them.
    pub async fn connect(&self, name: &str) -> Result<Vec<IntentDef>, McpError> {
        let server_config =
            self.config
                .servers
                .get(name)
                .ok_or_else(|| McpError::ServerNotFound {
                    name: name.to_string(),
                })?;

        // Check not already connected
        {
            let servers = self.servers.read().await;
            if servers.contains_key(name) {
                return Err(McpError::AlreadyConnected {
                    name: name.to_string(),
                });
            }
        }

        info!(server = name, command = %server_config.command, "connecting to MCP server");

        let service = spawn_and_initialize(name, server_config).await?;

        // Discover tools
        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Protocol {
                server: name.to_string(),
                message: format!("tools/list failed: {e}"),
            })?;

        info!(
            server = name,
            tool_count = tools.len(),
            "discovered MCP tools"
        );

        // Convert to IntentDefs and build routes
        let mut intent_defs = Vec::with_capacity(tools.len());
        let mut new_routes = Vec::new();

        for tool in &tools {
            let tool_name = tool.name.to_string();
            // Namespace MCP tools: "mcp:<server>:<tool>"
            let namespaced = format!("mcp:{name}:{tool_name}");

            let intent_def = mcp_tool_to_intent_def(&namespaced, tool, name);

            debug!(
                server = name,
                tool = %tool_name,
                namespaced = %namespaced,
                "registered MCP tool"
            );

            new_routes.push((
                namespaced.clone(),
                ToolRoute {
                    server_name: name.to_string(),
                    intent_def: intent_def.clone(),
                },
            ));
            intent_defs.push(intent_def);
        }

        // Store connection and routes
        {
            let mut servers = self.servers.write().await;
            servers.insert(name.to_string(), ConnectedServer { service });
        }
        {
            let mut routes = self.tool_routes.write().await;
            for (key, route) in new_routes {
                routes.insert(key, route);
            }
        }

        Ok(intent_defs)
    }

    /// Connect to all configured servers.
    ///
    /// Returns all discovered IntentDefs. Servers that fail to connect
    /// are logged as warnings but do not prevent other servers from connecting.
    pub async fn connect_all(&self) -> Vec<IntentDef> {
        let server_names: Vec<String> = self.config.servers.keys().cloned().collect();
        let mut all_defs = Vec::new();

        for name in &server_names {
            match self.connect(name).await {
                Ok(defs) => all_defs.extend(defs),
                Err(e) => {
                    warn!(server = %name, error = %e, "failed to connect MCP server, skipping");
                }
            }
        }

        all_defs
    }

    /// Call an MCP tool by its namespaced name (`mcp:<server>:<tool>`).
    pub async fn call_tool(
        &self,
        namespaced_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        // Look up route
        let (server_name, original_tool_name) = {
            let routes = self.tool_routes.read().await;
            let route = routes
                .get(namespaced_name)
                .ok_or_else(|| McpError::ToolNotFound {
                    tool: namespaced_name.to_string(),
                })?;
            let original = extract_original_tool_name(namespaced_name);
            (route.server_name.clone(), original)
        };

        // Get service handle
        let servers = self.servers.read().await;
        let connected = servers
            .get(&server_name)
            .ok_or_else(|| McpError::NotConnected {
                name: server_name.clone(),
            })?;

        let args_map = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            _ => Some(serde_json::Map::new()),
        };

        let result = connected
            .service
            .call_tool(CallToolRequestParams {
                meta: None,
                name: original_tool_name.into(),
                arguments: args_map,
                task: None,
            })
            .await
            .map_err(|e| McpError::Protocol {
                server: server_name.clone(),
                message: format!("tools/call failed: {e}"),
            })?;

        Ok(result)
    }

    /// List all discovered IntentDefs from connected MCP servers.
    pub async fn intent_defs(&self) -> Vec<IntentDef> {
        let routes = self.tool_routes.read().await;
        routes.values().map(|r| r.intent_def.clone()).collect()
    }

    /// List connected server names.
    pub async fn connected_servers(&self) -> Vec<String> {
        let servers = self.servers.read().await;
        servers.keys().cloned().collect()
    }

    /// Disconnect a single server by name.
    pub async fn disconnect(&self, name: &str) -> Result<(), McpError> {
        // Remove routes for this server
        {
            let mut routes = self.tool_routes.write().await;
            routes.retain(|_, route| route.server_name != name);
        }

        // Remove and shutdown server
        let connected = {
            let mut servers = self.servers.write().await;
            servers.remove(name)
        };

        match connected {
            Some(server) => {
                if let Err(e) = server.service.cancel().await {
                    warn!(
                        server = name,
                        error = %e,
                        "error during MCP server shutdown"
                    );
                }
                info!(server = name, "disconnected MCP server");
                Ok(())
            }
            None => Err(McpError::NotConnected {
                name: name.to_string(),
            }),
        }
    }

    /// Disconnect all connected servers.
    pub async fn disconnect_all(&self) {
        let names: Vec<String> = {
            let servers = self.servers.read().await;
            servers.keys().cloned().collect()
        };

        for name in &names {
            if let Err(e) = self.disconnect(name).await {
                warn!(server = %name, error = %e, "error disconnecting MCP server");
            }
        }
    }

    /// Returns the underlying configuration.
    pub fn config(&self) -> &McpConfig {
        &self.config
    }
}

/// Spawn an MCP server as a child process and complete initialization.
async fn spawn_and_initialize(
    name: &str,
    config: &McpServerConfig,
) -> Result<RunningService<RoleClient, ()>, McpError> {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args);
    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::SpawnFailed {
        name: name.to_string(),
        source: e,
    })?;

    let service = ().serve(transport).await.map_err(|e| McpError::Protocol {
        server: name.to_string(),
        message: format!("initialization failed: {e}"),
    })?;

    Ok(service)
}

/// Convert an rmcp Tool to an ORCS IntentDef.
fn mcp_tool_to_intent_def(
    namespaced_name: &str,
    tool: &rmcp::model::Tool,
    server_name: &str,
) -> IntentDef {
    let description = tool
        .description
        .as_deref()
        .unwrap_or("(no description)")
        .to_string();

    // Convert Arc<JsonObject> → serde_json::Value
    let parameters = serde_json::Value::Object((*tool.input_schema).clone());

    IntentDef {
        name: namespaced_name.to_string(),
        description: format!("[MCP:{server_name}] {description}"),
        parameters,
        resolver: orcs_types::intent::IntentResolver::Mcp {
            server_name: server_name.to_string(),
            tool_name: tool.name.to_string(),
        },
    }
}

/// Extract original tool name from namespaced name "mcp:<server>:<tool>".
fn extract_original_tool_name(namespaced: &str) -> String {
    // "mcp:server:tool_name" → "tool_name"
    namespaced
        .strip_prefix("mcp:")
        .and_then(|rest| rest.find(':').map(|i| &rest[i + 1..]))
        .unwrap_or(namespaced)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tool_name_valid() {
        assert_eq!(
            extract_original_tool_name("mcp:filesystem:read_file"),
            "read_file"
        );
        assert_eq!(
            extract_original_tool_name("mcp:my-server:complex.tool.name"),
            "complex.tool.name"
        );
    }

    #[test]
    fn extract_tool_name_no_prefix() {
        assert_eq!(extract_original_tool_name("no_prefix"), "no_prefix");
    }

    #[test]
    fn extract_tool_name_single_colon() {
        // "mcp:nocolon" → only one colon after mcp:, so find(':') on "nocolon" returns None
        assert_eq!(extract_original_tool_name("mcp:nocolon"), "mcp:nocolon");
    }

    #[test]
    fn mcp_tool_to_intent_def_basic() {
        let tool = rmcp::model::Tool {
            name: "read_file".into(),
            title: None,
            description: Some("Read a file from disk".into()),
            input_schema: std::sync::Arc::new(serde_json::Map::from_iter([
                ("type".to_string(), serde_json::json!("object")),
                (
                    "properties".to_string(),
                    serde_json::json!({
                        "path": { "type": "string" }
                    }),
                ),
            ])),
            output_schema: None,
            annotations: None,
            execution: None,
            icons: None,
            meta: None,
        };

        let def = super::mcp_tool_to_intent_def("mcp:fs:read_file", &tool, "fs");

        assert_eq!(def.name, "mcp:fs:read_file");
        assert!(def.description.contains("Read a file from disk"));
        assert!(def.description.starts_with("[MCP:fs]"));
        assert_eq!(def.parameters["type"], "object");
        match &def.resolver {
            orcs_types::intent::IntentResolver::Mcp {
                server_name,
                tool_name,
            } => {
                assert_eq!(server_name, "fs");
                assert_eq!(tool_name, "read_file");
            }
            other => panic!("expected Mcp resolver, got: {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_to_intent_def_no_description() {
        let tool = rmcp::model::Tool {
            name: "no_desc".into(),
            title: None,
            description: None,
            input_schema: std::sync::Arc::new(serde_json::Map::new()),
            output_schema: None,
            annotations: None,
            execution: None,
            icons: None,
            meta: None,
        };

        let def = super::mcp_tool_to_intent_def("mcp:srv:no_desc", &tool, "srv");
        assert!(def.description.contains("(no description)"));
    }

    #[test]
    fn new_manager_empty_config() {
        let manager = McpClientManager::new(McpConfig::default());
        assert!(manager.config().is_empty());
    }

    #[tokio::test]
    async fn connect_nonexistent_server_returns_error() {
        let manager = McpClientManager::new(McpConfig::default());
        let result = manager.connect("nonexistent").await;
        assert!(result.is_err());
        match result {
            Err(McpError::ServerNotFound { name }) => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("expected ServerNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn intent_defs_empty_when_no_connections() {
        let manager = McpClientManager::new(McpConfig::default());
        let defs = manager.intent_defs().await;
        assert!(defs.is_empty());
    }

    #[tokio::test]
    async fn connected_servers_empty_initially() {
        let manager = McpClientManager::new(McpConfig::default());
        let servers = manager.connected_servers().await;
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn disconnect_nonexistent_returns_error() {
        let manager = McpClientManager::new(McpConfig::default());
        let result = manager.disconnect("nonexistent").await;
        assert!(result.is_err());
        match result {
            Err(McpError::NotConnected { name }) => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("expected NotConnected, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_tool_not_connected_returns_error() {
        let manager = McpClientManager::new(McpConfig::default());
        let result = manager
            .call_tool("mcp:srv:tool", serde_json::json!({}))
            .await;
        assert!(result.is_err());
        match result {
            Err(McpError::ToolNotFound { tool }) => {
                assert_eq!(tool, "mcp:srv:tool");
            }
            other => panic!("expected ToolNotFound, got: {other:?}"),
        }
    }
}

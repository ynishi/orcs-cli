//! MCP server configuration types.
//!
//! Deserialized from `config.toml`:
//!
//! ```toml
//! [mcp.servers.filesystem]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
//!
//! [mcp.servers.custom]
//! command = "/usr/local/bin/my-mcp-server"
//! args = ["--stdio"]
//! env = { API_KEY = "from-env" }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level MCP configuration section.
///
/// Lives under `[mcp]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpConfig {
    /// Named MCP server definitions.
    pub servers: HashMap<String, McpServerConfig>,
}

impl McpConfig {
    /// Merge another MCP config into this one.
    ///
    /// Servers from `other` overwrite servers in `self` with the same name.
    pub fn merge(&mut self, other: &Self) {
        for (name, config) in &other.servers {
            self.servers.insert(name.clone(), config.clone());
        }
    }

    /// Returns true if no servers are configured.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// Configuration for a single MCP server.
///
/// Follows the same schema as Claude Desktop's `mcpServers` entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// Command to execute (e.g., "npx", "/usr/local/bin/server").
    pub command: String,

    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_default_is_empty() {
        let config = McpConfig::default();
        assert!(config.is_empty());
        assert!(config.servers.is_empty());
    }

    #[test]
    fn mcp_server_config_deserialize() {
        let toml = r#"
[servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[servers.custom]
command = "/usr/local/bin/my-mcp-server"
args = ["--stdio"]
"#;
        let config: McpConfig = toml::from_str(toml).expect("should parse MCP config");
        assert_eq!(config.servers.len(), 2);

        let fs = config
            .servers
            .get("filesystem")
            .expect("should have filesystem server");
        assert_eq!(fs.command, "npx");
        assert_eq!(
            fs.args,
            vec!["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        );
        assert!(fs.env.is_empty());

        let custom = config
            .servers
            .get("custom")
            .expect("should have custom server");
        assert_eq!(custom.command, "/usr/local/bin/my-mcp-server");
        assert_eq!(custom.args, vec!["--stdio"]);
    }

    #[test]
    fn mcp_server_config_with_env() {
        let toml = r#"
[servers.api]
command = "mcp-api-server"
args = []
env = { API_KEY = "test-key", DEBUG = "1" }
"#;
        let config: McpConfig = toml::from_str(toml).expect("should parse");
        let api = config.servers.get("api").expect("should have api server");
        assert_eq!(api.env.get("API_KEY"), Some(&"test-key".to_string()));
        assert_eq!(api.env.get("DEBUG"), Some(&"1".to_string()));
    }

    #[test]
    fn mcp_config_merge() {
        let mut base = McpConfig::default();
        base.servers.insert(
            "server-a".into(),
            McpServerConfig {
                command: "old-cmd".into(),
                args: vec![],
                env: HashMap::new(),
            },
        );
        base.servers.insert(
            "server-b".into(),
            McpServerConfig {
                command: "keep-cmd".into(),
                args: vec![],
                env: HashMap::new(),
            },
        );

        let mut overlay = McpConfig::default();
        overlay.servers.insert(
            "server-a".into(),
            McpServerConfig {
                command: "new-cmd".into(),
                args: vec!["--flag".into()],
                env: HashMap::new(),
            },
        );
        overlay.servers.insert(
            "server-c".into(),
            McpServerConfig {
                command: "added-cmd".into(),
                args: vec![],
                env: HashMap::new(),
            },
        );

        base.merge(&overlay);

        assert_eq!(base.servers.len(), 3);
        assert_eq!(
            base.servers
                .get("server-a")
                .expect("server-a should exist")
                .command,
            "new-cmd"
        );
        assert_eq!(
            base.servers
                .get("server-b")
                .expect("server-b should exist")
                .command,
            "keep-cmd"
        );
        assert!(base.servers.contains_key("server-c"));
    }

    #[test]
    fn mcp_config_toml_roundtrip() {
        let mut config = McpConfig::default();
        config.servers.insert(
            "test".into(),
            McpServerConfig {
                command: "test-cmd".into(),
                args: vec!["--arg".into()],
                env: HashMap::new(),
            },
        );

        let toml_str = toml::to_string_pretty(&config).expect("should serialize");
        let restored: McpConfig = toml::from_str(&toml_str).expect("should deserialize");
        assert_eq!(config, restored);
    }
}

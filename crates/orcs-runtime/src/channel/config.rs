//! Channel configuration for behavior customization.
//!
//! [`ChannelConfig`] defines the behavioral attributes of a Channel,
//! such as scheduling priority and spawn permissions.
//!
//! # Design
//!
//! Instead of using separate types (PrimaryChannel, BackgroundChannel),
//! we use configuration to express behavioral variations. This allows:
//!
//! - Simple struct-based design
//! - Easy extension via new config fields
//! - Future migration to trait-based design if needed
//!
//! # Priority Levels
//!
//! ```text
//! 255 ─ Primary (Human direct)
//! 100 ─ Tool (spawned for execution)
//!  50 ─ Normal (default)
//!  10 ─ Background (parallel tasks)
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_runtime::ChannelConfig;
//!
//! // Use predefined configs
//! let primary = ChannelConfig::interactive();
//! assert_eq!(primary.priority(), 255);
//!
//! // Or create custom config
//! let custom = ChannelConfig::new(100, true);
//! assert_eq!(custom.priority(), 100);
//! ```

use serde::{Deserialize, Serialize};

/// Priority constants for common channel types.
pub mod priority {
    /// Human direct interaction (highest priority).
    pub const PRIMARY: u8 = 255;
    /// Tool execution spawned from parent.
    pub const TOOL: u8 = 100;
    /// Normal operations (default).
    pub const NORMAL: u8 = 50;
    /// Background/parallel tasks (lowest priority).
    pub const BACKGROUND: u8 = 10;
}

/// Configuration for a [`Channel`](super::Channel).
///
/// Defines behavioral attributes without requiring separate types.
/// This approach allows flexible channel behavior while maintaining
/// a simple, unified Channel struct.
///
/// # Example
///
/// ```
/// use orcs_runtime::ChannelConfig;
///
/// let config = ChannelConfig::interactive();
/// assert!(config.can_spawn());
/// assert_eq!(config.priority(), 255);
///
/// let bg = ChannelConfig::background();
/// assert!(!bg.can_spawn());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Scheduling priority (0-255, higher = more priority).
    priority: u8,
    /// Whether this channel can spawn child channels.
    can_spawn: bool,
}

impl ChannelConfig {
    /// Creates a new configuration with specified attributes.
    ///
    /// # Arguments
    ///
    /// * `priority` - Scheduling priority (0-255)
    /// * `can_spawn` - Whether child channels can be spawned
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::ChannelConfig;
    ///
    /// let config = ChannelConfig::new(100, true);
    /// assert_eq!(config.priority(), 100);
    /// assert!(config.can_spawn());
    /// ```
    #[must_use]
    pub const fn new(priority: u8, can_spawn: bool) -> Self {
        Self {
            priority,
            can_spawn,
        }
    }

    /// Configuration for interactive channels.
    ///
    /// Interactive channels:
    /// - Have highest priority (255)
    /// - Can spawn child channels
    /// - Used for Human direct interaction (IO)
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::ChannelConfig;
    ///
    /// let config = ChannelConfig::interactive();
    /// assert_eq!(config.priority(), 255);
    /// assert!(config.can_spawn());
    /// ```
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            priority: priority::PRIMARY,
            can_spawn: true,
        }
    }

    /// Configuration for background channels.
    ///
    /// Background channels:
    /// - Have lowest priority (10)
    /// - Cannot spawn child channels
    /// - Used for parallel/async tasks
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::ChannelConfig;
    ///
    /// let config = ChannelConfig::background();
    /// assert_eq!(config.priority(), 10);
    /// assert!(!config.can_spawn());
    /// ```
    #[must_use]
    pub const fn background() -> Self {
        Self {
            priority: priority::BACKGROUND,
            can_spawn: false,
        }
    }

    /// Configuration for tool execution channels.
    ///
    /// Tool channels:
    /// - Have medium-high priority (100)
    /// - Can spawn child channels (for sub-tools)
    /// - Spawned for specific operations
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::ChannelConfig;
    ///
    /// let config = ChannelConfig::tool();
    /// assert_eq!(config.priority(), 100);
    /// assert!(config.can_spawn());
    /// ```
    #[must_use]
    pub const fn tool() -> Self {
        Self {
            priority: priority::TOOL,
            can_spawn: true,
        }
    }

    /// Returns the scheduling priority.
    ///
    /// Higher values indicate higher priority for scheduling.
    #[must_use]
    pub const fn priority(&self) -> u8 {
        self.priority
    }

    /// Returns whether this channel can spawn children.
    #[must_use]
    pub const fn can_spawn(&self) -> bool {
        self.can_spawn
    }

    /// Returns a new config with the specified priority.
    #[must_use]
    pub const fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }

    /// Returns a new config with spawn permission set.
    #[must_use]
    pub const fn with_spawn(mut self, can_spawn: bool) -> Self {
        self.can_spawn = can_spawn;
        self
    }
}

impl Default for ChannelConfig {
    /// Default configuration with normal priority and spawn enabled.
    fn default() -> Self {
        Self {
            priority: priority::NORMAL,
            can_spawn: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_new() {
        let config = ChannelConfig::new(100, true);
        assert_eq!(config.priority(), 100);
        assert!(config.can_spawn());
    }

    #[test]
    fn config_interactive() {
        let config = ChannelConfig::interactive();
        assert_eq!(config.priority(), priority::PRIMARY);
        assert!(config.can_spawn());
    }

    #[test]
    fn config_background() {
        let config = ChannelConfig::background();
        assert_eq!(config.priority(), priority::BACKGROUND);
        assert!(!config.can_spawn());
    }

    #[test]
    fn config_tool() {
        let config = ChannelConfig::tool();
        assert_eq!(config.priority(), priority::TOOL);
        assert!(config.can_spawn());
    }

    #[test]
    fn config_default() {
        let config = ChannelConfig::default();
        assert_eq!(config.priority(), priority::NORMAL);
        assert!(config.can_spawn());
    }

    #[test]
    fn config_with_priority() {
        let config = ChannelConfig::default().with_priority(200);
        assert_eq!(config.priority(), 200);
        assert!(config.can_spawn());
    }

    #[test]
    fn config_with_spawn() {
        let config = ChannelConfig::default().with_spawn(false);
        assert_eq!(config.priority(), priority::NORMAL);
        assert!(!config.can_spawn());
    }

    #[test]
    fn config_equality() {
        assert_eq!(ChannelConfig::interactive(), ChannelConfig::interactive());
        assert_ne!(ChannelConfig::interactive(), ChannelConfig::background());
    }

    #[test]
    fn config_serialize() {
        let config = ChannelConfig::interactive();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("255"));
        assert!(json.contains("true"));
    }

    #[test]
    fn config_deserialize() {
        let json = r#"{"priority":100,"can_spawn":false}"#;
        let config: ChannelConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.priority(), 100);
        assert!(!config.can_spawn());
    }
}

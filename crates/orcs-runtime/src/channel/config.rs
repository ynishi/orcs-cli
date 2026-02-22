//! Channel configuration for behavior customization.
//!
//! [`ChannelConfig`] defines the behavioral attributes of a Channel,
//! such as scheduling priority, spawn permissions, and privilege limits.
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
//! # Privilege Inheritance
//!
//! Child channels inherit `max_privilege` from their parent and cannot exceed it.
//! This ensures privilege reduction propagates down the channel tree.
//!
//! # Example
//!
//! ```
//! use orcs_runtime::{ChannelConfig, MaxPrivilege};
//!
//! // Use predefined configs
//! let primary = ChannelConfig::interactive();
//! assert_eq!(primary.priority(), 255);
//! assert_eq!(primary.max_privilege(), MaxPrivilege::Elevated);
//!
//! // Background channels have restricted privileges
//! let bg = ChannelConfig::background();
//! assert_eq!(bg.max_privilege(), MaxPrivilege::Standard);
//! ```

use serde::{Deserialize, Serialize};

/// Maximum privilege level a channel can operate at.
///
/// This is a static capability bound, not a runtime state.
/// Even if a Session is elevated, a channel with `MaxPrivilege::Standard`
/// cannot perform elevated operations.
///
/// # Inheritance
///
/// When spawning child channels:
/// - Child's `max_privilege` is capped by parent's `max_privilege`
/// - A Standard parent cannot spawn an Elevated child
///
/// # Example
///
/// ```
/// use orcs_runtime::MaxPrivilege;
///
/// let elevated = MaxPrivilege::Elevated;
/// let standard = MaxPrivilege::Standard;
///
/// // Elevated > Standard
/// assert!(elevated > standard);
///
/// // min() for inheritance
/// assert_eq!(elevated.min(standard), standard);
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum MaxPrivilege {
    /// Can only perform standard operations.
    ///
    /// Cannot:
    /// - Send global signals
    /// - Perform destructive file/git operations
    #[default]
    Standard,

    /// Can perform elevated operations (if Session is also elevated).
    ///
    /// This is the maximum capability - actual permission still requires
    /// an elevated Session at runtime.
    Elevated,
}

impl MaxPrivilege {
    /// Returns the minimum of two privilege levels.
    ///
    /// Used for privilege inheritance: child = parent.min(requested).
    #[must_use]
    pub fn min(self, other: Self) -> Self {
        if self <= other {
            self
        } else {
            other
        }
    }

    /// Returns true if this is the Elevated level.
    #[must_use]
    pub fn is_elevated(self) -> bool {
        matches!(self, Self::Elevated)
    }

    /// Returns true if this is the Standard level.
    #[must_use]
    pub fn is_standard(self) -> bool {
        matches!(self, Self::Standard)
    }
}

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
/// # Privilege Inheritance
///
/// When spawning child channels, `max_privilege` is inherited:
/// - Child cannot exceed parent's `max_privilege`
/// - Use [`inherit_from()`](Self::inherit_from) to apply inheritance
///
/// # Example
///
/// ```
/// use orcs_runtime::{ChannelConfig, MaxPrivilege};
///
/// let config = ChannelConfig::interactive();
/// assert!(config.can_spawn());
/// assert_eq!(config.priority(), 255);
/// assert_eq!(config.max_privilege(), MaxPrivilege::Elevated);
///
/// let bg = ChannelConfig::background();
/// assert!(!bg.can_spawn());
/// assert_eq!(bg.max_privilege(), MaxPrivilege::Standard);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Scheduling priority (0-255, higher = more priority).
    priority: u8,
    /// Whether this channel can spawn child channels.
    can_spawn: bool,
    /// Maximum privilege level this channel can operate at.
    #[serde(default)]
    max_privilege: MaxPrivilege,
}

impl ChannelConfig {
    /// Creates a new configuration with specified attributes.
    ///
    /// Uses `MaxPrivilege::Standard` by default. For elevated channels,
    /// use [`with_max_privilege()`](Self::with_max_privilege).
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
            max_privilege: MaxPrivilege::Standard,
        }
    }

    /// Configuration for interactive channels.
    ///
    /// Interactive channels:
    /// - Have highest priority (255)
    /// - Can spawn child channels
    /// - Have elevated privilege capability
    /// - Used for Human direct interaction (IO)
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{ChannelConfig, MaxPrivilege};
    ///
    /// let config = ChannelConfig::interactive();
    /// assert_eq!(config.priority(), 255);
    /// assert!(config.can_spawn());
    /// assert_eq!(config.max_privilege(), MaxPrivilege::Elevated);
    /// ```
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            priority: priority::PRIMARY,
            can_spawn: true,
            max_privilege: MaxPrivilege::Elevated,
        }
    }

    /// Configuration for background channels.
    ///
    /// Background channels:
    /// - Have lowest priority (10)
    /// - Cannot spawn child channels
    /// - Have standard privilege only
    /// - Used for parallel/async tasks
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{ChannelConfig, MaxPrivilege};
    ///
    /// let config = ChannelConfig::background();
    /// assert_eq!(config.priority(), 10);
    /// assert!(!config.can_spawn());
    /// assert_eq!(config.max_privilege(), MaxPrivilege::Standard);
    /// ```
    #[must_use]
    pub const fn background() -> Self {
        Self {
            priority: priority::BACKGROUND,
            can_spawn: false,
            max_privilege: MaxPrivilege::Standard,
        }
    }

    /// Configuration for tool execution channels.
    ///
    /// Tool channels:
    /// - Have medium-high priority (100)
    /// - Can spawn child channels (for sub-tools)
    /// - Have elevated privilege capability
    /// - Spawned for specific operations
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{ChannelConfig, MaxPrivilege};
    ///
    /// let config = ChannelConfig::tool();
    /// assert_eq!(config.priority(), 100);
    /// assert!(config.can_spawn());
    /// assert_eq!(config.max_privilege(), MaxPrivilege::Elevated);
    /// ```
    #[must_use]
    pub const fn tool() -> Self {
        Self {
            priority: priority::TOOL,
            can_spawn: true,
            max_privilege: MaxPrivilege::Elevated,
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

    /// Returns the maximum privilege level.
    #[must_use]
    pub const fn max_privilege(&self) -> MaxPrivilege {
        self.max_privilege
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

    /// Returns a new config with the specified max privilege.
    #[must_use]
    pub const fn with_max_privilege(mut self, max_privilege: MaxPrivilege) -> Self {
        self.max_privilege = max_privilege;
        self
    }

    /// Returns a config that inherits constraints from a parent.
    ///
    /// The child's `max_privilege` is capped by the parent's level.
    /// This ensures privilege reduction propagates down the channel tree.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{ChannelConfig, MaxPrivilege};
    ///
    /// let parent = ChannelConfig::background(); // Standard privilege
    /// let child_request = ChannelConfig::tool(); // Elevated privilege
    ///
    /// let actual = child_request.inherit_from(&parent);
    /// // Child is capped to parent's Standard level
    /// assert_eq!(actual.max_privilege(), MaxPrivilege::Standard);
    /// ```
    #[must_use]
    pub fn inherit_from(mut self, parent: &ChannelConfig) -> Self {
        self.max_privilege = self.max_privilege.min(parent.max_privilege);
        self
    }
}

impl Default for ChannelConfig {
    /// Default configuration with normal priority, spawn enabled, and standard privilege.
    fn default() -> Self {
        Self {
            priority: priority::NORMAL,
            can_spawn: true,
            max_privilege: MaxPrivilege::Standard,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === MaxPrivilege tests ===

    #[test]
    fn max_privilege_ordering() {
        assert!(MaxPrivilege::Elevated > MaxPrivilege::Standard);
        assert!(MaxPrivilege::Standard < MaxPrivilege::Elevated);
    }

    #[test]
    fn max_privilege_min() {
        assert_eq!(
            MaxPrivilege::Elevated.min(MaxPrivilege::Standard),
            MaxPrivilege::Standard
        );
        assert_eq!(
            MaxPrivilege::Standard.min(MaxPrivilege::Elevated),
            MaxPrivilege::Standard
        );
        assert_eq!(
            MaxPrivilege::Elevated.min(MaxPrivilege::Elevated),
            MaxPrivilege::Elevated
        );
    }

    #[test]
    fn max_privilege_is_elevated() {
        assert!(MaxPrivilege::Elevated.is_elevated());
        assert!(!MaxPrivilege::Standard.is_elevated());
    }

    #[test]
    fn max_privilege_default() {
        assert_eq!(MaxPrivilege::default(), MaxPrivilege::Standard);
    }

    // === ChannelConfig tests ===

    #[test]
    fn config_new() {
        let config = ChannelConfig::new(100, true);
        assert_eq!(config.priority(), 100);
        assert!(config.can_spawn());
        assert_eq!(config.max_privilege(), MaxPrivilege::Standard);
    }

    #[test]
    fn config_interactive() {
        let config = ChannelConfig::interactive();
        assert_eq!(config.priority(), priority::PRIMARY);
        assert!(config.can_spawn());
        assert_eq!(config.max_privilege(), MaxPrivilege::Elevated);
    }

    #[test]
    fn config_background() {
        let config = ChannelConfig::background();
        assert_eq!(config.priority(), priority::BACKGROUND);
        assert!(!config.can_spawn());
        assert_eq!(config.max_privilege(), MaxPrivilege::Standard);
    }

    #[test]
    fn config_tool() {
        let config = ChannelConfig::tool();
        assert_eq!(config.priority(), priority::TOOL);
        assert!(config.can_spawn());
        assert_eq!(config.max_privilege(), MaxPrivilege::Elevated);
    }

    #[test]
    fn config_default() {
        let config = ChannelConfig::default();
        assert_eq!(config.priority(), priority::NORMAL);
        assert!(config.can_spawn());
        assert_eq!(config.max_privilege(), MaxPrivilege::Standard);
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
    fn config_with_max_privilege() {
        let config = ChannelConfig::default().with_max_privilege(MaxPrivilege::Elevated);
        assert_eq!(config.max_privilege(), MaxPrivilege::Elevated);
    }

    #[test]
    fn config_equality() {
        assert_eq!(ChannelConfig::interactive(), ChannelConfig::interactive());
        assert_ne!(ChannelConfig::interactive(), ChannelConfig::background());
    }

    #[test]
    fn config_serialize() {
        let config = ChannelConfig::interactive();
        let json = serde_json::to_string(&config).expect("serialize ChannelConfig to JSON");
        assert!(json.contains("255"));
        assert!(json.contains("true"));
        assert!(json.contains("Elevated"));
    }

    // === Inheritance tests ===

    #[test]
    fn inherit_from_elevated_parent() {
        let parent = ChannelConfig::interactive(); // Elevated
        let child = ChannelConfig::tool().inherit_from(&parent); // Elevated

        // Elevated from Elevated parent stays Elevated
        assert_eq!(child.max_privilege(), MaxPrivilege::Elevated);
    }

    #[test]
    fn inherit_from_standard_parent() {
        let parent = ChannelConfig::background(); // Standard
        let child = ChannelConfig::tool().inherit_from(&parent); // requests Elevated

        // Elevated from Standard parent becomes Standard
        assert_eq!(child.max_privilege(), MaxPrivilege::Standard);
    }

    #[test]
    fn inherit_standard_from_elevated() {
        let parent = ChannelConfig::interactive(); // Elevated
        let child = ChannelConfig::background().inherit_from(&parent); // Standard

        // Standard request stays Standard (already lower)
        assert_eq!(child.max_privilege(), MaxPrivilege::Standard);
    }

    #[test]
    fn config_deserialize() {
        let json = r#"{"priority":100,"can_spawn":false}"#;
        let config: ChannelConfig =
            serde_json::from_str(json).expect("deserialize ChannelConfig from JSON");
        assert_eq!(config.priority(), 100);
        assert!(!config.can_spawn());
    }
}

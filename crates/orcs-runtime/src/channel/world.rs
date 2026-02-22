//! World - manages Channel lifecycle.
//!
//! The [`World`] is the central manager for all [`Channel`]s in the ORCS
//! system. It handles spawning, killing, and state transitions.
//!
//! # Responsibilities
//!
//! - **Create**: Create root channels (no parent)
//! - **Spawn**: Create child channels with parent relationships
//! - **Kill**: Remove channels and cascade to descendants
//! - **Complete**: Transition channels to completed state
//! - **Query**: Look up channels by ID
//!
//! # Example
//!
//! ```
//! use orcs_runtime::{World, ChannelConfig};
//!
//! let mut world = World::new();
//!
//! // Create a root channel (no parent)
//! let root = world.create_channel(ChannelConfig::interactive());
//!
//! // Spawn child channels
//! let agent = world.spawn(root).unwrap();
//! let tool = world.spawn(agent).unwrap();
//!
//! // Kill cascades to descendants
//! world.kill(agent, "cancelled".into());
//! assert!(world.get(&tool).is_none());
//! ```

use super::channel::Channel;
use super::config::ChannelConfig;
use super::traits::{ChannelCore, ChannelMut};
use orcs_types::{ChannelId, Principal};
use std::collections::HashMap;

/// Central manager for all [`Channel`]s.
///
/// The `World` maintains the channel tree and provides methods for
/// channel lifecycle management.
///
/// # Channel Tree
///
/// ```text
/// World
///   │
///   ├── IO Channel (interactive)
///   │     ├── Agent Channel
///   │     │     ├── Tool Channel 1
///   │     │     └── Tool Channel 2
///   │     │
///   │     └── Background Channel
///   │
///   └── Other Root Channels...
/// ```
///
/// # Thread Safety
///
/// `World` is not thread-safe by itself. In a concurrent environment,
/// wrap it in appropriate synchronization primitives (e.g., `Mutex`).
///
/// # Example
///
/// ```
/// use orcs_runtime::{World, ChannelConfig, ChannelCore, ChannelState};
///
/// let mut world = World::new();
///
/// // Create a root channel
/// let root = world.create_channel(ChannelConfig::interactive());
///
/// // Spawn and complete a child
/// let child = world.spawn(root).unwrap();
/// world.complete(child);
///
/// assert_eq!(
///     world.get(&child).unwrap().state(),
///     &ChannelState::Completed
/// );
/// ```
#[derive(Debug)]
pub struct World {
    channels: HashMap<ChannelId, Channel>,
}

impl World {
    /// Creates an empty World with no channels.
    ///
    /// Use [`create_channel()`](Self::create_channel) to create root channels.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::World;
    ///
    /// let world = World::new();
    /// assert_eq!(world.channel_count(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    /// Creates a root channel (no parent).
    ///
    /// Root channels have no parent and serve as entry points to the
    /// channel tree. Multiple root channels are allowed.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the new channel
    ///
    /// # Returns
    ///
    /// The ID of the newly created channel.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{World, ChannelConfig, ChannelCore};
    ///
    /// let mut world = World::new();
    ///
    /// // Create an interactive (IO) channel
    /// let io = world.create_channel(ChannelConfig::interactive());
    /// assert!(world.get(&io).unwrap().parent().is_none());
    /// assert_eq!(world.get(&io).unwrap().priority(), 255);
    ///
    /// // Create a background root channel
    /// let bg = world.create_channel(ChannelConfig::background());
    /// assert_eq!(world.channel_count(), 2);
    /// ```
    #[must_use]
    pub fn create_channel(&mut self, config: ChannelConfig) -> ChannelId {
        self.create_channel_with_principal(config, Principal::System)
    }

    /// Creates a root channel with a specific principal.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the new channel
    /// * `principal` - Principal for scope resolution
    #[must_use]
    pub fn create_channel_with_principal(
        &mut self,
        config: ChannelConfig,
        principal: Principal,
    ) -> ChannelId {
        let id = ChannelId::new();
        let channel = Channel::new(id, None, config, principal);
        self.channels.insert(id, channel);
        id
    }

    /// Spawns a new child channel under the given parent with default config.
    ///
    /// The new channel starts in [`Running`](crate::ChannelState::Running)
    /// state and is registered as a child of the parent.
    ///
    /// Uses [`ChannelConfig::default()`] for the new channel.
    /// For explicit configuration, use [`spawn_with()`](Self::spawn_with).
    ///
    /// # Arguments
    ///
    /// * `parent` - ID of the parent channel
    ///
    /// # Returns
    ///
    /// - `Some(ChannelId)` if the spawn succeeded
    /// - `None` if the parent channel does not exist
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{World, ChannelConfig, ChannelCore};
    ///
    /// let mut world = World::new();
    /// let root = world.create_channel(ChannelConfig::interactive());
    ///
    /// let child = world.spawn(root).expect("parent exists");
    /// assert_eq!(world.get(&child).unwrap().parent(), Some(root));
    /// ```
    pub fn spawn(&mut self, parent: ChannelId) -> Option<ChannelId> {
        self.spawn_with(parent, ChannelConfig::default())
    }

    /// Spawns a new child channel with explicit configuration.
    ///
    /// The new channel starts in [`Running`](crate::ChannelState::Running)
    /// state and is registered as a child of the parent.
    ///
    /// # Arguments
    ///
    /// * `parent` - ID of the parent channel
    /// * `config` - Configuration for the new channel
    ///
    /// # Returns
    ///
    /// - `Some(ChannelId)` if the spawn succeeded
    /// - `None` if the parent channel does not exist
    ///
    /// # Privilege Inheritance
    ///
    /// The child's `max_privilege` is automatically capped by the parent's level.
    /// This ensures privilege reduction propagates down the channel tree.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{World, ChannelConfig, ChannelCore, MaxPrivilege};
    ///
    /// let mut world = World::new();
    /// let root = world.create_channel(ChannelConfig::interactive());
    ///
    /// // Spawn a background channel
    /// let bg = world.spawn_with(root, ChannelConfig::background())
    ///     .expect("parent exists");
    /// assert_eq!(world.get(&bg).unwrap().priority(), 10);
    /// assert!(!world.get(&bg).unwrap().can_spawn());
    ///
    /// // Spawn a tool channel (inherits elevated from interactive parent)
    /// let tool = world.spawn_with(root, ChannelConfig::tool())
    ///     .expect("parent exists");
    /// assert_eq!(world.get(&tool).unwrap().priority(), 100);
    /// assert_eq!(world.get(&tool).unwrap().config().max_privilege(), MaxPrivilege::Elevated);
    ///
    /// // Spawn from background parent (max_privilege capped to Standard)
    /// let child = world.spawn_with(bg, ChannelConfig::tool())
    ///     .expect("parent exists");
    /// assert_eq!(world.get(&child).unwrap().config().max_privilege(), MaxPrivilege::Standard);
    /// ```
    pub fn spawn_with(&mut self, parent: ChannelId, config: ChannelConfig) -> Option<ChannelId> {
        let id = ChannelId::new();
        self.spawn_with_id(parent, id, config)
    }

    /// Spawns a new child channel with a pre-determined [`ChannelId`].
    ///
    /// Same as [`spawn_with()`](Self::spawn_with), but the caller provides the
    /// [`ChannelId`] instead of having one generated.  This is required when the
    /// caller needs to know the ID before the World is updated (e.g. to pass
    /// the ID into a `tokio::spawn` closure).
    ///
    /// Returns `Some(id)` on success, `None` if the parent does not exist.
    pub fn spawn_with_id(
        &mut self,
        parent: ChannelId,
        id: ChannelId,
        config: ChannelConfig,
    ) -> Option<ChannelId> {
        // Get parent channel for inheritance
        let parent_channel = self.channels.get(&parent)?;
        let parent_config = *parent_channel.config();
        let parent_principal = parent_channel.principal().clone();

        // Build ancestor path: [parent, ...parent's ancestors]
        let mut ancestor_path = Vec::with_capacity(parent_channel.depth() + 1);
        ancestor_path.push(parent);
        ancestor_path.extend_from_slice(parent_channel.ancestor_path());

        // Apply privilege inheritance
        let inherited_config = config.inherit_from(&parent_config);

        let channel = Channel::new_with_ancestors(
            id,
            parent,
            inherited_config,
            parent_principal,
            ancestor_path,
        );
        self.channels.insert(id, channel);

        // Register with parent
        if let Some(parent_channel) = self.channels.get_mut(&parent) {
            parent_channel.add_child(id);
        }

        Some(id)
    }

    /// Kills a channel and all its descendants.
    ///
    /// This method:
    /// 1. Recursively kills all child channels
    /// 2. Removes the channel from its parent's child set
    /// 3. Removes the channel from the World
    ///
    /// # Arguments
    ///
    /// * `id` - ID of the channel to kill
    /// * `reason` - Explanation for why the channel was killed
    ///
    /// # Note
    ///
    /// Unlike [`complete()`](Self::complete), `kill()` removes the
    /// channel entirely from the World rather than keeping it in
    /// a terminal state.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{World, ChannelConfig};
    ///
    /// let mut world = World::new();
    /// let root = world.create_channel(ChannelConfig::interactive());
    /// let agent = world.spawn(root).unwrap();
    /// let tool = world.spawn(agent).unwrap();
    ///
    /// // Killing agent also removes tool
    /// world.kill(agent, "task cancelled".into());
    ///
    /// assert!(world.get(&agent).is_none());
    /// assert!(world.get(&tool).is_none());
    /// assert_eq!(world.channel_count(), 1); // only root
    /// ```
    pub fn kill(&mut self, id: ChannelId, reason: String) {
        // Collect children first to avoid borrow issues
        let children: Vec<ChannelId> = self
            .channels
            .get(&id)
            .map(|ch| ch.children().iter().copied().collect())
            .unwrap_or_default();

        // Kill children recursively
        for child_id in children {
            self.kill(child_id, reason.clone());
        }

        // Remove from parent
        if let Some(channel) = self.channels.get(&id) {
            if let Some(parent_id) = channel.parent() {
                if let Some(parent) = self.channels.get_mut(&parent_id) {
                    parent.remove_child(&id);
                }
            }
        }

        self.channels.remove(&id);
    }

    /// Completes a channel, transitioning it to [`Completed`](crate::ChannelState::Completed).
    ///
    /// Unlike [`kill()`](Self::kill), the channel remains in the World
    /// with its completed state preserved.
    ///
    /// # Arguments
    ///
    /// * `id` - ID of the channel to complete
    ///
    /// # Returns
    ///
    /// - `true` if the channel was successfully completed
    /// - `false` if the channel doesn't exist or is already in a terminal state
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{World, ChannelConfig, ChannelState};
    ///
    /// let mut world = World::new();
    /// let root = world.create_channel(ChannelConfig::interactive());
    ///
    /// assert!(world.complete(root));
    /// assert!(!world.complete(root)); // Already completed
    /// ```
    pub fn complete(&mut self, id: ChannelId) -> bool {
        self.channels
            .get_mut(&id)
            .map(|ch| ch.complete())
            .unwrap_or(false)
    }

    /// Returns a reference to the channel with the given ID.
    ///
    /// # Returns
    ///
    /// - `Some(&Channel)` if the channel exists
    /// - `None` if no channel has this ID
    #[must_use]
    pub fn get(&self, id: &ChannelId) -> Option<&Channel> {
        self.channels.get(id)
    }

    /// Returns a mutable reference to the channel with the given ID.
    ///
    /// # Returns
    ///
    /// - `Some(&mut Channel)` if the channel exists
    /// - `None` if no channel has this ID
    pub fn get_mut(&mut self, id: &ChannelId) -> Option<&mut Channel> {
        self.channels.get_mut(id)
    }

    /// Returns all channel IDs in the World.
    ///
    /// The order is not guaranteed.
    #[must_use]
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.channels.keys().copied().collect()
    }

    /// Returns the total number of channels.
    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Checks if `child` is a descendant of `ancestor`.
    ///
    /// A channel is a descendant if there's a path from it to the ancestor
    /// through parent relationships. A channel is NOT its own descendant.
    ///
    /// # Performance
    ///
    /// Uses cached ancestor path for O(depth) lookup with cache-friendly
    /// sequential memory access (DOD optimization).
    ///
    /// # Arguments
    ///
    /// * `child` - ID of the potential descendant
    /// * `ancestor` - ID of the potential ancestor
    ///
    /// # Returns
    ///
    /// `true` if `child` is a descendant of `ancestor`, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{World, ChannelConfig};
    ///
    /// let mut world = World::new();
    /// let root = world.create_channel(ChannelConfig::interactive());
    /// let child = world.spawn(root).unwrap();
    /// let grandchild = world.spawn(child).unwrap();
    ///
    /// assert!(world.is_descendant_of(child, root));
    /// assert!(world.is_descendant_of(grandchild, root));
    /// assert!(world.is_descendant_of(grandchild, child));
    /// assert!(!world.is_descendant_of(root, child)); // parent is not descendant
    /// assert!(!world.is_descendant_of(root, root));  // not own descendant
    /// ```
    #[must_use]
    pub fn is_descendant_of(&self, child: ChannelId, ancestor: ChannelId) -> bool {
        if child == ancestor {
            return false;
        }

        self.channels
            .get(&child)
            .map(|ch| ch.is_descendant_of(ancestor))
            .unwrap_or(false)
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelState;

    #[test]
    fn world_creation() {
        let world = World::new();
        assert_eq!(world.channel_count(), 0);
    }

    #[test]
    fn create_channel_interactive() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        assert_eq!(world.channel_count(), 1);
        assert!(world.get(&io).is_some());
        assert!(world
            .get(&io)
            .expect("interactive channel should exist")
            .parent()
            .is_none());
        assert_eq!(
            world
                .get(&io)
                .expect("interactive channel should exist for priority check")
                .priority(),
            255
        );
    }

    #[test]
    fn create_multiple_root_channels() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());
        let bg = world.create_channel(ChannelConfig::background());

        assert_eq!(world.channel_count(), 2);
        assert!(world
            .get(&io)
            .expect("IO channel should exist")
            .parent()
            .is_none());
        assert!(world
            .get(&bg)
            .expect("background channel should exist")
            .parent()
            .is_none());
    }

    #[test]
    fn spawn_child() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());
        let child = world.spawn(root).expect("spawn child from root");

        assert_eq!(world.channel_count(), 2);
        assert!(world.get(&child).is_some());
        assert_eq!(
            world
                .get(&child)
                .expect("child channel should exist")
                .parent(),
            Some(root)
        );
        assert!(world
            .get(&root)
            .expect("root channel should exist")
            .children()
            .contains(&child));
    }

    #[test]
    fn spawn_invalid_parent() {
        let mut world = World::new();
        let invalid = ChannelId::new();
        let result = world.spawn(invalid);

        assert!(result.is_none());
    }

    #[test]
    fn kill_channel() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());
        let child = world.spawn(root).expect("spawn child for kill test");

        world.kill(child, "test".into());

        assert_eq!(world.channel_count(), 1);
        assert!(world.get(&child).is_none());
        assert!(!world
            .get(&root)
            .expect("root should exist after killing child")
            .children()
            .contains(&child));
    }

    #[test]
    fn kill_with_children() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());
        let child = world
            .spawn(root)
            .expect("spawn child for cascade kill test");
        let grandchild = world
            .spawn(child)
            .expect("spawn grandchild for cascade kill test");

        assert_eq!(world.channel_count(), 3);

        world.kill(child, "cascade".into());

        assert_eq!(world.channel_count(), 1);
        assert!(world.get(&child).is_none());
        assert!(world.get(&grandchild).is_none());
    }

    #[test]
    fn complete_channel() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());

        assert!(world.complete(root));

        assert_eq!(
            world
                .get(&root)
                .expect("root channel should exist after complete")
                .state(),
            &ChannelState::Completed
        );

        // Cannot complete twice
        assert!(!world.complete(root));
    }

    #[test]
    fn complete_nonexistent() {
        let mut world = World::new();
        let invalid = ChannelId::new();
        assert!(!world.complete(invalid));
    }

    #[test]
    fn interactive_has_high_priority() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let channel = world
            .get(&io)
            .expect("IO channel should exist for priority check");
        assert_eq!(channel.priority(), 255);
        assert!(channel.can_spawn());
    }

    #[test]
    fn spawn_with_background_config() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());

        let bg = world
            .spawn_with(root, ChannelConfig::background())
            .expect("spawn background channel from root");

        let channel = world.get(&bg).expect("background channel should exist");
        assert_eq!(channel.priority(), 10);
        assert!(!channel.can_spawn());
    }

    #[test]
    fn spawn_with_tool_config() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());

        let tool = world
            .spawn_with(root, ChannelConfig::tool())
            .expect("spawn tool channel from root");

        let channel = world.get(&tool).expect("tool channel should exist");
        assert_eq!(channel.priority(), 100);
        assert!(channel.can_spawn());
    }

    #[test]
    fn spawn_with_custom_config() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());

        let config = ChannelConfig::new(200, false);
        let custom = world
            .spawn_with(root, config)
            .expect("spawn custom config channel from root");

        let channel = world.get(&custom).expect("custom channel should exist");
        assert_eq!(channel.priority(), 200);
        assert!(!channel.can_spawn());
    }

    #[test]
    fn spawn_uses_default_config() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());

        let child = world.spawn(root).expect("spawn child with default config");

        let channel = world
            .get(&child)
            .expect("child channel should exist for default config check");
        // Default config: NORMAL priority (50), can_spawn = true
        assert_eq!(channel.priority(), 50);
        assert!(channel.can_spawn());
    }

    #[test]
    fn is_descendant_direct_child() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());
        let child = world.spawn(root).expect("spawn child for descendant test");

        assert!(world.is_descendant_of(child, root));
        assert!(!world.is_descendant_of(root, child));
    }

    #[test]
    fn is_descendant_grandchild() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());
        let child = world
            .spawn(root)
            .expect("spawn child for grandchild descendant test");
        let grandchild = world
            .spawn(child)
            .expect("spawn grandchild for descendant test");

        assert!(world.is_descendant_of(grandchild, root));
        assert!(world.is_descendant_of(grandchild, child));
        assert!(world.is_descendant_of(child, root));
    }

    #[test]
    fn is_descendant_not_self() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());

        assert!(!world.is_descendant_of(root, root));
    }

    #[test]
    fn is_descendant_siblings_not_related() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());
        let child1 = world.spawn(root).expect("spawn first sibling");
        let child2 = world.spawn(root).expect("spawn second sibling");

        assert!(!world.is_descendant_of(child1, child2));
        assert!(!world.is_descendant_of(child2, child1));
    }

    #[test]
    fn is_descendant_nonexistent() {
        let mut world = World::new();
        let root = world.create_channel(ChannelConfig::interactive());
        let nonexistent = ChannelId::new();

        assert!(!world.is_descendant_of(nonexistent, root));
        assert!(!world.is_descendant_of(root, nonexistent));
    }
}

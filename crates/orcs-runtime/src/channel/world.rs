//! World - manages Channel lifecycle.
//!
//! The [`World`] is the central manager for all [`Channel`]s in the ORCS
//! system. It handles spawning, killing, and state transitions.
//!
//! # Responsibilities
//!
//! - **Spawn**: Create new channels with parent-child relationships
//! - **Kill**: Remove channels and cascade to descendants
//! - **Complete**: Transition channels to completed state
//! - **Query**: Look up channels by ID
//!
//! # Example
//!
//! ```
//! use orcs_runtime::World;
//!
//! let mut world = World::new();
//!
//! // Create the primary (root) channel
//! let primary = world.create_primary().unwrap();
//!
//! // Spawn child channels
//! let agent = world.spawn(primary).unwrap();
//! let tool = world.spawn(agent).unwrap();
//!
//! // Kill cascades to descendants
//! world.kill(agent, "cancelled".into());
//! assert!(world.get(&tool).is_none());
//! ```

use super::channel::Channel;
use super::error::ChannelError;
use orcs_types::ChannelId;
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
///   └── Primary Channel (root)
///         │
///         ├── Agent Channel
///         │     ├── Tool Channel 1
///         │     └── Tool Channel 2
///         │
///         └── Background Channel
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
/// use orcs_runtime::{World, ChannelState};
///
/// let mut world = World::new();
///
/// // Create primary channel
/// let primary = world.create_primary().unwrap();
/// assert_eq!(world.primary(), Some(primary));
///
/// // Spawn and complete a child
/// let child = world.spawn(primary).unwrap();
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
    primary: Option<ChannelId>,
}

impl World {
    /// Creates an empty World with no channels.
    ///
    /// Call [`create_primary()`](Self::create_primary) to create the root channel.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::World;
    ///
    /// let world = World::new();
    /// assert!(world.primary().is_none());
    /// assert_eq!(world.channel_count(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
            primary: None,
        }
    }

    /// Creates the primary (root) channel.
    ///
    /// The primary channel has no parent and serves as the root of
    /// the channel tree. This should typically be called once at
    /// startup.
    ///
    /// # Returns
    ///
    /// - `Ok(ChannelId)` - The ID of the newly created primary channel
    /// - `Err(ChannelError::AlreadyExists)` - If a primary channel already exists
    ///
    /// # Errors
    ///
    /// Returns [`ChannelError::AlreadyExists`] if called when a primary
    /// channel has already been created.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{World, ChannelError};
    ///
    /// let mut world = World::new();
    /// let primary = world.create_primary().unwrap();
    ///
    /// assert_eq!(world.primary(), Some(primary));
    /// assert!(world.get(&primary).unwrap().parent().is_none());
    ///
    /// // Second call returns error
    /// assert!(matches!(
    ///     world.create_primary(),
    ///     Err(ChannelError::AlreadyExists(_))
    /// ));
    /// ```
    pub fn create_primary(&mut self) -> Result<ChannelId, ChannelError> {
        if let Some(existing) = self.primary {
            return Err(ChannelError::AlreadyExists(existing));
        }

        let id = ChannelId::new();
        let channel = Channel::new(id, None);
        self.channels.insert(id, channel);
        self.primary = Some(id);
        Ok(id)
    }

    /// Returns the primary channel's ID, if created.
    #[must_use]
    pub fn primary(&self) -> Option<ChannelId> {
        self.primary
    }

    /// Spawns a new child channel under the given parent.
    ///
    /// The new channel starts in [`Running`](crate::ChannelState::Running)
    /// state and is registered as a child of the parent.
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
    /// use orcs_runtime::World;
    ///
    /// let mut world = World::new();
    /// let primary = world.create_primary().unwrap();
    ///
    /// let child = world.spawn(primary).expect("parent exists");
    /// assert_eq!(world.get(&child).unwrap().parent(), Some(primary));
    /// ```
    pub fn spawn(&mut self, parent: ChannelId) -> Option<ChannelId> {
        if !self.channels.contains_key(&parent) {
            return None;
        }

        let id = ChannelId::new();
        let channel = Channel::new(id, Some(parent));
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
    /// use orcs_runtime::World;
    ///
    /// let mut world = World::new();
    /// let primary = world.create_primary().unwrap();
    /// let agent = world.spawn(primary).unwrap();
    /// let tool = world.spawn(agent).unwrap();
    ///
    /// // Killing agent also removes tool
    /// world.kill(agent, "task cancelled".into());
    ///
    /// assert!(world.get(&agent).is_none());
    /// assert!(world.get(&tool).is_none());
    /// assert_eq!(world.channel_count(), 1); // only primary
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
    /// use orcs_runtime::{World, ChannelState};
    ///
    /// let mut world = World::new();
    /// let primary = world.create_primary().unwrap();
    ///
    /// assert!(world.complete(primary));
    /// assert!(!world.complete(primary)); // Already completed
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
        assert!(world.primary().is_none());
        assert_eq!(world.channel_count(), 0);
    }

    #[test]
    fn create_primary() {
        let mut world = World::new();
        let primary = world.create_primary().unwrap();

        assert_eq!(world.primary(), Some(primary));
        assert_eq!(world.channel_count(), 1);
        assert!(world.get(&primary).is_some());
    }

    #[test]
    fn create_primary_twice_fails() {
        let mut world = World::new();
        let primary = world.create_primary().unwrap();

        let result = world.create_primary();
        assert!(matches!(result, Err(ChannelError::AlreadyExists(id)) if id == primary));
    }

    #[test]
    fn spawn_child() {
        let mut world = World::new();
        let primary = world.create_primary().unwrap();
        let child = world.spawn(primary).unwrap();

        assert_eq!(world.channel_count(), 2);
        assert!(world.get(&child).is_some());
        assert_eq!(world.get(&child).unwrap().parent(), Some(primary));
        assert!(world.get(&primary).unwrap().children().contains(&child));
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
        let primary = world.create_primary().unwrap();
        let child = world.spawn(primary).unwrap();

        world.kill(child, "test".into());

        assert_eq!(world.channel_count(), 1);
        assert!(world.get(&child).is_none());
        assert!(!world.get(&primary).unwrap().children().contains(&child));
    }

    #[test]
    fn kill_with_children() {
        let mut world = World::new();
        let primary = world.create_primary().unwrap();
        let child = world.spawn(primary).unwrap();
        let grandchild = world.spawn(child).unwrap();

        assert_eq!(world.channel_count(), 3);

        world.kill(child, "cascade".into());

        assert_eq!(world.channel_count(), 1);
        assert!(world.get(&child).is_none());
        assert!(world.get(&grandchild).is_none());
    }

    #[test]
    fn complete_channel() {
        let mut world = World::new();
        let primary = world.create_primary().unwrap();

        assert!(world.complete(primary));

        assert_eq!(
            world.get(&primary).unwrap().state(),
            &ChannelState::Completed
        );

        // Cannot complete twice
        assert!(!world.complete(primary));
    }

    #[test]
    fn complete_nonexistent() {
        let mut world = World::new();
        let invalid = ChannelId::new();
        assert!(!world.complete(invalid));
    }
}

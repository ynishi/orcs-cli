//! Channel - unit of parallel execution.
//!
//! A [`Channel`] represents an isolated execution context within the ORCS
//! system. Channels form a tree structure where child channels inherit
//! context from their parents.
//!
//! # State Machine
//!
//! ```text
//!                    ┌─────────┐
//!      ┌─────────────│ Running │─────────────┐
//!      │             └─────────┘             │
//!      │ complete()                  abort() │
//!      ▼                                     ▼
//! ┌───────────┐                       ┌─────────┐
//! │ Completed │                       │ Aborted │
//! └───────────┘                       └─────────┘
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_runtime::{Channel, ChannelState};
//! use orcs_types::ChannelId;
//!
//! let id = ChannelId::new();
//! let mut channel = Channel::new(id, None);
//!
//! assert!(channel.is_running());
//! assert!(channel.complete());
//! assert_eq!(channel.state(), &ChannelState::Completed);
//! ```

use orcs_types::ChannelId;
use std::collections::HashSet;

/// State of a [`Channel`].
///
/// Channels start in [`Running`](ChannelState::Running) state and can
/// transition to either [`Completed`](ChannelState::Completed) or
/// [`Aborted`](ChannelState::Aborted). Once in a terminal state,
/// no further transitions are allowed.
///
/// # State Transitions
///
/// | From | To | Method |
/// |------|----|--------|
/// | Running | Completed | [`Channel::complete()`] |
/// | Running | Aborted | [`Channel::abort()`] |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelState {
    /// Channel is actively running.
    Running,

    /// Channel completed successfully.
    Completed,

    /// Channel was aborted with a reason.
    ///
    /// The reason string provides context for debugging and logging.
    Aborted {
        /// Explanation of why the channel was aborted.
        reason: String,
    },
}

/// Unit of parallel execution.
///
/// A `Channel` represents an isolated execution context that can:
/// - Track its current state ([`ChannelState`])
/// - Maintain parent-child relationships
/// - Transition through its lifecycle
///
/// # Parent-Child Relationships
///
/// Channels form a tree structure:
/// - Primary channel has no parent (`parent() == None`)
/// - Child channels reference their parent
/// - A channel tracks all its children
///
/// # Example
///
/// ```
/// use orcs_runtime::{Channel, ChannelState};
/// use orcs_types::ChannelId;
///
/// // Create a root channel
/// let root_id = ChannelId::new();
/// let mut root = Channel::new(root_id, None);
///
/// // Create a child channel
/// let child_id = ChannelId::new();
/// let child = Channel::new(child_id, Some(root_id));
///
/// // Register the child with parent
/// root.add_child(child_id);
/// assert!(root.has_children());
/// assert_eq!(child.parent(), Some(root_id));
/// ```
#[derive(Debug)]
pub struct Channel {
    id: ChannelId,
    state: ChannelState,
    parent: Option<ChannelId>,
    children: HashSet<ChannelId>,
}

impl Channel {
    /// Creates a new channel in [`Running`](ChannelState::Running) state.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this channel
    /// * `parent` - Parent channel ID, or `None` for root channels
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::Channel;
    /// use orcs_types::ChannelId;
    ///
    /// // Root channel (no parent)
    /// let root = Channel::new(ChannelId::new(), None);
    ///
    /// // Child channel
    /// let parent_id = ChannelId::new();
    /// let child = Channel::new(ChannelId::new(), Some(parent_id));
    /// ```
    #[must_use]
    pub fn new(id: ChannelId, parent: Option<ChannelId>) -> Self {
        Self {
            id,
            state: ChannelState::Running,
            parent,
            children: HashSet::new(),
        }
    }

    /// Returns the channel's unique identifier.
    #[must_use]
    pub fn id(&self) -> ChannelId {
        self.id
    }

    /// Returns a reference to the current state.
    #[must_use]
    pub fn state(&self) -> &ChannelState {
        &self.state
    }

    /// Transitions to [`Completed`](ChannelState::Completed) state.
    ///
    /// Only succeeds if currently in [`Running`](ChannelState::Running) state.
    ///
    /// # Returns
    ///
    /// - `true` if the transition succeeded
    /// - `false` if already in a terminal state
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{Channel, ChannelState};
    /// use orcs_types::ChannelId;
    ///
    /// let mut channel = Channel::new(ChannelId::new(), None);
    /// assert!(channel.complete());
    /// assert!(!channel.complete()); // Already completed
    /// ```
    pub fn complete(&mut self) -> bool {
        if matches!(self.state, ChannelState::Running) {
            self.state = ChannelState::Completed;
            true
        } else {
            false
        }
    }

    /// Transitions to [`Aborted`](ChannelState::Aborted) state.
    ///
    /// Only succeeds if currently in [`Running`](ChannelState::Running) state.
    ///
    /// # Arguments
    ///
    /// * `reason` - Explanation of why the channel was aborted
    ///
    /// # Returns
    ///
    /// - `true` if the transition succeeded
    /// - `false` if already in a terminal state
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{Channel, ChannelState};
    /// use orcs_types::ChannelId;
    ///
    /// let mut channel = Channel::new(ChannelId::new(), None);
    /// assert!(channel.abort("user cancelled".to_string()));
    ///
    /// if let ChannelState::Aborted { reason } = channel.state() {
    ///     assert_eq!(reason, "user cancelled");
    /// }
    /// ```
    pub fn abort(&mut self, reason: String) -> bool {
        if matches!(self.state, ChannelState::Running) {
            self.state = ChannelState::Aborted { reason };
            true
        } else {
            false
        }
    }

    /// Returns the parent channel's ID, if any.
    ///
    /// Returns `None` for root/primary channels.
    #[must_use]
    pub fn parent(&self) -> Option<ChannelId> {
        self.parent
    }

    /// Returns `true` if the channel is in [`Running`](ChannelState::Running) state.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self.state, ChannelState::Running)
    }

    /// Registers a child channel.
    ///
    /// This should be called by [`World`](crate::World) when spawning
    /// a new channel to maintain the parent-child relationship.
    ///
    /// # Arguments
    ///
    /// * `id` - The child channel's ID
    pub fn add_child(&mut self, id: ChannelId) {
        self.children.insert(id);
    }

    /// Unregisters a child channel.
    ///
    /// This should be called by [`World`](crate::World) when killing
    /// or completing a child channel.
    ///
    /// # Arguments
    ///
    /// * `id` - The child channel's ID to remove
    pub fn remove_child(&mut self, id: &ChannelId) {
        self.children.remove(id);
    }

    /// Returns a reference to the set of child channel IDs.
    #[must_use]
    pub fn children(&self) -> &HashSet<ChannelId> {
        &self.children
    }

    /// Returns `true` if this channel has any children.
    #[must_use]
    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_creation() {
        let id = ChannelId::new();
        let channel = Channel::new(id, None);

        assert_eq!(channel.id(), id);
        assert!(channel.is_running());
        assert!(channel.parent().is_none());
        assert!(!channel.has_children());
    }

    #[test]
    fn channel_with_parent() {
        let parent_id = ChannelId::new();
        let child_id = ChannelId::new();
        let channel = Channel::new(child_id, Some(parent_id));

        assert_eq!(channel.parent(), Some(parent_id));
    }

    #[test]
    fn channel_state_transition() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None);

        assert!(channel.is_running());

        assert!(channel.complete());
        assert!(!channel.is_running());
        assert_eq!(channel.state(), &ChannelState::Completed);

        // Cannot transition from Completed
        assert!(!channel.complete());
        assert!(!channel.abort("test".into()));
    }

    #[test]
    fn channel_abort_transition() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None);

        assert!(channel.abort("reason".into()));
        assert!(!channel.is_running());
        assert!(matches!(channel.state(), ChannelState::Aborted { .. }));

        // Cannot transition from Aborted
        assert!(!channel.complete());
    }

    #[test]
    fn channel_children() {
        let parent_id = ChannelId::new();
        let child1 = ChannelId::new();
        let child2 = ChannelId::new();

        let mut parent = Channel::new(parent_id, None);
        assert!(!parent.has_children());

        parent.add_child(child1);
        parent.add_child(child2);
        assert!(parent.has_children());
        assert_eq!(parent.children().len(), 2);

        parent.remove_child(&child1);
        assert_eq!(parent.children().len(), 1);
    }
}

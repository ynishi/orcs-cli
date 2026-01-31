//! Channel - unit of parallel execution.
//!
//! A [`Channel`] represents an isolated execution context within the ORCS
//! system. Channels form a tree structure where child channels inherit
//! context from their parents.
//!
//! # Configuration
//!
//! Channel behavior is controlled by [`ChannelConfig`]:
//!
//! - **priority**: Scheduling priority (0-255)
//! - **can_spawn**: Whether child channels can be spawned
//!
//! # State Machine
//!
//! ```text
//!                         ┌─────────┐
//!       ┌─────────────────│ Running │─────────────────┐
//!       │                 └────┬────┘                 │
//!       │                      │                      │
//!       │ pause()              │ await_approval()     │ abort()
//!       ▼                      ▼                      ▼
//! ┌──────────┐         ┌────────────────┐      ┌─────────┐
//! │  Paused  │         │AwaitingApproval│      │ Aborted │
//! └────┬─────┘         └───────┬────────┘      └─────────┘
//!      │                       │
//!      │ resume()              │ approve()/reject()
//!      │                       │
//!      └───────►  Running  ◄───┘
//!                    │
//!                    │ complete()
//!                    ▼
//!              ┌───────────┐
//!              │ Completed │
//!              └───────────┘
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_runtime::{Channel, ChannelConfig, ChannelState};
//! use orcs_types::ChannelId;
//!
//! let id = ChannelId::new();
//! let mut channel = Channel::new(id, None, ChannelConfig::interactive());
//!
//! assert!(channel.is_running());
//! assert_eq!(channel.priority(), 255);
//! assert!(channel.complete());
//! assert_eq!(channel.state(), &ChannelState::Completed);
//! ```

use super::config::ChannelConfig;
use orcs_types::ChannelId;
use std::collections::HashSet;

/// State of a [`Channel`].
///
/// Channels start in [`Running`](ChannelState::Running) state and can
/// transition through various states. Terminal states are
/// [`Completed`](ChannelState::Completed) and [`Aborted`](ChannelState::Aborted).
///
/// # State Transitions
///
/// | From | To | Method |
/// |------|----|--------|
/// | Running | Completed | [`Channel::complete()`] |
/// | Running | Aborted | [`Channel::abort()`] |
/// | Running | Paused | [`Channel::pause()`] |
/// | Running | AwaitingApproval | [`Channel::await_approval()`] |
/// | Paused | Running | [`Channel::resume()`] |
/// | AwaitingApproval | Running | [`Channel::resolve_approval()`] |
/// | AwaitingApproval | Aborted | [`Channel::abort()`] (rejected) |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelState {
    /// Channel is actively running.
    Running,

    /// Channel is paused.
    ///
    /// Can be resumed with [`Channel::resume()`].
    Paused,

    /// Channel is waiting for Human approval (HIL).
    ///
    /// Contains the request ID that needs approval.
    AwaitingApproval {
        /// ID of the approval request.
        request_id: String,
    },

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
/// - Control behavior via [`ChannelConfig`]
/// - Transition through its lifecycle
///
/// # Parent-Child Relationships
///
/// Channels form a tree structure:
/// - Primary channel has no parent (`parent() == None`)
/// - Child channels reference their parent
/// - A channel tracks all its children
///
/// # Configuration
///
/// Channel behavior is controlled by [`ChannelConfig`]:
/// - **priority**: Determines scheduling order (0-255, higher = more priority)
/// - **can_spawn**: Whether this channel can create children
///
/// # Example
///
/// ```
/// use orcs_runtime::{Channel, ChannelConfig, ChannelState};
/// use orcs_types::ChannelId;
///
/// // Create a root channel
/// let root_id = ChannelId::new();
/// let mut root = Channel::new(root_id, None, ChannelConfig::interactive());
/// assert_eq!(root.priority(), 255);
/// assert!(root.can_spawn());
///
/// // Create a background child channel
/// let child_id = ChannelId::new();
/// let child = Channel::new(child_id, Some(root_id), ChannelConfig::background());
/// assert_eq!(child.priority(), 10);
/// assert!(!child.can_spawn());
///
/// root.add_child(child_id);
/// assert!(root.has_children());
/// ```
#[derive(Debug)]
pub struct Channel {
    id: ChannelId,
    state: ChannelState,
    parent: Option<ChannelId>,
    children: HashSet<ChannelId>,
    config: ChannelConfig,
}

impl Channel {
    /// Creates a new channel in [`Running`](ChannelState::Running) state.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this channel
    /// * `parent` - Parent channel ID, or `None` for root channels
    /// * `config` - Configuration controlling channel behavior
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{Channel, ChannelConfig};
    /// use orcs_types::ChannelId;
    ///
    /// // Primary channel (no parent, highest priority)
    /// let root = Channel::new(ChannelId::new(), None, ChannelConfig::interactive());
    /// assert_eq!(root.priority(), 255);
    ///
    /// // Background child channel
    /// let parent_id = ChannelId::new();
    /// let child = Channel::new(ChannelId::new(), Some(parent_id), ChannelConfig::background());
    /// assert_eq!(child.priority(), 10);
    /// ```
    #[must_use]
    pub fn new(id: ChannelId, parent: Option<ChannelId>, config: ChannelConfig) -> Self {
        Self {
            id,
            state: ChannelState::Running,
            parent,
            children: HashSet::new(),
            config,
        }
    }

    /// Returns the scheduling priority (0-255).
    ///
    /// Higher values indicate higher priority for scheduling.
    #[must_use]
    pub fn priority(&self) -> u8 {
        self.config.priority()
    }

    /// Returns whether this channel can spawn children.
    #[must_use]
    pub fn can_spawn(&self) -> bool {
        self.config.can_spawn()
    }

    /// Returns a reference to the channel's configuration.
    #[must_use]
    pub fn config(&self) -> &ChannelConfig {
        &self.config
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
    /// use orcs_runtime::{Channel, ChannelConfig, ChannelState};
    /// use orcs_types::ChannelId;
    ///
    /// let mut channel = Channel::new(ChannelId::new(), None, ChannelConfig::default());
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
    /// Can be called from [`Running`](ChannelState::Running),
    /// [`Paused`](ChannelState::Paused), or
    /// [`AwaitingApproval`](ChannelState::AwaitingApproval) states.
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
    /// use orcs_runtime::{Channel, ChannelConfig, ChannelState};
    /// use orcs_types::ChannelId;
    ///
    /// let mut channel = Channel::new(ChannelId::new(), None, ChannelConfig::default());
    /// assert!(channel.abort("user cancelled".to_string()));
    ///
    /// if let ChannelState::Aborted { reason } = channel.state() {
    ///     assert_eq!(reason, "user cancelled");
    /// }
    /// ```
    pub fn abort(&mut self, reason: String) -> bool {
        // Can abort from any non-terminal state
        if !self.is_terminal() {
            self.state = ChannelState::Aborted { reason };
            true
        } else {
            false
        }
    }

    /// Returns the parent channel's ID, if any.
    ///
    /// Returns `None` for root channels.
    #[must_use]
    pub fn parent(&self) -> Option<ChannelId> {
        self.parent
    }

    /// Returns `true` if the channel is in [`Running`](ChannelState::Running) state.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self.state, ChannelState::Running)
    }

    /// Returns `true` if the channel is in [`Paused`](ChannelState::Paused) state.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(self.state, ChannelState::Paused)
    }

    /// Returns `true` if the channel is in [`AwaitingApproval`](ChannelState::AwaitingApproval) state.
    #[must_use]
    pub fn is_awaiting_approval(&self) -> bool {
        matches!(self.state, ChannelState::AwaitingApproval { .. })
    }

    /// Returns `true` if the channel is in a terminal state (Completed or Aborted).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            ChannelState::Completed | ChannelState::Aborted { .. }
        )
    }

    /// Transitions to [`Paused`](ChannelState::Paused) state.
    ///
    /// Only succeeds if currently in [`Running`](ChannelState::Running) state.
    ///
    /// # Returns
    ///
    /// - `true` if the transition succeeded
    /// - `false` if not in Running state
    #[must_use = "check if state transition succeeded"]
    pub fn pause(&mut self) -> bool {
        if matches!(self.state, ChannelState::Running) {
            self.state = ChannelState::Paused;
            true
        } else {
            false
        }
    }

    /// Transitions from [`Paused`](ChannelState::Paused) back to [`Running`](ChannelState::Running).
    ///
    /// # Returns
    ///
    /// - `true` if the transition succeeded
    /// - `false` if not in Paused state
    #[must_use = "check if state transition succeeded"]
    pub fn resume(&mut self) -> bool {
        if matches!(self.state, ChannelState::Paused) {
            self.state = ChannelState::Running;
            true
        } else {
            false
        }
    }

    /// Transitions to [`AwaitingApproval`](ChannelState::AwaitingApproval) state.
    ///
    /// Only succeeds if currently in [`Running`](ChannelState::Running) state.
    ///
    /// # Arguments
    ///
    /// * `request_id` - ID of the approval request
    ///
    /// # Returns
    ///
    /// - `true` if the transition succeeded
    /// - `false` if not in Running state
    #[must_use = "check if state transition succeeded"]
    pub fn await_approval(&mut self, request_id: impl Into<String>) -> bool {
        if matches!(self.state, ChannelState::Running) {
            self.state = ChannelState::AwaitingApproval {
                request_id: request_id.into(),
            };
            true
        } else {
            false
        }
    }

    /// Resolves an approval and transitions back to [`Running`](ChannelState::Running).
    ///
    /// Only succeeds if currently in [`AwaitingApproval`](ChannelState::AwaitingApproval) state.
    ///
    /// # Returns
    ///
    /// - `Some(request_id)` if the transition succeeded
    /// - `None` if not in AwaitingApproval state
    pub fn resolve_approval(&mut self) -> Option<String> {
        if let ChannelState::AwaitingApproval { request_id } = &self.state {
            let id = request_id.clone();
            self.state = ChannelState::Running;
            Some(id)
        } else {
            None
        }
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
        let channel = Channel::new(id, None, ChannelConfig::interactive());

        assert_eq!(channel.id(), id);
        assert!(channel.is_running());
        assert!(channel.parent().is_none());
        assert!(!channel.has_children());
    }

    #[test]
    fn channel_with_parent() {
        let parent_id = ChannelId::new();
        let child_id = ChannelId::new();
        let channel = Channel::new(child_id, Some(parent_id), ChannelConfig::default());

        assert_eq!(channel.parent(), Some(parent_id));
    }

    #[test]
    fn channel_state_transition() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None, ChannelConfig::default());

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
        let mut channel = Channel::new(id, None, ChannelConfig::default());

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

        let mut parent = Channel::new(parent_id, None, ChannelConfig::interactive());
        assert!(!parent.has_children());

        parent.add_child(child1);
        parent.add_child(child2);
        assert!(parent.has_children());
        assert_eq!(parent.children().len(), 2);

        parent.remove_child(&child1);
        assert_eq!(parent.children().len(), 1);
    }

    #[test]
    fn channel_priority() {
        let id = ChannelId::new();

        let primary = Channel::new(id, None, ChannelConfig::interactive());
        assert_eq!(primary.priority(), 255);

        let background = Channel::new(ChannelId::new(), None, ChannelConfig::background());
        assert_eq!(background.priority(), 10);

        let tool = Channel::new(ChannelId::new(), None, ChannelConfig::tool());
        assert_eq!(tool.priority(), 100);
    }

    #[test]
    fn channel_can_spawn() {
        let primary = Channel::new(ChannelId::new(), None, ChannelConfig::interactive());
        assert!(primary.can_spawn());

        let background = Channel::new(ChannelId::new(), None, ChannelConfig::background());
        assert!(!background.can_spawn());

        let tool = Channel::new(ChannelId::new(), None, ChannelConfig::tool());
        assert!(tool.can_spawn());
    }

    #[test]
    fn channel_config_access() {
        let config = ChannelConfig::new(200, false);
        let channel = Channel::new(ChannelId::new(), None, config);

        assert_eq!(channel.config().priority(), 200);
        assert!(!channel.config().can_spawn());
    }

    // === M2 HIL State Tests ===

    #[test]
    fn channel_pause_resume() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None, ChannelConfig::default());

        assert!(channel.is_running());
        assert!(!channel.is_paused());

        assert!(channel.pause());
        assert!(!channel.is_running());
        assert!(channel.is_paused());

        // Cannot pause when already paused
        assert!(!channel.pause());

        assert!(channel.resume());
        assert!(channel.is_running());
        assert!(!channel.is_paused());

        // Cannot resume when running
        assert!(!channel.resume());
    }

    #[test]
    fn channel_await_approval() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None, ChannelConfig::default());

        assert!(channel.await_approval("req-123"));
        assert!(channel.is_awaiting_approval());
        assert!(!channel.is_running());

        // Cannot await approval when already awaiting
        assert!(!channel.await_approval("req-456"));

        if let ChannelState::AwaitingApproval { request_id } = channel.state() {
            assert_eq!(request_id, "req-123");
        } else {
            panic!("Expected AwaitingApproval state");
        }
    }

    #[test]
    fn channel_resolve_approval() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None, ChannelConfig::default());

        assert!(channel.await_approval("req-123"));

        let resolved_id = channel.resolve_approval();
        assert_eq!(resolved_id, Some("req-123".to_string()));
        assert!(channel.is_running());

        // Cannot resolve when running
        assert!(channel.resolve_approval().is_none());
    }

    #[test]
    fn channel_abort_from_awaiting() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None, ChannelConfig::default());

        assert!(channel.await_approval("req-123"));
        assert!(channel.abort("rejected by user".to_string()));

        assert!(channel.is_terminal());
        if let ChannelState::Aborted { reason } = channel.state() {
            assert_eq!(reason, "rejected by user");
        } else {
            panic!("Expected Aborted state");
        }
    }

    #[test]
    fn channel_terminal_state() {
        let id1 = ChannelId::new();
        let mut ch1 = Channel::new(id1, None, ChannelConfig::default());
        ch1.complete();
        assert!(ch1.is_terminal());

        let id2 = ChannelId::new();
        let mut ch2 = Channel::new(id2, None, ChannelConfig::default());
        ch2.abort("test".into());
        assert!(ch2.is_terminal());

        let id3 = ChannelId::new();
        let ch3 = Channel::new(id3, None, ChannelConfig::default());
        assert!(!ch3.is_terminal());
    }

    #[test]
    fn channel_cannot_pause_from_terminal() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None, ChannelConfig::default());
        channel.complete();

        assert!(!channel.pause());
        assert!(!channel.await_approval("req"));
    }

    #[test]
    fn channel_cannot_complete_from_paused() {
        let id = ChannelId::new();
        let mut channel = Channel::new(id, None, ChannelConfig::default());
        assert!(channel.pause());

        // Must resume first
        assert!(!channel.complete());
        assert!(channel.resume());
        assert!(channel.complete());
    }
}

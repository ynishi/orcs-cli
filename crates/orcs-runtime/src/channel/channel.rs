//! Channel - unit of parallel execution.
//!
//! A [`BaseChannel`] represents an isolated execution context within the ORCS
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
//! use orcs_runtime::{BaseChannel, ChannelConfig, ChannelCore, ChannelMut, ChannelState};
//! use orcs_types::{ChannelId, Principal};
//!
//! let id = ChannelId::new();
//! let principal = Principal::System;
//! let mut channel = BaseChannel::new(id, None, ChannelConfig::interactive(), principal);
//!
//! assert!(channel.is_running());
//! assert_eq!(channel.priority(), 255);
//! assert!(channel.complete());
//! assert_eq!(channel.state(), &ChannelState::Completed);
//! ```

use super::config::ChannelConfig;
use super::traits::{ChannelCore, ChannelMut};
use orcs_types::{ChannelId, Principal};
use std::collections::HashSet;

/// State of a channel.
///
/// Channels start in [`Running`](ChannelState::Running) state and can
/// transition through various states. Terminal states are
/// [`Completed`](ChannelState::Completed) and [`Aborted`](ChannelState::Aborted).
///
/// # State Transitions
///
/// | From | To | Method |
/// |------|----|--------|
/// | Running | Completed | [`ChannelMut::complete()`] |
/// | Running | Aborted | [`ChannelMut::abort()`] |
/// | Running | Paused | [`ChannelMut::pause()`] |
/// | Running | AwaitingApproval | [`ChannelMut::await_approval()`] |
/// | Paused | Running | [`ChannelMut::resume()`] |
/// | AwaitingApproval | Running | [`ChannelMut::resolve_approval()`] |
/// | AwaitingApproval | Aborted | [`ChannelMut::abort()`] (rejected) |
// TODO: AwaitingApproval state is HIL-specific and tightly coupled to Channel.
// Consider moving approval state management to HilComponent or a dedicated
// ApprovalManager, using Event/Signal for state coordination instead of
// embedding it directly in ChannelState.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelState {
    /// Channel is actively running.
    Running,

    /// Channel is paused.
    ///
    /// Can be resumed with [`ChannelMut::resume()`].
    Paused,

    /// Channel is waiting for Human approval (HIL).
    ///
    /// Contains the request ID that needs approval.
    ///
    /// TODO: This variant couples HIL logic directly into Channel state.
    /// Future refactor: manage approval state in HilComponent, use Signal
    /// to notify Channel of blocking/unblocking.
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

/// Base channel implementation.
///
/// A `BaseChannel` represents an isolated execution context that can:
/// - Track its current state ([`ChannelState`])
/// - Maintain parent-child relationships
/// - Control behavior via [`ChannelConfig`]
/// - Transition through its lifecycle
/// - Hold a [`Principal`] for scope resolution
///
/// # Parent-Child Relationships
///
/// Channels form a tree structure:
/// - Primary channel has no parent (`parent() == None`)
/// - Child channels reference their parent
/// - A channel tracks all its children
///
/// # Ancestor Path (DOD Optimization)
///
/// Each channel maintains a cached path to all ancestors for O(1) descendant checks.
/// The path is ordered from immediate parent to root: `[parent, grandparent, ..., root]`.
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
/// use orcs_runtime::{BaseChannel, ChannelConfig, ChannelCore, ChannelMut, ChannelState};
/// use orcs_types::{ChannelId, Principal};
///
/// // Create a root channel
/// let root_id = ChannelId::new();
/// let mut root = BaseChannel::new(root_id, None, ChannelConfig::interactive(), Principal::System);
/// assert_eq!(root.priority(), 255);
/// assert!(root.can_spawn());
///
/// // Create a background child channel
/// let child_id = ChannelId::new();
/// let child = BaseChannel::new(child_id, Some(root_id), ChannelConfig::background(), Principal::System);
/// assert_eq!(child.priority(), 10);
/// assert!(!child.can_spawn());
///
/// root.add_child(child_id);
/// assert!(root.has_children());
/// ```
#[derive(Debug)]
pub struct BaseChannel {
    id: ChannelId,
    state: ChannelState,
    parent: Option<ChannelId>,
    children: HashSet<ChannelId>,
    config: ChannelConfig,
    /// Principal for scope resolution.
    principal: Principal,
    /// Cached ancestor path for O(1) descendant checks.
    /// Ordered: [parent, grandparent, ..., root]
    ancestor_path: Vec<ChannelId>,
}

impl BaseChannel {
    /// Creates a new channel in [`Running`](ChannelState::Running) state.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this channel
    /// * `parent` - Parent channel ID, or `None` for root channels
    /// * `config` - Configuration controlling channel behavior
    /// * `principal` - Principal for scope resolution
    ///
    /// # Note
    ///
    /// The `ancestor_path` is initialized empty. When spawning via [`World`](crate::World),
    /// use [`new_with_ancestors`](Self::new_with_ancestors) to properly set the ancestor path.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_runtime::{BaseChannel, ChannelConfig, ChannelCore};
    /// use orcs_types::{ChannelId, Principal};
    ///
    /// // Primary channel (no parent, highest priority)
    /// let root = BaseChannel::new(ChannelId::new(), None, ChannelConfig::interactive(), Principal::System);
    /// assert_eq!(root.priority(), 255);
    ///
    /// // Background child channel
    /// let parent_id = ChannelId::new();
    /// let child = BaseChannel::new(ChannelId::new(), Some(parent_id), ChannelConfig::background(), Principal::System);
    /// assert_eq!(child.priority(), 10);
    /// ```
    #[must_use]
    pub fn new(
        id: ChannelId,
        parent: Option<ChannelId>,
        config: ChannelConfig,
        principal: Principal,
    ) -> Self {
        Self {
            id,
            state: ChannelState::Running,
            parent,
            children: HashSet::new(),
            config,
            principal,
            ancestor_path: Vec::new(),
        }
    }

    /// Creates a new channel with a pre-computed ancestor path.
    ///
    /// This constructor is used by [`World`](crate::World) when spawning child channels
    /// to maintain the ancestor path for O(1) descendant checks.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this channel
    /// * `parent` - Parent channel ID
    /// * `config` - Configuration controlling channel behavior
    /// * `principal` - Principal for scope resolution
    /// * `ancestor_path` - Pre-computed path: [parent, grandparent, ..., root]
    #[must_use]
    pub fn new_with_ancestors(
        id: ChannelId,
        parent: ChannelId,
        config: ChannelConfig,
        principal: Principal,
        ancestor_path: Vec<ChannelId>,
    ) -> Self {
        Self {
            id,
            state: ChannelState::Running,
            parent: Some(parent),
            children: HashSet::new(),
            config,
            principal,
            ancestor_path,
        }
    }
}

// === ChannelCore trait implementation ===

impl ChannelCore for BaseChannel {
    fn id(&self) -> ChannelId {
        self.id
    }

    fn principal(&self) -> &Principal {
        &self.principal
    }

    fn state(&self) -> &ChannelState {
        &self.state
    }

    fn config(&self) -> &ChannelConfig {
        &self.config
    }

    fn parent(&self) -> Option<ChannelId> {
        self.parent
    }

    fn children(&self) -> &HashSet<ChannelId> {
        &self.children
    }

    fn ancestor_path(&self) -> &[ChannelId] {
        &self.ancestor_path
    }
}

// === ChannelMut trait implementation ===

impl ChannelMut for BaseChannel {
    fn complete(&mut self) -> bool {
        if matches!(self.state, ChannelState::Running) {
            self.state = ChannelState::Completed;
            true
        } else {
            false
        }
    }

    fn abort(&mut self, reason: String) -> bool {
        if !self.is_terminal() {
            self.state = ChannelState::Aborted { reason };
            true
        } else {
            false
        }
    }

    fn pause(&mut self) -> bool {
        if matches!(self.state, ChannelState::Running) {
            self.state = ChannelState::Paused;
            true
        } else {
            false
        }
    }

    fn resume(&mut self) -> bool {
        if matches!(self.state, ChannelState::Paused) {
            self.state = ChannelState::Running;
            true
        } else {
            false
        }
    }

    fn await_approval(&mut self, request_id: String) -> bool {
        if matches!(self.state, ChannelState::Running) {
            self.state = ChannelState::AwaitingApproval { request_id };
            true
        } else {
            false
        }
    }

    fn resolve_approval(&mut self, approval_id: &str) -> Option<String> {
        if let ChannelState::AwaitingApproval { request_id } = &self.state {
            if request_id == approval_id {
                let id = request_id.clone();
                self.state = ChannelState::Running;
                Some(id)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn add_child(&mut self, id: ChannelId) {
        self.children.insert(id);
    }

    fn remove_child(&mut self, id: &ChannelId) {
        self.children.remove(id);
    }
}

/// Type alias for backward compatibility.
///
/// New code should use [`BaseChannel`] directly or work with the
/// [`ChannelCore`] / [`ChannelMut`] traits.
pub type Channel = BaseChannel;

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::PrincipalId;

    fn test_principal() -> Principal {
        Principal::User(PrincipalId::new())
    }

    #[test]
    fn channel_creation() {
        let id = ChannelId::new();
        let channel = BaseChannel::new(id, None, ChannelConfig::interactive(), test_principal());

        assert_eq!(channel.id(), id);
        assert!(channel.is_running());
        assert!(channel.parent().is_none());
        assert!(!channel.has_children());
    }

    #[test]
    fn channel_with_parent() {
        let parent_id = ChannelId::new();
        let child_id = ChannelId::new();
        let channel = BaseChannel::new(
            child_id,
            Some(parent_id),
            ChannelConfig::default(),
            test_principal(),
        );

        assert_eq!(channel.parent(), Some(parent_id));
    }

    #[test]
    fn channel_principal() {
        let principal = Principal::System;
        let channel = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::default(),
            principal.clone(),
        );

        assert_eq!(channel.principal(), &principal);
    }

    #[test]
    fn channel_state_transition() {
        let id = ChannelId::new();
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());

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
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());

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

        let mut parent = BaseChannel::new(
            parent_id,
            None,
            ChannelConfig::interactive(),
            test_principal(),
        );
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

        let primary = BaseChannel::new(id, None, ChannelConfig::interactive(), test_principal());
        assert_eq!(primary.priority(), 255);

        let background = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::background(),
            test_principal(),
        );
        assert_eq!(background.priority(), 10);

        let tool = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::tool(),
            test_principal(),
        );
        assert_eq!(tool.priority(), 100);
    }

    #[test]
    fn channel_can_spawn() {
        let primary = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::interactive(),
            test_principal(),
        );
        assert!(primary.can_spawn());

        let background = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::background(),
            test_principal(),
        );
        assert!(!background.can_spawn());

        let tool = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::tool(),
            test_principal(),
        );
        assert!(tool.can_spawn());
    }

    #[test]
    fn channel_config_access() {
        let config = ChannelConfig::new(200, false);
        let channel = BaseChannel::new(ChannelId::new(), None, config, test_principal());

        assert_eq!(channel.config().priority(), 200);
        assert!(!channel.config().can_spawn());
    }

    // === M2 HIL State Tests ===

    #[test]
    fn channel_pause_resume() {
        let id = ChannelId::new();
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());

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
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());

        assert!(channel.await_approval("req-123".to_string()));
        assert!(channel.is_awaiting_approval());
        assert!(!channel.is_running());

        // Cannot await approval when already awaiting
        assert!(!channel.await_approval("req-456".to_string()));

        if let ChannelState::AwaitingApproval { request_id } = channel.state() {
            assert_eq!(request_id, "req-123");
        } else {
            panic!("Expected AwaitingApproval state");
        }
    }

    #[test]
    fn channel_resolve_approval() {
        let id = ChannelId::new();
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());

        assert!(channel.await_approval("req-123".to_string()));

        // Wrong approval_id → no match
        assert!(channel.resolve_approval("wrong-id").is_none());
        assert!(channel.is_awaiting_approval());

        // Correct approval_id → resolves
        let resolved_id = channel.resolve_approval("req-123");
        assert_eq!(resolved_id, Some("req-123".to_string()));
        assert!(channel.is_running());

        // Cannot resolve when running
        assert!(channel.resolve_approval("req-123").is_none());
    }

    #[test]
    fn channel_abort_from_awaiting() {
        let id = ChannelId::new();
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());

        assert!(channel.await_approval("req-123".to_string()));
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
        let mut ch1 = BaseChannel::new(id1, None, ChannelConfig::default(), test_principal());
        ch1.complete();
        assert!(ch1.is_terminal());

        let id2 = ChannelId::new();
        let mut ch2 = BaseChannel::new(id2, None, ChannelConfig::default(), test_principal());
        ch2.abort("test".into());
        assert!(ch2.is_terminal());

        let id3 = ChannelId::new();
        let ch3 = BaseChannel::new(id3, None, ChannelConfig::default(), test_principal());
        assert!(!ch3.is_terminal());
    }

    #[test]
    fn channel_cannot_pause_from_terminal() {
        let id = ChannelId::new();
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());
        channel.complete();

        assert!(!channel.pause());
        assert!(!channel.await_approval("req".to_string()));
    }

    #[test]
    fn channel_cannot_complete_from_paused() {
        let id = ChannelId::new();
        let mut channel = BaseChannel::new(id, None, ChannelConfig::default(), test_principal());
        assert!(channel.pause());

        // Must resume first
        assert!(!channel.complete());
        assert!(channel.resume());
        assert!(channel.complete());
    }

    // === Ancestor Path Tests (DOD) ===

    #[test]
    fn channel_root_has_empty_ancestor_path() {
        let root = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::interactive(),
            test_principal(),
        );
        assert!(root.ancestor_path().is_empty());
        assert_eq!(root.depth(), 0);
    }

    #[test]
    fn channel_with_ancestors() {
        let root_id = ChannelId::new();
        let parent_id = ChannelId::new();

        let child = BaseChannel::new_with_ancestors(
            ChannelId::new(),
            parent_id,
            ChannelConfig::default(),
            test_principal(),
            vec![parent_id, root_id],
        );

        assert_eq!(child.ancestor_path(), &[parent_id, root_id]);
        assert_eq!(child.depth(), 2);
    }

    #[test]
    fn channel_is_descendant_of() {
        let root_id = ChannelId::new();
        let parent_id = ChannelId::new();
        let other_id = ChannelId::new();

        let child = BaseChannel::new_with_ancestors(
            ChannelId::new(),
            parent_id,
            ChannelConfig::default(),
            test_principal(),
            vec![parent_id, root_id],
        );

        assert!(child.is_descendant_of(parent_id));
        assert!(child.is_descendant_of(root_id));
        assert!(!child.is_descendant_of(other_id));
        assert!(!child.is_descendant_of(child.id())); // not self
    }

    #[test]
    fn channel_root_not_descendant_of_anything() {
        let root = BaseChannel::new(
            ChannelId::new(),
            None,
            ChannelConfig::interactive(),
            test_principal(),
        );
        let other_id = ChannelId::new();

        assert!(!root.is_descendant_of(other_id));
        assert!(!root.is_descendant_of(root.id()));
    }
}

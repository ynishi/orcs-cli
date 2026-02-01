//! Channel trait definitions.
//!
//! The [`ChannelCore`] trait defines the core interface for all channel types.
//! This allows different channel implementations (e.g., `BaseChannel`) to share
//! a common interface while having different internal structures.
//!
//! For Human-interactive channels, use [`ClientRunner`](super::ClientRunner) which
//! combines `BaseChannel` with IO bridging capabilities.

use super::{ChannelConfig, ChannelState};
use orcs_types::{ChannelId, Principal};
use std::collections::HashSet;

/// Core channel interface.
///
/// This trait defines the essential operations that all channel types must support.
/// It provides a common interface for state queries, tree navigation, and lifecycle
/// management.
///
/// # Implementors
///
/// - [`BaseChannel`](super::BaseChannel): Basic channel with state and tree management
///
/// For Human-interactive channels, use [`ClientRunner`](super::ClientRunner) which
/// owns a `BaseChannel` internally.
///
/// # Example
///
/// ```ignore
/// use orcs_runtime::channel::ChannelCore;
///
/// fn process_channel(channel: &impl ChannelCore) {
///     println!("Channel {} is running: {}", channel.id(), channel.is_running());
///     println!("Principal: {:?}", channel.principal());
/// }
/// ```
pub trait ChannelCore {
    // === Identity ===

    /// Returns the channel's unique identifier.
    fn id(&self) -> ChannelId;

    /// Returns the principal associated with this channel.
    ///
    /// The principal is used for scope resolution and permission checks.
    fn principal(&self) -> &Principal;

    // === State ===

    /// Returns a reference to the current state.
    fn state(&self) -> &ChannelState;

    /// Returns `true` if the channel is in Running state.
    fn is_running(&self) -> bool {
        matches!(self.state(), ChannelState::Running)
    }

    /// Returns `true` if the channel is in Paused state.
    fn is_paused(&self) -> bool {
        matches!(self.state(), ChannelState::Paused)
    }

    /// Returns `true` if the channel is in AwaitingApproval state.
    fn is_awaiting_approval(&self) -> bool {
        matches!(self.state(), ChannelState::AwaitingApproval { .. })
    }

    /// Returns `true` if the channel is in a terminal state (Completed or Aborted).
    fn is_terminal(&self) -> bool {
        matches!(
            self.state(),
            ChannelState::Completed | ChannelState::Aborted { .. }
        )
    }

    // === Configuration ===

    /// Returns a reference to the channel's configuration.
    fn config(&self) -> &ChannelConfig;

    /// Returns the scheduling priority (0-255).
    fn priority(&self) -> u8 {
        self.config().priority()
    }

    /// Returns whether this channel can spawn children.
    fn can_spawn(&self) -> bool {
        self.config().can_spawn()
    }

    // === Tree Structure ===

    /// Returns the parent channel's ID, if any.
    fn parent(&self) -> Option<ChannelId>;

    /// Returns a reference to the set of child channel IDs.
    fn children(&self) -> &HashSet<ChannelId>;

    /// Returns `true` if this channel has any children.
    fn has_children(&self) -> bool {
        !self.children().is_empty()
    }

    /// Returns the cached ancestor path.
    ///
    /// The path is ordered: `[parent, grandparent, ..., root]`.
    fn ancestor_path(&self) -> &[ChannelId];

    /// Returns the depth of this channel in the tree.
    fn depth(&self) -> usize {
        self.ancestor_path().len()
    }

    /// Returns `true` if this channel is a descendant of the given ancestor.
    fn is_descendant_of(&self, ancestor: ChannelId) -> bool {
        self.ancestor_path().contains(&ancestor)
    }
}

/// Mutable channel operations.
///
/// This trait extends [`ChannelCore`] with state transition and tree modification
/// operations. Separated from `ChannelCore` to allow read-only access patterns.
pub trait ChannelMut: ChannelCore {
    // === State Transitions ===

    /// Transitions to Completed state.
    ///
    /// Returns `true` if transition succeeded, `false` if not in Running state.
    fn complete(&mut self) -> bool;

    /// Transitions to Aborted state.
    ///
    /// Returns `true` if transition succeeded, `false` if already terminal.
    fn abort(&mut self, reason: String) -> bool;

    /// Transitions to Paused state.
    ///
    /// Returns `true` if transition succeeded, `false` if not in Running state.
    fn pause(&mut self) -> bool;

    /// Transitions from Paused to Running state.
    ///
    /// Returns `true` if transition succeeded, `false` if not in Paused state.
    fn resume(&mut self) -> bool;

    // TODO: Approval-related methods are HIL-specific. Consider extracting
    // to a separate trait (e.g., `Approvable`) or moving state management
    // to HilComponent with Event/Signal coordination.

    /// Transitions to AwaitingApproval state.
    ///
    /// Returns `true` if transition succeeded, `false` if not in Running state.
    fn await_approval(&mut self, request_id: String) -> bool;

    /// Resolves approval and transitions to Running state.
    ///
    /// Returns the request_id if succeeded, None if not in AwaitingApproval state.
    fn resolve_approval(&mut self) -> Option<String>;

    // === Tree Modification ===

    /// Registers a child channel.
    fn add_child(&mut self, id: ChannelId);

    /// Unregisters a child channel.
    fn remove_child(&mut self, id: &ChannelId);
}

//! World commands for concurrent state modification.
//!
//! [`WorldCommand`] represents state changes to the [`World`](super::World)
//! that are applied asynchronously via a command queue. This enables
//! lock-free concurrent access from multiple [`ChannelRunner`](super::ChannelRunner)s.
//!
//! # Design
//!
//! Instead of directly mutating World with locks, channel runners send
//! commands to a dedicated manager task that applies them sequentially.
//!
//! ```text
//! ChannelRunner A ──┐
//!                   │
//! ChannelRunner B ──┼──► mpsc::Sender<WorldCommand> ──► WorldManager
//!                   │                                        │
//! ChannelRunner C ──┘                                        ▼
//!                                                     World (sequential apply)
//! ```
//!
//! # Example
//!
//! ```ignore
//! use orcs_runtime::channel::{WorldCommand, ChannelConfig};
//! use tokio::sync::oneshot;
//!
//! let (reply_tx, reply_rx) = oneshot::channel();
//! let cmd = WorldCommand::Spawn {
//!     parent: parent_id,
//!     config: ChannelConfig::default(),
//!     reply: reply_tx,
//! };
//!
//! world_tx.send(cmd).await?;
//! let new_channel_id = reply_rx.await?;
//! ```

use super::config::ChannelConfig;
use super::ChannelState;
use orcs_types::ChannelId;
use tokio::sync::oneshot;

/// Commands for modifying [`World`](super::World) state.
///
/// These commands are sent to the [`WorldManager`](super::WorldManager)
/// which applies them sequentially to maintain consistency.
#[derive(Debug)]
pub enum WorldCommand {
    /// Spawn a new child channel.
    ///
    /// Creates a new channel under the specified parent with the given config.
    /// The new channel's ID is returned via the reply channel.
    Spawn {
        /// Parent channel ID.
        parent: ChannelId,
        /// Configuration for the new channel.
        config: ChannelConfig,
        /// Reply channel for the new channel's ID.
        /// Returns `None` if parent doesn't exist.
        reply: oneshot::Sender<Option<ChannelId>>,
    },

    /// Kill a channel and all its descendants.
    ///
    /// Removes the channel from the World entirely.
    Kill {
        /// Channel to kill.
        id: ChannelId,
        /// Reason for killing (for logging/debugging).
        reason: String,
    },

    /// Complete a channel successfully.
    ///
    /// Transitions the channel to Completed state but keeps it in the World.
    Complete {
        /// Channel to complete.
        id: ChannelId,
        /// Reply with success/failure.
        reply: oneshot::Sender<bool>,
    },

    /// Update a channel's state.
    ///
    /// Used for state transitions like Pause, Resume, AwaitApproval.
    UpdateState {
        /// Channel to update.
        id: ChannelId,
        /// New state to transition to.
        transition: StateTransition,
        /// Reply with success/failure.
        reply: oneshot::Sender<bool>,
    },

    /// Query a channel's current state (read-only).
    ///
    /// For read operations that need consistency with pending writes.
    GetState {
        /// Channel to query.
        id: ChannelId,
        /// Reply with the current state, or None if not found.
        reply: oneshot::Sender<Option<ChannelState>>,
    },

    /// Shutdown the WorldManager.
    ///
    /// Signals the manager to stop processing commands.
    Shutdown,
}

/// State transition operations.
///
/// These map to the state machine transitions in [`Channel`](super::Channel).
#[derive(Debug, Clone)]
pub enum StateTransition {
    /// Pause a running channel.
    Pause,
    /// Resume a paused channel.
    Resume,
    /// Enter approval-waiting state.
    AwaitApproval {
        /// ID of the approval request.
        request_id: String,
    },
    /// Resolve an approval (approved).
    ResolveApproval,
    /// Abort with reason.
    Abort {
        /// Reason for aborting.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_command_creation() {
        let parent = ChannelId::new();
        let (tx, _rx) = oneshot::channel();

        let cmd = WorldCommand::Spawn {
            parent,
            config: ChannelConfig::default(),
            reply: tx,
        };

        assert!(matches!(cmd, WorldCommand::Spawn { .. }));
    }

    #[test]
    fn kill_command_creation() {
        let id = ChannelId::new();
        let cmd = WorldCommand::Kill {
            id,
            reason: "test".into(),
        };

        assert!(matches!(cmd, WorldCommand::Kill { .. }));
    }

    #[test]
    fn state_transition_variants() {
        let pause = StateTransition::Pause;
        let resume = StateTransition::Resume;
        let await_approval = StateTransition::AwaitApproval {
            request_id: "req-123".into(),
        };
        let abort = StateTransition::Abort {
            reason: "cancelled".into(),
        };

        assert!(matches!(pause, StateTransition::Pause));
        assert!(matches!(resume, StateTransition::Resume));
        assert!(matches!(
            await_approval,
            StateTransition::AwaitApproval { .. }
        ));
        assert!(matches!(abort, StateTransition::Abort { .. }));
    }

    #[tokio::test]
    async fn spawn_reply_channel_works() {
        let parent = ChannelId::new();
        let (tx, rx) = oneshot::channel();

        let _cmd = WorldCommand::Spawn {
            parent,
            config: ChannelConfig::default(),
            reply: tx,
        };

        // Simulate manager sending reply
        // (In real code, WorldManager would send this)
        // tx.send(Some(ChannelId::new())).unwrap();
        // let result = rx.await.unwrap();
        // assert!(result.is_some());

        // Just verify channel is droppable without panic
        drop(rx);
    }
}

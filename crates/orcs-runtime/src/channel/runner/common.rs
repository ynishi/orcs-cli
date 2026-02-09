//! Common helpers for ChannelRunner implementations.
//!
//! Shared logic for signal handling, state transitions, and component dispatch.

use crate::channel::command::{StateTransition, WorldCommand};
use crate::channel::traits::ChannelCore;
use crate::channel::World;
use orcs_component::Component;
use orcs_event::{Signal, SignalKind, SignalResponse};
use orcs_types::ChannelId;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, warn};

/// Timeout for abort confirmation (milliseconds).
///
/// Short timeout since abort is fire-and-forget during shutdown.
pub const ABORT_TIMEOUT_MS: u64 = 100;

/// Result of signal processing at the channel level.
#[derive(Debug, Clone)]
pub enum SignalAction {
    /// Continue running the event loop.
    Continue,
    /// Stop the runner (channel terminated).
    Stop {
        /// Reason for stopping.
        reason: String,
    },
    /// Apply a state transition.
    Transition(StateTransition),
}

/// Dispatches a signal to a Component and returns the action to take.
///
/// # Arguments
///
/// * `signal` - The signal to dispatch
/// * `component` - The component to receive the signal
///
/// # Returns
///
/// `SignalAction::Stop` if component requests abort, otherwise `Continue`.
pub async fn dispatch_signal_to_component(
    signal: &Signal,
    component: &Arc<Mutex<Box<dyn Component>>>,
) -> SignalAction {
    let response = {
        let mut comp = component.lock().await;
        comp.on_signal(signal)
    };

    match response {
        SignalResponse::Abort => SignalAction::Stop {
            reason: "component aborted".to_string(),
        },
        SignalResponse::Handled | SignalResponse::Ignored => SignalAction::Continue,
    }
}

/// Determines the channel-level action based on signal kind.
///
/// This is called after the component has processed the signal.
///
/// # Arguments
///
/// * `kind` - The signal kind to process
///
/// # Returns
///
/// The action to take at the channel level.
#[must_use]
pub fn determine_channel_action(kind: &SignalKind) -> SignalAction {
    match kind {
        SignalKind::Veto => SignalAction::Stop {
            reason: "veto received".to_string(),
        },
        SignalKind::Cancel => SignalAction::Stop {
            reason: "cancelled".to_string(),
        },
        SignalKind::Pause => SignalAction::Transition(StateTransition::Pause),
        SignalKind::Resume => SignalAction::Transition(StateTransition::Resume),
        SignalKind::Approve { .. } => SignalAction::Transition(StateTransition::ResolveApproval),
        // Reject clears the pending approval but does NOT stop the channel.
        // Component handles rejection via on_signal and notifies via Emitter.
        SignalKind::Reject { .. } => SignalAction::Continue,
        SignalKind::Steer { .. } | SignalKind::Modify { .. } => SignalAction::Continue,
    }
}

/// Sends a state transition command to the WorldManager.
///
/// Waits for confirmation from the WorldManager.
///
/// # Arguments
///
/// * `world_tx` - Sender for world commands
/// * `id` - Channel ID to update
/// * `transition` - State transition to apply
///
/// # Returns
///
/// `true` if the transition was accepted, `false` otherwise.
pub async fn send_transition(
    world_tx: &mpsc::Sender<WorldCommand>,
    id: ChannelId,
    transition: StateTransition,
) -> bool {
    let transition_name = format!("{:?}", transition);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let cmd = WorldCommand::UpdateState {
        id,
        transition,
        reply: reply_tx,
    };

    if world_tx.send(cmd).await.is_err() {
        warn!("Runner {}: failed to send {} command", id, transition_name);
        return false;
    }

    match reply_rx.await {
        Ok(true) => {
            debug!("Runner {}: {} confirmed", id, transition_name);
            true
        }
        Ok(false) => {
            warn!("Runner {}: {} rejected by World", id, transition_name);
            false
        }
        Err(_) => {
            warn!("Runner {}: {} reply channel dropped", id, transition_name);
            false
        }
    }
}

/// Sends an abort command with best-effort confirmation.
///
/// Uses a short timeout since the runner is stopping anyway.
///
/// # Arguments
///
/// * `world_tx` - Sender for world commands
/// * `id` - Channel ID to abort
/// * `reason` - Reason for aborting
pub async fn send_abort(world_tx: &mpsc::Sender<WorldCommand>, id: ChannelId, reason: &str) {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let cmd = WorldCommand::UpdateState {
        id,
        transition: StateTransition::Abort {
            reason: reason.to_string(),
        },
        reply: reply_tx,
    };

    if world_tx.send(cmd).await.is_err() {
        debug!(
            "Runner {}: failed to send abort command (reason: {})",
            id, reason
        );
        return;
    }

    // Best-effort wait with short timeout
    match tokio::time::timeout(Duration::from_millis(ABORT_TIMEOUT_MS), reply_rx).await {
        Ok(Ok(true)) => debug!("Runner {}: abort confirmed", id),
        Ok(Ok(false)) => warn!("Runner {}: abort rejected by World", id),
        Ok(Err(_)) => warn!("Runner {}: abort reply channel dropped", id),
        Err(_) => debug!("Runner {}: abort timed out (fire-and-forget)", id),
    }
}

/// Checks if a channel is still active (not in terminal state).
///
/// # Arguments
///
/// * `world` - Reference to the World
/// * `id` - Channel ID to check
///
/// # Returns
///
/// `true` if the channel exists and is not in a terminal state.
pub async fn is_channel_active(world: &Arc<RwLock<World>>, id: ChannelId) -> bool {
    let world = world.read().await;
    world.get(&id).map(|ch| !ch.is_terminal()).unwrap_or(false)
}

/// Checks if a channel is paused.
///
/// # Arguments
///
/// * `world` - Reference to the World
/// * `id` - Channel ID to check
///
/// # Returns
///
/// `true` if the channel exists and is paused.
pub async fn is_channel_paused(world: &Arc<RwLock<World>>, id: ChannelId) -> bool {
    let world = world.read().await;
    world.get(&id).map(|ch| ch.is_paused()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determine_channel_action_veto() {
        let action = determine_channel_action(&SignalKind::Veto);
        assert!(matches!(action, SignalAction::Stop { .. }));
    }

    #[test]
    fn determine_channel_action_cancel() {
        let action = determine_channel_action(&SignalKind::Cancel);
        assert!(matches!(action, SignalAction::Stop { .. }));
    }

    #[test]
    fn determine_channel_action_pause() {
        let action = determine_channel_action(&SignalKind::Pause);
        assert!(matches!(
            action,
            SignalAction::Transition(StateTransition::Pause)
        ));
    }

    #[test]
    fn determine_channel_action_resume() {
        let action = determine_channel_action(&SignalKind::Resume);
        assert!(matches!(
            action,
            SignalAction::Transition(StateTransition::Resume)
        ));
    }

    #[test]
    fn determine_channel_action_approve() {
        let action = determine_channel_action(&SignalKind::Approve {
            approval_id: "req-1".to_string(),
        });
        assert!(matches!(
            action,
            SignalAction::Transition(StateTransition::ResolveApproval)
        ));
    }

    #[test]
    fn determine_channel_action_reject() {
        let action = determine_channel_action(&SignalKind::Reject {
            approval_id: "req-1".to_string(),
            reason: Some("denied".to_string()),
        });
        // Reject should continue, not stop the channel
        assert!(matches!(action, SignalAction::Continue));
    }

    #[test]
    fn determine_channel_action_steer() {
        let action = determine_channel_action(&SignalKind::Steer {
            message: "hint".to_string(),
        });
        assert!(matches!(action, SignalAction::Continue));
    }
}

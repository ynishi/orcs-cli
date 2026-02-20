//! WorldManager - Concurrent World state management.
//!
//! [`WorldManager`] provides safe concurrent access to the [`World`](super::World)
//! by processing state modifications through a command queue.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                        WorldManager                             │
//! │                                                                 │
//! │  ┌──────────────────┐    ┌─────────────────────────────────┐  │
//! │  │ command_rx       │───►│ apply_command() loop            │  │
//! │  │ (mpsc::Receiver) │    │ - Spawn/Kill/Complete/Update    │  │
//! │  └──────────────────┘    └─────────────────────────────────┘  │
//! │                                         │                      │
//! │                                         ▼                      │
//! │                          ┌─────────────────────────────┐      │
//! │                          │ Arc<RwLock<World>>          │      │
//! │                          │ - Sequential writes         │      │
//! │                          │ - Parallel reads via clone  │      │
//! │                          └─────────────────────────────┘      │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use orcs_runtime::channel::{WorldManager, WorldCommand};
//!
//! // Create manager
//! let (manager, cmd_tx) = WorldManager::new();
//!
//! // Get read-only World access
//! let world = manager.world();
//!
//! // Spawn manager task
//! tokio::spawn(manager.run());
//!
//! // Send commands from any task
//! let (reply_tx, reply_rx) = oneshot::channel();
//! cmd_tx.send(WorldCommand::Spawn { ... }).await?;
//! ```

use super::command::{StateTransition, WorldCommand};
use super::traits::{ChannelCore, ChannelMut};
use super::World;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

/// Default command channel buffer size.
///
/// 256 commands provides sufficient buffering for concurrent channel operations.
/// Higher values increase memory usage; lower values may cause backpressure
/// during burst spawn/kill operations.
const COMMAND_BUFFER_SIZE: usize = 256;

/// Handle for sending commands to [`WorldManager`].
pub type WorldCommandSender = mpsc::Sender<WorldCommand>;

/// Manages World state with concurrent access support.
///
/// `WorldManager` owns the `World` and processes state modifications
/// through a command queue. This enables multiple [`ChannelRunner`]s
/// to modify the World without holding locks.
///
/// # Thread Safety
///
/// - **Reads**: Via `Arc<RwLock<World>>` clone (parallel reads allowed)
/// - **Writes**: Via command queue (sequential, no contention)
///
/// # Example
///
/// ```ignore
/// let (manager, cmd_tx) = WorldManager::new();
///
/// // Clone for read access
/// let world_read = manager.world();
///
/// // Spawn the manager loop
/// tokio::spawn(manager.run());
///
/// // Send commands
/// cmd_tx.send(WorldCommand::Kill { id, reason }).await?;
/// ```
pub struct WorldManager {
    /// The managed World instance.
    world: Arc<RwLock<World>>,
    /// Receiver for incoming commands.
    command_rx: mpsc::Receiver<WorldCommand>,
}

impl WorldManager {
    /// Creates a new WorldManager with an empty World.
    ///
    /// Returns the manager and a sender for commands.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (manager, cmd_tx) = WorldManager::new();
    /// tokio::spawn(manager.run());
    /// ```
    #[must_use]
    pub fn new() -> (Self, WorldCommandSender) {
        Self::with_world(World::new())
    }

    /// Creates a WorldManager with an existing World.
    ///
    /// Use this when you need to initialize the World before
    /// starting the manager (e.g., creating the IO channel).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut world = World::new();
    /// let io = world.create_channel(ChannelConfig::interactive());
    ///
    /// let (manager, cmd_tx) = WorldManager::with_world(world);
    /// ```
    #[must_use]
    pub fn with_world(world: World) -> (Self, WorldCommandSender) {
        let (tx, rx) = mpsc::channel(COMMAND_BUFFER_SIZE);
        let manager = Self {
            world: Arc::new(RwLock::new(world)),
            command_rx: rx,
        };
        (manager, tx)
    }

    /// Returns a clone of the World handle for read access.
    ///
    /// The returned `Arc<RwLock<World>>` can be shared across tasks
    /// for concurrent read access.
    #[must_use]
    pub fn world(&self) -> Arc<RwLock<World>> {
        Arc::clone(&self.world)
    }

    /// Runs the command processing loop.
    ///
    /// This method consumes the manager and runs until:
    /// - A `Shutdown` command is received
    /// - All command senders are dropped (channel closed)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (manager, cmd_tx) = WorldManager::new();
    /// let handle = tokio::spawn(manager.run());
    ///
    /// // ... use cmd_tx ...
    ///
    /// cmd_tx.send(WorldCommand::Shutdown).await?;
    /// handle.await?;
    /// ```
    pub async fn run(mut self) {
        info!("WorldManager started");

        while let Some(cmd) = self.command_rx.recv().await {
            if matches!(cmd, WorldCommand::Shutdown) {
                info!("WorldManager received shutdown");
                break;
            }
            self.apply_command(cmd).await;
        }

        info!("WorldManager stopped");
    }

    /// Applies a single command to the World.
    async fn apply_command(&self, cmd: WorldCommand) {
        match cmd {
            WorldCommand::Spawn {
                parent,
                config,
                reply,
            } => {
                let result = {
                    let mut world = self.world.write().await;
                    world.spawn_with(parent, config)
                };
                debug!("Spawn command: parent={}, result={:?}", parent, result);
                let _ = reply.send(result);
            }

            WorldCommand::SpawnWithId {
                parent,
                id,
                config,
                reply,
            } => {
                let result = {
                    let mut world = self.world.write().await;
                    world.spawn_with_id(parent, id, config)
                };
                debug!(
                    "SpawnWithId command: parent={}, id={}, success={}",
                    parent,
                    id,
                    result.is_some()
                );
                let _ = reply.send(result.is_some());
            }

            WorldCommand::Kill { id, reason } => {
                let mut world = self.world.write().await;
                world.kill(id, reason.clone());
                debug!("Kill command: id={}, reason={}", id, reason);
            }

            WorldCommand::Complete { id, reply } => {
                let result = {
                    let mut world = self.world.write().await;
                    world.complete(id)
                };
                debug!("Complete command: id={}, result={}", id, result);
                let _ = reply.send(result);
            }

            WorldCommand::UpdateState {
                id,
                transition,
                reply,
            } => {
                let result = {
                    let mut world = self.world.write().await;
                    if let Some(channel) = world.get_mut(&id) {
                        match transition {
                            StateTransition::Pause => channel.pause(),
                            StateTransition::Resume => channel.resume(),
                            StateTransition::AwaitApproval { request_id } => {
                                channel.await_approval(request_id)
                            }
                            StateTransition::ResolveApproval { approval_id } => {
                                channel.resolve_approval(&approval_id).is_some()
                            }
                            StateTransition::Abort { reason } => channel.abort(reason),
                        }
                    } else {
                        warn!("UpdateState: channel {} not found", id);
                        false
                    }
                };
                debug!("UpdateState command: id={}, result={}", id, result);
                let _ = reply.send(result);
            }

            WorldCommand::GetState { id, reply } => {
                let result = {
                    let world = self.world.read().await;
                    world.get(&id).map(|ch| ch.state().clone())
                };
                debug!("GetState command: id={}, found={}", id, result.is_some());
                let _ = reply.send(result);
            }

            WorldCommand::Shutdown => {
                // Handled in run() loop
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelConfig, ChannelState};

    #[tokio::test]
    async fn manager_creation() {
        let (manager, _tx) = WorldManager::new();
        let world = manager.world();
        let w = world.read().await;
        assert_eq!(w.channel_count(), 0);
    }

    #[tokio::test]
    async fn manager_with_existing_world() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, _tx) = WorldManager::with_world(world);
        let w = manager.world();
        let r = w.read().await;
        assert!(r.get(&io).is_some());
    }

    #[tokio::test]
    async fn spawn_command() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, tx) = WorldManager::with_world(world);
        let world_handle = manager.world();

        // Start manager
        let manager_handle = tokio::spawn(manager.run());

        // Send spawn command
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WorldCommand::Spawn {
            parent: io,
            config: ChannelConfig::background(),
            reply: reply_tx,
        })
        .await
        .unwrap();

        // Wait for reply
        let child_id = reply_rx.await.unwrap();
        assert!(child_id.is_some());

        // Verify in world
        {
            let w = world_handle.read().await;
            assert_eq!(w.channel_count(), 2);
        }

        // Shutdown
        tx.send(WorldCommand::Shutdown).await.unwrap();
        manager_handle.await.unwrap();
    }

    #[tokio::test]
    async fn kill_command() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());
        let child = world.spawn(io).unwrap();

        let (manager, tx) = WorldManager::with_world(world);
        let world_handle = manager.world();

        let manager_handle = tokio::spawn(manager.run());

        // Kill child
        tx.send(WorldCommand::Kill {
            id: child,
            reason: "test".into(),
        })
        .await
        .unwrap();

        // Give time to process
        tokio::task::yield_now().await;

        // Verify
        {
            let w = world_handle.read().await;
            assert_eq!(w.channel_count(), 1);
            assert!(w.get(&child).is_none());
        }

        tx.send(WorldCommand::Shutdown).await.unwrap();
        manager_handle.await.unwrap();
    }

    #[tokio::test]
    async fn complete_command() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, tx) = WorldManager::with_world(world);
        let world_handle = manager.world();

        let manager_handle = tokio::spawn(manager.run());

        // Complete IO channel
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WorldCommand::Complete {
            id: io,
            reply: reply_tx,
        })
        .await
        .unwrap();

        let result = reply_rx.await.unwrap();
        assert!(result);

        // Verify state
        {
            let w = world_handle.read().await;
            let ch = w.get(&io).unwrap();
            assert_eq!(ch.state(), &ChannelState::Completed);
        }

        tx.send(WorldCommand::Shutdown).await.unwrap();
        manager_handle.await.unwrap();
    }

    #[tokio::test]
    async fn update_state_pause_resume() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, tx) = WorldManager::with_world(world);
        let world_handle = manager.world();

        let manager_handle = tokio::spawn(manager.run());

        // Pause
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WorldCommand::UpdateState {
            id: io,
            transition: StateTransition::Pause,
            reply: reply_tx,
        })
        .await
        .unwrap();

        assert!(reply_rx.await.unwrap());

        {
            let w = world_handle.read().await;
            assert!(w.get(&io).unwrap().is_paused());
        }

        // Resume
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WorldCommand::UpdateState {
            id: io,
            transition: StateTransition::Resume,
            reply: reply_tx,
        })
        .await
        .unwrap();

        assert!(reply_rx.await.unwrap());

        {
            let w = world_handle.read().await;
            assert!(w.get(&io).unwrap().is_running());
        }

        tx.send(WorldCommand::Shutdown).await.unwrap();
        manager_handle.await.unwrap();
    }

    #[tokio::test]
    async fn get_state_command() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, tx) = WorldManager::with_world(world);

        let manager_handle = tokio::spawn(manager.run());

        // Get existing
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WorldCommand::GetState {
            id: io,
            reply: reply_tx,
        })
        .await
        .unwrap();

        let state = reply_rx.await.unwrap();
        assert_eq!(state, Some(ChannelState::Running));

        // Get non-existing
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WorldCommand::GetState {
            id: orcs_types::ChannelId::new(),
            reply: reply_tx,
        })
        .await
        .unwrap();

        let state = reply_rx.await.unwrap();
        assert!(state.is_none());

        tx.send(WorldCommand::Shutdown).await.unwrap();
        manager_handle.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_command() {
        let (manager, tx) = WorldManager::new();
        let manager_handle = tokio::spawn(manager.run());

        tx.send(WorldCommand::Shutdown).await.unwrap();

        // Should complete without hanging
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(100), manager_handle).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn concurrent_read_access() {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, tx) = WorldManager::with_world(world);
        let world1 = manager.world();
        let world2 = manager.world();

        let manager_handle = tokio::spawn(manager.run());

        // Concurrent reads should not block
        let (r1, r2) = tokio::join!(
            async {
                let w = world1.read().await;
                w.get(&io).is_some()
            },
            async {
                let w = world2.read().await;
                w.channel_count()
            }
        );

        assert!(r1);
        assert_eq!(r2, 1);

        tx.send(WorldCommand::Shutdown).await.unwrap();
        manager_handle.await.unwrap();
    }
}

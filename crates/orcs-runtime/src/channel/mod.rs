//! Channel and World management for ORCS CLI.
//!
//! This crate provides the parallel execution infrastructure for the ORCS
//! (Orchestrated Runtime for Collaborative Systems) architecture.
//!
//! # Architecture Overview
//!
//! Channels execute in parallel, each with its own [`ChannelRunner`]:
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────────────────┐
//! │                              OrcsEngine                                    │
//! │                                                                            │
//! │  ┌──────────────────────────────────────────────────────────────────────┐ │
//! │  │                         WorldManager                                  │ │
//! │  │   - Arc<RwLock<World>> (read: parallel, write: command queue)        │ │
//! │  │   - mpsc::Receiver<WorldCommand>                                     │ │
//! │  └──────────────────────────────────────────────────────────────────────┘ │
//! │                                    │                                       │
//! │         ┌──────────────────────────┼──────────────────────────┐           │
//! │         │                          │                          │           │
//! │         ▼                          ▼                          ▼           │
//! │  ┌─────────────┐           ┌─────────────┐           ┌─────────────┐     │
//! │  │ChannelRunner│           │ChannelRunner│           │ChannelRunner│     │
//! │  │  (Primary)  │──spawn──▶ │  (Agent)    │──spawn──▶ │  (Tool)     │     │
//! │  │             │           │             │           │             │     │
//! │  │ event_rx ◄──┼───────────┼─────────────┼───────────┼── EventBus  │     │
//! │  └─────────────┘           └─────────────┘           └─────────────┘     │
//! │         │                          │                          │           │
//! │         └──────────────────────────┴──────────────────────────┘           │
//! │                                    │                                       │
//! │                                    ▼                                       │
//! │                    mpsc::Sender<WorldCommand>                              │
//! └────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Parallel Execution Model
//!
//! Each channel runs in its own tokio task via [`ChannelRunner`]:
//!
//! - **Event injection**: External events arrive via [`ChannelHandle::inject()`]
//! - **Signal broadcast**: Signals reach all runners via broadcast channel
//! - **World modification**: Changes go through [`WorldCommand`] queue
//! - **Read access**: Parallel reads via `Arc<RwLock<World>>`
//!
//! # Core Concepts
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`Channel`] | Unit of parallel execution with state |
//! | [`World`] | Manages all Channel lifecycles |
//! | [`ChannelState`] | Running, Completed, or Aborted |
//!
//! # Channel Lifecycle
//!
//! ```text
//! ┌─────────┐
//! │ Running │ ─────────────────────────────────────┐
//! └────┬────┘                                      │
//!      │                                           │
//!      ├───complete()───▶ ┌───────────┐            │
//!      │                  │ Completed │            │
//!      │                  └───────────┘            │
//!      │                                           │
//!      └───abort()──────▶ ┌─────────┐              │
//!                         │ Aborted │              │
//!                         └─────────┘              │
//!                                                  │
//!      Note: No transitions from terminal states ◀─┘
//! ```
//!
//! # Parent-Child Relationships
//!
//! Channels form a tree structure:
//!
//! - **Primary Channel**: Root channel (no parent)
//! - **Child Channels**: Spawned from a parent
//! - **Cascade kill**: Killing a parent kills all descendants
//!
//! # Example: Basic Usage
//!
//! ```
//! use orcs_runtime::{World, ChannelConfig, ChannelCore, ChannelState};
//!
//! // Create World
//! let mut world = World::new();
//!
//! // Create root channel (IO)
//! let io = world.create_channel(ChannelConfig::interactive());
//! assert!(world.get(&io).is_some());
//!
//! // Spawn child channel
//! let child = world.spawn(io).expect("parent exists");
//! assert_eq!(world.channel_count(), 2);
//!
//! // Complete the child
//! world.complete(child);
//! assert_eq!(
//!     world.get(&child).unwrap().state(),
//!     &ChannelState::Completed
//! );
//! ```
//!
//! # Example: Channel Tree
//!
//! ```
//! use orcs_runtime::{World, ChannelConfig};
//!
//! let mut world = World::new();
//! let io = world.create_channel(ChannelConfig::interactive());
//!
//! // Build hierarchical channel tree
//! let agent = world.spawn(io).unwrap();
//! let tool1 = world.spawn(agent).unwrap();
//! let tool2 = world.spawn(agent).unwrap();
//!
//! // Killing agent also kills tool1 and tool2
//! world.kill(agent, "task cancelled".to_string());
//! assert_eq!(world.channel_count(), 1); // only IO remains
//! ```
//!
//! # Error Handling
//!
//! [`ChannelError`] provides structured error types:
//!
//! | Error | Code | Description |
//! |-------|------|-------------|
//! | `NotFound` | `CHANNEL_NOT_FOUND` | Channel does not exist |
//! | `InvalidState` | `INVALID_STATE` | Invalid state transition |
//! | `ParentNotFound` | `PARENT_NOT_FOUND` | Parent channel missing |
//! | `AlreadyExists` | `ALREADY_EXISTS` | Channel ID collision |
//!
//! # Crate Structure
//!
//! - [`Channel`] - Parallel execution unit
//! - [`ChannelState`] - State enumeration
//! - [`World`] - Channel lifecycle manager
//! - [`ChannelError`] - Error types
//!
//! # Related Crates
//!
//! - [`orcs_types`] - Core identifier types ([`ChannelId`])
//! - [`orcs_event`] - Event types for channel communication
//! - `orcs_component` - Components that run within channels
//!
//! [`ChannelId`]: orcs_types::ChannelId

#[allow(clippy::module_inception)]
mod channel;
mod client;
mod command;
mod config;
mod error;
mod manager;
mod runner;
mod traits;
mod world;

pub use channel::{BaseChannel, Channel, ChannelState};
pub use client::ClientChannel;
pub use command::{StateTransition, WorldCommand};
pub use config::{priority, ChannelConfig, MaxPrivilege};
pub use error::ChannelError;
pub use manager::{WorldCommandSender, WorldManager};
pub use runner::{ChannelHandle, ChannelRunner, ChannelRunnerFactory, Event};
pub use traits::{ChannelCore, ChannelMut};
pub use world::World;

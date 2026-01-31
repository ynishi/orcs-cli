//! Channel and World management for ORCS CLI.
//!
//! This crate provides the parallel execution infrastructure for the ORCS
//! (Orchestrated Runtime for Collaborative Systems) architecture.
//!
//! # Architecture Overview
//!
//! Channels are the unit of parallel execution, managed by the World:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │                            OrcsEngine                                │
//! │  ┌────────────────────────────────────────────────────────────────┐  │
//! │  │                           World                                │  │
//! │  │   - Channel spawn/kill                                         │  │
//! │  │   - Parent-child relationships                                 │  │
//! │  │   - State management                                           │  │
//! │  └────────────────────────────────────────────────────────────────┘  │
//! │                                    │                                  │
//! │          ┌─────────────────────────┼─────────────────────────┐       │
//! │          │                         │                         │       │
//! │          ▼                         ▼                         ▼       │
//! │    ┌──────────┐              ┌──────────┐              ┌──────────┐  │
//! │    │ Primary  │───spawn───▶  │  Agent   │───spawn───▶  │   Tool   │  │
//! │    │ Channel  │              │ Channel  │              │ Channel  │  │
//! │    └──────────┘              └──────────┘              └──────────┘  │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
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
//! use orcs_runtime::{World, ChannelState};
//!
//! // Create World
//! let mut world = World::new();
//!
//! // Create primary channel (root)
//! let primary = world.create_primary().unwrap();
//! assert!(world.get(&primary).is_some());
//!
//! // Spawn child channel
//! let child = world.spawn(primary).expect("parent exists");
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
//! use orcs_runtime::World;
//!
//! let mut world = World::new();
//! let primary = world.create_primary().unwrap();
//!
//! // Build hierarchical channel tree
//! let agent = world.spawn(primary).unwrap();
//! let tool1 = world.spawn(agent).unwrap();
//! let tool2 = world.spawn(agent).unwrap();
//!
//! // Killing agent also kills tool1 and tool2
//! world.kill(agent, "task cancelled".to_string());
//! assert_eq!(world.channel_count(), 1); // only primary remains
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
mod error;
mod world;

pub use channel::{Channel, ChannelState};
pub use error::ChannelError;
pub use world::World;

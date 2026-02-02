//! Channel runners for parallel execution.
//!
//! This module provides execution contexts for channels:
//!
//! - [`ChannelRunner`] - Basic runner for background/agent channels
//! - [`ClientRunner`] - Runner with IO bridging for Human-interactive channels
//!
//! # Architecture
//!
//! Both runners share common logic via the [`common`] module:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                        common.rs                             │
//! │  - dispatch_signal_to_component()                            │
//! │  - determine_channel_action()                                │
//! │  - send_transition() / send_abort()                          │
//! │  - is_channel_active() / is_channel_paused()                 │
//! └─────────────────────────────────────────────────────────────┘
//!                    ▲                       ▲
//!                    │                       │
//!         ┌─────────────────────┐ ┌─────────────────────┐
//!         │   ChannelRunner     │ │   ClientRunner      │
//!         │                     │ │                     │
//!         │ - event_rx          │ │ - event_rx          │
//!         │ - signal_rx         │ │ - signal_rx         │
//!         │ - component         │ │ - component         │
//!         │                     │ │ + io_bridge         │
//!         │                     │ │ + principal         │
//!         └─────────────────────┘ └─────────────────────┘
//! ```
//!
//! # Usage
//!
//! For background channels (agents, tools):
//!
//! ```ignore
//! let (runner, handle) = ChannelRunner::new(
//!     channel_id, world_tx, world, signal_rx, component,
//! );
//! tokio::spawn(runner.run());
//! ```
//!
//! For Human-interactive channels:
//!
//! ```ignore
//! let (runner, handle) = ClientRunner::new(
//!     channel_id, world_tx, world, signal_rx, component,
//!     io_port, principal,
//! );
//! tokio::spawn(runner.run());
//! ```

mod base;
mod child_context;
mod child_spawner;
mod client;
mod common;
mod emitter;
mod paused_queue;

pub use base::{ChannelHandle, ChannelRunner, ChannelRunnerFactory, Event};
pub use child_context::{ChildContextImpl, LuaChildLoader};
pub use child_spawner::{ChildSpawner, SpawnedChildHandle};
pub use client::{ClientRunner, ClientRunnerConfig};
pub use emitter::EventEmitter;

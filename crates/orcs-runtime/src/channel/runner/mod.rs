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
//! let (runner, handle) = ChannelRunner::builder(
//!     channel_id, world_tx, world, signal_rx, component,
//! )
//! .with_emitter(signal_tx)      // optional: enable event emission
//! .with_child_spawner()         // optional: enable child spawning
//! .build();
//!
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
mod rpc;

pub use base::{
    ChannelHandle, ChannelRunner, Event, ExitReason, OutputReceiver, OutputSender, RunnerResult,
};
// Builder is re-exported for external use (internal code uses it via base::ChannelRunnerBuilder)
#[allow(unused_imports)]
pub use base::ChannelRunnerBuilder;
pub use child_context::{ChildContextImpl, LuaChildLoader};
pub use child_spawner::{ChildSpawner, SpawnedChildHandle};
pub use client::{ClientRunner, ClientRunnerConfig};
pub use emitter::EventEmitter;

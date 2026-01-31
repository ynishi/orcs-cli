//! ORCS Runtime - Internal implementation layer.
//!
//! This crate provides the internal runtime infrastructure for ORCS
//! (Orchestrated Runtime for Collaborative Systems). It is NOT part
//! of the Plugin SDK and should not be directly depended on by plugins.
//!
//! # Crate Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Plugin SDK Layer                         │
//! │  (External, SemVer stable)                                   │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-types     : ID types, Principal, ErrorCode            │
//! │  orcs-event     : Signal, Request, Event                    │
//! │  orcs-component : Component trait (WIT target)              │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   Runtime Layer (THIS CRATE)                 │
//! │  (Internal, implementation details)                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  auth/     : Session, PrivilegeLevel, PermissionChecker     │
//! │  channel/  : Channel, World, ChannelState                   │
//! │  engine/   : OrcsEngine, EventBus, ComponentHandle          │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   Application Layer                          │
//! │  (orcs-app: re-exports + AppError)                          │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   Frontend Layer                             │
//! │  (orcs-cli, orcs-gui, orcs-net)                             │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Modules
//!
//! ## [`auth`] - Authentication & Authorization
//!
//! Internal permission management:
//!
//! - [`Session`](auth::Session): Principal + PrivilegeLevel context
//! - [`PrivilegeLevel`](auth::PrivilegeLevel): Standard or Elevated
//! - [`PermissionChecker`](auth::PermissionChecker): Policy trait
//!
//! Note: [`Principal`](orcs_types::Principal) is in `orcs-types` for Plugin SDK access.
//!
//! ## [`channel`] - Parallel Execution
//!
//! Channel lifecycle management:
//!
//! - [`Channel`](channel::Channel): Execution unit with state
//! - [`World`](channel::World): Channel tree manager
//! - [`ChannelState`](channel::ChannelState): Running/Completed/Aborted
//!
//! ## [`engine`] - Core Runtime
//!
//! Main runtime infrastructure:
//!
//! - [`OrcsEngine`](engine::OrcsEngine): Main runtime loop
//! - [`EventBus`](engine::EventBus): Message routing
//! - [`ComponentHandle`](engine::ComponentHandle): Component communication
//!
//! ## [`components`] - Builtin Components
//!
//! Core components for the runtime:
//!
//! - [`HilComponent`](components::HilComponent): Human-in-the-Loop approval
//!
//! ## [`io`] - Human I/O
//!
//! Input/output for Human interaction:
//!
//! - [`HumanInput`](io::HumanInput): stdin command parsing
//! - [`OutputSink`](io::OutputSink): output display trait
//!
//! # Why This Separation?
//!
//! The runtime layer is intentionally separate from the Plugin SDK because:
//!
//! 1. **Stability boundary**: SDK types are SemVer stable, runtime internals can change
//! 2. **Minimal plugin dependencies**: Plugins only need types/event/component
//! 3. **Implementation freedom**: Runtime can be refactored without breaking plugins
//! 4. **Clear boundaries**: Prevents accidental coupling to internal details

pub mod auth;
pub mod channel;
pub mod components;
pub mod engine;
pub mod io;

// Re-exports for convenience
pub use auth::{DefaultPolicy, PermissionChecker, PrivilegeLevel, Session};
pub use channel::{Channel, ChannelError, ChannelState, World};
pub use components::{ApprovalRequest, ApprovalResult, EchoWithHilComponent, HilComponent};
pub use engine::{ComponentHandle, EngineError, EventBus, OrcsEngine};
pub use io::{ConsoleOutput, HumanInput, InputCommand, OutputSink};

// Re-export Principal from orcs_types (it's part of the public API)
pub use orcs_types::Principal;

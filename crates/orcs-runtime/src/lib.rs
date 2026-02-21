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
//! - [`Session`] — Principal + PrivilegeLevel context
//! - [`PrivilegeLevel`] — Standard or Elevated
//! - [`PermissionChecker`] — Policy trait
//!
//! Note: [`Principal`] is in `orcs-types` for Plugin SDK access.
//!
//! ## [`channel`] - Parallel Execution
//!
//! Channel lifecycle management:
//!
//! - [`Channel`] — Execution unit with state
//! - [`World`] — Channel tree manager
//! - [`ChannelState`] — Running/Completed/Aborted
//!
//! ## [`engine`] - Core Runtime
//!
//! Main runtime infrastructure:
//!
//! - [`OrcsEngine`] — Main runtime loop
//! - [`EventBus`] — Message routing
//! - [`ComponentHandle`] — Component communication
//!
//! ## [`components`] - Builtin Components
//!
//! Core components for the runtime:
//!
//! - [`HilComponent`] — Human-in-the-Loop approval
//!
//! ## [`io`] - Human I/O
//!
//! Input/output for Human interaction:
//!
//! - [`InputParser`](io::InputParser): stdin command parsing
//! - [`ConsoleRenderer`](io::ConsoleRenderer): console output rendering
//!
//! ## [`config`] - Configuration Management
//!
//! Hierarchical configuration with layered merging:
//!
//! - [`OrcsConfig`]: Unified configuration type
//! - [`ConfigLoader`]: Multi-source config loader
//!
//! Configuration priority: Environment > Project > Global > Default
//!
//! ## [`session`] - Session Persistence
//!
//! Session data storage (separate from config):
//!
//! - [`SessionAsset`]: Conversation history, snapshots
//! - [`SessionStore`]: Storage abstraction
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
pub mod board;
pub mod channel;
pub mod components;
pub mod config;
pub mod engine;
pub mod io;
pub mod sandbox;
pub mod session;

// Re-exports for convenience
pub use auth::{
    AccessDenied, CommandPermission, DefaultGrantStore, DefaultPolicy, GrantPolicy,
    PermissionChecker, PermissionPolicy, PrivilegeLevel, Session,
};
pub use board::{
    shared_board, shared_board_with_capacity, Board, BoardEntry, BoardEntryKind, SharedBoard,
};
pub use channel::{
    priority, BaseChannel, Channel, ChannelConfig, ChannelCore, ChannelError, ChannelHandle,
    ChannelMut, ChannelRunner, ChannelState, ChildContextImpl, ChildSpawner, ClientRunner, Event,
    LuaChildLoader, MaxPrivilege, OutputReceiver, OutputSender, SpawnedChildHandle,
    StateTransition, World, WorldCommand, WorldCommandSender, WorldManager,
};
pub use components::{
    ApprovalRequest, ApprovalResult, DecoratorConfig, EchoWithHilComponent, HilComponent,
    NoopComponent,
};
pub use config::{
    default_config_dir, default_config_path, save_global_config, ComponentsConfig, ConfigError,
    ConfigLoader, ConfigResolver, EnvOverrides, HilConfig, ModelConfig, NoOpResolver, OrcsConfig,
    PathsConfig, ProfileDef, ProfileEntry, ProfileStore, UiConfig,
};
pub use engine::{ComponentHandle, EngineError, EventBus, OrcsEngine};
pub use io::{
    IOInput, IOInputHandle, IOOutput, IOOutputHandle, IOPort, InputCommand, InputContext,
    OutputStyle,
};
pub use orcs_hook::{shared_hook_registry, SharedHookRegistry};
pub use sandbox::{ProjectSandbox, SandboxError, SandboxPolicy};
pub use session::{
    default_session_path, LocalFileStore, SessionAsset, SessionMeta, SessionStore, StorageError,
    SyncState, SyncStatus,
};

// Re-export Principal from orcs_types (it's part of the public API)
pub use orcs_types::Principal;

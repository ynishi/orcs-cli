//! ORCS Application Layer.
//!
//! This crate provides:
//!
//! - **[`OrcsApp`]**: High-level application wrapper with HIL integration
//! - **Re-exports**: Convenient access to all ORCS crates
//! - **[`AppError`]**: Unified application-level error type
//!
//! # Quick Start
//!
//! ```ignore
//! use orcs_app::{OrcsApp, NoOpResolver};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut app = OrcsApp::builder(NoOpResolver).build().await?;
//!     app.run_interactive().await?;
//!     Ok(())
//! }
//! ```
//!
//! # Crate Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Plugin SDK Layer                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-types, orcs-event, orcs-component                     │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Runtime Layer                             │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-runtime (auth, channel, engine, components, io)       │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                 Application Layer  ◄── HERE                  │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-app (OrcsApp + re-exports + AppError)                 │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   Frontend Layer                             │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-cli (uses OrcsApp → interactive mode)                 │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Error Handling Strategy
//!
//! ```text
//! Internal Errors (ChannelError, EngineError, etc.)
//!                    ↓ From impl
//!               AppError (this crate)
//!                    ↓ anyhow::Error / eprintln
//!               CLI output
//! ```

mod app;
pub mod builtins;
mod error;
mod printer_slot;

pub use app::{OrcsApp, OrcsAppBuilder};
pub use error::AppError;
pub use printer_slot::SharedPrinterSlot;

// Re-export from Plugin SDK Layer
pub use orcs_component::{
    Child, Component, ComponentError, Identifiable, Progress, SignalReceiver, Status, StatusDetail,
    Statusable,
};
pub use orcs_event::{EventError, Request, Signal, SignalKind, SignalResponse, DEFAULT_TIMEOUT_MS};
pub use orcs_types::{
    ChannelId, ComponentId, ErrorCode, EventId, Principal, PrincipalId, RequestId, SignalScope,
    TryNew,
};

// Re-export from Runtime Layer
pub use orcs_runtime::{
    priority, Channel, ChannelConfig, ChannelError, ChannelHandle, ChannelRunner, ChannelState,
    ClientRunner, ComponentHandle, DecoratorConfig, DefaultPolicy, EchoWithHilComponent,
    EngineError, Event, EventBus, OrcsEngine, OutputReceiver, OutputSender, PermissionChecker,
    PrivilegeLevel, Session, StateTransition, World, WorldCommand, WorldCommandSender,
    WorldManager,
};

// Re-export configuration types
pub use orcs_runtime::{
    ComponentsConfig, ConfigError, ConfigLoader, ConfigResolver, EnvOverrides, HilConfig,
    ModelConfig, NoOpResolver, OrcsConfig, PathsConfig, UiConfig,
};

// Re-export sandbox types
pub use orcs_runtime::{ProjectSandbox, SandboxError, SandboxPolicy};

// Re-export work directory type
pub use orcs_runtime::WorkDir;

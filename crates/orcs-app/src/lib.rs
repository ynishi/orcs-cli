//! ORCS Application Layer.
//!
//! This crate provides:
//!
//! - **Re-exports**: Convenient access to all ORCS crates
//! - **AppError**: Unified application-level error type
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
//! │  orcs-runtime (auth, channel, engine)                       │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                 Application Layer  ◄── HERE                  │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-app (re-exports + AppError)                           │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   Frontend Layer                             │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-cli (uses AppError → anyhow/eprintln)                 │
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

mod error;

pub use error::AppError;

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
    Channel, ChannelError, ChannelState, ComponentHandle, DefaultPolicy, EngineError, EventBus,
    OrcsEngine, PermissionChecker, PrivilegeLevel, Session, World,
};

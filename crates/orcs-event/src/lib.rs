//! Event system for ORCS CLI.
//!
//! This crate provides the core event types for communication
//! between Components via the EventBus in the ORCS
//! (Orchestrated Runtime for Collaborative Systems) architecture.
//!
//! # Crate Architecture
//!
//! This crate is part of the **Plugin SDK** layer:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Plugin SDK Layer                          │
//! │  (External, SemVer stable, safe to depend on)               │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-types     : ID types, Principal, ErrorCode            │
//! │  orcs-event     : Signal, Request, Event  ◄── HERE          │
//! │  orcs-component : Component trait (WIT target)              │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Event Architecture Overview
//!
//! ORCS uses an EventBus-based architecture where all communication
//! between Components flows through unified message types:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │                           Human (Superpower)                         │
//! │                    Veto / Sanction / Steer / Approve                 │
//! └──────────────────────────────────────────────────────────────────────┘
//!                                     │ Signal
//!                                     ▼
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │                            OrcsEngine                                │
//! │  ┌────────────────────────────────────────────────────────────────┐  │
//! │  │                          EventBus                              │  │
//! │  │   - Request/Response routing                                   │  │
//! │  │   - Event broadcast                                            │  │
//! │  │   - Signal dispatch (highest priority)                         │  │
//! │  └────────────────────────────────────────────────────────────────┘  │
//! └──────────────────────────────────────────────────────────────────────┘
//!           │ EventBus
//!           ├──────────────┬──────────────┬──────────────┐
//!           ▼              ▼              ▼              ▼
//!     ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐
//!     │   LLM    │   │  Tools   │   │   HIL    │   │  WASM    │
//!     │Component │   │Component │   │Component │   │Component │
//!     └──────────┘   └──────────┘   └──────────┘   └──────────┘
//! ```
//!
//! # Message Types
//!
//! | Type | Direction | Response | Priority | Use Case |
//! |------|-----------|----------|----------|----------|
//! | [`Signal`] | Human → System | No | Highest | Control interrupts |
//! | [`Request`] | Component → Component | Yes | Normal | Synchronous queries |
//! | Event (future) | Component → Subscribers | No | Low | Async notifications |
//!
//! # Permission Model
//!
//! Event types themselves do **not** contain permission logic.
//! Permission checking is performed at the EventBus layer using
//! the runtime layer's Session.
//!
//! This separation enables:
//!
//! - **Dynamic privilege**: Same principal with different levels over time
//! - **Policy flexibility**: Change rules without modifying message types
//! - **Audit clarity**: Clear separation of "who" from "what permission"
//!
//! # Human as Superpower
//!
//! A core principle of ORCS: Human is not the LLM's assistant.
//! Human controls from above.
//!
//! - **Veto**: Stop all operations immediately (Global scope)
//! - **Cancel**: Stop operations in a specific channel
//! - **Approve/Reject**: HIL (Human-in-the-Loop) decisions
//!
//! # Signal Flow Example
//!
//! ```text
//! User Input (Ctrl+C)
//!     │
//!     ▼
//! ┌─────────────────┐
//! │  OrcsEngine     │
//! │  signal(Veto)   │
//! └─────────────────┘
//!     │ Signal { kind: Veto, scope: Global }
//!     ▼
//! ┌─────────────────┐    ┌─────────────────┐
//! │  LlmComponent   │    │  ToolsComponent │
//! │  on_signal()    │    │  on_signal()    │
//! │  → Abort        │    │  → Abort        │
//! └─────────────────┘    └─────────────────┘
//! ```
//!
//! # Request Flow Example
//!
//! ```text
//! LlmComponent
//!     │ Request { operation: "write", target: ToolsComponent }
//!     ▼
//! ┌─────────────────┐
//! │  EventBus       │
//! │  (route)        │
//! └─────────────────┘
//!     │
//!     ▼
//! ┌─────────────────┐
//! │  ToolsComponent │
//! │  on_request()   │
//! │  → write file   │
//! └─────────────────┘
//!     │ Response { success: true }
//!     ▼
//! LlmComponent
//! ```
//!
//! # Error Handling
//!
//! All errors implement [`orcs_types::ErrorCode`] for unified handling:
//!
//! ```
//! use orcs_event::EventError;
//! use orcs_types::{ErrorCode, RequestId};
//!
//! let err = EventError::Timeout(RequestId::new());
//!
//! // Machine-readable code for programmatic handling
//! assert_eq!(err.code(), "EVENT_TIMEOUT");
//!
//! // Recoverability info for retry logic
//! assert!(err.is_recoverable());
//! ```
//!
//! # Usage
//!
//! ```
//! use orcs_event::{Signal, SignalKind, Request, EventError, EventCategory};
//! use orcs_types::{Principal, ComponentId, ChannelId, PrincipalId, SignalScope};
//!
//! // Create a principal (identity only, no permission)
//! let principal = Principal::User(PrincipalId::new());
//!
//! // Create signals
//! let channel = ChannelId::new();
//! let cancel = Signal::cancel(channel, principal.clone());
//! let veto = Signal::veto(principal);
//!
//! // Create a request
//! let source = ComponentId::builtin("llm");
//! let request = Request::new(
//!     EventCategory::Echo,
//!     "chat",
//!     source,
//!     channel,
//!     serde_json::json!({ "message": "Hello" }),
//! );
//! ```
//!
//! # Crate Structure
//!
//! - [`Signal`], [`SignalKind`], [`SignalResponse`] - Control interrupts
//! - [`Request`] - Synchronous queries
//! - [`EventError`] - Event system errors
//! - [`DEFAULT_TIMEOUT_MS`] - Default request timeout
//!
//! # Related Crates
//!
//! - [`orcs_types`] - Core identifier types ([`ChannelId`], [`ComponentId`], [`Principal`], etc.)
//! - `orcs-runtime` - Runtime layer (Session, PrivilegeLevel, EventBus)
//!
//! [`ChannelId`]: orcs_types::ChannelId
//! [`ComponentId`]: orcs_types::ComponentId
//! [`Principal`]: orcs_types::Principal

mod category;
mod error;
mod request;
mod signal;

pub use category::EventCategory;
pub use error::EventError;
pub use request::{Request, RequestArgs, DEFAULT_TIMEOUT_MS};
pub use signal::{Signal, SignalKind, SignalResponse};

// Re-export from orcs_types for convenience
pub use orcs_types::{Principal, SignalScope};

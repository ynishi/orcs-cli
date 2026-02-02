//! ORCS Engine - Runtime and EventBus.
//!
//! This crate provides the core runtime infrastructure for ORCS
//! (Orchestrated Runtime for Collaborative Systems).
//!
//! # Architecture Overview
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │                     Human (Superpower)                       │
//! │              Veto / Sanction / Steer / Approve               │
//! └──────────────────────────────────────────────────────────────┘
//!                              │ Signal
//!                              ▼
//! ┌──────────────────────────────────────────────────────────────┐
//! │                        OrcsEngine                            │
//! │  ┌────────────────────────────────────────────────────────┐  │
//! │  │                      EventBus                          │  │
//! │  │   - Request/Response routing                           │  │
//! │  │   - Signal dispatch (highest priority)                 │  │
//! │  └────────────────────────────────────────────────────────┘  │
//! │                              │                                │
//! │  ┌────────────────────────────────────────────────────────┐  │
//! │  │                       World                            │  │
//! │  │   - Channel spawn/kill                                 │  │
//! │  │   - Permission inheritance                             │  │
//! │  └────────────────────────────────────────────────────────┘  │
//! └──────────────────────────────────────────────────────────────┘
//!        │ EventBus
//!        ├──────────────┬──────────────┬──────────────┐
//!        ▼              ▼              ▼              ▼
//!  ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐
//!  │   LLM    │   │  Tools   │   │   HIL    │   │  WASM    │
//!  │Component │   │Component │   │Component │   │Component │
//!  └──────────┘   └──────────┘   └──────────┘   └──────────┘
//! ```
//!
//! # Core Principles
//!
//! ## Human as Superpower
//!
//! Human is NOT the LLM's assistant - Human controls from above.
//!
//! - **Veto**: Stop everything immediately
//! - **Sanction**: Stop specific processing
//! - **Steer**: Intervene during execution
//! - **Approve/Reject**: HIL (Human-in-the-Loop) approval
//!
//! ## EventBus Unified Communication
//!
//! All Component communication flows through EventBus:
//!
//! - **Request/Response**: Synchronous queries
//! - **Signal**: Human interrupts (highest priority)
//!
//! ## Component = Functional Domain Boundary
//!
//! Components are flat but have clear responsibility separation.
//! Engine focuses on infrastructure, guaranteeing Component implementation freedom.
//!
//! # Main Types
//!
//! - [`OrcsEngine`]: Main runtime loop and lifecycle management
//! - [`EventBus`]: Unified message routing between Components
//! - [`ComponentHandle`]: Handle for Component to receive messages
//! - [`EngineError`]: Engine layer errors (implements [`ErrorCode`])
//!
//! # Error Handling
//!
//! Engine operations return [`EngineError`] which implements [`orcs_types::ErrorCode`].
//! CLI/Application layer should convert these to user-facing errors.
//!
//! [`ErrorCode`]: orcs_types::ErrorCode

#[allow(clippy::module_inception)]
mod engine;
mod error;
mod eventbus;

pub use engine::OrcsEngine;
pub use error::EngineError;
pub use eventbus::{ComponentHandle, EventBus};

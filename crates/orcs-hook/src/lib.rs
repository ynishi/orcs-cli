//! Hook system for ORCS CLI.
//!
//! This crate provides the hook abstraction layer for the ORCS
//! (Orchestrated Runtime for Collaborative Systems) architecture.
//!
//! # Crate Architecture
//!
//! This crate sits between the **Plugin SDK** and **Runtime** layers:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Plugin SDK Layer                          │
//! │  (External, SemVer stable, safe to depend on)               │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-types     : ID types, Principal, ErrorCode            │
//! │  orcs-event     : Signal, Request, Event                    │
//! │  orcs-component : Component trait (WIT target)              │
//! └─────────────────────────────────────────────────────────────┘
//!           ↕ depends on SDK, depended on by Runtime
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      Hook Layer                  ◄── HERE   │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-hook : Hook trait, Registry, FQL, Config              │
//! └─────────────────────────────────────────────────────────────┘
//!           ↕
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Runtime Layer                             │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-runtime : Session, EventBus, ChannelRunner            │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Overview
//!
//! Hooks allow cross-cutting concerns (logging, auditing, capability
//! injection, payload transformation, metrics, etc.) to be injected
//! at lifecycle points throughout the ORCS runtime via a single,
//! unified configuration interface.
//!
//! # Core Concepts
//!
//! ## Hook Points
//!
//! [`HookPoint`] enumerates 26 lifecycle points across 8 categories:
//! Component, Request, Signal, Child, Channel, Tool, Auth, and EventBus.
//!
//! ## FQL (Fully Qualified Locator)
//!
//! [`FqlPattern`] provides pattern matching for component addressing:
//!
//! ```text
//! <scope>::<target>[/<child_path>][#<instance>]
//! ```
//!
//! Examples: `"builtin::llm"`, `"*::*"`, `"builtin::llm/agent-1"`.
//!
//! ## Hook Trait
//!
//! The [`Hook`] trait defines a single hook handler:
//!
//! ```ignore
//! pub trait Hook: Send + Sync {
//!     fn id(&self) -> &str;
//!     fn fql_pattern(&self) -> &FqlPattern;
//!     fn hook_point(&self) -> HookPoint;
//!     fn priority(&self) -> i32 { 100 }
//!     fn execute(&self, ctx: HookContext) -> HookAction;
//! }
//! ```
//!
//! ## Hook Actions
//!
//! [`HookAction`] determines what happens after a hook executes:
//!
//! - `Continue(ctx)` — pass (modified) context downstream
//! - `Skip(value)` — skip the operation (pre-hooks only)
//! - `Abort { reason }` — abort with error (pre-hooks only)
//! - `Replace(value)` — replace result payload (post-hooks only)
//!
//! ## Registry
//!
//! [`HookRegistry`] is the central dispatch engine. It manages
//! hook registration, priority ordering, FQL filtering, and
//! chain execution semantics.
//!
//! ## Configuration
//!
//! [`HooksConfig`] and [`HookDef`] provide TOML-serializable
//! declarative hook definitions for `OrcsConfig` integration.
//!
//! # Concurrency
//!
//! The registry is designed to be wrapped in
//! `Arc<std::sync::RwLock<HookRegistry>>` following the same pattern
//! as `SharedChannelHandles` in the runtime.
//!
//! # Example
//!
//! ```
//! use orcs_hook::{
//!     HookRegistry, HookPoint, HookContext, HookAction, FqlPattern, Hook,
//! };
//! use orcs_types::{ComponentId, ChannelId, Principal};
//! use serde_json::json;
//!
//! // Create a registry
//! let mut registry = HookRegistry::new();
//!
//! // Build a context
//! let ctx = HookContext::new(
//!     HookPoint::RequestPreDispatch,
//!     ComponentId::builtin("llm"),
//!     ChannelId::new(),
//!     Principal::System,
//!     0,
//!     json!({"operation": "chat"}),
//! );
//!
//! // Dispatch (no hooks registered → Continue with unchanged context)
//! let action = registry.dispatch(
//!     HookPoint::RequestPreDispatch,
//!     &ComponentId::builtin("llm"),
//!     None,
//!     ctx,
//! );
//! assert!(action.is_continue());
//! ```

mod action;
mod config;
mod context;
mod error;
mod fql;
pub mod hook;
mod point;
mod registry;

// Re-export core types
pub use action::HookAction;
pub use config::{HookDef, HookDefValidationError, HooksConfig};
pub use context::{HookContext, DEFAULT_MAX_DEPTH};
pub use error::HookError;
pub use fql::{FqlPattern, PatternSegment};
pub use hook::Hook;
pub use point::HookPoint;
pub use registry::HookRegistry;

// Re-export testing utilities
pub mod testing {
    //! Test utilities for the hook system.
    //!
    //! Provides [`MockHook`] for use in tests.
    pub use crate::hook::testing::MockHook;
}

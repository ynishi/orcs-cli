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

use std::sync::{Arc, RwLock};

/// Thread-safe shared reference to a [`HookRegistry`].
///
/// Follows the same pattern as `SharedChannelHandles` in the runtime:
/// - `dispatch()` takes `&self` → read lock
/// - `register()` / `unregister()` take `&mut self` → write lock
///
/// `std::sync::RwLock` (not tokio) because the lock is never held across
/// `.await` points.
pub type SharedHookRegistry = Arc<RwLock<HookRegistry>>;

/// Creates a new empty [`SharedHookRegistry`].
#[must_use]
pub fn shared_hook_registry() -> SharedHookRegistry {
    Arc::new(RwLock::new(HookRegistry::new()))
}

// Re-export testing utilities
pub mod testing {
    //! Test utilities for the hook system.
    //!
    //! Provides [`MockHook`] for use in tests.
    pub use crate::hook::testing::MockHook;
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{ChannelId, ComponentId, Principal};
    use serde_json::json;

    #[test]
    fn shared_registry_creation() {
        let reg = shared_hook_registry();
        let guard = reg.read().unwrap();
        assert!(guard.is_empty());
    }

    #[test]
    fn shared_registry_register_and_dispatch() {
        let reg = shared_hook_registry();

        // Write lock: register a hook
        {
            let mut guard = reg.write().unwrap();
            let hook =
                testing::MockHook::pass_through("test", "*::*", HookPoint::RequestPreDispatch);
            guard.register(Box::new(hook));
            assert_eq!(guard.len(), 1);
        }

        // Read lock: dispatch
        {
            let guard = reg.read().unwrap();
            let ctx = HookContext::new(
                HookPoint::RequestPreDispatch,
                ComponentId::builtin("llm"),
                ChannelId::new(),
                Principal::System,
                0,
                json!({"op": "test"}),
            );
            let action = guard.dispatch(
                HookPoint::RequestPreDispatch,
                &ComponentId::builtin("llm"),
                None,
                ctx,
            );
            assert!(action.is_continue());
        }
    }

    #[test]
    fn shared_registry_clone_shares_state() {
        let reg = shared_hook_registry();
        let reg2 = Arc::clone(&reg);

        {
            let mut guard = reg.write().unwrap();
            guard.register(Box::new(testing::MockHook::pass_through(
                "shared",
                "*::*",
                HookPoint::RequestPreDispatch,
            )));
        }

        let guard = reg2.read().unwrap();
        assert_eq!(guard.len(), 1);
    }
}

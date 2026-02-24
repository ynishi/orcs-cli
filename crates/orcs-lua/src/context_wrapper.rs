//! Capability context storage for Lua.
//!
//! The [`ChildContext`] is stored in Lua's `app_data` as a [`ContextWrapper`].
//! Both Component and Child callers set this so that
//! [`dispatch_rust_tool`](crate::tool_registry::dispatch_rust_tool) can
//! perform capability checks at dispatch time.

use orcs_component::ChildContext;
use parking_lot::Mutex;
use std::sync::Arc;

/// Wrapper to store [`ChildContext`] in Lua's `app_data`.
///
/// Used by both Component and Child paths:
/// - Component: `ContextWrapper(Arc::clone(&existing_arc))`
/// - Child: `ContextWrapper(Arc::new(Mutex::new(ctx)))`
pub(crate) struct ContextWrapper(pub(crate) Arc<Mutex<Box<dyn ChildContext>>>);

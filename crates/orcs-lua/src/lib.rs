//! Lua scripting support for ORCS components.
//!
//! This crate enables writing ORCS Components and Children in Lua scripts.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │              LuaComponent (Rust)                    │
//! │  impl Component for LuaComponent                    │
//! │  ┌───────────────────────────────────────────────┐  │
//! │  │  lua: Lua (mlua)                              │  │
//! │  │  id: ComponentId                              │  │
//! │  │  callbacks: LuaCallbacks                       │  │
//! │  └───────────────────────────────────────────────┘  │
//! │                         │                           │
//! │                         ▼                           │
//! │  ┌───────────────────────────────────────────────┐  │
//! │  │           Lua Script (.lua)                   │  │
//! │  │  return {                                      │  │
//! │  │    id = "my-component",                       │  │
//! │  │    subscriptions = {"Echo"},                  │  │
//! │  │    on_request = function(req) ... end,        │  │
//! │  │    on_signal = function(sig) ... end,         │  │
//! │  │  }                                            │  │
//! │  └───────────────────────────────────────────────┘  │
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! # Example Lua Script
//!
//! ```lua
//! -- echo_component.lua
//! return {
//!     id = "lua-echo",
//!     subscriptions = {"Echo"},
//!
//!     on_request = function(request)
//!         if request.operation == "echo" then
//!             return { success = true, data = request.payload }
//!         end
//!         return { success = false, error = "unknown operation" }
//!     end,
//!
//!     on_signal = function(signal)
//!         if signal.kind == "Veto" then
//!             return "Abort"
//!         end
//!         return "Ignored"
//!     end,
//! }
//! ```
//!
//! # Hot Reload
//!
//! `LuaComponent::reload()` allows reloading the script from file.
//! Automatic file watching is not implemented yet.
//!
//! TODO: Integrate `notify` crate for automatic hot reload on file change.

mod child;
mod component;
mod error;
mod types;

pub use child::LuaChild;
pub use component::LuaComponent;
pub use error::LuaError;
pub use types::{LuaRequest, LuaResponse, LuaSignal};

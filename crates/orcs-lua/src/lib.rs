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
//!
//! # Script Loading
//!
//! Scripts can be loaded from:
//! 1. **Embedded** - Built into the binary at compile time
//! 2. **Runtime** - Loaded from filesystem at runtime
//!
//! ```ignore
//! use orcs_lua::ScriptLoader;
//!
//! // Load embedded script
//! let component = ScriptLoader::load_embedded("echo")?;
//!
//! // Load with runtime fallback
//! let loader = ScriptLoader::new().with_path("./scripts");
//! let component = loader.load("echo")?;
//! ```

mod child;
mod component;
pub mod embedded;
mod error;
mod loader;
pub mod orcs_helpers;
pub mod testing;
mod types;

pub use child::LuaChild;
pub use component::{LuaComponent, LuaComponentLoader};
pub use error::LuaError;
pub use loader::ScriptLoader;
pub use orcs_helpers::{ensure_orcs_table, register_base_orcs_functions};
pub use types::{LuaRequest, LuaResponse, LuaSignal};

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
//! # Sandbox
//!
//! All file operations (`orcs.read`, `orcs.write`, `orcs.grep`, `orcs.glob`,
//! `orcs.mkdir`, `orcs.remove`, `orcs.mv`) are sandboxed via
//! [`SandboxPolicy`](orcs_runtime::sandbox::SandboxPolicy). The sandbox is
//! injected at construction time and controls:
//!
//! - Which filesystem paths are accessible for reads and writes
//! - The working directory for `orcs.exec()` commands
//! - The value of `orcs.pwd` in Lua
//!
//! Dangerous Lua stdlib functions (`io.*`, `os.execute`, `loadfile`, etc.)
//! are disabled after registration to prevent sandbox bypass.
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
//! Scripts are loaded from filesystem search paths:
//!
//! ```ignore
//! use orcs_lua::ScriptLoader;
//! use orcs_runtime::sandbox::ProjectSandbox;
//! use std::sync::Arc;
//!
//! let sandbox = Arc::new(ProjectSandbox::new(".").unwrap());
//!
//! let loader = ScriptLoader::new(sandbox)
//!     .with_path("~/.orcs/components")
//!     .with_path("/versioned/builtins/components");
//! let component = loader.load("echo")?;
//! ```

pub(crate) mod cap_tools;
mod child;
mod component;
mod error;
pub mod hook_helpers;
mod loader;
mod lua_env;
pub mod orcs_helpers;
pub mod scenario;
pub mod testing;
pub mod tool_registry;
pub mod tools;
mod types;

pub use child::LuaChild;
pub use component::{LuaComponent, LuaComponentLoader};
pub use error::LuaError;
pub use hook_helpers::{
    load_hooks_from_config, register_hook_function, register_hook_stub, register_unhook_function,
    HookLoadError, HookLoadResult, LuaHook,
};
pub use loader::{LoadResult, LoadWarning, ScriptLoader};
pub use lua_env::LuaEnv;
pub use orcs_helpers::{ensure_orcs_table, register_base_orcs_functions};
pub use tools::register_tool_functions;
pub use types::{LuaRequest, LuaResponse, LuaSignal};

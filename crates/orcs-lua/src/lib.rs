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

pub(crate) mod builtin_tools;
mod child;
mod component;
pub(crate) mod context_wrapper;
mod error;
pub mod hook_helpers;
pub mod http_command;
pub mod llm_adapter;
pub mod llm_command;
mod loader;
mod lua_env;
pub mod orcs_helpers;
pub(crate) mod resolve_loop;
pub mod sandbox_eval;
pub mod sanitize;
#[cfg(any(test, feature = "test-utils"))]
pub mod scenario;
#[cfg(any(test, feature = "test-utils"))]
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
pub use llm_command::{llm_request_impl, register_llm_deny_stub};
pub use loader::{LoadResult, LoadWarning, ScriptLoader};
pub use lua_env::LuaEnv;
pub use orcs_helpers::{ensure_orcs_table, register_base_orcs_functions};
pub use tools::register_tool_functions;
pub use types::{LuaRequest, LuaResponse, LuaSignal};

/// Extracts `(grant_pattern, description)` from a `ComponentError::Suspended`
/// wrapped inside an `mlua::Error`.
///
/// Recurses through `CallbackError` wrappers since mlua nests callback errors.
/// Used by `dispatch_intents_to_results` to convert intent-level permission
/// denials into LLM-visible tool_result errors instead of aborting the
/// resolve loop.
pub(crate) fn extract_suspended_info(err: &mlua::Error) -> Option<(String, String)> {
    match err {
        mlua::Error::ExternalError(ext) => ext
            .downcast_ref::<orcs_component::ComponentError>()
            .and_then(|ce| match ce {
                orcs_component::ComponentError::Suspended {
                    grant_pattern,
                    pending_request,
                    ..
                } => {
                    let desc = pending_request
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown operation")
                        .to_string();
                    Some((grant_pattern.clone(), desc))
                }
                _ => None,
            }),
        mlua::Error::CallbackError { cause, .. } => extract_suspended_info(cause),
        _ => None,
    }
}

#[cfg(test)]
mod extract_suspended_info_tests {
    use super::*;
    use orcs_component::ComponentError;
    use std::sync::Arc;

    #[test]
    fn extracts_grant_pattern_and_description() {
        let err = mlua::Error::ExternalError(Arc::new(ComponentError::Suspended {
            approval_id: "ap-001".into(),
            grant_pattern: "intent:write".into(),
            pending_request: serde_json::json!({
                "command": "intent:write",
                "description": "Write to file: /tmp/test.txt",
            }),
        }));
        let result = extract_suspended_info(&err);
        assert!(result.is_some(), "should extract from Suspended");
        let (pattern, desc) = result.expect("already asserted Some");
        assert_eq!(pattern, "intent:write");
        assert_eq!(desc, "Write to file: /tmp/test.txt");
    }

    #[test]
    fn extracts_through_callback_error() {
        let inner = mlua::Error::ExternalError(Arc::new(ComponentError::Suspended {
            approval_id: "ap-002".into(),
            grant_pattern: "intent:remove".into(),
            pending_request: serde_json::json!({
                "description": "Remove file: /tmp/old.txt",
            }),
        }));
        let err = mlua::Error::CallbackError {
            traceback: "stack trace".into(),
            cause: Arc::new(inner),
        };
        let (pattern, desc) =
            extract_suspended_info(&err).expect("should extract through CallbackError");
        assert_eq!(pattern, "intent:remove");
        assert_eq!(desc, "Remove file: /tmp/old.txt");
    }

    #[test]
    fn falls_back_to_unknown_when_no_description() {
        let err = mlua::Error::ExternalError(Arc::new(ComponentError::Suspended {
            approval_id: "ap-003".into(),
            grant_pattern: "intent:mkdir".into(),
            pending_request: serde_json::json!({"command": "intent:mkdir"}),
        }));
        let (pattern, desc) =
            extract_suspended_info(&err).expect("should extract even without description");
        assert_eq!(pattern, "intent:mkdir");
        assert_eq!(desc, "unknown operation");
    }

    #[test]
    fn returns_none_for_non_suspended() {
        let err =
            mlua::Error::ExternalError(Arc::new(ComponentError::ExecutionFailed("timeout".into())));
        assert!(
            extract_suspended_info(&err).is_none(),
            "ExecutionFailed should not match"
        );
    }

    #[test]
    fn returns_none_for_runtime_error() {
        let err = mlua::Error::RuntimeError("some error".into());
        assert!(
            extract_suspended_info(&err).is_none(),
            "RuntimeError should not match"
        );
    }
}

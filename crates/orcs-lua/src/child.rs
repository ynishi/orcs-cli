//! LuaChild implementation.
//!
//! Wraps a Lua table to implement the Child trait.

use crate::error::LuaError;
use crate::types::{parse_signal_response, parse_status, LuaSignal};
use mlua::{Function, Lua, RegistryKey, Table};
use orcs_component::{Child, Identifiable, SignalReceiver, Status, Statusable};
use orcs_event::{Signal, SignalResponse};
use std::sync::{Arc, Mutex};

/// A child entity implemented in Lua.
///
/// Can be used inside a LuaComponent to manage child entities.
///
/// # Lua Table Format
///
/// ```lua
/// {
///     id = "child-1",
///     status = "Idle",  -- Optional, defaults to "Idle"
///
///     on_signal = function(sig)
///         return "Handled" | "Ignored" | "Abort"
///     end,
///
///     abort = function()  -- Optional
///     end,
/// }
/// ```
pub struct LuaChild {
    /// Shared Lua runtime (from parent component).
    lua: Arc<Mutex<Lua>>,
    /// Child identifier.
    id: String,
    /// Current status.
    status: Status,
    /// Registry key for on_signal callback.
    on_signal_key: RegistryKey,
    /// Registry key for abort callback (optional).
    abort_key: Option<RegistryKey>,
}

// SAFETY: Lua runtime is protected by Mutex
unsafe impl Send for LuaChild {}
unsafe impl Sync for LuaChild {}

impl LuaChild {
    /// Creates a LuaChild from a Lua table.
    ///
    /// # Arguments
    ///
    /// * `lua` - Shared Lua runtime
    /// * `table` - Lua table defining the child
    ///
    /// # Errors
    ///
    /// Returns error if table is missing required fields.
    pub fn from_table(lua: Arc<Mutex<Lua>>, table: Table) -> Result<Self, LuaError> {
        let lua_guard = lua.lock().map_err(|e| {
            LuaError::Runtime(mlua::Error::ExternalError(Arc::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;

        // Extract id
        let id: String = table
            .get("id")
            .map_err(|_| LuaError::MissingCallback("id".to_string()))?;

        // Extract status (optional)
        let status_str: String = table.get("status").unwrap_or_else(|_| "Idle".to_string());
        let status = parse_status(&status_str);

        // Extract on_signal callback
        let on_signal_fn: Function = table
            .get("on_signal")
            .map_err(|_| LuaError::MissingCallback("on_signal".to_string()))?;

        let on_signal_key = lua_guard.create_registry_value(on_signal_fn)?;

        // Extract abort callback (optional)
        let abort_key = table
            .get::<Function>("abort")
            .ok()
            .map(|f| lua_guard.create_registry_value(f))
            .transpose()?;

        drop(lua_guard);

        Ok(Self {
            lua,
            id,
            status,
            on_signal_key,
            abort_key,
        })
    }

    /// Creates a simple LuaChild with just an ID.
    ///
    /// The on_signal callback will return Ignored for all signals.
    pub fn simple(lua: Arc<Mutex<Lua>>, id: impl Into<String>) -> Result<Self, LuaError> {
        let id = id.into();
        let lua_guard = lua.lock().map_err(|e| {
            LuaError::Runtime(mlua::Error::ExternalError(Arc::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;

        // Create a simple on_signal function that returns "Ignored"
        let on_signal_fn = lua_guard.create_function(|_, _: mlua::Value| Ok("Ignored"))?;

        let on_signal_key = lua_guard.create_registry_value(on_signal_fn)?;

        drop(lua_guard);

        Ok(Self {
            lua,
            id,
            status: Status::Idle,
            on_signal_key,
            abort_key: None,
        })
    }
}

impl Identifiable for LuaChild {
    fn id(&self) -> &str {
        &self.id
    }
}

impl SignalReceiver for LuaChild {
    fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
        let Ok(lua) = self.lua.lock() else {
            return SignalResponse::Ignored;
        };

        let Ok(on_signal): Result<Function, _> = lua.registry_value(&self.on_signal_key) else {
            return SignalResponse::Ignored;
        };

        let lua_sig = LuaSignal::from_signal(signal);

        let result: Result<String, _> = on_signal.call(lua_sig);

        match result {
            Ok(response_str) => {
                let response = parse_signal_response(&response_str);
                if matches!(response, SignalResponse::Abort) {
                    drop(lua);
                    self.status = Status::Aborted;
                }
                response
            }
            Err(e) => {
                tracing::warn!("Lua child on_signal error: {}", e);
                SignalResponse::Ignored
            }
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;

        // Call Lua abort if available
        if let Some(abort_key) = &self.abort_key {
            if let Ok(lua) = self.lua.lock() {
                if let Ok(abort_fn) = lua.registry_value::<Function>(abort_key) {
                    if let Err(e) = abort_fn.call::<()>(()) {
                        tracing::warn!("Lua child abort error: {}", e);
                    }
                }
            }
        }
    }
}

impl Statusable for LuaChild {
    fn status(&self) -> Status {
        self.status
    }
}

impl Child for LuaChild {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_identifiable() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let child = LuaChild::simple(lua, "child-1").expect("create child");

        assert_eq!(child.id(), "child-1");
    }

    #[test]
    fn child_statusable() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let child = LuaChild::simple(lua, "child-1").expect("create child");

        assert_eq!(child.status(), Status::Idle);
    }

    #[test]
    fn child_abort_changes_status() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let mut child = LuaChild::simple(lua, "child-1").expect("create child");

        child.abort();
        assert_eq!(child.status(), Status::Aborted);
    }

    #[test]
    fn child_is_object_safe() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let child = LuaChild::simple(lua, "child-1").expect("create child");

        // Should compile - proves Child is object-safe
        let _boxed: Box<dyn Child> = Box::new(child);
    }
}

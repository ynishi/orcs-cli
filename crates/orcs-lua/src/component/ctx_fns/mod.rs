//! Child context Lua function registration.
//!
//! Registers functions into the `orcs` Lua table, split by responsibility:
//! - [`exec_fns`] — execution, permission, and capability-gated service gates
//! - [`spawn_fns`] — child/runner spawning and lifecycle
//! - [`comm_fns`] — inter-component communication

mod comm_fns;
mod exec_fns;
mod spawn_fns;

use crate::error::LuaError;
use mlua::{Lua, Table};
use orcs_component::ChildContext;
use orcs_runtime::sandbox::SandboxPolicy;
use parking_lot::Mutex;
use std::sync::Arc;

/// Registers child-context functions into the `orcs` Lua table.
///
/// Also stores `ContextWrapper` for capability-gated dispatch via `dispatch_rust_tool`.
pub(super) fn register(
    lua: &Lua,
    ctx: Arc<Mutex<Box<dyn ChildContext>>>,
    sandbox: Arc<dyn SandboxPolicy>,
) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;
    let sandbox_root = sandbox.root().to_path_buf();

    // Store ChildContext for capability-gated dispatch.
    // dispatch_rust_tool reads ContextWrapper from app_data for capability checks.
    lua.set_app_data(crate::context_wrapper::ContextWrapper(Arc::clone(&ctx)));

    exec_fns::register(lua, &orcs_table, &ctx, &sandbox_root)?;
    spawn_fns::register(lua, &orcs_table, &ctx)?;
    comm_fns::register(lua, &orcs_table, &ctx)?;

    tracing::debug!("Registered child context functions (exec, spawn, comm)");
    Ok(())
}

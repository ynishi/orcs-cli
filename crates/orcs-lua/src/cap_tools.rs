//! Capability-gated file tool registration for Lua.
//!
//! Shared implementation used by both [`LuaComponent`](crate::LuaComponent)
//! and [`LuaChild`](crate::LuaChild) to override `orcs.read/write/grep/glob/mkdir/remove/mv`
//! with versions that check [`Capability`] flags before executing.
//!
//! # Context Storage
//!
//! The [`ChildContext`] is stored in Lua's `app_data` as a [`ContextWrapper`].
//! Both Component and Child callers set this before calling
//! [`register_capability_gated_tools`].

use crate::tools::{tool_glob, tool_grep, tool_mkdir, tool_mv, tool_read, tool_remove, tool_write};
use mlua::{Lua, Table};
use orcs_component::{Capability, ChildContext};
use orcs_runtime::sandbox::SandboxPolicy;
use std::sync::{Arc, Mutex};

/// Wrapper to store [`ChildContext`] in Lua's `app_data`.
///
/// Used by both Component and Child paths:
/// - Component: `ContextWrapper(Arc::clone(&existing_arc))`
/// - Child: `ContextWrapper(Arc::new(Mutex::new(ctx)))`
pub(crate) struct ContextWrapper(pub(crate) Arc<Mutex<Box<dyn ChildContext>>>);

/// Executes `f` with a reference to the [`ChildContext`] stored in `app_data`.
///
/// # Errors
///
/// Returns `RuntimeError` if no context is available or the mutex is poisoned.
pub(crate) fn with_context<F, R>(lua: &Lua, f: F) -> mlua::Result<R>
where
    F: FnOnce(&dyn ChildContext) -> mlua::Result<R>,
{
    let wrapper = lua
        .app_data_ref::<ContextWrapper>()
        .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;
    let ctx = wrapper
        .0
        .lock()
        .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
    f(&**ctx)
}

/// Returns a Lua table `{ ok = false, error = "permission denied: ..." }`.
fn cap_denied(lua: &Lua, cap_name: &str) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set(
        "error",
        format!("permission denied: Capability::{cap_name} not granted"),
    )?;
    Ok(result)
}

/// Overrides file tools in `orcs_table` with capability-gated versions.
///
/// Reads the [`ChildContext`] from Lua `app_data` (via [`ContextWrapper`])
/// at call time to check capabilities.
///
/// # Capability Mapping
///
/// | Capability | Tools |
/// |------------|-------|
/// | `READ`     | `orcs.read`, `orcs.grep`, `orcs.glob` |
/// | `WRITE`    | `orcs.write`, `orcs.mkdir` |
/// | `DELETE`   | `orcs.remove` |
/// | `DELETE \| WRITE` | `orcs.mv` |
///
/// # Prerequisites
///
/// Caller must set `lua.set_app_data(ContextWrapper(...))` before calling this.
pub(crate) fn register_capability_gated_tools(
    lua: &Lua,
    orcs_table: &Table,
    sandbox: &Arc<dyn SandboxPolicy>,
) -> Result<(), mlua::Error> {
    // orcs.read — requires READ
    {
        let sb = Arc::clone(sandbox);
        let f = lua.create_function(move |lua, path: String| {
            let has = with_context(lua, |ctx| Ok(ctx.has_capability(Capability::READ)))?;
            if !has {
                return cap_denied(lua, "READ");
            }
            let result = lua.create_table()?;
            match tool_read(&path, sb.as_ref()) {
                Ok((content, size)) => {
                    result.set("ok", true)?;
                    result.set("content", content)?;
                    result.set("size", size)?;
                }
                Err(e) => {
                    result.set("ok", false)?;
                    result.set("error", e)?;
                }
            }
            Ok(result)
        })?;
        orcs_table.set("read", f)?;
    }

    // orcs.write — requires WRITE
    {
        let sb = Arc::clone(sandbox);
        let f = lua.create_function(move |lua, (path, content): (String, String)| {
            let has = with_context(lua, |ctx| Ok(ctx.has_capability(Capability::WRITE)))?;
            if !has {
                return cap_denied(lua, "WRITE");
            }
            let result = lua.create_table()?;
            match tool_write(&path, &content, sb.as_ref()) {
                Ok(bytes) => {
                    result.set("ok", true)?;
                    result.set("bytes_written", bytes)?;
                }
                Err(e) => {
                    result.set("ok", false)?;
                    result.set("error", e)?;
                }
            }
            Ok(result)
        })?;
        orcs_table.set("write", f)?;
    }

    // orcs.grep — requires READ
    {
        let sb = Arc::clone(sandbox);
        let f = lua.create_function(move |lua, (pattern, path): (String, String)| {
            let has = with_context(lua, |ctx| Ok(ctx.has_capability(Capability::READ)))?;
            if !has {
                return cap_denied(lua, "READ");
            }
            let result = lua.create_table()?;
            match tool_grep(&pattern, &path, sb.as_ref()) {
                Ok(grep_matches) => {
                    let matches_table = lua.create_table()?;
                    for (i, m) in grep_matches.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("line_number", m.line_number)?;
                        entry.set("line", m.line.as_str())?;
                        matches_table.set(i + 1, entry)?;
                    }
                    result.set("ok", true)?;
                    result.set("matches", matches_table)?;
                    result.set("count", grep_matches.len())?;
                }
                Err(e) => {
                    result.set("ok", false)?;
                    result.set("error", e)?;
                }
            }
            Ok(result)
        })?;
        orcs_table.set("grep", f)?;
    }

    // orcs.glob — requires READ
    {
        let sb = Arc::clone(sandbox);
        let f = lua.create_function(move |lua, (pattern, dir): (String, Option<String>)| {
            let has = with_context(lua, |ctx| Ok(ctx.has_capability(Capability::READ)))?;
            if !has {
                return cap_denied(lua, "READ");
            }
            let result = lua.create_table()?;
            match tool_glob(&pattern, dir.as_deref(), sb.as_ref()) {
                Ok(files) => {
                    let files_table = lua.create_table()?;
                    for (i, file) in files.iter().enumerate() {
                        files_table.set(i + 1, file.as_str())?;
                    }
                    result.set("ok", true)?;
                    result.set("files", files_table)?;
                    result.set("count", files.len())?;
                }
                Err(e) => {
                    result.set("ok", false)?;
                    result.set("error", e)?;
                }
            }
            Ok(result)
        })?;
        orcs_table.set("glob", f)?;
    }

    // orcs.mkdir — requires WRITE
    {
        let sb = Arc::clone(sandbox);
        let f = lua.create_function(move |lua, path: String| {
            let has = with_context(lua, |ctx| Ok(ctx.has_capability(Capability::WRITE)))?;
            if !has {
                return cap_denied(lua, "WRITE");
            }
            let result = lua.create_table()?;
            match tool_mkdir(&path, sb.as_ref()) {
                Ok(()) => result.set("ok", true)?,
                Err(e) => {
                    result.set("ok", false)?;
                    result.set("error", e)?;
                }
            }
            Ok(result)
        })?;
        orcs_table.set("mkdir", f)?;
    }

    // orcs.remove — requires DELETE
    {
        let sb = Arc::clone(sandbox);
        let f = lua.create_function(move |lua, path: String| {
            let has = with_context(lua, |ctx| Ok(ctx.has_capability(Capability::DELETE)))?;
            if !has {
                return cap_denied(lua, "DELETE");
            }
            let result = lua.create_table()?;
            match tool_remove(&path, sb.as_ref()) {
                Ok(()) => result.set("ok", true)?,
                Err(e) => {
                    result.set("ok", false)?;
                    result.set("error", e)?;
                }
            }
            Ok(result)
        })?;
        orcs_table.set("remove", f)?;
    }

    // orcs.mv — requires DELETE | WRITE
    {
        let sb = Arc::clone(sandbox);
        let f = lua.create_function(move |lua, (src, dst): (String, String)| {
            let has = with_context(lua, |ctx| {
                let needs = Capability::DELETE | Capability::WRITE;
                Ok(ctx.capabilities().contains(needs))
            })?;
            if !has {
                return cap_denied(lua, "DELETE | WRITE");
            }
            let result = lua.create_table()?;
            match tool_mv(&src, &dst, sb.as_ref()) {
                Ok(()) => result.set("ok", true)?,
                Err(e) => {
                    result.set("ok", false)?;
                    result.set("error", e)?;
                }
            }
            Ok(result)
        })?;
        orcs_table.set("mv", f)?;
    }

    tracing::debug!("registered capability-gated file tools");
    Ok(())
}

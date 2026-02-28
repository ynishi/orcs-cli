//! Inter-component communication function registration.
//!
//! Functions registered:
//! - `orcs.send_to_child(id, msg)` — send message to child (sync)
//! - `orcs.send_to_child_async(id, msg)` — send message to child (fire-and-forget)
//! - `orcs.send_to_children_batch(ids, inputs)` — parallel batch send
//! - `orcs.request_batch(requests)` — parallel RPC batch

use crate::types::{lua_to_json, serde_json_to_lua};
use mlua::{Lua, LuaSerdeExt, Table};
use orcs_component::ChildContext;
use parking_lot::Mutex;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    orcs_table: &Table,
    ctx: &Arc<Mutex<Box<dyn ChildContext>>>,
) -> Result<(), mlua::Error> {
    // ── orcs.send_to_child(child_id, message) ────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let send_to_child_fn =
            lua.create_function(move |lua, (child_id, message): (String, mlua::Value)| {
                let ctx_guard = ctx_clone.lock();

                // Convert Lua value to JSON
                let input = lua_to_json(message, lua)?;

                let result_table = lua.create_table()?;
                match ctx_guard.send_to_child(&child_id, input) {
                    Ok(child_result) => {
                        result_table.set("ok", true)?;
                        // Convert ChildResult to Lua
                        match child_result {
                            orcs_component::ChildResult::Ok(data) => {
                                // Convert JSON to Lua value safely (no eval)
                                let lua_data = serde_json_to_lua(&data, lua)?;
                                result_table.set("result", lua_data)?;
                            }
                            orcs_component::ChildResult::Err(e) => {
                                result_table.set("ok", false)?;
                                result_table.set("error", e.to_string())?;
                            }
                            orcs_component::ChildResult::Aborted => {
                                result_table.set("ok", false)?;
                                result_table.set("error", "child aborted")?;
                            }
                        }
                    }
                    Err(e) => {
                        result_table.set("ok", false)?;
                        result_table.set("error", e.to_string())?;
                    }
                }

                Ok(result_table)
            })?;
        orcs_table.set("send_to_child", send_to_child_fn)?;
    }

    // ── orcs.send_to_child_async(child_id, message) ──────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let send_to_child_async_fn =
            lua.create_function(move |lua, (child_id, message): (String, mlua::Value)| {
                let ctx_guard = ctx_clone.lock();

                let input = lua_to_json(message, lua)?;

                let result_table = lua.create_table()?;
                match ctx_guard.send_to_child_async(&child_id, input) {
                    Ok(()) => {
                        result_table.set("ok", true)?;
                    }
                    Err(e) => {
                        result_table.set("ok", false)?;
                        result_table.set("error", e.to_string())?;
                    }
                }

                Ok(result_table)
            })?;
        orcs_table.set("send_to_child_async", send_to_child_async_fn)?;
    }

    // ── orcs.send_to_children_batch(ids, inputs) ─────────────────────
    //
    // ids:    Lua table (array) of child ID strings
    // inputs: Lua table (array) of input values (same length as ids)
    //
    // Returns a Lua table (array) of result tables, one per child,
    // in the same order as the input arrays.
    {
        let ctx_clone = Arc::clone(ctx);
        let send_batch_fn =
            lua.create_function(move |lua, (ids, inputs): (mlua::Table, mlua::Table)| {
                let ctx_guard = ctx_clone.lock();

                let ids_len = ids.len()? as usize;
                let inputs_len = inputs.len()? as usize;
                if ids_len != inputs_len {
                    return Err(mlua::Error::RuntimeError(format!(
                        "ids length ({}) != inputs length ({})",
                        ids_len, inputs_len
                    )));
                }

                let mut requests = Vec::with_capacity(ids_len);
                for i in 1..=ids_len {
                    let id: String = ids.get(i)?;
                    let input_val: mlua::Value = inputs.get(i)?;
                    let json_input = lua_to_json(input_val, lua)?;
                    requests.push((id, json_input));
                }

                let results = ctx_guard.send_to_children_batch(requests);
                drop(ctx_guard);

                let results_table = lua.create_table()?;
                for (i, (_id, result)) in results.into_iter().enumerate() {
                    let entry = lua.create_table()?;
                    match result {
                        Ok(orcs_component::ChildResult::Ok(data)) => {
                            entry.set("ok", true)?;
                            let lua_data = serde_json_to_lua(&data, lua)?;
                            entry.set("result", lua_data)?;
                        }
                        Ok(orcs_component::ChildResult::Err(e)) => {
                            entry.set("ok", false)?;
                            entry.set("error", e.to_string())?;
                        }
                        Ok(orcs_component::ChildResult::Aborted) => {
                            entry.set("ok", false)?;
                            entry.set("error", "child aborted")?;
                        }
                        Err(e) => {
                            entry.set("ok", false)?;
                            entry.set("error", e.to_string())?;
                        }
                    }
                    results_table.set(i + 1, entry)?; // Lua 1-indexed
                }

                Ok(results_table)
            })?;
        orcs_table.set("send_to_children_batch", send_batch_fn)?;
    }

    // ── orcs.request_batch(requests) ─────────────────────────────────
    //
    // requests: Lua table (array) of { target, operation, payload, timeout_ms? }
    //
    // All RPC calls execute concurrently (via ChildContext::request_batch).
    // Results are returned in the same order as the input array.
    {
        let ctx_clone = Arc::clone(ctx);
        let request_batch_fn = lua.create_function(move |lua, requests: mlua::Table| {
            let ctx_guard = ctx_clone.lock();

            let len = requests.len()? as usize;
            let mut batch = Vec::with_capacity(len);
            for i in 1..=len {
                let entry: mlua::Table = requests.get(i)?;
                let target: String = entry.get("target").map_err(|_| {
                    mlua::Error::RuntimeError(format!("requests[{}].target required", i))
                })?;
                let operation: String = entry.get("operation").map_err(|_| {
                    mlua::Error::RuntimeError(format!("requests[{}].operation required", i))
                })?;
                let payload_val: mlua::Value = entry.get("payload").unwrap_or(mlua::Value::Nil);
                let json_payload = lua_to_json(payload_val, lua)?;
                let timeout_ms: Option<u64> = entry.get("timeout_ms").ok();
                batch.push((target, operation, json_payload, timeout_ms));
            }

            let results = ctx_guard.request_batch(batch);
            drop(ctx_guard);

            let results_table = lua.create_table()?;
            for (i, result) in results.into_iter().enumerate() {
                let entry = lua.create_table()?;
                match result {
                    Ok(value) => {
                        entry.set("success", true)?;
                        let lua_data = lua.to_value(&value)?;
                        entry.set("data", lua_data)?;
                    }
                    Err(err) => {
                        entry.set("success", false)?;
                        entry.set("error", err)?;
                    }
                }
                results_table.set(i + 1, entry)?; // Lua 1-indexed
            }

            Ok(results_table)
        })?;
        orcs_table.set("request_batch", request_batch_fn)?;
    }

    Ok(())
}

//! Emitter Lua function registration.
//!
//! Registers the following functions into the `orcs` Lua table:
//! - `orcs.output(msg)` — emit output event
//! - `orcs.output_with_level(msg, level)` — emit output with level
//! - `orcs.emit_event(category, operation, payload)` — broadcast Extension event
//! - `orcs.board_recent(n)` — query shared Board
//! - `orcs.request(target, operation, payload, opts?)` — Component-to-Component RPC

use crate::error::LuaError;
use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use orcs_component::Emitter;
use parking_lot::Mutex;
use std::sync::Arc;

/// Registers emitter-backed Lua functions into the `orcs` Lua table.
///
/// Called when `set_emitter()` is invoked to enable event emission.
pub(super) fn register(lua: &Lua, emitter: Arc<Mutex<Box<dyn Emitter>>>) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;

    // orcs.output(msg) - emit output event via emitter
    let emitter_clone = Arc::clone(&emitter);
    let output_fn = lua.create_function(move |_, msg: String| {
        emitter_clone.lock().emit_output(&msg);
        Ok(())
    })?;
    orcs_table.set("output", output_fn)?;

    // orcs.output_with_level(msg, level) - emit output with level
    let emitter_clone2 = Arc::clone(&emitter);
    let output_level_fn = lua.create_function(move |_, (msg, level): (String, String)| {
        emitter_clone2.lock().emit_output_with_level(&msg, &level);
        Ok(())
    })?;
    orcs_table.set("output_with_level", output_level_fn)?;

    // orcs.emit_event(category, operation, payload) -> bool
    // Returns true if at least one channel received the event.
    // Note: "received" means injected into the channel buffer, not that a
    // subscriber matched.  The count includes the emitter's own channel.
    let emitter_clone3 = Arc::clone(&emitter);
    let emit_event_fn = lua.create_function(
        move |lua, (category, operation, payload): (String, String, LuaValue)| {
            let json_payload: serde_json::Value = lua.from_value(payload)?;
            let delivered = emitter_clone3
                .lock()
                .emit_event(&category, &operation, json_payload);
            Ok(delivered)
        },
    )?;
    orcs_table.set("emit_event", emit_event_fn)?;

    // orcs.board_recent(n) -> table[] - query shared Board via Emitter trait
    let emitter_clone4 = Arc::clone(&emitter);
    let board_recent_fn = lua.create_function(move |lua, n: usize| {
        let entries = emitter_clone4.lock().board_recent(n);

        let result = lua.create_table()?;
        for (i, entry) in entries.into_iter().enumerate() {
            let lua_val = lua.to_value(&entry)?;
            result.set(i + 1, lua_val)?;
        }
        Ok(result)
    })?;
    orcs_table.set("board_recent", board_recent_fn)?;

    // orcs.request(target, operation, payload, opts?) -> table
    // Component-to-Component RPC via EventBus routing
    let emitter_clone5 = Arc::clone(&emitter);
    let request_fn =
        lua.create_function(
            move |lua,
                  (target, operation, payload, opts): (
                String,
                String,
                LuaValue,
                Option<Table>,
            )| {
                let json_payload: serde_json::Value = lua.from_value(payload)?;
                let timeout_ms = opts.and_then(|t| t.get::<u64>("timeout_ms").ok());

                let em = emitter_clone5.lock();

                match em.request(&target, &operation, json_payload, timeout_ms) {
                    Ok(value) => {
                        let result = lua.create_table()?;
                        result.set("success", true)?;
                        let lua_data = lua.to_value(&value)?;
                        result.set("data", lua_data)?;
                        Ok(result)
                    }
                    Err(err) => {
                        let result = lua.create_table()?;
                        result.set("success", false)?;
                        result.set("error", err)?;
                        Ok(result)
                    }
                }
            },
        )?;
    orcs_table.set("request", request_fn)?;

    tracing::debug!(
        "Registered orcs.output, orcs.emit_event, orcs.board_recent, and orcs.request functions with emitter"
    );
    Ok(())
}

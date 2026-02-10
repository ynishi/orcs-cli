//! Lua Scenario Test Framework for ORCS.
//!
//! Provides infrastructure for writing integration tests as Lua scripts
//! that exercise the full ORCS runtime pipeline (EventBus → Component → Channel → Permission).
//!
//! # Architecture
//!
//! Two separate Lua VMs for clean isolation:
//!
//! ```text
//! ┌───────────────────┐         ┌───────────────────────┐
//! │  Scenario Lua VM  │         │  Component Lua VM     │
//! │  (test driver)    │ ──RPC─→ │  (under test)         │
//! │                   │         │                       │
//! │  orcs.test.*      │         │  on_request()         │
//! │  h:request()      │         │  on_signal()          │
//! │  h:veto()         │         │  orcs.read/write/...  │
//! └───────────────────┘         └───────────────────────┘
//! ```
//!
//! # Example Scenario
//!
//! ```lua
//! -- tests/scenarios/echo_basic.lua
//! local test = orcs.test
//!
//! return {
//!     name = "Echo Component Basic Scenarios",
//!     component = [[ return { id = "echo", ... } ]],
//!     scenarios = {
//!         { name = "echo works", run = function(h)
//!             local result = h:request("Echo", "echo", { message = "hi" })
//!             test.eq(result.message, "hi")
//!         end },
//!     },
//! }
//! ```

use crate::testing::LuaTestHarness;
use crate::types::{lua_to_json, parse_event_category, serde_json_to_lua};
use mlua::{Function, Lua, Result as LuaResult, Table, UserData, UserDataMethods, Value};
use orcs_component::testing::RequestResult;
use orcs_component::Status;
use orcs_event::SignalResponse;
use orcs_runtime::sandbox::SandboxPolicy;
use std::path::Path;
use std::sync::{Arc, Mutex};

// =============================================================================
// Test Assertions (orcs.test.*)
// =============================================================================

/// Registers `orcs.test.*` assertion functions in the Lua VM.
///
/// All assertion failures throw a Lua error, which propagates
/// to Rust as `mlua::Error` → test failure.
pub fn register_test_assertions(lua: &Lua) -> LuaResult<()> {
    // Ensure orcs table exists
    let orcs: Table = match lua.globals().get::<Table>("orcs") {
        Ok(t) => t,
        Err(_) => {
            let t = lua.create_table()?;
            lua.globals().set("orcs", t.clone())?;
            t
        }
    };

    let test_table = lua.create_table()?;

    // orcs.test.eq(actual, expected, msg?)
    test_table.set(
        "eq",
        lua.create_function(|lua, (actual, expected, msg): (Value, Value, Option<String>)| {
            let eq = lua_values_equal(&actual, &expected, lua);
            if !eq {
                let msg = msg.unwrap_or_default();
                let actual_str = format_lua_value(&actual, lua);
                let expected_str = format_lua_value(&expected, lua);
                return Err(mlua::Error::external(format!(
                    "ASSERT FAILED: eq\n  expected: {expected_str}\n  actual:   {actual_str}\n  {msg}"
                )));
            }
            Ok(())
        })?,
    )?;

    // orcs.test.neq(actual, expected, msg?)
    test_table.set(
        "neq",
        lua.create_function(
            |lua, (actual, expected, msg): (Value, Value, Option<String>)| {
                let eq = lua_values_equal(&actual, &expected, lua);
                if eq {
                    let msg = msg.unwrap_or_default();
                    let actual_str = format_lua_value(&actual, lua);
                    return Err(mlua::Error::external(format!(
                        "ASSERT FAILED: neq\n  value: {actual_str}\n  should not be equal\n  {msg}"
                    )));
                }
                Ok(())
            },
        )?,
    )?;

    // orcs.test.ok(value, msg?)
    test_table.set(
        "ok",
        lua.create_function(|lua, (value, msg): (Value, Option<String>)| {
            let truthy = is_truthy(&value);
            if !truthy {
                let msg = msg.unwrap_or_default();
                let val_str = format_lua_value(&value, lua);
                return Err(mlua::Error::external(format!(
                    "ASSERT FAILED: ok\n  value: {val_str}\n  expected truthy\n  {msg}"
                )));
            }
            Ok(())
        })?,
    )?;

    // orcs.test.fail(msg?)
    test_table.set(
        "fail",
        lua.create_function(|_, msg: Option<String>| -> LuaResult<()> {
            let msg = msg.unwrap_or_else(|| "explicit fail".to_string());
            Err(mlua::Error::external(format!(
                "ASSERT FAILED: fail\n  {msg}"
            )))
        })?,
    )?;

    // orcs.test.contains(str, substr, msg?)
    test_table.set(
        "contains",
        lua.create_function(
            |_, (haystack, needle, msg): (String, String, Option<String>)| -> LuaResult<()> {
                if !haystack.contains(&needle) {
                    let msg = msg.unwrap_or_default();
                    return Err(mlua::Error::external(format!(
                        "ASSERT FAILED: contains\n  string:    \"{haystack}\"\n  substring: \"{needle}\"\n  {msg}"
                    )));
                }
                Ok(())
            },
        )?,
    )?;

    // orcs.test.has_key(tbl, key, msg?)
    test_table.set(
        "has_key",
        lua.create_function(
            |_, (tbl, key, msg): (Table, String, Option<String>)| -> LuaResult<()> {
                let val: Value = tbl.get(key.as_str())?;
                if matches!(val, Value::Nil) {
                    let msg = msg.unwrap_or_default();
                    return Err(mlua::Error::external(format!(
                        "ASSERT FAILED: has_key\n  key \"{key}\" not found\n  {msg}"
                    )));
                }
                Ok(())
            },
        )?,
    )?;

    // orcs.test.len(tbl, expected, msg?)
    test_table.set(
        "len",
        lua.create_function(
            |_, (tbl, expected, msg): (Table, i64, Option<String>)| -> LuaResult<()> {
                let actual = tbl.raw_len() as i64;
                if actual != expected {
                    let msg = msg.unwrap_or_default();
                    return Err(mlua::Error::external(format!(
                        "ASSERT FAILED: len\n  expected: {expected}\n  actual:   {actual}\n  {msg}"
                    )));
                }
                Ok(())
            },
        )?,
    )?;

    // orcs.test.gt(a, b, msg?)
    test_table.set(
        "gt",
        lua.create_function(
            |_, (a, b, msg): (f64, f64, Option<String>)| -> LuaResult<()> {
                if a <= b {
                    let msg = msg.unwrap_or_default();
                    return Err(mlua::Error::external(format!(
                        "ASSERT FAILED: gt\n  {a} is not > {b}\n  {msg}"
                    )));
                }
                Ok(())
            },
        )?,
    )?;

    // orcs.test.type_is(value, typename, msg?)
    test_table.set(
        "type_is",
        lua.create_function(
            |_, (value, expected_type, msg): (Value, String, Option<String>)| -> LuaResult<()> {
                let actual_type = lua_type_name(&value);
                if actual_type != expected_type {
                    let msg = msg.unwrap_or_default();
                    return Err(mlua::Error::external(format!(
                        "ASSERT FAILED: type_is\n  expected type: {expected_type}\n  actual type:   {actual_type}\n  {msg}"
                    )));
                }
                Ok(())
            },
        )?,
    )?;

    orcs.set("test", test_table)?;
    Ok(())
}

// =============================================================================
// ScenarioHarness (mlua::UserData)
// =============================================================================

/// Lua UserData wrapper around LuaTestHarness.
///
/// Exposes the test harness API to Lua scenario scripts via method calls:
/// - `h:request(category, operation, payload)` → table (data) or error
/// - `h:veto()` → string ("Abort"/"Handled"/"Ignored")
/// - `h:cancel()` → string
/// - `h:approve(approval_id)` → string
/// - `h:reject(approval_id, reason?)` → string
/// - `h:status()` → string ("Idle"/"Running"/"Aborted"/...)
/// - `h:init()` → nil or error
/// - `h:shutdown()` → nil
/// - `h:request_log()` → array of tables
/// - `h:signal_log()` → array of tables
/// - `h:clear_logs()` → nil
struct ScenarioHarness {
    inner: Arc<Mutex<LuaTestHarness>>,
}

impl UserData for ScenarioHarness {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // h:request(category, operation, payload) → table or error
        methods.add_method(
            "request",
            |lua, this, (cat_str, op, payload_lua): (String, String, Value)| {
                let category = parse_event_category(&cat_str)
                    .ok_or_else(|| mlua::Error::external("invalid event category"))?;

                let json_payload = lua_to_json(payload_lua, lua).unwrap_or(serde_json::Value::Null);

                let mut harness = this
                    .inner
                    .lock()
                    .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;

                match harness.request(category, &op, json_payload) {
                    Ok(val) => serde_json_to_lua(&val, lua),
                    Err(e) => Err(mlua::Error::external(format!("request failed: {e}"))),
                }
            },
        );

        // h:veto() → string
        methods.add_method("veto", |_, this, ()| {
            let mut harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            let response = harness.veto();
            Ok(signal_response_to_string(response))
        });

        // h:cancel() → string
        methods.add_method("cancel", |_, this, ()| {
            let mut harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            let response = harness.cancel();
            Ok(signal_response_to_string(response))
        });

        // h:approve(approval_id) → string
        methods.add_method("approve", |_, this, approval_id: String| {
            let mut harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            let response = harness.approve(&approval_id);
            Ok(signal_response_to_string(response))
        });

        // h:reject(approval_id, reason?) → string
        methods.add_method(
            "reject",
            |_, this, (approval_id, reason): (String, Option<String>)| {
                let mut harness = this
                    .inner
                    .lock()
                    .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
                let response = harness.reject(&approval_id, reason);
                Ok(signal_response_to_string(response))
            },
        );

        // h:status() → string
        methods.add_method("status", |_, this, ()| {
            let harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            Ok(status_to_string(harness.status()))
        });

        // h:init() → nil or error
        methods.add_method("init", |_, this, ()| {
            let mut harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            harness
                .init()
                .map_err(|e| mlua::Error::external(format!("init failed: {e}")))?;
            Ok(())
        });

        // h:shutdown() → nil
        methods.add_method("shutdown", |_, this, ()| {
            let mut harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            harness.shutdown();
            Ok(())
        });

        // h:abort() → nil
        methods.add_method("abort", |_, this, ()| {
            let mut harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            harness.abort();
            Ok(())
        });

        // h:request_log() → array of tables
        methods.add_method("request_log", |lua, this, ()| {
            let harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            let log = harness.request_log();
            let table = lua.create_table()?;
            for (i, record) in log.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("operation", record.operation.as_str())?;
                entry.set("category", record.category.to_string())?;
                entry.set("success", matches!(record.result, RequestResult::Ok(_)))?;
                table.raw_set(i + 1, entry)?;
            }
            Ok(table)
        });

        // h:signal_log() → array of tables
        methods.add_method("signal_log", |lua, this, ()| {
            let harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            let log = harness.signal_log();
            let table = lua.create_table()?;
            for (i, record) in log.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("kind", record.kind.clone())?;
                entry.set("response", record.response.clone())?;
                table.raw_set(i + 1, entry)?;
            }
            Ok(table)
        });

        // h:clear_logs() → nil
        methods.add_method("clear_logs", |_, this, ()| {
            let mut harness = this
                .inner
                .lock()
                .map_err(|e| mlua::Error::external(format!("lock poisoned: {e}")))?;
            harness.clear_logs();
            Ok(())
        });
    }
}

// =============================================================================
// ScenarioRunner
// =============================================================================

/// Result of running a single scenario test case.
#[derive(Debug)]
pub enum ScenarioResult {
    /// Test passed.
    Pass(String),
    /// Test failed with error message.
    Fail(String, String),
}

impl ScenarioResult {
    /// Returns true if the test passed.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass(_))
    }

    /// Returns the scenario name.
    pub fn name(&self) -> &str {
        match self {
            Self::Pass(n) | Self::Fail(n, _) => n,
        }
    }
}

/// Result of running all scenarios in a file.
pub struct ScenarioFileResult {
    /// File path or name.
    pub file: String,
    /// Suite name from Lua.
    pub suite_name: String,
    /// Individual scenario results.
    pub results: Vec<ScenarioResult>,
}

impl ScenarioFileResult {
    /// Returns true if all scenarios passed.
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(|r| r.is_pass())
    }

    /// Returns count of passed/failed scenarios.
    pub fn summary(&self) -> (usize, usize) {
        let passed = self.results.iter().filter(|r| r.is_pass()).count();
        let failed = self.results.len() - passed;
        (passed, failed)
    }

    /// Formats a human-readable report.
    pub fn report(&self) -> String {
        let mut lines = Vec::new();
        let (passed, failed) = self.summary();
        lines.push(format!(
            "━━ {} ━━  ({} passed, {} failed)",
            self.suite_name, passed, failed,
        ));

        for result in &self.results {
            match result {
                ScenarioResult::Pass(name) => {
                    lines.push(format!("  ✓ {name}"));
                }
                ScenarioResult::Fail(name, err) => {
                    lines.push(format!("  ✗ {name}"));
                    // Indent error message
                    for line in err.lines() {
                        lines.push(format!("    {line}"));
                    }
                }
            }
        }
        lines.join("\n")
    }
}

/// Runs Lua scenario test files.
///
/// Each file should return a Lua table with:
/// - `name`: Suite name (string)
/// - `component`: Inline Lua script for the component under test (string)
///   OR `component_name`: Name of a script in the crate's scripts/ directory (string)
///   OR `component_file`: Path to a Lua script file (string)
/// - `scenarios`: Array of `{ name: string, run: function(harness) }` tables
pub struct ScenarioRunner {
    sandbox: Arc<dyn SandboxPolicy>,
}

impl ScenarioRunner {
    /// Creates a new runner with the given sandbox policy.
    pub fn new(sandbox: Arc<dyn SandboxPolicy>) -> Self {
        Self { sandbox }
    }

    /// Runs all scenarios in a single Lua file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be loaded or parsed.
    pub fn run_file(&self, path: &Path) -> Result<ScenarioFileResult, String> {
        let file_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        // 1. Create scenario VM and register test assertions
        let scenario_lua = Lua::new();
        register_test_assertions(&scenario_lua)
            .map_err(|e| format!("failed to register test assertions: {e}"))?;

        // 2. Load scenario file
        let script = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

        let scenario_table: Table = scenario_lua
            .load(&script)
            .set_name(file_name.as_str())
            .eval()
            .map_err(|e| format!("failed to parse scenario: {e}"))?;

        // 3. Extract metadata
        let suite_name: String = scenario_table
            .get("name")
            .unwrap_or_else(|_| file_name.clone());

        // 4. Resolve component source
        let component_source = self
            .resolve_component_source(&scenario_table)
            .map_err(|e| format!("failed to resolve component: {e}"))?;

        // 5. Get scenarios array
        let scenarios: Table = scenario_table
            .get("scenarios")
            .map_err(|e| format!("missing 'scenarios' field: {e}"))?;

        // 6. Run each scenario
        let mut results = Vec::new();

        for entry in scenarios.sequence_values::<Table>() {
            let entry = entry.map_err(|e| format!("invalid scenario entry: {e}"))?;

            let name: String = entry.get("name").unwrap_or_else(|_| "unnamed".to_string());

            let run_fn: Function = match entry.get("run") {
                Ok(f) => f,
                Err(e) => {
                    results.push(ScenarioResult::Fail(
                        name,
                        format!("missing 'run' function: {e}"),
                    ));
                    continue;
                }
            };

            // Fresh harness for each scenario
            let harness_result =
                LuaTestHarness::from_script(&component_source, self.sandbox.clone());

            let harness = match harness_result {
                Ok(h) => h,
                Err(e) => {
                    results.push(ScenarioResult::Fail(
                        name,
                        format!("component creation failed: {e}"),
                    ));
                    continue;
                }
            };

            let harness_proxy = ScenarioHarness {
                inner: Arc::new(Mutex::new(harness)),
            };

            match run_fn.call::<()>(harness_proxy) {
                Ok(()) => results.push(ScenarioResult::Pass(name)),
                Err(e) => results.push(ScenarioResult::Fail(name, e.to_string())),
            }
        }

        Ok(ScenarioFileResult {
            file: file_name,
            suite_name,
            results,
        })
    }

    /// Runs a scenario from an inline Lua string (for Rust tests).
    pub fn run_inline(&self, script: &str) -> Result<ScenarioFileResult, String> {
        let scenario_lua = Lua::new();
        register_test_assertions(&scenario_lua)
            .map_err(|e| format!("failed to register test assertions: {e}"))?;

        let scenario_table: Table = scenario_lua
            .load(script)
            .set_name("inline")
            .eval()
            .map_err(|e| format!("failed to parse scenario: {e}"))?;

        let suite_name: String = scenario_table
            .get("name")
            .unwrap_or_else(|_| "inline".to_string());

        let component_source = self
            .resolve_component_source(&scenario_table)
            .map_err(|e| format!("failed to resolve component: {e}"))?;

        let scenarios: Table = scenario_table
            .get("scenarios")
            .map_err(|e| format!("missing 'scenarios' field: {e}"))?;

        let mut results = Vec::new();

        for entry in scenarios.sequence_values::<Table>() {
            let entry = entry.map_err(|e| format!("invalid scenario entry: {e}"))?;
            let name: String = entry.get("name").unwrap_or_else(|_| "unnamed".to_string());
            let run_fn: Function = match entry.get("run") {
                Ok(f) => f,
                Err(e) => {
                    results.push(ScenarioResult::Fail(name, format!("missing 'run': {e}")));
                    continue;
                }
            };

            let harness = match LuaTestHarness::from_script(&component_source, self.sandbox.clone())
            {
                Ok(h) => h,
                Err(e) => {
                    results.push(ScenarioResult::Fail(name, format!("component load: {e}")));
                    continue;
                }
            };

            let proxy = ScenarioHarness {
                inner: Arc::new(Mutex::new(harness)),
            };

            match run_fn.call::<()>(proxy) {
                Ok(()) => results.push(ScenarioResult::Pass(name)),
                Err(e) => results.push(ScenarioResult::Fail(name, e.to_string())),
            }
        }

        Ok(ScenarioFileResult {
            file: "inline".to_string(),
            suite_name,
            results,
        })
    }

    /// Resolves the component source from the scenario table.
    fn resolve_component_source(&self, table: &Table) -> Result<String, String> {
        // Priority: component > component_name > component_file
        if let Ok(inline) = table.get::<String>("component") {
            return Ok(inline);
        }

        // component_name: resolve from crate scripts directory by name
        // Also supports legacy key "component_embedded" for backward compatibility
        let name_key = table
            .get::<String>("component_name")
            .or_else(|_| table.get::<String>("component_embedded"));
        if let Ok(name) = name_key {
            let scripts_dir = crate::ScriptLoader::crate_scripts_dir();
            // Try single file: {scripts}/{name}.lua
            let file_path = scripts_dir.join(format!("{name}.lua"));
            if file_path.exists() {
                return std::fs::read_to_string(&file_path)
                    .map_err(|e| format!("failed to read script '{name}': {e}"));
            }
            // Try directory: {scripts}/{name}/init.lua
            let init_path = scripts_dir.join(&name).join("init.lua");
            if init_path.exists() {
                return std::fs::read_to_string(&init_path)
                    .map_err(|e| format!("failed to read '{name}/init.lua': {e}"));
            }
            return Err(format!(
                "component '{name}' not found in {}",
                scripts_dir.display()
            ));
        }

        if let Ok(path_str) = table.get::<String>("component_file") {
            return std::fs::read_to_string(&path_str)
                .map_err(|e| format!("failed to read component file '{path_str}': {e}"));
        }

        Err(
            "scenario must specify one of: 'component' (inline), 'component_name', or 'component_file'"
                .to_string(),
        )
    }
}

// =============================================================================
// Helper functions
// =============================================================================

fn signal_response_to_string(r: SignalResponse) -> String {
    match r {
        SignalResponse::Abort => "Abort".to_string(),
        SignalResponse::Handled => "Handled".to_string(),
        SignalResponse::Ignored => "Ignored".to_string(),
    }
}

fn status_to_string(s: Status) -> String {
    match s {
        Status::Initializing => "Initializing".to_string(),
        Status::Idle => "Idle".to_string(),
        Status::Running => "Running".to_string(),
        Status::Paused => "Paused".to_string(),
        Status::AwaitingApproval => "AwaitingApproval".to_string(),
        Status::Completed => "Completed".to_string(),
        Status::Error => "Error".to_string(),
        Status::Aborted => "Aborted".to_string(),
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Nil => false,
        Value::Boolean(b) => *b,
        _ => true,
    }
}

fn lua_type_name(value: &Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(_) => "boolean".to_string(),
        Value::Integer(_) | Value::Number(_) => "number".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Table(_) => "table".to_string(),
        Value::Function(_) => "function".to_string(),
        Value::UserData(_) => "userdata".to_string(),
        _ => "other".to_string(),
    }
}

/// Compare two Lua values for equality (deep comparison for tables).
fn lua_values_equal(a: &Value, b: &Value, lua: &Lua) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Boolean(a), Value::Boolean(b)) => a == b,
        (Value::Integer(a), Value::Integer(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => (a - b).abs() < f64::EPSILON,
        // Cross-type numeric comparison (Lua treats integer and number as same type)
        (Value::Integer(a), Value::Number(b)) | (Value::Number(b), Value::Integer(a)) => {
            (*a as f64 - b).abs() < f64::EPSILON
        }
        (Value::String(a), Value::String(b)) => a.as_bytes() == b.as_bytes(),
        (Value::Table(a), Value::Table(b)) => {
            // Convert both to JSON for deep comparison
            let ja = lua_to_json(Value::Table(a.clone()), lua);
            let jb = lua_to_json(Value::Table(b.clone()), lua);
            match (ja, jb) {
                (Ok(ja), Ok(jb)) => ja == jb,
                _ => false,
            }
        }
        _ => false,
    }
}

/// Format a Lua value for error messages.
fn format_lua_value(value: &Value, lua: &Lua) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("\"{}\"", s.to_string_lossy()),
        Value::Table(_) => match lua_to_json(value.clone(), lua) {
            Ok(j) => j.to_string(),
            Err(_) => "<table>".to_string(),
        },
        Value::Function(_) => "<function>".to_string(),
        Value::UserData(_) => "<userdata>".to_string(),
        _ => "<other>".to_string(),
    }
}

// =============================================================================
// Convenience: assert helper for #[test]
// =============================================================================

/// Asserts that all scenarios in a result passed.
///
/// Panics with a detailed report if any scenario failed.
pub fn assert_all_pass(result: &ScenarioFileResult) {
    if !result.all_passed() {
        // orcs-lint: allow(no-panic) reason="test assertion helper — panic is intentional"
        panic!("\n{}\n", result.report());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;

    fn test_sandbox() -> Arc<dyn SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    #[test]
    fn test_assertions_eq_pass() {
        let lua = Lua::new();
        register_test_assertions(&lua).unwrap();
        let result: LuaResult<()> = lua.load("orcs.test.eq(1, 1)").exec();
        assert!(result.is_ok());
    }

    #[test]
    fn test_assertions_eq_fail() {
        let lua = Lua::new();
        register_test_assertions(&lua).unwrap();
        let result: LuaResult<()> = lua.load("orcs.test.eq(1, 2)").exec();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ASSERT FAILED: eq"));
    }

    #[test]
    fn test_assertions_eq_with_message() {
        let lua = Lua::new();
        register_test_assertions(&lua).unwrap();
        let result: LuaResult<()> = lua
            .load(r#"orcs.test.eq("a", "b", "values should match")"#)
            .exec();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("values should match"));
    }

    #[test]
    fn test_assertions_ok_truthy() {
        let lua = Lua::new();
        register_test_assertions(&lua).unwrap();
        assert!(lua.load("orcs.test.ok(true)").exec().is_ok());
        assert!(lua.load("orcs.test.ok(1)").exec().is_ok());
        assert!(lua.load(r#"orcs.test.ok("hello")"#).exec().is_ok());
        assert!(lua.load("orcs.test.ok({})").exec().is_ok());
    }

    #[test]
    fn test_assertions_ok_falsy() {
        let lua = Lua::new();
        register_test_assertions(&lua).unwrap();
        assert!(lua.load("orcs.test.ok(false)").exec().is_err());
        assert!(lua.load("orcs.test.ok(nil)").exec().is_err());
    }

    #[test]
    fn test_assertions_contains() {
        let lua = Lua::new();
        register_test_assertions(&lua).unwrap();
        assert!(lua
            .load(r#"orcs.test.contains("hello world", "world")"#)
            .exec()
            .is_ok());
        assert!(lua
            .load(r#"orcs.test.contains("hello", "xyz")"#)
            .exec()
            .is_err());
    }

    #[test]
    fn test_assertions_fail() {
        let lua = Lua::new();
        register_test_assertions(&lua).unwrap();
        let result: LuaResult<()> = lua.load(r#"orcs.test.fail("boom")"#).exec();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn run_inline_scenario_pass() {
        let runner = ScenarioRunner::new(test_sandbox());
        let result = runner
            .run_inline(
                r#"
            return {
                name = "inline test",
                component = [[
                    return {
                        id = "echo",
                        subscriptions = {"Echo"},
                        on_request = function(req)
                            return { success = true, data = req.payload }
                        end,
                        on_signal = function(sig)
                            if sig.kind == "Veto" then return "Abort" end
                            return "Handled"
                        end,
                    }
                ]],
                scenarios = {
                    { name = "echo works", run = function(h)
                        local result = h:request("Echo", "echo", { message = "hi" })
                        orcs.test.eq(result.message, "hi")
                    end },
                    { name = "status is idle", run = function(h)
                        orcs.test.eq(h:status(), "Idle")
                    end },
                },
            }
        "#,
            )
            .unwrap();

        assert_all_pass(&result);
        assert_eq!(result.results.len(), 2);
    }

    #[test]
    fn run_inline_scenario_with_failure() {
        let runner = ScenarioRunner::new(test_sandbox());
        let result = runner
            .run_inline(
                r#"
            return {
                name = "failing test",
                component = [[
                    return {
                        id = "echo",
                        subscriptions = {"Echo"},
                        on_request = function(req)
                            return { success = true, data = { value = 42 } }
                        end,
                        on_signal = function(sig)
                            return "Handled"
                        end,
                    }
                ]],
                scenarios = {
                    { name = "this should fail", run = function(h)
                        local result = h:request("Echo", "test", {})
                        orcs.test.eq(result.value, 99, "expected 99 but got 42")
                    end },
                },
            }
        "#,
            )
            .unwrap();

        assert!(!result.all_passed());
        assert!(matches!(&result.results[0], ScenarioResult::Fail(_, _)));
    }

    #[test]
    fn run_inline_veto_scenario() {
        let runner = ScenarioRunner::new(test_sandbox());
        let result = runner
            .run_inline(
                r#"
            return {
                name = "veto test",
                component = [[
                    return {
                        id = "echo",
                        subscriptions = {"Echo"},
                        on_request = function(req) return { success = true } end,
                        on_signal = function(sig)
                            if sig.kind == "Veto" then return "Abort" end
                            return "Handled"
                        end,
                    }
                ]],
                scenarios = {
                    { name = "veto aborts", run = function(h)
                        local response = h:veto()
                        orcs.test.eq(response, "Abort")
                        orcs.test.eq(h:status(), "Aborted")
                    end },
                },
            }
        "#,
            )
            .unwrap();

        assert_all_pass(&result);
    }

    #[test]
    fn run_inline_embedded_component() {
        let runner = ScenarioRunner::new(test_sandbox());
        let result = runner
            .run_inline(
                r#"
            return {
                name = "embedded echo test",
                component_embedded = "echo",
                scenarios = {
                    { name = "embedded echo loads", run = function(h)
                        orcs.test.eq(h:status(), "Idle")
                    end },
                },
            }
        "#,
            )
            .unwrap();

        assert_all_pass(&result);
    }

    #[test]
    fn fresh_harness_per_scenario() {
        let runner = ScenarioRunner::new(test_sandbox());
        let result = runner
            .run_inline(
                r#"
            return {
                name = "isolation test",
                component = [[
                    return {
                        id = "echo",
                        subscriptions = {"Echo"},
                        on_request = function(req) return { success = true } end,
                        on_signal = function(sig)
                            if sig.kind == "Veto" then return "Abort" end
                            return "Handled"
                        end,
                    }
                ]],
                scenarios = {
                    { name = "first: veto", run = function(h)
                        h:veto()
                        orcs.test.eq(h:status(), "Aborted")
                    end },
                    { name = "second: fresh state", run = function(h)
                        -- Should NOT be aborted because harness is fresh
                        orcs.test.eq(h:status(), "Idle")
                    end },
                },
            }
        "#,
            )
            .unwrap();

        assert_all_pass(&result);
    }

    #[test]
    fn report_format() {
        let result = ScenarioFileResult {
            file: "test".to_string(),
            suite_name: "Test Suite".to_string(),
            results: vec![
                ScenarioResult::Pass("passes".to_string()),
                ScenarioResult::Fail("fails".to_string(), "some error".to_string()),
            ],
        };

        let report = result.report();
        assert!(report.contains("Test Suite"));
        assert!(report.contains("✓ passes"));
        assert!(report.contains("✗ fails"));
        assert!(report.contains("some error"));
        assert!(report.contains("1 passed, 1 failed"));
    }
}

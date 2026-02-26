//! Sandboxed Lua code evaluation.
//!
//! Provides `orcs.sandbox_eval(code)` — a Rust-backed function that evaluates
//! arbitrary Lua code strings in a restricted environment.
//!
//! # Security Model
//!
//! The sandbox uses a **whitelist-only** environment table. Only safe standard
//! library functions are exposed. Dangerous globals (`os`, `io`, `debug`,
//! `require`, `load`, `loadfile`, `dofile`) are not present.
//!
//! An instruction-count hook (via `mlua::Lua::set_hook`) enforces a hard
//! execution limit to prevent infinite loops. Output from `print()` is
//! captured into a buffer instead of going to stdout.
//!
//! # Why Rust?
//!
//! The orcs Lua VM removes `load` and `debug` from globals
//! (see `sandbox_lua_globals`). Without `load()` we cannot compile code
//! strings from Lua, and without `debug.setupvalue` / `debug.sethook`
//! we cannot isolate environments or set instruction limits. mlua's Rust
//! API (`Lua::load`, `Chunk::set_environment`, `Lua::set_hook`) operates
//! independently of Lua globals, making Rust the natural implementation point.

use mlua::{HookTriggers, Lua, Result as LuaResult, Table, Value};
use std::sync::{Arc, Mutex};

/// Maximum Lua instructions before aborting execution.
const MAX_INSTRUCTIONS: u32 = 1_000_000;

/// Maximum captured output bytes.
const MAX_OUTPUT_BYTES: usize = 32_768;

/// Registers `orcs.sandbox_eval(code)` on the given orcs table.
///
/// Must be called **before** `sandbox_lua_globals()` strips `load` from globals,
/// though this function does not depend on the Lua `load` global — it uses
/// `mlua::Lua::load()` directly.
pub fn register_sandbox_eval(lua: &Lua, orcs_table: &Table) -> LuaResult<()> {
    let sandbox_eval_fn = lua.create_function(sandbox_eval)?;
    orcs_table.set("sandbox_eval", sandbox_eval_fn)?;
    Ok(())
}

/// Core sandbox evaluation function exposed as `orcs.sandbox_eval(code)`.
///
/// Returns a Lua table: `{ ok: bool, output: string, result?: string, error?: string }`
///
/// # Thread Safety
///
/// `set_hook` / `remove_hook` are called around `chunk.eval()`. This is safe because
/// mlua's `Lua` is `!Send + !Sync`, so concurrent access from another thread is
/// prevented at compile time.
fn sandbox_eval(lua: &Lua, code: String) -> LuaResult<Table> {
    if code.is_empty() {
        return build_error(lua, "code must be a non-empty string", "");
    }

    // Output capture buffer (shared with the print closure)
    let output_buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let output_size: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

    // Build sandbox environment table
    let env = build_sandbox_env(lua, Arc::clone(&output_buf), Arc::clone(&output_size))?;

    // Compile the code using mlua (bypasses Lua's global `load` removal)
    let chunk = lua.load(&code).set_name("=sandbox").set_environment(env);

    // Set instruction limit hook
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(MAX_INSTRUCTIONS),
        |_lua, _debug| {
            Err(mlua::Error::RuntimeError(format!(
                "instruction limit exceeded ({MAX_INSTRUCTIONS})"
            )))
        },
    );

    // Execute
    let exec_result: LuaResult<Value> = chunk.eval();

    // Clear hook immediately
    lua.remove_hook();

    // Collect output (recover from poisoned mutex to avoid silent data loss)
    let output_str = output_buf
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .join("\n");

    match exec_result {
        Ok(value) => {
            let result_table = lua.create_table()?;
            result_table.set("ok", true)?;
            result_table.set("output", output_str.as_str())?;

            // Serialize return value if non-nil
            if !matches!(value, Value::Nil) {
                let result_str = lua_value_to_string(lua, &value);
                result_table.set("result", result_str)?;
            }

            Ok(result_table)
        }
        Err(e) => {
            let err_msg = format_lua_error(&e);
            build_error(lua, &err_msg, &output_str)
        }
    }
}

/// Builds the whitelist-only sandbox environment table.
fn build_sandbox_env(
    lua: &Lua,
    output_buf: Arc<Mutex<Vec<String>>>,
    output_size: Arc<Mutex<usize>>,
) -> LuaResult<Table> {
    let env = lua.create_table()?;

    // --- Captured print ---
    let buf = Arc::clone(&output_buf);
    let size = Arc::clone(&output_size);
    let print_fn = lua.create_function(move |_, args: mlua::MultiValue| {
        let parts: Vec<String> = args.iter().map(lua_display).collect();
        let line = parts.join("\t");
        let line_len = line.len() + 1; // +1 for newline
        if let Ok(mut sz) = size.lock() {
            *sz += line_len;
            if *sz <= MAX_OUTPUT_BYTES {
                if let Ok(mut b) = buf.lock() {
                    b.push(line);
                }
            }
        }
        Ok(())
    })?;
    env.set("print", print_fn)?;

    // --- Core language functions ---
    let globals = lua.globals();
    for name in &[
        "tostring",
        "tonumber",
        "type",
        "pairs",
        "ipairs",
        "next",
        "select",
        "error",
        "pcall",
        "xpcall",
        "assert",
        "rawget",
        "rawset",
        "rawlen",
        "rawequal",
        "setmetatable",
        "getmetatable",
        "unpack",
    ] {
        if let Ok(val) = globals.get::<Value>(*name) {
            if !matches!(val, Value::Nil) {
                env.set(*name, val)?;
            }
        }
    }

    // --- Safe standard libraries (read-only references) ---
    for lib_name in &["math", "string", "table"] {
        if let Ok(val) = globals.get::<Value>(*lib_name) {
            if !matches!(val, Value::Nil) {
                env.set(*lib_name, val)?;
            }
        }
    }

    // --- Read-only orcs subset ---
    let orcs: Table = globals.get("orcs")?;
    let safe_orcs = lua.create_table()?;

    // JSON encode/decode
    if let Ok(f) = orcs.get::<mlua::Function>("json_encode") {
        safe_orcs.set("json_encode", f)?;
    }
    if let Ok(f) = orcs.get::<mlua::Function>("json_decode") {
        safe_orcs.set("json_decode", f)?;
    }

    // pwd (string value)
    if let Ok(pwd) = orcs.get::<String>("pwd") {
        safe_orcs.set("pwd", pwd)?;
    }

    // git_info (read-only function)
    if let Ok(f) = orcs.get::<mlua::Function>("git_info") {
        safe_orcs.set("git_info", f)?;
    }

    // Read-only file tools
    if let Ok(f) = orcs.get::<mlua::Function>("read") {
        safe_orcs.set("read", f)?;
    }
    if let Ok(f) = orcs.get::<mlua::Function>("grep") {
        safe_orcs.set("grep", f)?;
    }
    if let Ok(f) = orcs.get::<mlua::Function>("glob") {
        safe_orcs.set("glob", f)?;
    }

    env.set("orcs", safe_orcs)?;

    Ok(env)
}

/// Convert a Lua value to a display string (for print()).
fn lua_display(value: &Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => {
            // Match Lua's default: no trailing zeros for integers
            if *n == (*n as i64) as f64 {
                format!("{}.0", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Value::String(s) => s
            .to_str()
            .map_or_else(|_| "<invalid utf8>".into(), |s| s.to_string()),
        Value::Table(_) => format!("table: {:p}", value),
        Value::Function(_) => format!("function: {:p}", value),
        Value::UserData(_) => format!("userdata: {:p}", value),
        _ => format!("{value:?}"),
    }
}

/// Convert a Lua return value to a string for the result field.
fn lua_value_to_string(lua: &Lua, value: &Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => format!("{n}"),
        Value::String(s) => s
            .to_str()
            .map_or_else(|_| "<invalid utf8>".into(), |s| s.to_string()),
        Value::Table(_) => {
            // Try JSON serialization for tables
            match crate::types::lua_to_json(value.clone(), lua) {
                Ok(json) => serde_json::to_string(&json).unwrap_or_else(|_| "<table>".to_string()),
                Err(_) => "<table>".to_string(),
            }
        }
        _ => format!("{value:?}"),
    }
}

/// Format an mlua::Error into a user-friendly string.
fn format_lua_error(err: &mlua::Error) -> String {
    match err {
        mlua::Error::RuntimeError(msg) => msg.clone(),
        mlua::Error::CallbackError { cause, .. } => format_lua_error(cause),
        mlua::Error::SyntaxError {
            message,
            incomplete_input: _,
        } => format!("compile error: {message}"),
        _ => format!("{err}"),
    }
}

/// Build an error result table.
fn build_error(lua: &Lua, error: &str, output: &str) -> LuaResult<Table> {
    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set("error", error)?;
    result.set("output", output)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_lua() -> Lua {
        let lua = Lua::new();

        // Create minimal orcs table for tests
        let orcs = lua.create_table().expect("create orcs table");
        let json_encode = lua
            .create_function(|lua, val: Value| {
                let json = crate::types::lua_to_json(val, lua)?;
                let s = serde_json::to_string(&json)
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                Ok(s)
            })
            .expect("create json_encode");
        orcs.set("json_encode", json_encode)
            .expect("set json_encode");

        let json_decode = lua
            .create_function(|lua, s: String| {
                let val: serde_json::Value = serde_json::from_str(&s)
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                crate::types::serde_json_to_lua(&val, lua)
            })
            .expect("create json_decode");
        orcs.set("json_decode", json_decode)
            .expect("set json_decode");
        orcs.set("pwd", "/test").expect("set pwd");

        lua.globals().set("orcs", orcs).expect("set orcs global");

        // Register sandbox_eval
        let orcs_ref: Table = lua.globals().get("orcs").expect("get orcs");
        register_sandbox_eval(&lua, &orcs_ref).expect("register sandbox_eval");

        // Simulate sandbox by removing globals (like sandbox_lua_globals does)
        lua.load(
            r#"
            io = nil
            load = nil
            loadfile = nil
            dofile = nil
            debug = nil
            require = nil
            package = nil
            "#,
        )
        .exec()
        .expect("sandbox globals");

        lua
    }

    fn eval(lua: &Lua, code: &str) -> Table {
        let orcs: Table = lua.globals().get("orcs").expect("get orcs");
        let f: mlua::Function = orcs.get("sandbox_eval").expect("get sandbox_eval");
        f.call::<Table>(code.to_string())
            .expect("call sandbox_eval")
    }

    fn is_ok(result: &Table) -> bool {
        result.get::<bool>("ok").unwrap_or(false)
    }

    fn get_output(result: &Table) -> String {
        result.get::<String>("output").unwrap_or_default()
    }

    fn get_result(result: &Table) -> Option<String> {
        result.get::<String>("result").ok()
    }

    fn get_error(result: &Table) -> String {
        result.get::<String>("error").unwrap_or_default()
    }

    // === Positive Tests (正常系) ===

    #[test]
    fn basic_print() {
        let lua = setup_lua();
        let r = eval(&lua, r#"print("hello world")"#);
        assert!(is_ok(&r), "should succeed");
        assert_eq!(get_output(&r), "hello world");
    }

    #[test]
    fn return_value_integer() {
        let lua = setup_lua();
        let r = eval(&lua, "return 1 + 2");
        assert!(is_ok(&r));
        assert_eq!(get_result(&r), Some("3".to_string()));
    }

    #[test]
    fn math_operations() {
        let lua = setup_lua();
        let r = eval(&lua, "return math.sqrt(144)");
        assert!(is_ok(&r));
        let res = get_result(&r).expect("should have result");
        assert!(res == "12" || res == "12.0", "got: {res}");
    }

    #[test]
    fn string_operations() {
        let lua = setup_lua();
        let r = eval(&lua, r#"print(string.upper("hello"))"#);
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "HELLO");
    }

    #[test]
    fn table_sort() {
        let lua = setup_lua();
        let r = eval(
            &lua,
            r#"local t={3,1,2}; table.sort(t); print(table.concat(t,","))"#,
        );
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "1,2,3");
    }

    #[test]
    fn multiple_prints() {
        let lua = setup_lua();
        let r = eval(&lua, r#"print("a"); print("b"); print("c")"#);
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "a\nb\nc");
    }

    #[test]
    fn print_multi_args_tab_separated() {
        let lua = setup_lua();
        let r = eval(&lua, r#"print("a", 1, true)"#);
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "a\t1\ttrue");
    }

    #[test]
    fn pcall_works_inside_sandbox() {
        let lua = setup_lua();
        let r = eval(
            &lua,
            r#"local ok, e = pcall(function() error("x") end); print(ok)"#,
        );
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "false");
    }

    #[test]
    fn setmetatable_works() {
        let lua = setup_lua();
        let r = eval(
            &lua,
            r#"local t = setmetatable({}, {__tostring = function() return "custom" end}); print(tostring(t))"#,
        );
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "custom");
    }

    #[test]
    fn orcs_pwd_accessible() {
        let lua = setup_lua();
        let r = eval(&lua, "print(orcs.pwd)");
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "/test");
    }

    #[test]
    fn orcs_json_roundtrip() {
        let lua = setup_lua();
        let r = eval(
            &lua,
            r#"
            local encoded = orcs.json_encode({name = "test", value = 42})
            local decoded = orcs.json_decode(encoded)
            print(decoded.name .. ":" .. decoded.value)
            "#,
        );
        assert!(is_ok(&r));
        assert_eq!(get_output(&r), "test:42");
    }

    // === Negative Tests (異常系) ===

    #[test]
    fn empty_code_rejected() {
        let lua = setup_lua();
        let r = eval(&lua, "");
        assert!(!is_ok(&r));
        assert_eq!(get_error(&r), "code must be a non-empty string");
    }

    #[test]
    fn compile_error() {
        let lua = setup_lua();
        let r = eval(&lua, "if then end");
        assert!(!is_ok(&r));
        assert!(
            get_error(&r).contains("compile error") || get_error(&r).contains("syntax error"),
            "got: {}",
            get_error(&r)
        );
    }

    #[test]
    fn runtime_error() {
        let lua = setup_lua();
        let r = eval(&lua, r#"error("intentional")"#);
        assert!(!is_ok(&r));
        assert!(
            get_error(&r).contains("intentional"),
            "got: {}",
            get_error(&r)
        );
    }

    #[test]
    fn os_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, r#"os.execute("echo pwned")"#);
        assert!(!is_ok(&r), "os should be blocked in sandbox");
    }

    #[test]
    fn io_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, r#"io.open("/etc/passwd")"#);
        assert!(!is_ok(&r), "io should be blocked in sandbox");
    }

    #[test]
    fn require_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, r#"require("os")"#);
        assert!(!is_ok(&r), "require should be blocked in sandbox");
    }

    #[test]
    fn load_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, r#"load("print('escaped')")()"#);
        assert!(!is_ok(&r), "load should be blocked in sandbox");
    }

    #[test]
    fn dofile_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, r#"dofile("/etc/passwd")"#);
        assert!(!is_ok(&r), "dofile should be blocked in sandbox");
    }

    #[test]
    fn loadfile_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, r#"loadfile("/etc/passwd")()"#);
        assert!(!is_ok(&r), "loadfile should be blocked in sandbox");
    }

    #[test]
    fn debug_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, "debug.getinfo(1)");
        assert!(!is_ok(&r), "debug should be blocked in sandbox");
    }

    #[test]
    fn instruction_limit() {
        let lua = setup_lua();
        let r = eval(&lua, "while true do end");
        assert!(!is_ok(&r));
        assert!(
            get_error(&r).contains("instruction limit"),
            "got: {}",
            get_error(&r)
        );
    }

    #[test]
    fn output_before_error_captured() {
        let lua = setup_lua();
        let r = eval(&lua, r#"print("before"); error("boom")"#);
        assert!(!is_ok(&r));
        assert_eq!(get_output(&r), "before");
        assert!(get_error(&r).contains("boom"), "got: {}", get_error(&r));
    }

    #[test]
    fn orcs_write_blocked() {
        let lua = setup_lua();
        // orcs.write should not be in sandbox env
        let r = eval(&lua, r#"orcs.write("/tmp/test", "pwned")"#);
        assert!(!is_ok(&r), "orcs.write should be blocked in sandbox");
    }

    #[test]
    fn orcs_exec_blocked() {
        let lua = setup_lua();
        let r = eval(&lua, r#"orcs.exec("echo pwned")"#);
        assert!(!is_ok(&r), "orcs.exec should be blocked in sandbox");
    }
}

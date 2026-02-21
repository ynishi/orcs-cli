//! Intent registry: structured intent definitions with unified dispatch.
//!
//! Provides a single source of truth for intent metadata (name, description,
//! parameters as JSON Schema, resolver) and a unified `orcs.dispatch(name, args)`
//! function that validates and routes to the appropriate resolver.
//!
//! # Design
//!
//! ```text
//! IntentRegistry (dynamic, stored in Lua app_data)
//!   ├── IntentDef { name, description, parameters (JSON Schema), resolver }
//!   │     resolver = Internal       → dispatch_internal() (8 builtin tools)
//!   │     resolver = Component{..}  → Component RPC via EventBus
//!   │
//!   └── Lua API:
//!         orcs.dispatch(name, args)   → resolve + execute
//!         orcs.tool_schemas()         → legacy Lua table format (backward compat)
//!         orcs.intent_defs()          → JSON Schema format (for LLM tools param)
//!         orcs.register_intent(def)   → dynamic addition (Component tools)
//! ```

use crate::error::LuaError;
use crate::types::serde_json_to_lua;
use mlua::{Lua, Table};
use orcs_types::intent::{IntentDef, IntentResolver};

// ── IntentRegistry ───────────────────────────────────────────────────

/// Registry of named intents. Stored in Lua app_data.
///
/// Initialized with 8 builtin Internal tools. Components can register
/// additional intents at runtime via `orcs.register_intent()`.
pub struct IntentRegistry {
    defs: Vec<IntentDef>,
}

impl Default for IntentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IntentRegistry {
    /// Create a new registry pre-populated with the 8 builtin tools.
    pub fn new() -> Self {
        Self {
            defs: builtin_intent_defs(),
        }
    }

    /// Look up an intent definition by name.
    pub fn get(&self, name: &str) -> Option<&IntentDef> {
        self.defs.iter().find(|d| d.name == name)
    }

    /// Register a new intent definition. Returns error if name is already taken.
    pub fn register(&mut self, def: IntentDef) -> Result<(), String> {
        if self.defs.iter().any(|d| d.name == def.name) {
            return Err(format!("intent already registered: {}", def.name));
        }
        self.defs.push(def);
        Ok(())
    }

    /// All registered intent definitions.
    pub fn all(&self) -> &[IntentDef] {
        &self.defs
    }

    /// Number of registered intents.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

// ── Builtin IntentDefs (8 tools) ─────────────────────────────────────

/// Helper: create a JSON Schema for a tool with the given properties.
fn json_schema(properties: &[(&str, &str, bool)]) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();

    for &(name, description, is_required) in properties {
        props.insert(
            name.to_string(),
            serde_json::json!({
                "type": "string",
                "description": description,
            }),
        );
        if is_required {
            required.push(serde_json::Value::String(name.to_string()));
        }
    }

    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

/// The 8 builtin tool definitions as IntentDefs.
fn builtin_intent_defs() -> Vec<IntentDef> {
    vec![
        IntentDef {
            name: "read".into(),
            description: "Read file contents. Path relative to project root.".into(),
            parameters: json_schema(&[("path", "File path to read", true)]),
            resolver: IntentResolver::Internal,
        },
        IntentDef {
            name: "write".into(),
            description: "Write file contents (atomic). Creates parent dirs.".into(),
            parameters: json_schema(&[
                ("path", "File path to write", true),
                ("content", "Content to write", true),
            ]),
            resolver: IntentResolver::Internal,
        },
        IntentDef {
            name: "grep".into(),
            description: "Search with regex. Path can be file or directory (recursive).".into(),
            parameters: json_schema(&[
                ("pattern", "Regex pattern to search for", true),
                ("path", "File or directory to search in", true),
            ]),
            resolver: IntentResolver::Internal,
        },
        IntentDef {
            name: "glob".into(),
            description: "Find files by glob pattern. Dir defaults to project root.".into(),
            parameters: json_schema(&[
                ("pattern", "Glob pattern (e.g. '**/*.rs')", true),
                ("dir", "Base directory (defaults to project root)", false),
            ]),
            resolver: IntentResolver::Internal,
        },
        IntentDef {
            name: "mkdir".into(),
            description: "Create directory (with parents).".into(),
            parameters: json_schema(&[("path", "Directory path to create", true)]),
            resolver: IntentResolver::Internal,
        },
        IntentDef {
            name: "remove".into(),
            description: "Remove file or directory.".into(),
            parameters: json_schema(&[("path", "Path to remove", true)]),
            resolver: IntentResolver::Internal,
        },
        IntentDef {
            name: "mv".into(),
            description: "Move / rename file or directory.".into(),
            parameters: json_schema(&[
                ("src", "Source path", true),
                ("dst", "Destination path", true),
            ]),
            resolver: IntentResolver::Internal,
        },
        IntentDef {
            name: "exec".into(),
            description: "Execute shell command. cwd = project root.".into(),
            parameters: json_schema(&[("cmd", "Shell command to execute", true)]),
            resolver: IntentResolver::Internal,
        },
    ]
}

// ── Dispatch ─────────────────────────────────────────────────────────

/// Dispatches a tool call by name. Routes through IntentRegistry.
///
/// 1. Look up intent in registry
/// 2. Route by resolver: Internal → dispatch_internal, Component → (future)
/// 3. Unknown name → error
fn dispatch_tool(lua: &Lua, name: &str, args: &Table) -> mlua::Result<Table> {
    let resolver = {
        let registry = ensure_registry(lua)?;
        match registry.get(name) {
            Some(def) => def.resolver.clone(),
            None => {
                let result = lua.create_table()?;
                set_error(&result, &format!("unknown intent: {name}"))?;
                return Ok(result);
            }
        }
    };

    match resolver {
        IntentResolver::Internal => dispatch_internal(lua, name, args),
        IntentResolver::Component {
            component_fqn,
            operation,
        } => dispatch_component(lua, name, &component_fqn, &operation, args),
    }
}

/// Dispatches an Internal intent to the corresponding `orcs.*` Lua function.
///
/// Each builtin tool has a different Lua function signature, so tool-specific
/// argument extraction is necessary. This is an implementation detail of
/// the Internal resolver — hidden behind the unified dispatch_tool().
fn dispatch_internal(lua: &Lua, name: &str, args: &Table) -> mlua::Result<Table> {
    let orcs: Table = lua.globals().get("orcs")?;

    match name {
        "read" => {
            let path: String = get_required_arg(args, "path")?;
            let f: mlua::Function = orcs.get("read")?;
            f.call(path)
        }
        "write" => {
            let path: String = get_required_arg(args, "path")?;
            let content: String = get_required_arg(args, "content")?;
            let f: mlua::Function = orcs.get("write")?;
            f.call((path, content))
        }
        "grep" => {
            let pattern: String = get_required_arg(args, "pattern")?;
            let path: String = get_required_arg(args, "path")?;
            let f: mlua::Function = orcs.get("grep")?;
            f.call((pattern, path))
        }
        "glob" => {
            let pattern: String = get_required_arg(args, "pattern")?;
            let dir: Option<String> = args.get("dir").ok();
            let f: mlua::Function = orcs.get("glob")?;
            f.call((pattern, dir))
        }
        "mkdir" => {
            let path: String = get_required_arg(args, "path")?;
            let f: mlua::Function = orcs.get("mkdir")?;
            f.call(path)
        }
        "remove" => {
            let path: String = get_required_arg(args, "path")?;
            let f: mlua::Function = orcs.get("remove")?;
            f.call(path)
        }
        "mv" => {
            let src: String = get_required_arg(args, "src")?;
            let dst: String = get_required_arg(args, "dst")?;
            let f: mlua::Function = orcs.get("mv")?;
            f.call((src, dst))
        }
        "exec" => {
            let cmd: String = get_required_arg(args, "cmd")?;
            let f: mlua::Function = orcs.get("exec")?;
            f.call(cmd)
        }
        _ => {
            // Internal resolver for unknown name — should not happen if
            // registry is consistent, but handle defensively.
            let result = lua.create_table()?;
            set_error(
                &result,
                &format!("internal dispatch error: no handler for '{name}'"),
            )?;
            Ok(result)
        }
    }
}

/// Dispatches a Component intent via RPC.
///
/// Calls `orcs.request(component_fqn, operation, args)` which is
/// registered by emitter_fns.rs (Component) or child.rs (ChildContext).
///
/// The RPC returns `{ success: bool, data?, error? }`. This function
/// normalizes the response to `{ ok: bool, data?, error?, duration_ms }`,
/// matching the Internal dispatch contract.
fn dispatch_component(
    lua: &Lua,
    intent_name: &str,
    component_fqn: &str,
    operation: &str,
    args: &Table,
) -> mlua::Result<Table> {
    let orcs: Table = lua.globals().get("orcs")?;

    let request_fn = match orcs.get::<mlua::Function>("request") {
        Ok(f) => f,
        Err(_) => {
            let result = lua.create_table()?;
            set_error(
                &result,
                "component dispatch unavailable: no execution context (orcs.request not registered)",
            )?;
            return Ok(result);
        }
    };

    // Build RPC payload (shallow copy of args table)
    let payload = lua.create_table()?;
    for pair in args.pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair?;
        payload.set(k, v)?;
    }

    // Execute with timing
    let start = std::time::Instant::now();
    let rpc_result: Table = request_fn.call((component_fqn, operation, payload))?;
    let duration_ms = start.elapsed().as_millis() as u64;

    tracing::debug!(
        "component dispatch: {intent_name} → {component_fqn}::{operation} ({duration_ms}ms)"
    );

    // Normalize { success, data, error } → { ok, data, error, duration_ms }
    let result = lua.create_table()?;
    let success: bool = rpc_result.get("success").unwrap_or(false);
    result.set("ok", success)?;
    result.set("duration_ms", duration_ms)?;

    if success {
        // Forward data if present
        if let Ok(data) = rpc_result.get::<mlua::Value>("data") {
            result.set("data", data)?;
        }
    } else {
        // Forward error message
        let error_msg: String = rpc_result
            .get("error")
            .unwrap_or_else(|_| format!("component RPC failed: {component_fqn}::{operation}"));
        result.set("error", error_msg)?;
    }

    Ok(result)
}

// ── Registry Helpers ─────────────────────────────────────────────────

/// Ensure IntentRegistry exists in app_data. Returns a reference.
fn ensure_registry(lua: &Lua) -> mlua::Result<mlua::AppDataRef<'_, IntentRegistry>> {
    if lua.app_data_ref::<IntentRegistry>().is_none() {
        lua.set_app_data(IntentRegistry::new());
    }
    lua.app_data_ref::<IntentRegistry>().ok_or_else(|| {
        mlua::Error::RuntimeError("IntentRegistry not available after initialization".into())
    })
}

/// Generates formatted tool descriptions from the registry.
pub fn generate_descriptions(lua: &Lua) -> String {
    let registry = match ensure_registry(lua) {
        Ok(r) => r,
        Err(_) => return "IntentRegistry not available.\n".to_string(),
    };

    let mut out = String::from("Available tools (use via orcs.dispatch):\n\n");

    for def in registry.all() {
        // Extract argument names from JSON Schema
        let args_fmt = extract_arg_names(&def.parameters);
        out.push_str(&format!(
            "{}({}) - {}\n",
            def.name, args_fmt, def.description
        ));
    }

    out.push_str("\norcs.pwd - Project root path (string).\n");
    out
}

/// Extract argument names from a JSON Schema `properties` + `required` for display.
fn extract_arg_names(schema: &serde_json::Value) -> String {
    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return String::new(),
    };

    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    properties
        .keys()
        .map(|name| {
            if required.contains(&name.as_str()) {
                name.clone()
            } else {
                format!("{name}?")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Arg extraction helpers ───────────────────────────────────────────

/// Extracts a required string argument from the args table.
fn get_required_arg(args: &Table, name: &str) -> mlua::Result<String> {
    args.get::<String>(name)
        .map_err(|_| mlua::Error::RuntimeError(format!("missing required argument: {name}")))
}

/// Sets error fields on a result table.
fn set_error(result: &Table, msg: &str) -> mlua::Result<()> {
    result.set("ok", false)?;
    result.set("error", msg.to_string())?;
    Ok(())
}

// ── Lua API Registration ─────────────────────────────────────────────

/// Registers intent-based Lua APIs in the runtime.
///
/// - `orcs.dispatch(name, args)` — unified intent dispatcher
/// - `orcs.tool_schemas()` — legacy Lua table format (backward compat)
/// - `orcs.intent_defs()` — JSON Schema format (for LLM tools param)
/// - `orcs.register_intent(def)` — dynamic intent registration
/// - `orcs.tool_descriptions()` — formatted text descriptions
pub fn register_dispatch_functions(lua: &Lua) -> Result<(), LuaError> {
    // Ensure registry is initialized
    if lua.app_data_ref::<IntentRegistry>().is_none() {
        lua.set_app_data(IntentRegistry::new());
    }

    let orcs_table: Table = lua.globals().get("orcs")?;

    // orcs.dispatch(name, args) -> result table
    let dispatch_fn =
        lua.create_function(|lua, (name, args): (String, Table)| dispatch_tool(lua, &name, &args))?;
    orcs_table.set("dispatch", dispatch_fn)?;

    // orcs.tool_schemas() -> legacy Lua table format (backward compat)
    let schemas_fn = lua.create_function(|lua, ()| {
        let registry = ensure_registry(lua)?;
        let result = lua.create_table()?;

        for (i, def) in registry.all().iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", def.name.as_str())?;
            entry.set("description", def.description.as_str())?;

            // Convert JSON Schema properties to legacy args format
            let args_table = lua.create_table()?;
            if let Some(properties) = def.parameters.get("properties").and_then(|p| p.as_object()) {
                let required: Vec<&str> = def
                    .parameters
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                for (j, (prop_name, prop_schema)) in properties.iter().enumerate() {
                    let arg_entry = lua.create_table()?;
                    arg_entry.set("name", prop_name.as_str())?;

                    let is_required = required.contains(&prop_name.as_str());
                    let type_str = if is_required { "string" } else { "string?" };
                    arg_entry.set("type", type_str)?;
                    arg_entry.set("required", is_required)?;

                    let description = prop_schema
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    arg_entry.set("description", description)?;

                    args_table.set(j + 1, arg_entry)?;
                }
            }
            entry.set("args", args_table)?;
            result.set(i + 1, entry)?;
        }

        Ok(result)
    })?;
    orcs_table.set("tool_schemas", schemas_fn)?;

    // orcs.intent_defs() -> JSON Schema format for LLM tools parameter
    let intent_defs_fn = lua.create_function(|lua, ()| {
        let registry = ensure_registry(lua)?;
        let result = lua.create_table()?;

        for (i, def) in registry.all().iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", def.name.as_str())?;
            entry.set("description", def.description.as_str())?;

            // parameters as native Lua table (JSON Schema → Lua via serde_json_to_lua)
            let params_value = serde_json_to_lua(&def.parameters, lua)?;
            entry.set("parameters", params_value)?;

            result.set(i + 1, entry)?;
        }

        Ok(result)
    })?;
    orcs_table.set("intent_defs", intent_defs_fn)?;

    // orcs.register_intent(def) -> register a new intent definition
    let register_fn = lua.create_function(|lua, def_table: Table| {
        let name: String = def_table
            .get("name")
            .map_err(|_| mlua::Error::RuntimeError("register_intent: 'name' is required".into()))?;
        let description: String = def_table.get("description").map_err(|_| {
            mlua::Error::RuntimeError("register_intent: 'description' is required".into())
        })?;

        // Component resolver fields
        let component_fqn: String = def_table.get("component").map_err(|_| {
            mlua::Error::RuntimeError("register_intent: 'component' is required".into())
        })?;
        let operation: String = def_table
            .get("operation")
            .unwrap_or_else(|_| "execute".to_string());

        // Parameters: accept a table or default to empty schema
        let parameters = match def_table.get::<Table>("params") {
            Ok(params_table) => {
                // Convert Lua table to JSON Schema
                let mut properties = serde_json::Map::new();
                let mut required = Vec::new();

                for pair in params_table.pairs::<String, Table>() {
                    let (param_name, param_def) = pair?;
                    let type_str: String = param_def
                        .get("type")
                        .unwrap_or_else(|_| "string".to_string());
                    let desc: String = param_def
                        .get("description")
                        .unwrap_or_else(|_| String::new());
                    let is_required: bool = param_def.get("required").unwrap_or(false);

                    properties.insert(
                        param_name.clone(),
                        serde_json::json!({
                            "type": type_str,
                            "description": desc,
                        }),
                    );
                    if is_required {
                        required.push(serde_json::Value::String(param_name));
                    }
                }

                serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "required": required,
                })
            }
            Err(_) => serde_json::json!({"type": "object", "properties": {}}),
        };

        let intent_def = IntentDef {
            name: name.clone(),
            description,
            parameters,
            resolver: IntentResolver::Component {
                component_fqn,
                operation,
            },
        };

        // Mutate registry
        if let Some(mut registry) = lua.remove_app_data::<IntentRegistry>() {
            let result = registry.register(intent_def);
            lua.set_app_data(registry);

            let result_table = lua.create_table()?;
            match result {
                Ok(()) => {
                    result_table.set("ok", true)?;
                }
                Err(e) => {
                    result_table.set("ok", false)?;
                    result_table.set("error", e)?;
                }
            }
            Ok(result_table)
        } else {
            Err(mlua::Error::RuntimeError(
                "IntentRegistry not initialized".into(),
            ))
        }
    })?;
    orcs_table.set("register_intent", register_fn)?;

    // orcs.tool_descriptions() -> formatted text
    let desc = generate_descriptions(lua);
    let tool_desc_fn = lua.create_function(move |_, ()| Ok(desc.clone()))?;
    orcs_table.set("tool_descriptions", tool_desc_fn)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orcs_helpers::register_base_orcs_functions;
    use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_sandbox() -> (PathBuf, Arc<dyn SandboxPolicy>) {
        let dir = tempdir();
        let sandbox = ProjectSandbox::new(&dir).expect("test sandbox");
        (dir, Arc::new(sandbox))
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orcs-registry-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("should create temp dir");
        dir.canonicalize().expect("should canonicalize temp dir")
    }

    fn setup_lua(sandbox: Arc<dyn SandboxPolicy>) -> Lua {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, sandbox).expect("should register base functions");
        lua
    }

    // --- IntentRegistry unit tests ---

    #[test]
    fn registry_new_has_8_builtins() {
        let registry = IntentRegistry::new();
        assert_eq!(registry.len(), 8, "should have 8 builtin intents");
    }

    #[test]
    fn registry_get_existing() {
        let registry = IntentRegistry::new();
        let def = registry.get("read").expect("'read' should exist");
        assert_eq!(def.name, "read");
        assert_eq!(def.resolver, IntentResolver::Internal);
    }

    #[test]
    fn registry_get_nonexistent() {
        let registry = IntentRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn registry_register_new_intent() {
        let mut registry = IntentRegistry::new();
        let def = IntentDef {
            name: "custom_tool".into(),
            description: "A custom tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            resolver: IntentResolver::Component {
                component_fqn: "lua::my_comp".into(),
                operation: "execute".into(),
            },
        };
        registry
            .register(def)
            .expect("should register successfully");
        assert_eq!(registry.len(), 9);
        assert!(registry.get("custom_tool").is_some());
    }

    #[test]
    fn registry_register_duplicate_fails() {
        let mut registry = IntentRegistry::new();
        let def = IntentDef {
            name: "read".into(),
            description: "duplicate".into(),
            parameters: serde_json::json!({}),
            resolver: IntentResolver::Internal,
        };
        let err = registry.register(def).expect_err("should reject duplicate");
        assert!(
            err.contains("already registered"),
            "error should mention duplicate, got: {err}"
        );
    }

    #[test]
    fn registry_all_intent_defs_have_json_schema() {
        let registry = IntentRegistry::new();
        for def in registry.all() {
            assert_eq!(
                def.parameters.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "intent '{}' should have JSON Schema with type=object",
                def.name
            );
            assert!(
                def.parameters.get("properties").is_some(),
                "intent '{}' should have properties",
                def.name
            );
        }
    }

    #[test]
    fn builtin_intent_names_match_expected() {
        let registry = IntentRegistry::new();
        let names: Vec<&str> = registry.all().iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["read", "write", "grep", "glob", "mkdir", "remove", "mv", "exec"]
        );
    }

    // --- dispatch tests (unchanged behavior) ---

    #[test]
    fn dispatch_read() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("test.txt"), "hello dispatch").expect("should write test file");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("read", {{path="{}"}})"#,
                root.join("test.txt").display()
            ))
            .eval()
            .expect("dispatch read should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content"),
            "hello dispatch"
        );
    }

    #[test]
    fn dispatch_write_and_read() {
        let (root, sandbox) = test_sandbox();
        let path = root.join("written.txt");

        let lua = setup_lua(sandbox);
        let code = format!(
            r#"
            local w = orcs.dispatch("write", {{path="{p}", content="via dispatch"}})
            local r = orcs.dispatch("read", {{path="{p}"}})
            return r
            "#,
            p = path.display()
        );
        let result: Table = lua
            .load(&code)
            .eval()
            .expect("dispatch write+read should succeed");
        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content"),
            "via dispatch"
        );
    }

    #[test]
    fn dispatch_grep() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("search.txt"), "line one\nline two\nthird")
            .expect("should write search file");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("grep", {{pattern="line", path="{}"}})"#,
                root.join("search.txt").display()
            ))
            .eval()
            .expect("dispatch grep should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(result.get::<usize>("count").expect("should have count"), 2);
    }

    #[test]
    fn dispatch_glob() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("a.rs"), "").expect("write a.rs");
        fs::write(root.join("b.rs"), "").expect("write b.rs");
        fs::write(root.join("c.txt"), "").expect("write c.txt");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("glob", {{pattern="*.rs", dir="{}"}})"#,
                root.display()
            ))
            .eval()
            .expect("dispatch glob should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(result.get::<usize>("count").expect("should have count"), 2);
    }

    #[test]
    fn dispatch_mkdir_remove() {
        let (root, sandbox) = test_sandbox();
        let dir_path = root.join("sub/deep");

        let lua = setup_lua(sandbox);
        let code = format!(
            r#"
            local m = orcs.dispatch("mkdir", {{path="{p}"}})
            local r = orcs.dispatch("remove", {{path="{p}"}})
            return {{mkdir=m, remove=r}}
            "#,
            p = dir_path.display()
        );
        let result: Table = lua
            .load(&code)
            .eval()
            .expect("dispatch mkdir+remove should succeed");
        let mkdir: Table = result.get("mkdir").expect("should have mkdir");
        let remove: Table = result.get("remove").expect("should have remove");
        assert!(mkdir.get::<bool>("ok").expect("mkdir ok"));
        assert!(remove.get::<bool>("ok").expect("remove ok"));
    }

    #[test]
    fn dispatch_mv() {
        let (root, sandbox) = test_sandbox();
        let src = root.join("src.txt");
        let dst = root.join("dst.txt");
        fs::write(&src, "move me").expect("write src");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("mv", {{src="{}", dst="{}"}})"#,
                src.display(),
                dst.display()
            ))
            .eval()
            .expect("dispatch mv should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert!(dst.exists());
        assert!(!src.exists());
    }

    #[test]
    fn dispatch_unknown_tool() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(r#"return orcs.dispatch("nonexistent", {arg="val"})"#)
            .eval()
            .expect("dispatch unknown should return error table");

        assert!(!result.get::<bool>("ok").expect("should have ok field"));
        assert!(result
            .get::<String>("error")
            .expect("should have error")
            .contains("unknown intent"));
    }

    #[test]
    fn dispatch_missing_required_arg() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result = lua
            .load(r#"return orcs.dispatch("read", {})"#)
            .eval::<Table>();

        assert!(result.is_err());
        let err = result.expect_err("should error on missing arg").to_string();
        assert!(err.contains("missing required argument"), "got: {err}");
    }

    // --- tool_schemas tests (backward compat) ---

    #[test]
    fn tool_schemas_returns_all() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let schemas: Table = lua
            .load("return orcs.tool_schemas()")
            .eval()
            .expect("tool_schemas should return table");

        let count = schemas.len().expect("should have length") as usize;
        assert_eq!(count, 8, "should return 8 builtin tools");

        // Verify first schema structure (backward compat format)
        let first: Table = schemas.get(1).expect("should have first entry");
        assert_eq!(
            first.get::<String>("name").expect("should have name"),
            "read"
        );
        assert!(!first
            .get::<String>("description")
            .expect("should have description")
            .is_empty());

        let args: Table = first.get("args").expect("should have args");
        let first_arg: Table = args.get(1).expect("should have first arg");
        assert_eq!(first_arg.get::<String>("name").expect("arg name"), "path");
        assert_eq!(first_arg.get::<String>("type").expect("arg type"), "string");
        assert!(first_arg.get::<bool>("required").expect("arg required"));
    }

    // --- generate_descriptions tests ---

    #[test]
    fn descriptions_include_all_tools() {
        let lua = Lua::new();
        lua.set_app_data(IntentRegistry::new());
        let desc = generate_descriptions(&lua);
        let expected_tools = [
            "read", "write", "grep", "glob", "mkdir", "remove", "mv", "exec",
        ];
        for tool in expected_tools {
            assert!(desc.contains(tool), "missing tool in descriptions: {tool}");
        }
    }

    // --- exec dispatch delegates to registered function ---

    #[test]
    fn dispatch_exec_uses_registered_exec() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Default exec is deny-stub
        let result: Table = lua
            .load(r#"return orcs.dispatch("exec", {cmd="echo hi"})"#)
            .eval()
            .expect("dispatch exec should return table");

        // Should return the deny-stub result (not error, just ok=false)
        assert!(!result.get::<bool>("ok").expect("should have ok field"));
    }

    // --- intent_defs tests ---

    #[test]
    fn intent_defs_returns_all() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let defs: Table = lua
            .load("return orcs.intent_defs()")
            .eval()
            .expect("intent_defs should return table");

        let count = defs.len().expect("should have length") as usize;
        assert_eq!(count, 8, "should return 8 builtin intents");

        let first: Table = defs.get(1).expect("should have first entry");
        assert_eq!(
            first.get::<String>("name").expect("should have name"),
            "read"
        );
        assert!(!first
            .get::<String>("description")
            .expect("should have description")
            .is_empty());

        // parameters should be present (as string or table)
        assert!(
            first.get::<mlua::Value>("parameters").is_ok(),
            "should have parameters"
        );
    }

    // --- register_intent tests ---

    #[test]
    fn register_intent_adds_to_registry() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                return orcs.register_intent({
                    name = "custom_action",
                    description = "A custom action",
                    component = "lua::my_component",
                    operation = "do_stuff",
                    params = {
                        input = { type = "string", description = "Input data", required = true },
                    },
                })
                "#,
            )
            .eval()
            .expect("register_intent should return table");

        assert!(
            result.get::<bool>("ok").expect("should have ok field"),
            "registration should succeed"
        );

        // Verify it appears in tool_schemas
        let schemas: Table = lua
            .load("return orcs.tool_schemas()")
            .eval()
            .expect("tool_schemas after register");
        let count = schemas.len().expect("should have length") as usize;
        assert_eq!(count, 9, "should now have 9 intents (8 builtin + 1 custom)");
    }

    #[test]
    fn register_intent_duplicate_fails() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                return orcs.register_intent({
                    name = "read",
                    description = "duplicate",
                    component = "lua::x",
                })
                "#,
            )
            .eval()
            .expect("register_intent should return table");

        assert!(
            !result.get::<bool>("ok").expect("should have ok field"),
            "duplicate registration should fail"
        );
        assert!(result
            .get::<String>("error")
            .expect("should have error")
            .contains("already registered"));
    }

    // --- Component dispatch tests ---

    #[test]
    fn dispatch_component_no_request_fn_returns_error() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Register a Component intent without providing orcs.request
        lua.load(
            r#"
            orcs.register_intent({
                name = "comp_action",
                description = "component action",
                component = "lua::test_comp",
                operation = "do_stuff",
            })
            "#,
        )
        .exec()
        .expect("register should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("comp_action", {input="hello"})"#)
            .eval()
            .expect("should return error table");

        assert!(
            !result.get::<bool>("ok").expect("should have ok"),
            "should fail without orcs.request"
        );
        let error: String = result.get("error").expect("should have error");
        assert!(
            error.contains("no execution context"),
            "error should mention missing context, got: {error}"
        );
    }

    #[test]
    fn dispatch_component_success_normalized() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Register a Component intent
        lua.load(
            r#"
            orcs.register_intent({
                name = "mock_comp",
                description = "mock component",
                component = "lua::mock",
                operation = "echo",
            })
            "#,
        )
        .exec()
        .expect("register should succeed");

        // Mock orcs.request to return { success=true, data={echo="hi"} }
        lua.load(
            r#"
            orcs.request = function(target, operation, payload)
                return { success = true, data = { echo = payload.input, target = target, op = operation } }
            end
            "#,
        )
        .exec()
        .expect("mock should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("mock_comp", {input="hello"})"#)
            .eval()
            .expect("dispatch should return table");

        // Normalized response: ok (not success)
        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "should succeed"
        );

        // duration_ms should be present
        let duration: u64 = result.get("duration_ms").expect("should have duration_ms");
        assert!(
            duration < 1000,
            "local mock should be fast, got: {duration}ms"
        );

        // data should be forwarded
        let data: Table = result.get("data").expect("should have data");
        assert_eq!(
            data.get::<String>("echo").expect("should have echo"),
            "hello"
        );
        assert_eq!(
            data.get::<String>("target").expect("should have target"),
            "lua::mock"
        );
        assert_eq!(data.get::<String>("op").expect("should have op"), "echo");
    }

    #[test]
    fn dispatch_component_failure_normalized() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Register + mock failing request
        lua.load(
            r#"
            orcs.register_intent({
                name = "fail_comp",
                description = "failing component",
                component = "lua::fail",
                operation = "explode",
            })
            orcs.request = function(target, operation, payload)
                return { success = false, error = "component exploded" }
            end
            "#,
        )
        .exec()
        .expect("setup should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("fail_comp", {})"#)
            .eval()
            .expect("dispatch should return table");

        assert!(
            !result.get::<bool>("ok").expect("should have ok"),
            "should report failure"
        );
        let error: String = result.get("error").expect("should have error");
        assert_eq!(error, "component exploded");

        // duration_ms still present
        assert!(result.get::<u64>("duration_ms").is_ok());
    }

    #[test]
    fn dispatch_component_forwards_all_args() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        lua.load(
            r#"
            orcs.register_intent({
                name = "args_comp",
                description = "args test",
                component = "lua::args_test",
                operation = "check_args",
            })
            -- Mock that captures and returns the payload
            orcs.request = function(target, operation, payload)
                return { success = true, data = payload }
            end
            "#,
        )
        .exec()
        .expect("setup should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("args_comp", {a="1", b="2", c="3"})"#)
            .eval()
            .expect("dispatch should return table");

        assert!(result.get::<bool>("ok").expect("should have ok"));
        let data: Table = result.get("data").expect("should have data");
        assert_eq!(data.get::<String>("a").expect("arg a"), "1");
        assert_eq!(data.get::<String>("b").expect("arg b"), "2");
        assert_eq!(data.get::<String>("c").expect("arg c"), "3");
    }
}

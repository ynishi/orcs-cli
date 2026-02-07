//! Tool registry: structured tool definitions with unified dispatch.
//!
//! Provides a single source of truth for tool metadata (name, description,
//! arguments, required capability) and a unified `orcs.dispatch(name, args)`
//! function that validates arguments and calls the underlying Rust implementation.
//!
//! # Design
//!
//! ```text
//! ToolSchema (static metadata)
//!   ├── name, description
//!   ├── args: [ArgSchema { name, typ, required, description }]
//!   └── capability: Option<Capability>
//!
//! orcs.dispatch("read", {path="src/main.rs"})
//!   → lookup schema by name
//!   → validate required args
//!   → call tool_* function
//!   → return unified {ok, data/error} result
//!
//! orcs.tool_schemas()
//!   → returns Lua table of all tool schemas
//!   → used by agents to build LLM prompts
//! ```

use crate::error::LuaError;
use mlua::{Lua, Table};

/// Argument type for tool schema definitions.
#[derive(Debug, Clone, Copy)]
pub enum ArgType {
    String,
    OptionalString,
}

impl ArgType {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::OptionalString => "string?",
        }
    }

    fn is_required(self) -> bool {
        matches!(self, Self::String)
    }
}

/// Schema for a single tool argument.
#[derive(Debug, Clone)]
pub struct ArgSchema {
    pub name: &'static str,
    pub typ: ArgType,
    pub description: &'static str,
}

/// Schema for a tool.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub args: &'static [ArgSchema],
}

/// Returns all tool schemas.
///
/// This is the single source of truth for tool metadata.
/// Tool descriptions, dispatch, and Lua registration all derive from this.
pub fn all_schemas() -> &'static [ToolSchema] {
    use ArgType::{OptionalString, String};

    static SCHEMAS: &[ToolSchema] = &[
        ToolSchema {
            name: "read",
            description: "Read file contents. Path relative to project root.",
            args: &[ArgSchema {
                name: "path",
                typ: String,
                description: "File path to read",
            }],
        },
        ToolSchema {
            name: "write",
            description: "Write file contents (atomic). Creates parent dirs.",
            args: &[
                ArgSchema {
                    name: "path",
                    typ: String,
                    description: "File path to write",
                },
                ArgSchema {
                    name: "content",
                    typ: String,
                    description: "Content to write",
                },
            ],
        },
        ToolSchema {
            name: "grep",
            description: "Search with regex. Path can be file or directory (recursive).",
            args: &[
                ArgSchema {
                    name: "pattern",
                    typ: String,
                    description: "Regex pattern to search for",
                },
                ArgSchema {
                    name: "path",
                    typ: String,
                    description: "File or directory to search in",
                },
            ],
        },
        ToolSchema {
            name: "glob",
            description: "Find files by glob pattern. Dir defaults to project root.",
            args: &[
                ArgSchema {
                    name: "pattern",
                    typ: String,
                    description: "Glob pattern (e.g. '**/*.rs')",
                },
                ArgSchema {
                    name: "dir",
                    typ: OptionalString,
                    description: "Base directory (defaults to project root)",
                },
            ],
        },
        ToolSchema {
            name: "mkdir",
            description: "Create directory (with parents).",
            args: &[ArgSchema {
                name: "path",
                typ: String,
                description: "Directory path to create",
            }],
        },
        ToolSchema {
            name: "remove",
            description: "Remove file or directory.",
            args: &[ArgSchema {
                name: "path",
                typ: String,
                description: "Path to remove",
            }],
        },
        ToolSchema {
            name: "mv",
            description: "Move / rename file or directory.",
            args: &[
                ArgSchema {
                    name: "src",
                    typ: String,
                    description: "Source path",
                },
                ArgSchema {
                    name: "dst",
                    typ: String,
                    description: "Destination path",
                },
            ],
        },
        ToolSchema {
            name: "exec",
            description: "Execute shell command. cwd = project root.",
            args: &[ArgSchema {
                name: "cmd",
                typ: String,
                description: "Shell command to execute",
            }],
        },
    ];

    SCHEMAS
}

/// Generates formatted tool descriptions from schemas.
///
/// This replaces the hardcoded `TOOL_DESCRIPTIONS` constant.
pub fn generate_descriptions() -> String {
    let mut out = String::from("Available tools (use via orcs.dispatch):\n\n");

    for schema in all_schemas() {
        let args_fmt: Vec<String> = schema
            .args
            .iter()
            .map(|a| {
                if a.typ.is_required() {
                    a.name.to_string()
                } else {
                    format!("{}?", a.name)
                }
            })
            .collect();

        out.push_str(&format!(
            "{}({}) - {}\n",
            schema.name,
            args_fmt.join(", "),
            schema.description,
        ));
    }

    out.push_str("\norcs.pwd - Project root path (string).\n");
    out
}

/// Dispatches a tool call by name with validated arguments.
///
/// Delegates to the registered `orcs.*` Lua functions, which may be
/// the base sandbox-only versions or capability-gated overrides.
/// This ensures dispatch respects the same permission checks as direct calls.
fn dispatch_tool(lua: &Lua, name: &str, args: &Table) -> mlua::Result<Table> {
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
            let result = lua.create_table()?;
            set_error(&result, &format!("unknown tool: {name}"))?;
            Ok(result)
        }
    }
}

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

/// Registers `orcs.dispatch` and `orcs.tool_schemas` in the Lua runtime.
///
/// - `orcs.dispatch(name, args)` — unified tool dispatcher
/// - `orcs.tool_schemas()` — returns structured tool definitions as Lua table
///
/// `dispatch` delegates to the registered `orcs.*` functions, so it
/// automatically respects capability gating if overrides are installed.
pub fn register_dispatch_functions(lua: &Lua) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;

    // orcs.dispatch(name, args) -> result table
    let dispatch_fn =
        lua.create_function(|lua, (name, args): (String, Table)| dispatch_tool(lua, &name, &args))?;
    orcs_table.set("dispatch", dispatch_fn)?;

    // orcs.tool_schemas() -> table of tool schemas
    let schemas_fn = lua.create_function(|lua, ()| {
        let schemas = all_schemas();
        let result = lua.create_table()?;

        for (i, schema) in schemas.iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", schema.name)?;
            entry.set("description", schema.description)?;

            let args_table = lua.create_table()?;
            for (j, arg) in schema.args.iter().enumerate() {
                let arg_entry = lua.create_table()?;
                arg_entry.set("name", arg.name)?;
                arg_entry.set("type", arg.typ.as_str())?;
                arg_entry.set("required", arg.typ.is_required())?;
                arg_entry.set("description", arg.description)?;
                args_table.set(j + 1, arg_entry)?;
            }
            entry.set("args", args_table)?;

            result.set(i + 1, entry)?;
        }

        Ok(result)
    })?;
    orcs_table.set("tool_schemas", schemas_fn)?;

    // Override tool_descriptions with schema-generated version
    let desc = generate_descriptions();
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
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn setup_lua(sandbox: Arc<dyn SandboxPolicy>) -> Lua {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, sandbox).unwrap();
        lua
    }

    // --- dispatch tests ---

    #[test]
    fn dispatch_read() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("test.txt"), "hello dispatch").unwrap();

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("read", {{path="{}"}})"#,
                root.join("test.txt").display()
            ))
            .eval()
            .unwrap();

        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<String>("content").unwrap(), "hello dispatch");
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
        let result: Table = lua.load(&code).eval().unwrap();
        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<String>("content").unwrap(), "via dispatch");
    }

    #[test]
    fn dispatch_grep() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("search.txt"), "line one\nline two\nthird").unwrap();

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("grep", {{pattern="line", path="{}"}})"#,
                root.join("search.txt").display()
            ))
            .eval()
            .unwrap();

        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<usize>("count").unwrap(), 2);
    }

    #[test]
    fn dispatch_glob() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("a.rs"), "").unwrap();
        fs::write(root.join("b.rs"), "").unwrap();
        fs::write(root.join("c.txt"), "").unwrap();

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("glob", {{pattern="*.rs", dir="{}"}})"#,
                root.display()
            ))
            .eval()
            .unwrap();

        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<usize>("count").unwrap(), 2);
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
        let result: Table = lua.load(&code).eval().unwrap();
        let mkdir: Table = result.get("mkdir").unwrap();
        let remove: Table = result.get("remove").unwrap();
        assert!(mkdir.get::<bool>("ok").unwrap());
        assert!(remove.get::<bool>("ok").unwrap());
    }

    #[test]
    fn dispatch_mv() {
        let (root, sandbox) = test_sandbox();
        let src = root.join("src.txt");
        let dst = root.join("dst.txt");
        fs::write(&src, "move me").unwrap();

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(&format!(
                r#"return orcs.dispatch("mv", {{src="{}", dst="{}"}})"#,
                src.display(),
                dst.display()
            ))
            .eval()
            .unwrap();

        assert!(result.get::<bool>("ok").unwrap());
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
            .unwrap();

        assert!(!result.get::<bool>("ok").unwrap());
        assert!(result
            .get::<String>("error")
            .unwrap()
            .contains("unknown tool"));
    }

    #[test]
    fn dispatch_missing_required_arg() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result = lua
            .load(r#"return orcs.dispatch("read", {})"#)
            .eval::<Table>();

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing required argument"), "got: {err}");
    }

    // --- tool_schemas tests ---

    #[test]
    fn tool_schemas_returns_all() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let schemas: Table = lua.load("return orcs.tool_schemas()").eval().unwrap();

        let count = schemas.len().unwrap() as usize;
        assert_eq!(count, all_schemas().len());

        // Verify first schema structure
        let first: Table = schemas.get(1).unwrap();
        assert_eq!(first.get::<String>("name").unwrap(), "read");
        assert!(!first.get::<String>("description").unwrap().is_empty());

        let args: Table = first.get("args").unwrap();
        let first_arg: Table = args.get(1).unwrap();
        assert_eq!(first_arg.get::<String>("name").unwrap(), "path");
        assert_eq!(first_arg.get::<String>("type").unwrap(), "string");
        assert!(first_arg.get::<bool>("required").unwrap());
    }

    // --- generate_descriptions tests ---

    #[test]
    fn descriptions_include_all_tools() {
        let desc = generate_descriptions();
        for schema in all_schemas() {
            assert!(
                desc.contains(schema.name),
                "missing tool in descriptions: {}",
                schema.name
            );
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
            .unwrap();

        // Should return the deny-stub result (not error, just ok=false)
        assert!(!result.get::<bool>("ok").unwrap());
    }
}

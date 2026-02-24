//! Built-in RustTool implementations for the 7 file tools.
//!
//! Each struct wraps the existing `tool_*` function from [`crate::tools`],
//! adding [`RustTool`] metadata (name, description, schema, capability).
//!
//! `exec` is NOT included here — it has a per-command approval flow
//! tightly coupled to `ChildContext` / `ComponentError::Suspended`.

use orcs_component::tool::{RustTool, ToolContext, ToolError};
use orcs_component::Capability;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::tools::{tool_glob, tool_grep, tool_mkdir, tool_mv, tool_read, tool_remove, tool_write};

// ─── Helper ────────────────────────────────────────────────────────

/// Creates a JSON Schema with `type: "object"`.
fn schema(properties: &[(&str, &str, bool)]) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();

    for &(name, description, is_required) in properties {
        props.insert(
            name.to_string(),
            json!({
                "type": "string",
                "description": description,
            }),
        );
        if is_required {
            required.push(Value::String(name.to_string()));
        }
    }

    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

/// Extracts a required string arg from a JSON object.
fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::new(format!("missing required argument: {key}")))
}

// ─── ReadTool ──────────────────────────────────────────────────────

pub struct ReadTool;

impl RustTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read file contents. Path relative to project root."
    }
    fn parameters_schema(&self) -> Value {
        schema(&[("path", "File path to read", true)])
    }
    fn required_capability(&self) -> Capability {
        Capability::READ
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let path = required_str(&args, "path")?;
        let (content, size) = tool_read(path, ctx.sandbox()).map_err(ToolError::from)?;
        Ok(json!({"content": content, "size": size}))
    }
}

// ─── WriteTool ─────────────────────────────────────────────────────

pub struct WriteTool;

impl RustTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write file contents (atomic). Creates parent dirs."
    }
    fn parameters_schema(&self) -> Value {
        schema(&[
            ("path", "File path to write", true),
            ("content", "Content to write", true),
        ])
    }
    fn required_capability(&self) -> Capability {
        Capability::WRITE
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let path = required_str(&args, "path")?;
        let content = required_str(&args, "content")?;
        let bytes = tool_write(path, content, ctx.sandbox()).map_err(ToolError::from)?;
        Ok(json!({"bytes_written": bytes}))
    }
}

// ─── GrepTool ──────────────────────────────────────────────────────

pub struct GrepTool;

impl RustTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search with regex. Path can be file or directory (recursive)."
    }
    fn parameters_schema(&self) -> Value {
        schema(&[
            ("pattern", "Regex pattern to search for", true),
            ("path", "File or directory to search in", true),
        ])
    }
    fn required_capability(&self) -> Capability {
        Capability::READ
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let pattern = required_str(&args, "pattern")?;
        let path = required_str(&args, "path")?;
        let matches = tool_grep(pattern, path, ctx.sandbox()).map_err(ToolError::from)?;
        let matches_json: Vec<Value> = matches
            .iter()
            .map(|m| json!({"line_number": m.line_number, "line": m.line}))
            .collect();
        Ok(json!({"matches": matches_json, "count": matches.len()}))
    }
}

// ─── GlobTool ──────────────────────────────────────────────────────

pub struct GlobTool;

impl RustTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files by glob pattern. Dir defaults to project root."
    }
    fn parameters_schema(&self) -> Value {
        schema(&[
            ("pattern", "Glob pattern (e.g. '**/*.rs')", true),
            ("dir", "Base directory (defaults to project root)", false),
        ])
    }
    fn required_capability(&self) -> Capability {
        Capability::READ
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let pattern = required_str(&args, "pattern")?;
        let dir = args.get("dir").and_then(|v| v.as_str());
        let files = tool_glob(pattern, dir, ctx.sandbox()).map_err(ToolError::from)?;
        Ok(json!({"files": files, "count": files.len()}))
    }
}

// ─── MkdirTool ─────────────────────────────────────────────────────

pub struct MkdirTool;

impl RustTool for MkdirTool {
    fn name(&self) -> &str {
        "mkdir"
    }
    fn description(&self) -> &str {
        "Create directory (with parents)."
    }
    fn parameters_schema(&self) -> Value {
        schema(&[("path", "Directory path to create", true)])
    }
    fn required_capability(&self) -> Capability {
        Capability::WRITE
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let path = required_str(&args, "path")?;
        tool_mkdir(path, ctx.sandbox()).map_err(ToolError::from)?;
        Ok(json!({}))
    }
}

// ─── RemoveTool ────────────────────────────────────────────────────

pub struct RemoveTool;

impl RustTool for RemoveTool {
    fn name(&self) -> &str {
        "remove"
    }
    fn description(&self) -> &str {
        "Remove file or directory."
    }
    fn parameters_schema(&self) -> Value {
        schema(&[("path", "Path to remove", true)])
    }
    fn required_capability(&self) -> Capability {
        Capability::DELETE
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let path = required_str(&args, "path")?;
        tool_remove(path, ctx.sandbox()).map_err(ToolError::from)?;
        Ok(json!({}))
    }
}

// ─── MvTool ────────────────────────────────────────────────────────

pub struct MvTool;

impl RustTool for MvTool {
    fn name(&self) -> &str {
        "mv"
    }
    fn description(&self) -> &str {
        "Move / rename file or directory."
    }
    fn parameters_schema(&self) -> Value {
        schema(&[
            ("src", "Source path", true),
            ("dst", "Destination path", true),
        ])
    }
    fn required_capability(&self) -> Capability {
        Capability::DELETE | Capability::WRITE
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
        let src = required_str(&args, "src")?;
        let dst = required_str(&args, "dst")?;
        tool_mv(src, dst, ctx.sandbox()).map_err(ToolError::from)?;
        Ok(json!({}))
    }
}

// ─── Registry ──────────────────────────────────────────────────────

/// Returns the 7 built-in file tools as `Arc<dyn RustTool>`.
///
/// `exec` is excluded — it has a per-command approval flow that
/// requires `ChildContext` / `ComponentError::Suspended`.
pub fn builtin_rust_tools() -> Vec<Arc<dyn RustTool>> {
    vec![
        Arc::new(ReadTool),
        Arc::new(WriteTool),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
        Arc::new(MkdirTool),
        Arc::new(RemoveTool),
        Arc::new(MvTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::tool::ToolContext;
    use orcs_runtime::sandbox::ProjectSandbox;
    use std::path::PathBuf;

    fn test_sandbox() -> (PathBuf, Arc<dyn orcs_runtime::sandbox::SandboxPolicy>) {
        let dir = std::env::temp_dir().join(format!(
            "orcs-builtin-tools-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("should create temp dir");
        let sandbox = ProjectSandbox::new(&dir).expect("should create sandbox");
        (dir, Arc::new(sandbox))
    }

    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── Identity tests ──────────────────────────────────────────

    #[test]
    fn all_tools_have_unique_names() {
        let tools = builtin_rust_tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let len_before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            len_before,
            "duplicate tool names found: {names:?}"
        );
    }

    #[test]
    fn all_tools_have_object_schema() {
        for tool in builtin_rust_tools() {
            let schema = tool.parameters_schema();
            assert_eq!(
                schema["type"],
                "object",
                "{}: parameters_schema must have type=object",
                tool.name()
            );
        }
    }

    #[test]
    fn read_only_classification() {
        let tools = builtin_rust_tools();
        for tool in &tools {
            let expected = matches!(tool.name(), "read" | "grep" | "glob");
            assert_eq!(
                tool.is_read_only(),
                expected,
                "{}: is_read_only mismatch",
                tool.name()
            );
        }
    }

    #[test]
    fn capability_mapping() {
        let tools = builtin_rust_tools();
        for tool in &tools {
            let cap = tool.required_capability();
            match tool.name() {
                "read" | "grep" | "glob" => assert_eq!(cap, Capability::READ, "{}", tool.name()),
                "write" | "mkdir" => assert_eq!(cap, Capability::WRITE, "{}", tool.name()),
                "remove" => assert_eq!(cap, Capability::DELETE, "{}", tool.name()),
                "mv" => {
                    assert!(
                        cap.contains(Capability::DELETE | Capability::WRITE),
                        "mv should require DELETE|WRITE"
                    );
                }
                _ => panic!("unexpected tool: {}", tool.name()),
            }
        }
    }

    #[test]
    fn intent_def_generation() {
        for tool in builtin_rust_tools() {
            let def = tool.intent_def();
            assert_eq!(def.name, tool.name());
            assert_eq!(def.description, tool.description());
            assert_eq!(def.resolver, orcs_types::intent::IntentResolver::Internal);
        }
    }

    #[test]
    fn builtin_count_is_seven() {
        assert_eq!(
            builtin_rust_tools().len(),
            7,
            "expected 7 file tools (exec excluded)"
        );
    }

    // ── Execute tests ───────────────────────────────────────────

    #[test]
    fn read_tool_execute() {
        let (dir, sandbox) = test_sandbox();
        let file_path = dir.join("hello.txt");
        std::fs::write(&file_path, "hello world").expect("should write test file");

        let tool = ReadTool;
        let ctx = ToolContext::new(sandbox.as_ref());
        let result = tool
            .execute(json!({"path": "hello.txt"}), &ctx)
            .expect("read should succeed");

        assert_eq!(result["content"], "hello world");
        assert_eq!(result["size"], 11);
        cleanup(&dir);
    }

    #[test]
    fn read_tool_missing_file() {
        let (dir, sandbox) = test_sandbox();
        let tool = ReadTool;
        let ctx = ToolContext::new(sandbox.as_ref());

        let err = tool
            .execute(json!({"path": "nonexistent.txt"}), &ctx)
            .expect_err("should fail for missing file");
        assert!(
            !err.message().is_empty(),
            "error message should not be empty"
        );
        cleanup(&dir);
    }

    #[test]
    fn read_tool_missing_arg() {
        let (dir, sandbox) = test_sandbox();
        let tool = ReadTool;
        let ctx = ToolContext::new(sandbox.as_ref());

        let err = tool
            .execute(json!({}), &ctx)
            .expect_err("should fail for missing arg");
        assert!(
            err.message().contains("path"),
            "error should mention 'path', got: {}",
            err.message()
        );
        cleanup(&dir);
    }

    #[test]
    fn write_tool_execute() {
        let (dir, sandbox) = test_sandbox();
        let tool = WriteTool;
        let ctx = ToolContext::new(sandbox.as_ref());

        let result = tool
            .execute(json!({"path": "out.txt", "content": "written"}), &ctx)
            .expect("write should succeed");

        assert_eq!(result["bytes_written"], 7);

        let content =
            std::fs::read_to_string(dir.join("out.txt")).expect("should read written file");
        assert_eq!(content, "written");
        cleanup(&dir);
    }

    #[test]
    fn grep_tool_execute() {
        let (dir, sandbox) = test_sandbox();
        std::fs::write(dir.join("data.txt"), "foo\nbar\nfoo bar\n")
            .expect("should write test file");

        let tool = GrepTool;
        let ctx = ToolContext::new(sandbox.as_ref());
        let result = tool
            .execute(json!({"pattern": "foo", "path": "data.txt"}), &ctx)
            .expect("grep should succeed");

        assert_eq!(result["count"], 2);
        cleanup(&dir);
    }

    #[test]
    fn glob_tool_execute() {
        let (dir, sandbox) = test_sandbox();
        std::fs::write(dir.join("a.rs"), "").expect("should write a.rs");
        std::fs::write(dir.join("b.rs"), "").expect("should write b.rs");
        std::fs::write(dir.join("c.txt"), "").expect("should write c.txt");

        let tool = GlobTool;
        let ctx = ToolContext::new(sandbox.as_ref());
        let result = tool
            .execute(json!({"pattern": "*.rs"}), &ctx)
            .expect("glob should succeed");

        assert_eq!(result["count"], 2);
        cleanup(&dir);
    }

    #[test]
    fn mkdir_tool_execute() {
        let (dir, sandbox) = test_sandbox();
        let tool = MkdirTool;
        let ctx = ToolContext::new(sandbox.as_ref());

        tool.execute(json!({"path": "sub/nested"}), &ctx)
            .expect("mkdir should succeed");

        assert!(dir.join("sub/nested").is_dir());
        cleanup(&dir);
    }

    #[test]
    fn remove_tool_execute() {
        let (dir, sandbox) = test_sandbox();
        let file_path = dir.join("to_remove.txt");
        std::fs::write(&file_path, "delete me").expect("should write file");

        let tool = RemoveTool;
        let ctx = ToolContext::new(sandbox.as_ref());
        tool.execute(json!({"path": "to_remove.txt"}), &ctx)
            .expect("remove should succeed");

        assert!(!file_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn mv_tool_execute() {
        let (dir, sandbox) = test_sandbox();
        std::fs::write(dir.join("src.txt"), "moved").expect("should write src file");

        let tool = MvTool;
        let ctx = ToolContext::new(sandbox.as_ref());
        tool.execute(json!({"src": "src.txt", "dst": "dst.txt"}), &ctx)
            .expect("mv should succeed");

        assert!(!dir.join("src.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("dst.txt")).expect("should read dst"),
            "moved"
        );
        cleanup(&dir);
    }
}

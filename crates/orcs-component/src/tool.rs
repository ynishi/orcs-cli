//! RustTool trait for self-describing, sandboxed tool plugins.
//!
//! A `RustTool` is a unit of functionality that:
//! - Declares its identity (name, description, parameter schema)
//! - Declares its permission requirement (capability, read-only flag)
//! - Executes sandboxed logic given validated arguments
//!
//! # Dispatch Flow
//!
//! ```text
//! orcs.dispatch("read", {path: "src/main.rs"})
//!   → IntentRegistry lookup → RustTool found
//!   → capability check (READ) ✓
//!   → approval check (read_only=true → exempt) ✓
//!   → tool.execute(args, &ctx)
//!   → Result<Value, ToolError>
//! ```
//!
//! # Adding a New Tool
//!
//! ```rust
//! use orcs_component::{RustTool, ToolContext, ToolError, Capability};
//! use serde_json::{json, Value};
//!
//! struct CountLinesTool;
//!
//! impl RustTool for CountLinesTool {
//!     fn name(&self) -> &str { "count_lines" }
//!
//!     fn description(&self) -> &str {
//!         "Count the number of lines in a file"
//!     }
//!
//!     fn parameters_schema(&self) -> Value {
//!         json!({
//!             "type": "object",
//!             "properties": {
//!                 "path": { "type": "string", "description": "File path" }
//!             },
//!             "required": ["path"]
//!         })
//!     }
//!
//!     fn required_capability(&self) -> Capability { Capability::READ }
//!     fn is_read_only(&self) -> bool { true }
//!
//!     fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<Value, ToolError> {
//!         let path = args["path"].as_str()
//!             .ok_or_else(|| ToolError::new("missing 'path' argument"))?;
//!         let canonical = ctx.sandbox()
//!             .validate_read(path)
//!             .map_err(|e| ToolError::new(e.to_string()))?;
//!         let content = std::fs::read_to_string(&canonical)
//!             .map_err(|e| ToolError::new(e.to_string()))?;
//!         Ok(json!({ "lines": content.lines().count() }))
//!     }
//! }
//! ```

use crate::capability::Capability;
use crate::context::ChildContext;
use orcs_auth::SandboxPolicy;
use orcs_types::intent::{IntentDef, IntentResolver};
use thiserror::Error;

// ─── ToolError ──────────────────────────────────────────────────────

/// Error from tool execution.
///
/// Kept intentionally simple. The dispatcher translates this
/// into Lua error tables or `IntentResult.error`.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ToolError {
    message: String,
}

impl ToolError {
    /// Create a new tool error.
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }

    /// Error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<String> for ToolError {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<orcs_auth::SandboxError> for ToolError {
    fn from(e: orcs_auth::SandboxError) -> Self {
        Self::new(e.to_string())
    }
}

// ─── ToolContext ────────────────────────────────────────────────────

/// Execution context provided to [`RustTool::execute`].
///
/// Created by the dispatcher before calling a tool. Provides:
/// - **Sandbox**: path validation for file I/O
/// - **ChildContext** (optional): runtime interaction (command
///   permissions, output emission, etc.)
///
/// # Lifetime
///
/// Borrows from the dispatcher's lock scope. Tools must not
/// store references beyond `execute()`.
pub struct ToolContext<'a> {
    sandbox: &'a dyn SandboxPolicy,
    child_ctx: Option<&'a dyn ChildContext>,
}

impl<'a> ToolContext<'a> {
    /// Create a context with only sandbox (for tests or standalone use).
    pub fn new(sandbox: &'a dyn SandboxPolicy) -> Self {
        Self {
            sandbox,
            child_ctx: None,
        }
    }

    /// Attach a child context (builder pattern).
    #[must_use]
    pub fn with_child_ctx(mut self, ctx: &'a dyn ChildContext) -> Self {
        self.child_ctx = Some(ctx);
        self
    }

    /// Sandbox policy for file path validation.
    pub fn sandbox(&self) -> &dyn SandboxPolicy {
        self.sandbox
    }

    /// Optional child context for runtime interaction.
    ///
    /// Available when running inside a Component's execution context.
    /// `None` in standalone or test scenarios.
    pub fn child_ctx(&self) -> Option<&dyn ChildContext> {
        self.child_ctx
    }
}

impl std::fmt::Debug for ToolContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("sandbox_root", &self.sandbox.root())
            .field("has_child_ctx", &self.child_ctx.is_some())
            .finish()
    }
}

// ─── RustTool ──────────────────────────────────────────────────────

/// A self-describing, sandboxed tool that integrates with `IntentRegistry`.
///
/// Each implementor defines:
/// - **Identity**: name + description + parameter schema (for LLM tools array)
/// - **Permission**: required capability + read-only flag (for dispatcher)
/// - **Logic**: execute function (sandboxed, capability pre-checked)
///
/// # Pre-conditions for `execute()`
///
/// The dispatcher guarantees these before calling `execute()`:
/// 1. `required_capability()` is granted to the caller
/// 2. Mutation approval obtained (for non-`is_read_only()` tools)
///
/// Tools should NOT re-check capabilities. They may use
/// `ctx.sandbox()` for path validation and `ctx.child_ctx()`
/// for command-level permission checks (e.g., `exec`).
pub trait RustTool: Send + Sync {
    /// Unique tool name (e.g., "read", "write", "grep").
    ///
    /// Must be unique within the `IntentRegistry`.
    fn name(&self) -> &str;

    /// Human-readable description (sent to LLM as tool description).
    fn description(&self) -> &str;

    /// JSON Schema for parameters (sent to LLM as tool parameters).
    ///
    /// Must be a valid JSON Schema object with `"type": "object"`.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Required capability for execution.
    ///
    /// The dispatcher checks this BEFORE calling `execute()`.
    fn required_capability(&self) -> Capability;

    /// Whether this tool is read-only.
    ///
    /// Read-only tools are exempt from mutation approval (HIL gate).
    /// Examples: `read`, `grep`, `glob`.
    fn is_read_only(&self) -> bool;

    /// Execute the tool with the given arguments.
    ///
    /// # Arguments
    ///
    /// - `args` — JSON object matching [`parameters_schema()`](Self::parameters_schema)
    /// - `ctx` — Execution context with sandbox and optional child context
    ///
    /// # Returns
    ///
    /// JSON value representing the tool output (structure is tool-specific).
    fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<serde_json::Value, ToolError>;

    /// Generate an [`IntentDef`] for this tool.
    ///
    /// Used by `IntentRegistry` to register the tool and expose it
    /// to LLM tool_calls.
    ///
    /// Default implementation uses [`name()`](Self::name),
    /// [`description()`](Self::description), and
    /// [`parameters_schema()`](Self::parameters_schema)
    /// with `IntentResolver::Internal`.
    fn intent_def(&self) -> IntentDef {
        IntentDef {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
            resolver: IntentResolver::Internal,
        }
    }
}

// ─── Object Safety ─────────────────────────────────────────────────

// Verify RustTool is object-safe at compile time.
const _: () = {
    fn _assert_object_safe(_: &dyn RustTool) {}
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Stub SandboxPolicy for tests ────────────────────────────

    #[derive(Debug)]
    struct StubSandbox {
        root: std::path::PathBuf,
    }

    impl StubSandbox {
        fn new(root: &str) -> Self {
            Self {
                root: std::path::PathBuf::from(root),
            }
        }
    }

    impl SandboxPolicy for StubSandbox {
        fn project_root(&self) -> &std::path::Path {
            &self.root
        }
        fn root(&self) -> &std::path::Path {
            &self.root
        }
        fn validate_read(&self, path: &str) -> Result<std::path::PathBuf, orcs_auth::SandboxError> {
            Ok(self.root.join(path))
        }
        fn validate_write(
            &self,
            path: &str,
        ) -> Result<std::path::PathBuf, orcs_auth::SandboxError> {
            Ok(self.root.join(path))
        }
    }

    // ── Stub RustTool for tests ─────────────────────────────────

    struct EchoTool;

    impl RustTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo back the input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Message to echo" }
                },
                "required": ["message"]
            })
        }
        fn required_capability(&self) -> Capability {
            Capability::READ
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn execute(
            &self,
            args: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<serde_json::Value, ToolError> {
            let msg = args["message"]
                .as_str()
                .ok_or_else(|| ToolError::new("missing 'message'"))?;
            Ok(json!({ "echoed": msg }))
        }
    }

    // ── Tests ───────────────────────────────────────────────────

    #[test]
    fn tool_identity() {
        let tool = EchoTool;
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.description(), "Echo back the input");
        assert!(tool.is_read_only());
        assert_eq!(tool.required_capability(), Capability::READ);
    }

    #[test]
    fn tool_parameters_schema_is_object() {
        let tool = EchoTool;
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["type"], "object",
            "parameters_schema must have type=object"
        );
        assert!(
            schema["properties"]["message"].is_object(),
            "should define 'message' property"
        );
    }

    #[test]
    fn tool_execute_success() {
        let tool = EchoTool;
        let sandbox = StubSandbox::new("/project");
        let ctx = ToolContext::new(&sandbox);

        let result = tool.execute(json!({"message": "hello"}), &ctx);
        let value = result.expect("should succeed");
        assert_eq!(value["echoed"], "hello");
    }

    #[test]
    fn tool_execute_missing_arg() {
        let tool = EchoTool;
        let sandbox = StubSandbox::new("/project");
        let ctx = ToolContext::new(&sandbox);

        let result = tool.execute(json!({}), &ctx);
        let err = result.expect_err("should fail with missing arg");
        assert!(
            err.message().contains("missing"),
            "error should mention missing arg, got: {}",
            err.message()
        );
    }

    #[test]
    fn tool_intent_def_default() {
        let tool = EchoTool;
        let def = tool.intent_def();

        assert_eq!(def.name, "echo");
        assert_eq!(def.description, "Echo back the input");
        assert_eq!(def.resolver, IntentResolver::Internal);
        assert_eq!(def.parameters["type"], "object");
    }

    #[test]
    fn tool_error_from_string() {
        let err = ToolError::from("something went wrong".to_string());
        assert_eq!(err.message(), "something went wrong");
        assert_eq!(err.to_string(), "something went wrong");
    }

    #[test]
    fn tool_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = ToolError::from(io_err);
        assert!(
            err.message().contains("file not found"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn tool_error_from_sandbox() {
        let sandbox_err = orcs_auth::SandboxError::OutsideBoundary {
            path: "/etc/passwd".into(),
            root: "/project".into(),
        };
        let err = ToolError::from(sandbox_err);
        assert!(
            err.message().contains("/etc/passwd"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn tool_error_clone() {
        let err = ToolError::new("test");
        let cloned = err.clone();
        assert_eq!(err.message(), cloned.message());
    }

    #[test]
    fn tool_context_debug() {
        let sandbox = StubSandbox::new("/project");
        let ctx = ToolContext::new(&sandbox);
        let debug = format!("{:?}", ctx);
        assert!(debug.contains("ToolContext"), "got: {debug}");
        assert!(debug.contains("/project"), "got: {debug}");
        assert!(debug.contains("has_child_ctx"), "got: {debug}");
    }

    #[test]
    fn tool_context_without_child_ctx() {
        let sandbox = StubSandbox::new("/project");
        let ctx = ToolContext::new(&sandbox);
        assert!(ctx.child_ctx().is_none());
        assert_eq!(ctx.sandbox().root(), std::path::Path::new("/project"));
    }

    #[test]
    fn tool_object_safety() {
        let tool: Box<dyn RustTool> = Box::new(EchoTool);
        assert_eq!(tool.name(), "echo");

        let sandbox = StubSandbox::new("/project");
        let ctx = ToolContext::new(&sandbox);
        let result = tool.execute(json!({"message": "dyn dispatch"}), &ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn tool_arc_dyn() {
        let tool: std::sync::Arc<dyn RustTool> = std::sync::Arc::new(EchoTool);
        let clone = std::sync::Arc::clone(&tool);
        assert_eq!(tool.name(), clone.name());
    }

    // ── Mutation tool test ──────────────────────────────────────

    struct WriteTool;

    impl RustTool for WriteTool {
        fn name(&self) -> &str {
            "write_stub"
        }
        fn description(&self) -> &str {
            "Stub write tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            })
        }
        fn required_capability(&self) -> Capability {
            Capability::WRITE
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn execute(
            &self,
            args: serde_json::Value,
            ctx: &ToolContext<'_>,
        ) -> Result<serde_json::Value, ToolError> {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| ToolError::new("missing 'path'"))?;
            let _target = ctx.sandbox().validate_write(path)?;
            Ok(json!({ "bytes_written": 42 }))
        }
    }

    #[test]
    fn mutation_tool_not_read_only() {
        let tool = WriteTool;
        assert!(!tool.is_read_only());
        assert_eq!(tool.required_capability(), Capability::WRITE);
    }

    #[test]
    fn mutation_tool_intent_def() {
        let tool = WriteTool;
        let def = tool.intent_def();
        assert_eq!(def.name, "write_stub");
        assert_eq!(def.resolver, IntentResolver::Internal);
    }
}

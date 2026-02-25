//! Integration tests for orcs.read, orcs.write, orcs.grep, orcs.glob
//!
//! Verifies tools work end-to-end from Lua scripts via LuaTestHarness.
//!
//! Note: LuaTestHarness internally calls register_tool_functions which
//! captures cwd as the sandbox root. All temp files must be under cwd.

use orcs_component::{
    Capability, ChildConfig, ChildContext, ChildHandle, ChildResult, CommandPermission,
    EventCategory, RunError, SpawnError,
};
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use orcs_types::ChannelId;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

/// Creates a temp dir under cwd/target/test-tmp/ so it's within the sandbox root.
///
/// Uses PID + atomic counter to guarantee uniqueness across parallel test threads.
/// (nanosecond timestamps alone can collide on macOS due to clock granularity)
fn tempdir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let cwd = std::env::current_dir().expect("should get current working directory");
    let dir = cwd.join(format!(
        "target/test-tmp/orcs-tools-e2e-{}-{}",
        std::process::id(),
        seq,
    ));
    fs::create_dir_all(&dir).expect("should create test temp directory");
    dir
}

fn escape_path(p: &std::path::Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

// ─── orcs.read ──────────────────────────────────────────────────────

mod read {
    use super::*;

    fn read_harness(file_path: &str) -> LuaTestHarness {
        let script = format!(
            r#"
            return {{
                id = "read-test",
                subscriptions = {{"Echo"}},
                on_request = function(req)
                    local r = orcs.read("{}")
                    return {{ success = r.ok, data = {{ content = r.content, size = r.size, error = r.error }} }}
                end,
                on_signal = function(sig) return "Ignored" end,
            }}
            "#,
            file_path
        );
        LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create read test harness from script")
    }

    #[test]
    fn read_existing_file() {
        let dir = tempdir();
        let file = dir.join("read_test.txt");
        fs::write(&file, "hello from read").expect("should write test file for read");

        let mut h = read_harness(&escape_path(&file));
        let result = h.request(EventCategory::Echo, "read", json!(null));

        assert!(result.is_ok());
        let data = result.expect("read request should succeed");
        assert_eq!(data["content"], "hello from read");
        assert_eq!(data["size"], 15);
    }

    #[test]
    fn read_nonexistent_returns_error() {
        let mut h = read_harness("nonexistent_path_xyz.txt");
        let result = h.request(EventCategory::Echo, "read", json!(null));

        // success = false → ComponentError
        assert!(result.is_err());
    }

    #[test]
    fn read_outside_cwd_returns_error() {
        let mut h = read_harness("/etc/hosts");
        let result = h.request(EventCategory::Echo, "read", json!(null));

        // success = false (access denied) → ComponentError
        assert!(result.is_err());
    }

    #[test]
    fn read_utf8_content() {
        let dir = tempdir();
        let file = dir.join("utf8.txt");
        fs::write(&file, "日本語テスト").expect("should write UTF-8 test file");

        let mut h = read_harness(&escape_path(&file));
        let result = h
            .request(EventCategory::Echo, "read", json!(null))
            .expect("read UTF-8 content should succeed");
        assert_eq!(result["content"], "日本語テスト");
    }
}

// ─── orcs.write ─────────────────────────────────────────────────────

mod write {
    use super::*;

    #[test]
    fn write_and_verify() {
        let dir = tempdir();
        let file = dir.join("write_test.txt");
        let path = escape_path(&file);

        let script = format!(
            r#"
            return {{
                id = "write-test",
                subscriptions = {{"Echo"}},
                on_request = function(req)
                    local w = orcs.write("{}", "written content")
                    return {{ success = w.ok, data = {{ bytes_written = w.bytes_written }} }}
                end,
                on_signal = function(sig) return "Ignored" end,
            }}
            "#,
            path
        );

        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create write test harness");
        let result = h
            .request(EventCategory::Echo, "write", json!(null))
            .expect("write request should succeed");

        assert_eq!(result["bytes_written"], 15);
        assert_eq!(
            fs::read_to_string(&file).expect("should read back written file"),
            "written content"
        );
    }

    #[test]
    fn write_creates_subdirs() {
        let dir = tempdir();
        let file = dir.join("sub/deep/file.txt");
        let path = escape_path(&file);

        let script = format!(
            r#"
            return {{
                id = "write-subdir",
                subscriptions = {{"Echo"}},
                on_request = function(req)
                    local w = orcs.write("{}", "nested")
                    return {{ success = w.ok, data = {{ bytes = w.bytes_written }} }}
                end,
                on_signal = function(sig) return "Ignored" end,
            }}
            "#,
            path
        );

        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create subdir write test harness");
        let result = h
            .request(EventCategory::Echo, "write", json!(null))
            .expect("write with subdirs should succeed");

        assert_eq!(result["bytes"], 6);
        assert_eq!(
            fs::read_to_string(&file).expect("should read back nested file"),
            "nested"
        );
    }
}

// ─── orcs.grep ──────────────────────────────────────────────────────

mod grep {
    use super::*;

    #[test]
    fn grep_finds_matches() {
        let dir = tempdir();
        let file = dir.join("grep_test.txt");
        fs::write(&file, "line one\nline two\nthird").expect("should write grep test file");
        let path = escape_path(&file);

        let script = format!(
            r#"
            return {{
                id = "grep-test",
                subscriptions = {{"Echo"}},
                on_request = function(req)
                    local g = orcs.grep("line", "{}")
                    if not g.ok then
                        return {{ success = false, error = g.error }}
                    end
                    local first = g.matches[1]
                    return {{
                        success = true,
                        data = {{
                            count = g.count,
                            first_line_num = first and first.line_number or 0,
                            first_line = first and first.line or ""
                        }}
                    }}
                end,
                on_signal = function(sig) return "Ignored" end,
            }}
            "#,
            path
        );

        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create grep test harness");
        let result = h
            .request(EventCategory::Echo, "grep", json!(null))
            .expect("grep request should succeed");

        assert_eq!(result["count"], 2);
        assert_eq!(result["first_line_num"], 1);
        assert_eq!(result["first_line"], "line one");
    }

    #[test]
    fn grep_regex_works() {
        let dir = tempdir();
        let file = dir.join("regex_test.txt");
        fs::write(&file, "foo123\nbar\nfoo456").expect("should write regex test file");
        let path = escape_path(&file);

        let script = format!(
            r#"
            return {{
                id = "grep-regex",
                subscriptions = {{"Echo"}},
                on_request = function(req)
                    local g = orcs.grep("foo\\d+", "{}")
                    return {{ success = g.ok, data = {{ count = g.count }} }}
                end,
                on_signal = function(sig) return "Ignored" end,
            }}
            "#,
            path
        );

        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create regex grep test harness");
        let result = h
            .request(EventCategory::Echo, "grep", json!(null))
            .expect("regex grep request should succeed");
        assert_eq!(result["count"], 2);
    }
}

// ─── orcs.glob ──────────────────────────────────────────────────────

mod glob {
    use super::*;

    #[test]
    fn glob_finds_by_extension() {
        let dir = tempdir();
        fs::write(dir.join("a.rs"), "").expect("should write a.rs");
        fs::write(dir.join("b.rs"), "").expect("should write b.rs");
        fs::write(dir.join("c.txt"), "").expect("should write c.txt");
        let dir_path = escape_path(&dir);

        let script = format!(
            r#"
            return {{
                id = "glob-test",
                subscriptions = {{"Echo"}},
                on_request = function(req)
                    local g = orcs.glob("*.rs", "{}")
                    return {{ success = g.ok, data = {{ count = g.count }} }}
                end,
                on_signal = function(sig) return "Ignored" end,
            }}
            "#,
            dir_path
        );

        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create glob test harness");
        let result = h
            .request(EventCategory::Echo, "glob", json!(null))
            .expect("glob request should succeed");
        assert_eq!(result["count"], 2);
    }

    #[test]
    fn glob_default_dir_uses_cwd() {
        // glob without dir arg should use current directory
        let script = r#"
            return {
                id = "glob-cwd",
                subscriptions = {"Echo"},
                on_request = function(req)
                    local g = orcs.glob("Cargo.toml")
                    return { success = g.ok, data = { count = g.count } }
                end,
                on_signal = function(sig) return "Ignored" end,
            }
        "#;

        let mut h = LuaTestHarness::from_script(script, test_sandbox())
            .expect("should create glob cwd test harness");
        let result = h
            .request(EventCategory::Echo, "glob", json!(null))
            .expect("glob with default dir should succeed");
        // Should find at least Cargo.toml in workspace root
        assert!(
            result["count"]
                .as_i64()
                .expect("count should be an integer")
                >= 1
        );
    }
}

// ─── Write + Read roundtrip ─────────────────────────────────────────

#[test]
fn write_read_roundtrip() {
    let dir = tempdir();
    let file = dir.join("roundtrip.txt");
    let path = escape_path(&file);

    let script = format!(
        r#"
        return {{
            id = "roundtrip",
            subscriptions = {{"Echo"}},
            on_request = function(req)
                orcs.write("{path}", "roundtrip data 123")
                local r = orcs.read("{path}")
                return {{ success = r.ok, data = {{ content = r.content }} }}
            end,
            on_signal = function(sig) return "Ignored" end,
        }}
        "#,
        path = path
    );

    let mut h = LuaTestHarness::from_script(&script, test_sandbox())
        .expect("should create roundtrip test harness");
    let result = h
        .request(EventCategory::Echo, "test", json!(null))
        .expect("write-read roundtrip should succeed");
    assert_eq!(result["content"], "roundtrip data 123");
}

// ─── Write + Grep ───────────────────────────────────────────────────

#[test]
fn write_then_grep() {
    let dir = tempdir();
    let file = dir.join("write_grep.txt");
    let path = escape_path(&file);

    let script = format!(
        r#"
        return {{
            id = "write-grep",
            subscriptions = {{"Echo"}},
            on_request = function(req)
                orcs.write("{path}", "error: something failed\ninfo: all good\nerror: another one")
                local g = orcs.grep("error", "{path}")
                return {{ success = g.ok, data = {{ count = g.count }} }}
            end,
            on_signal = function(sig) return "Ignored" end,
        }}
        "#,
        path = path
    );

    let mut h = LuaTestHarness::from_script(&script, test_sandbox())
        .expect("should create write-then-grep test harness");
    let result = h
        .request(EventCategory::Echo, "test", json!(null))
        .expect("write-then-grep should succeed");
    assert_eq!(result["count"], 2);
}

// ─── Capability-gated file tools ────────────────────────────────────

/// Minimal mock ChildContext with configurable capabilities.
#[derive(Debug, Clone)]
struct CapMockContext {
    caps: Capability,
}

impl CapMockContext {
    fn with_caps(caps: Capability) -> Box<dyn ChildContext> {
        Box::new(Self { caps })
    }
}

impl ChildContext for CapMockContext {
    fn parent_id(&self) -> &str {
        "cap-test"
    }
    fn emit_output(&self, _msg: &str) {}
    fn emit_output_with_level(&self, _msg: &str, _level: &str) {}
    fn child_count(&self) -> usize {
        0
    }
    fn max_children(&self) -> usize {
        0
    }
    fn spawn_child(&self, _config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
        Err(SpawnError::Internal("not supported".into()))
    }
    fn send_to_child(&self, _id: &str, _input: Value) -> Result<ChildResult, RunError> {
        Err(RunError::NotFound("not supported".into()))
    }
    fn capabilities(&self) -> Capability {
        self.caps
    }
    fn check_command_permission(&self, _cmd: &str) -> CommandPermission {
        CommandPermission::Denied("not supported".into())
    }
    fn can_execute_command(&self, _cmd: &str) -> bool {
        false
    }
    fn can_spawn_child_auth(&self) -> bool {
        false
    }
    fn can_spawn_runner_auth(&self) -> bool {
        false
    }
    fn grant_command(&self, _pattern: &str) {}
    fn spawn_runner_from_script(
        &self,
        _script: &str,
        _id: Option<&str>,
        _globals: Option<&serde_json::Value>,
    ) -> Result<(ChannelId, String), SpawnError> {
        Err(SpawnError::Internal("not supported".into()))
    }
    fn clone_box(&self) -> Box<dyn ChildContext> {
        Box::new(self.clone())
    }
}

mod capability_gating {
    use super::*;

    /// Lua script that tries read/write/remove and returns results.
    fn cap_test_script(file_path: &str) -> String {
        format!(
            r#"
            return {{
                id = "cap-test",
                subscriptions = {{"Echo"}},
                on_request = function(req)
                    if req.operation == "read" then
                        local r = orcs.read("{path}")
                        return {{ success = r.ok, data = {{ content = r.content, error = r.error }} }}
                    elseif req.operation == "write" then
                        local r = orcs.write("{path}", "written by test")
                        return {{ success = r.ok, data = {{ bytes = r.bytes_written, error = r.error }} }}
                    elseif req.operation == "remove" then
                        local r = orcs.remove("{path}")
                        return {{ success = r.ok, data = {{ error = r.error }} }}
                    end
                    return {{ success = false, error = "unknown op" }}
                end,
                on_signal = function(sig) return "Ignored" end,
            }}
            "#,
            path = file_path
        )
    }

    #[test]
    fn read_allowed_with_read_capability() {
        let dir = tempdir();
        let file = dir.join("cap_read.txt");
        fs::write(&file, "cap-gated content").expect("should write capability test file");

        let script = cap_test_script(&escape_path(&file));
        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create capability read test harness");

        // Set ChildContext with READ capability
        h.component_mut()
            .set_child_context(CapMockContext::with_caps(Capability::ALL));

        let result = h
            .request(EventCategory::Echo, "read", json!(null))
            .expect("read with READ capability should succeed");
        assert_eq!(result["content"], "cap-gated content");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_denied_without_read_capability() {
        let dir = tempdir();
        let file = dir.join("cap_read_deny.txt");
        fs::write(&file, "should not read").expect("should write deny-read test file");

        let script = cap_test_script(&escape_path(&file));
        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create deny-read test harness");

        // Set ChildContext with WRITE only (no READ)
        h.component_mut()
            .set_child_context(CapMockContext::with_caps(Capability::WRITE));

        let result = h.request(EventCategory::Echo, "read", json!(null));
        // Request succeeds (Lua returns success=false), so check inner data
        assert!(
            result.is_err() || {
                let v = result.expect("read result should be available for inspection");
                let err = v["error"].as_str().unwrap_or("");
                err.contains("Capability::READ")
            }
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_denied_without_write_capability() {
        let dir = tempdir();
        let file = dir.join("cap_write_deny.txt");

        let script = cap_test_script(&escape_path(&file));
        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create deny-write test harness");

        // Set ChildContext with READ only (no WRITE)
        h.component_mut()
            .set_child_context(CapMockContext::with_caps(Capability::READ));

        let result = h.request(EventCategory::Echo, "write", json!(null));
        assert!(
            result.is_err() || {
                let v = result.expect("write result should be available for inspection");
                let err = v["error"].as_str().unwrap_or("");
                err.contains("Capability::WRITE")
            }
        );

        // File should NOT have been created
        assert!(!file.exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_denied_without_delete_capability() {
        let dir = tempdir();
        let file = dir.join("cap_remove_deny.txt");
        fs::write(&file, "should survive").expect("should write deny-delete test file");

        let script = cap_test_script(&escape_path(&file));
        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create deny-delete test harness");

        // Set ChildContext with READ | WRITE (no DELETE)
        h.component_mut()
            .set_child_context(CapMockContext::with_caps(
                Capability::READ | Capability::WRITE,
            ));

        let result = h.request(EventCategory::Echo, "remove", json!(null));
        assert!(
            result.is_err() || {
                let v = result.expect("remove result should be available for inspection");
                let err = v["error"].as_str().unwrap_or("");
                err.contains("Capability::DELETE")
            }
        );

        // File should still exist
        assert!(file.exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_child_context_read_works_sandbox_only() {
        // Without ChildContext, base tools (sandbox-only) are used
        let dir = tempdir();
        let file = dir.join("no_ctx_read.txt");
        fs::write(&file, "sandbox only").expect("should write sandbox-only test file");

        let script = cap_test_script(&escape_path(&file));
        let mut h = LuaTestHarness::from_script(&script, test_sandbox())
            .expect("should create sandbox-only test harness");

        // No set_child_context — base tools active
        let result = h
            .request(EventCategory::Echo, "read", json!(null))
            .expect("sandbox-only read should succeed");
        assert_eq!(result["content"], "sandbox only");

        fs::remove_dir_all(&dir).ok();
    }
}

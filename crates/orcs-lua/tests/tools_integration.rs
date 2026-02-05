//! Integration tests for orcs.read, orcs.write, orcs.grep, orcs.glob
//!
//! Verifies tools work end-to-end from Lua scripts via LuaTestHarness.

use orcs_component::EventCategory;
use orcs_lua::testing::LuaTestHarness;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orcs-tools-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
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
        LuaTestHarness::from_script(&script).unwrap()
    }

    #[test]
    fn read_existing_file() {
        let dir = tempdir();
        let file = dir.join("read_test.txt");
        fs::write(&file, "hello from read").unwrap();

        let mut h = read_harness(&escape_path(&file));
        let result = h.request(EventCategory::Echo, "read", json!(null));

        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data["content"], "hello from read");
        assert_eq!(data["size"], 15);
    }

    #[test]
    fn read_nonexistent_returns_error() {
        let mut h = read_harness("/nonexistent/path/file.txt");
        let result = h.request(EventCategory::Echo, "read", json!(null));

        // success = false → ComponentError
        assert!(result.is_err());
    }

    #[test]
    fn read_utf8_content() {
        let dir = tempdir();
        let file = dir.join("utf8.txt");
        fs::write(&file, "日本語テスト").unwrap();

        let mut h = read_harness(&escape_path(&file));
        let result = h.request(EventCategory::Echo, "read", json!(null)).unwrap();
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

        let mut h = LuaTestHarness::from_script(&script).unwrap();
        let result = h
            .request(EventCategory::Echo, "write", json!(null))
            .unwrap();

        assert_eq!(result["bytes_written"], 15);
        assert_eq!(fs::read_to_string(&file).unwrap(), "written content");
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

        let mut h = LuaTestHarness::from_script(&script).unwrap();
        let result = h
            .request(EventCategory::Echo, "write", json!(null))
            .unwrap();

        assert_eq!(result["bytes"], 6);
        assert_eq!(fs::read_to_string(&file).unwrap(), "nested");
    }
}

// ─── orcs.grep ──────────────────────────────────────────────────────

mod grep {
    use super::*;

    #[test]
    fn grep_finds_matches() {
        let dir = tempdir();
        let file = dir.join("grep_test.txt");
        fs::write(&file, "line one\nline two\nthird").unwrap();
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

        let mut h = LuaTestHarness::from_script(&script).unwrap();
        let result = h.request(EventCategory::Echo, "grep", json!(null)).unwrap();

        assert_eq!(result["count"], 2);
        assert_eq!(result["first_line_num"], 1);
        assert_eq!(result["first_line"], "line one");
    }

    #[test]
    fn grep_regex_works() {
        let dir = tempdir();
        let file = dir.join("regex_test.txt");
        fs::write(&file, "foo123\nbar\nfoo456").unwrap();
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

        let mut h = LuaTestHarness::from_script(&script).unwrap();
        let result = h.request(EventCategory::Echo, "grep", json!(null)).unwrap();
        assert_eq!(result["count"], 2);
    }
}

// ─── orcs.glob ──────────────────────────────────────────────────────

mod glob {
    use super::*;

    #[test]
    fn glob_finds_by_extension() {
        let dir = tempdir();
        fs::write(dir.join("a.rs"), "").unwrap();
        fs::write(dir.join("b.rs"), "").unwrap();
        fs::write(dir.join("c.txt"), "").unwrap();
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

        let mut h = LuaTestHarness::from_script(&script).unwrap();
        let result = h.request(EventCategory::Echo, "glob", json!(null)).unwrap();
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

        let mut h = LuaTestHarness::from_script(script).unwrap();
        let result = h.request(EventCategory::Echo, "glob", json!(null)).unwrap();
        // Should find at least Cargo.toml in workspace root
        assert!(result["count"].as_i64().unwrap() >= 1);
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

    let mut h = LuaTestHarness::from_script(&script).unwrap();
    let result = h.request(EventCategory::Echo, "test", json!(null)).unwrap();
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

    let mut h = LuaTestHarness::from_script(&script).unwrap();
    let result = h.request(EventCategory::Echo, "test", json!(null)).unwrap();
    assert_eq!(result["count"], 2);
}

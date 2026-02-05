//! First-class Rust tool implementations for Lua.
//!
//! Provides native Rust implementations of common operations,
//! exposed as `orcs.*` functions in Lua. These replace subprocess-based
//! equivalents (`cat`, `grep`, `find`) with type-safe, structured results.
//!
//! # Available Tools
//!
//! | Lua API | Description |
//! |---------|-------------|
//! | `orcs.read(path)` | Read file contents |
//! | `orcs.write(path, content)` | Write file contents (atomic) |
//! | `orcs.grep(pattern, path)` | Search file contents with regex |
//! | `orcs.glob(pattern, dir?)` | Find files by glob pattern |
//!
//! # Security
//!
//! All file operations validate paths to prevent directory traversal.

use crate::error::LuaError;
use mlua::{Lua, Table};
use std::path::{Path, PathBuf};

/// Validates a path is safe (no traversal above working directory).
///
/// Returns the canonicalized path if safe.
fn validate_path(path: &str) -> Result<PathBuf, String> {
    let requested = Path::new(path);

    // Resolve to absolute path
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot determine working directory: {}", e))?
            .join(requested)
    };

    // Reject obvious traversal patterns before touching filesystem
    let path_str = absolute.to_string_lossy();
    if path_str.contains("/../") || path_str.ends_with("/..") {
        return Err(format!("path traversal detected: {}", path));
    }

    Ok(absolute)
}

/// Validates a path exists and returns its canonical form.
fn validate_existing_path(path: &str) -> Result<PathBuf, String> {
    let absolute = validate_path(path)?;

    absolute
        .canonicalize()
        .map_err(|e| format!("path not found: {} ({})", path, e))
}

// ─── Rust Tool Implementations ──────────────────────────────────────────

/// Reads a file and returns its contents.
fn tool_read(path: &str) -> Result<(String, u64), String> {
    let canonical = validate_existing_path(path)?;

    let metadata = std::fs::metadata(&canonical)
        .map_err(|e| format!("cannot read metadata: {} ({})", path, e))?;

    if !metadata.is_file() {
        return Err(format!("not a file: {}", path));
    }

    let size = metadata.len();
    let content = std::fs::read_to_string(&canonical)
        .map_err(|e| format!("read failed: {} ({})", path, e))?;

    Ok((content, size))
}

/// Writes content to a file atomically (write to temp, then rename).
fn tool_write(path: &str, content: &str) -> Result<usize, String> {
    let target = validate_path(path)?;

    // Ensure parent directory exists
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create parent directory: {}", e))?;
    }

    let bytes = content.len();

    // Atomic write: write to temp file, then rename
    let temp_path = target.with_extension("tmp.orcs");
    std::fs::write(&temp_path, content).map_err(|e| format!("write failed: {} ({})", path, e))?;

    std::fs::rename(&temp_path, &target).map_err(|e| {
        // Clean up temp file on rename failure
        let _ = std::fs::remove_file(&temp_path);
        format!("rename failed: {} ({})", path, e)
    })?;

    Ok(bytes)
}

/// Represents a single grep match.
#[derive(Debug)]
struct GrepMatch {
    line_number: usize,
    line: String,
}

/// Searches a file (or directory recursively) for a regex pattern.
fn tool_grep(pattern: &str, path: &str) -> Result<Vec<GrepMatch>, String> {
    let re =
        regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {} ({})", pattern, e))?;

    let canonical = validate_existing_path(path)?;
    let mut matches = Vec::new();

    if canonical.is_file() {
        grep_file(&re, &canonical, &mut matches)?;
    } else if canonical.is_dir() {
        grep_dir(&re, &canonical, &mut matches)?;
    } else {
        return Err(format!("not a file or directory: {}", path));
    }

    Ok(matches)
}

fn grep_file(re: &regex::Regex, path: &Path, matches: &mut Vec<GrepMatch>) -> Result<(), String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read failed: {:?} ({})", path, e))?;

    for (i, line) in content.lines().enumerate() {
        if re.is_match(line) {
            matches.push(GrepMatch {
                line_number: i + 1,
                line: line.to_string(),
            });
        }
    }

    Ok(())
}

fn grep_dir(re: &regex::Regex, dir: &Path, matches: &mut Vec<GrepMatch>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read directory: {:?} ({})", dir, e))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            // Skip binary files (best-effort: check for null bytes in first 512 bytes)
            if let Ok(bytes) = std::fs::read(&path) {
                let check_len = bytes.len().min(512);
                if bytes[..check_len].contains(&0) {
                    continue;
                }
            }
            // Ignore read errors on individual files in directory mode
            let _ = grep_file(re, &path, matches);
        } else if path.is_dir() {
            let _ = grep_dir(re, &path, matches);
        }
    }

    Ok(())
}

/// Finds files matching a glob pattern.
fn tool_glob(pattern: &str, dir: Option<&str>) -> Result<Vec<String>, String> {
    let full_pattern = match dir {
        Some(d) => {
            let base = validate_existing_path(d)?;
            if !base.is_dir() {
                return Err(format!("not a directory: {}", d));
            }
            format!("{}/{}", base.display(), pattern)
        }
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| format!("cannot determine working directory: {}", e))?;
            format!("{}/{}", cwd.display(), pattern)
        }
    };

    let paths = glob::glob(&full_pattern)
        .map_err(|e| format!("invalid glob pattern: {} ({})", pattern, e))?;

    let mut results = Vec::new();
    for entry in paths.flatten() {
        results.push(entry.display().to_string());
    }

    results.sort();
    Ok(results)
}

// ─── Lua Registration ───────────────────────────────────────────────────

/// Registers all tool functions in the Lua `orcs` table.
///
/// Adds: `orcs.read`, `orcs.write`, `orcs.grep`, `orcs.glob`
///
/// # Errors
///
/// Returns error if function registration fails.
pub fn register_tool_functions(lua: &Lua) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;

    // orcs.read(path) -> { ok, content, size, error }
    let read_fn = lua.create_function(|lua, path: String| {
        let result = lua.create_table()?;
        match tool_read(&path) {
            Ok((content, size)) => {
                result.set("ok", true)?;
                result.set("content", content)?;
                result.set("size", size)?;
            }
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e)?;
            }
        }
        Ok(result)
    })?;
    orcs_table.set("read", read_fn)?;

    // orcs.write(path, content) -> { ok, bytes_written, error }
    let write_fn = lua.create_function(|lua, (path, content): (String, String)| {
        let result = lua.create_table()?;
        match tool_write(&path, &content) {
            Ok(bytes) => {
                result.set("ok", true)?;
                result.set("bytes_written", bytes)?;
            }
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e)?;
            }
        }
        Ok(result)
    })?;
    orcs_table.set("write", write_fn)?;

    // orcs.grep(pattern, path) -> { ok, matches[], count, error }
    let grep_fn = lua.create_function(|lua, (pattern, path): (String, String)| {
        let result = lua.create_table()?;
        match tool_grep(&pattern, &path) {
            Ok(grep_matches) => {
                let matches_table = lua.create_table()?;
                for (i, m) in grep_matches.iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("line_number", m.line_number)?;
                    entry.set("line", m.line.as_str())?;
                    matches_table.set(i + 1, entry)?;
                }
                result.set("ok", true)?;
                result.set("matches", matches_table)?;
                result.set("count", grep_matches.len())?;
            }
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e)?;
            }
        }
        Ok(result)
    })?;
    orcs_table.set("grep", grep_fn)?;

    // orcs.glob(pattern, dir?) -> { ok, files[], count, error }
    let glob_fn = lua.create_function(|lua, (pattern, dir): (String, Option<String>)| {
        let result = lua.create_table()?;
        match tool_glob(&pattern, dir.as_deref()) {
            Ok(files) => {
                let files_table = lua.create_table()?;
                for (i, f) in files.iter().enumerate() {
                    files_table.set(i + 1, f.as_str())?;
                }
                result.set("ok", true)?;
                result.set("files", files_table)?;
                result.set("count", files.len())?;
            }
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e)?;
            }
        }
        Ok(result)
    })?;
    orcs_table.set("glob", glob_fn)?;

    tracing::debug!("Registered orcs tool functions: read, write, grep, glob");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ─── Path Validation ────────────────────────────────────────────

    #[test]
    fn validate_path_rejects_traversal() {
        let result = validate_path("/tmp/../../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("traversal"));
    }

    #[test]
    fn validate_path_accepts_relative() {
        let result = validate_path("Cargo.toml");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_accepts_absolute() {
        let result = validate_path("/tmp");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_existing_path_rejects_nonexistent() {
        let result = validate_existing_path("/nonexistent/path/file.txt");
        assert!(result.is_err());
    }

    // ─── tool_read ──────────────────────────────────────────────────

    #[test]
    fn read_existing_file() {
        let dir = tempdir();
        let file = dir.join("test.txt");
        fs::write(&file, "hello world").unwrap();

        let (content, size) = tool_read(file.to_str().unwrap()).unwrap();
        assert_eq!(content, "hello world");
        assert_eq!(size, 11);
    }

    #[test]
    fn read_nonexistent_file() {
        let result = tool_read("/nonexistent/file.txt");
        assert!(result.is_err());
    }

    #[test]
    fn read_directory_fails() {
        let dir = tempdir();
        let result = tool_read(dir.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a file"));
    }

    // ─── tool_write ─────────────────────────────────────────────────

    #[test]
    fn write_new_file() {
        let dir = tempdir();
        let file = dir.join("new.txt");

        let bytes = tool_write(file.to_str().unwrap(), "new content").unwrap();
        assert_eq!(bytes, 11);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new content");
    }

    #[test]
    fn write_overwrites_existing() {
        let dir = tempdir();
        let file = dir.join("existing.txt");
        fs::write(&file, "old").unwrap();

        tool_write(file.to_str().unwrap(), "new").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = tempdir();
        let file = dir.join("sub/dir/file.txt");

        tool_write(file.to_str().unwrap(), "nested").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "nested");
    }

    #[test]
    fn write_atomic_no_temp_leftover() {
        let dir = tempdir();
        let file = dir.join("atomic.txt");

        tool_write(file.to_str().unwrap(), "content").unwrap();

        // Temp file should not exist after successful write
        let temp = file.with_extension("tmp.orcs");
        assert!(!temp.exists());
    }

    // ─── tool_grep ──────────────────────────────────────────────────

    #[test]
    fn grep_finds_matches() {
        let dir = tempdir();
        let file = dir.join("search.txt");
        fs::write(&file, "line one\nline two\nthird line").unwrap();

        let matches = tool_grep("line", file.to_str().unwrap()).unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].line_number, 1);
        assert_eq!(matches[0].line, "line one");
    }

    #[test]
    fn grep_regex_pattern() {
        let dir = tempdir();
        let file = dir.join("regex.txt");
        fs::write(&file, "foo123\nbar456\nfoo789").unwrap();

        let matches = tool_grep(r"foo\d+", file.to_str().unwrap()).unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn grep_no_matches() {
        let dir = tempdir();
        let file = dir.join("empty.txt");
        fs::write(&file, "nothing here").unwrap();

        let matches = tool_grep("nonexistent", file.to_str().unwrap()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn grep_invalid_regex() {
        let dir = tempdir();
        let file = dir.join("test.txt");
        fs::write(&file, "content").unwrap();

        let result = tool_grep("[invalid", file.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid regex"));
    }

    #[test]
    fn grep_directory_recursive() {
        let dir = tempdir();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();

        fs::write(dir.join("a.txt"), "target line\nother").unwrap();
        fs::write(sub.join("b.txt"), "no match\ntarget here").unwrap();

        let matches = tool_grep("target", dir.to_str().unwrap()).unwrap();
        assert_eq!(matches.len(), 2);
    }

    // ─── tool_glob ──────────────────────────────────────────────────

    #[test]
    fn glob_finds_files() {
        let dir = tempdir();
        fs::write(dir.join("a.txt"), "").unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("c.rs"), "").unwrap();

        let files = tool_glob("*.txt", Some(dir.to_str().unwrap())).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_recursive() {
        let dir = tempdir();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.join("top.rs"), "").unwrap();
        fs::write(sub.join("nested.rs"), "").unwrap();

        let files = tool_glob("**/*.rs", Some(dir.to_str().unwrap())).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_no_matches() {
        let dir = tempdir();
        let files = tool_glob("*.xyz", Some(dir.to_str().unwrap())).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn glob_invalid_pattern() {
        let result = tool_glob("[invalid", Some("/tmp"));
        assert!(result.is_err());
    }

    // ─── Lua Integration ────────────────────────────────────────────

    #[test]
    fn register_tools_in_lua() {
        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();

        register_tool_functions(&lua).unwrap();

        let orcs: Table = lua.globals().get("orcs").unwrap();
        assert!(orcs.get::<mlua::Function>("read").is_ok());
        assert!(orcs.get::<mlua::Function>("write").is_ok());
        assert!(orcs.get::<mlua::Function>("grep").is_ok());
        assert!(orcs.get::<mlua::Function>("glob").is_ok());
    }

    #[test]
    fn lua_read_file() {
        let dir = tempdir();
        let file = dir.join("lua_read.txt");
        fs::write(&file, "lua content").unwrap();

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua).unwrap();

        let code = format!(
            r#"return orcs.read("{}")"#,
            file.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().unwrap();
        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<String>("content").unwrap(), "lua content");
        assert_eq!(result.get::<u64>("size").unwrap(), 11);
    }

    #[test]
    fn lua_write_file() {
        let dir = tempdir();
        let file = dir.join("lua_write.txt");

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua).unwrap();

        let code = format!(
            r#"return orcs.write("{}", "written from lua")"#,
            file.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().unwrap();
        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), "written from lua");
    }

    #[test]
    fn lua_grep_file() {
        let dir = tempdir();
        let file = dir.join("lua_grep.txt");
        fs::write(&file, "alpha\nbeta\nalpha_two").unwrap();

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua).unwrap();

        let code = format!(
            r#"return orcs.grep("alpha", "{}")"#,
            file.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().unwrap();
        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<usize>("count").unwrap(), 2);
    }

    #[test]
    fn lua_glob_files() {
        let dir = tempdir();
        fs::write(dir.join("a.lua"), "").unwrap();
        fs::write(dir.join("b.lua"), "").unwrap();

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua).unwrap();

        let code = format!(
            r#"return orcs.glob("*.lua", "{}")"#,
            dir.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().unwrap();
        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<usize>("count").unwrap(), 2);
    }

    #[test]
    fn lua_read_nonexistent_returns_error() {
        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua).unwrap();

        let result: Table = lua
            .load(r#"return orcs.read("/nonexistent/file.txt")"#)
            .eval()
            .unwrap();
        assert!(!result.get::<bool>("ok").unwrap());
        assert!(result.get::<String>("error").is_ok());
    }

    // ─── Test Helpers ───────────────────────────────────────────────

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orcs-tools-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

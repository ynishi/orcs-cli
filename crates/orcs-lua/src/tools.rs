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
//! | `orcs.mkdir(path)` | Create directory (with parents) |
//! | `orcs.remove(path)` | Remove file or directory |
//! | `orcs.mv(src, dst)` | Move / rename |
//!
//! # Security
//!
//! All file operations are sandboxed via [`SandboxPolicy`]. Paths are
//! validated through the sandbox before any I/O occurs. The sandbox is
//! injected at registration time — no implicit `current_dir()` dependency.

use crate::error::LuaError;
use mlua::{Lua, Table};
use orcs_runtime::sandbox::SandboxPolicy;
use std::path::Path;
use std::sync::Arc;

// ─── Rust Tool Implementations ──────────────────────────────────────────

/// Reads a file and returns its contents.
fn tool_read(path: &str, sandbox: &dyn SandboxPolicy) -> Result<(String, u64), String> {
    let canonical = sandbox.validate_read(path).map_err(|e| e.to_string())?;

    let metadata =
        std::fs::metadata(&canonical).map_err(|e| format!("cannot read metadata: {path} ({e})"))?;

    if !metadata.is_file() {
        return Err(format!("not a file: {path}"));
    }

    let size = metadata.len();
    let content =
        std::fs::read_to_string(&canonical).map_err(|e| format!("read failed: {path} ({e})"))?;

    Ok((content, size))
}

/// Writes content to a file atomically (write to temp, then rename).
///
/// Uses [`tempfile::NamedTempFile`] to create a temp file with an
/// unpredictable name in the same directory as the target. This prevents
/// symlink attacks on predictable temp file paths.
fn tool_write(path: &str, content: &str, sandbox: &dyn SandboxPolicy) -> Result<usize, String> {
    let target = sandbox.validate_write(path).map_err(|e| e.to_string())?;

    // Ensure parent directory exists
    let parent = target
        .parent()
        .ok_or_else(|| format!("cannot determine parent directory: {path}"))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("cannot create parent directory: {e}"))?;

    let bytes = content.len();

    // Atomic write: create temp file in the same (validated) directory, then persist.
    // parent is derived from validate_write() output, so it is within the sandbox.
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("temp file creation failed: {path} ({e})"))?;

    use std::io::Write;
    temp.write_all(content.as_bytes())
        .map_err(|e| format!("write failed: {path} ({e})"))?;

    temp.persist(&target)
        .map_err(|e| format!("rename failed: {path} ({e})"))?;

    Ok(bytes)
}

/// Represents a single grep match.
#[derive(Debug)]
struct GrepMatch {
    line_number: usize,
    line: String,
}

/// Maximum directory recursion depth for grep.
const MAX_GREP_DEPTH: usize = 32;

/// Maximum number of grep matches to collect.
const MAX_GREP_MATCHES: usize = 10_000;

/// Searches a file (or directory recursively) for a regex pattern.
///
/// When searching a directory, uses `sandbox.root()` as the symlink
/// boundary to prevent recursive traversal from escaping the sandbox.
/// Recursion is limited to [`MAX_GREP_DEPTH`] levels and results are
/// capped at [`MAX_GREP_MATCHES`] entries.
fn tool_grep(
    pattern: &str,
    path: &str,
    sandbox: &dyn SandboxPolicy,
) -> Result<Vec<GrepMatch>, String> {
    let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {pattern} ({e})"))?;

    let canonical = sandbox.validate_read(path).map_err(|e| e.to_string())?;
    let mut matches = Vec::new();

    let sandbox_root = sandbox.root();
    if canonical.is_file() {
        grep_file(&re, &canonical, &mut matches)?;
    } else if canonical.is_dir() {
        grep_dir(&re, &canonical, sandbox_root, &mut matches, 0)?;
    } else {
        return Err(format!("not a file or directory: {path}"));
    }

    Ok(matches)
}

fn grep_file(re: &regex::Regex, path: &Path, matches: &mut Vec<GrepMatch>) -> Result<(), String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read failed: {:?} ({e})", path))?;

    for (i, line) in content.lines().enumerate() {
        if matches.len() >= MAX_GREP_MATCHES {
            break;
        }
        if re.is_match(line) {
            matches.push(GrepMatch {
                line_number: i + 1,
                line: line.to_string(),
            });
        }
    }

    Ok(())
}

/// Recursively greps a directory for regex matches.
///
/// `sandbox_root` is used as the symlink boundary: any path that
/// canonicalizes outside `sandbox_root` is silently skipped.
/// Binary files (detected by null bytes in the first 512 bytes) are also skipped.
///
/// Recursion is bounded by `depth` (max [`MAX_GREP_DEPTH`]) and total
/// matches are capped at [`MAX_GREP_MATCHES`].
fn grep_dir(
    re: &regex::Regex,
    dir: &Path,
    sandbox_root: &Path,
    matches: &mut Vec<GrepMatch>,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_GREP_DEPTH {
        tracing::debug!("grep: max depth ({MAX_GREP_DEPTH}) reached at {:?}", dir);
        return Ok(());
    }
    if matches.len() >= MAX_GREP_MATCHES {
        return Ok(());
    }

    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read directory: {:?} ({e})", dir))?;

    for entry in entries.flatten() {
        if matches.len() >= MAX_GREP_MATCHES {
            break;
        }

        let path = entry.path();

        // Symlink guard: canonicalize and verify still within sandbox
        let canonical = match path.canonicalize() {
            Ok(c) if c.starts_with(sandbox_root) => c,
            _ => continue, // outside sandbox or broken symlink — skip
        };

        if canonical.is_file() {
            // Skip binary files (best-effort: check for null bytes in first 512 bytes)
            let is_binary = {
                use std::io::Read;
                match std::fs::File::open(&canonical) {
                    Ok(mut file) => {
                        let mut buf = [0u8; 512];
                        match file.read(&mut buf) {
                            Ok(n) => buf[..n].contains(&0),
                            Err(_) => true, // read failure → skip
                        }
                    }
                    Err(_) => true, // open failure → skip
                }
            };
            if is_binary {
                continue;
            }
            if let Err(e) = grep_file(re, &canonical, matches) {
                tracing::debug!("grep: skip {:?}: {e}", canonical);
            }
        } else if canonical.is_dir() {
            if let Err(e) = grep_dir(re, &canonical, sandbox_root, matches, depth + 1) {
                tracing::debug!("grep: skip dir {:?}: {e}", canonical);
            }
        }
    }

    Ok(())
}

/// Finds files matching a glob pattern.
///
/// Rejects patterns containing `..` to prevent scanning outside the sandbox
/// (even though results are filtered, directory traversal is observable via timing).
fn tool_glob(
    pattern: &str,
    dir: Option<&str>,
    sandbox: &dyn SandboxPolicy,
) -> Result<Vec<String>, String> {
    // Reject path traversal in glob patterns
    if pattern.contains("..") {
        return Err("glob pattern must not contain '..'".to_string());
    }

    let full_pattern = match dir {
        Some(d) => {
            let base = sandbox.validate_read(d).map_err(|e| e.to_string())?;
            if !base.is_dir() {
                return Err(format!("not a directory: {d}"));
            }
            format!("{}/{pattern}", base.display())
        }
        None => {
            format!("{}/{pattern}", sandbox.root().display())
        }
    };

    let paths =
        glob::glob(&full_pattern).map_err(|e| format!("invalid glob pattern: {pattern} ({e})"))?;

    let sandbox_root = sandbox.root();
    let mut results = Vec::new();
    for entry in paths.flatten() {
        // Symlink guard: canonicalize and verify still within sandbox
        if let Ok(canonical) = entry.canonicalize() {
            if canonical.starts_with(sandbox_root) {
                results.push(canonical.display().to_string());
            }
        }
    }

    results.sort();
    Ok(results)
}

/// Creates a directory (and all parents) under the sandbox.
///
/// The path is validated via `sandbox.validate_write()` before creation.
fn tool_mkdir(path: &str, sandbox: &dyn SandboxPolicy) -> Result<(), String> {
    let target = sandbox.validate_write(path).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&target).map_err(|e| format!("mkdir failed: {path} ({e})"))
}

/// Removes a file or directory under the sandbox.
///
/// Uses `remove_file` for files and `remove_dir_all` for directories.
/// Validated via `validate_write()` (destructive) + `validate_read()` (symlink resolution).
fn tool_remove(path: &str, sandbox: &dyn SandboxPolicy) -> Result<(), String> {
    // Destructive operation: check write boundary
    sandbox.validate_write(path).map_err(|e| e.to_string())?;
    // Canonicalize + existence check via validate_read
    let canonical = sandbox.validate_read(path).map_err(|e| e.to_string())?;

    if canonical.is_file() {
        std::fs::remove_file(&canonical).map_err(|e| format!("remove failed: {path} ({e})"))
    } else if canonical.is_dir() {
        std::fs::remove_dir_all(&canonical).map_err(|e| format!("remove failed: {path} ({e})"))
    } else {
        Err(format!("not found: {path}"))
    }
}

/// Moves (renames) a file or directory within the sandbox.
///
/// Both source and destination are validated through the sandbox.
/// Creates destination parent directories if they don't exist.
fn tool_mv(src: &str, dst: &str, sandbox: &dyn SandboxPolicy) -> Result<(), String> {
    let src_canonical = sandbox.validate_read(src).map_err(|e| e.to_string())?;
    let dst_target = sandbox.validate_write(dst).map_err(|e| e.to_string())?;

    // Ensure destination parent exists
    if let Some(parent) = dst_target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create parent directory: {e}"))?;
    }

    std::fs::rename(&src_canonical, &dst_target)
        .map_err(|e| format!("mv failed: {src} -> {dst} ({e})"))
}

// ─── Lua Registration ───────────────────────────────────────────────────

/// Registers all tool functions in the Lua `orcs` table.
///
/// The sandbox controls which paths are accessible. All file operations
/// validate paths through the sandbox before any I/O.
///
/// Adds: `orcs.read`, `orcs.write`, `orcs.grep`, `orcs.glob`, `orcs.mkdir`, `orcs.remove`, `orcs.mv`
///
/// # Errors
///
/// Returns error if function registration fails.
pub fn register_tool_functions(lua: &Lua, sandbox: Arc<dyn SandboxPolicy>) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;

    // orcs.read(path) -> { ok, content, size, error }
    let sb = Arc::clone(&sandbox);
    let read_fn = lua.create_function(move |lua, path: String| {
        let result = lua.create_table()?;
        match tool_read(&path, sb.as_ref()) {
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
    let sb = Arc::clone(&sandbox);
    let write_fn = lua.create_function(move |lua, (path, content): (String, String)| {
        let result = lua.create_table()?;
        match tool_write(&path, &content, sb.as_ref()) {
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
    let sb = Arc::clone(&sandbox);
    let grep_fn = lua.create_function(move |lua, (pattern, path): (String, String)| {
        let result = lua.create_table()?;
        match tool_grep(&pattern, &path, sb.as_ref()) {
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
    let sb = Arc::clone(&sandbox);
    let glob_fn = lua.create_function(move |lua, (pattern, dir): (String, Option<String>)| {
        let result = lua.create_table()?;
        match tool_glob(&pattern, dir.as_deref(), sb.as_ref()) {
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

    // orcs.mkdir(path) -> { ok, error }
    let sb = Arc::clone(&sandbox);
    let mkdir_fn = lua.create_function(move |lua, path: String| {
        let result = lua.create_table()?;
        match tool_mkdir(&path, sb.as_ref()) {
            Ok(()) => result.set("ok", true)?,
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e)?;
            }
        }
        Ok(result)
    })?;
    orcs_table.set("mkdir", mkdir_fn)?;

    // orcs.remove(path) -> { ok, error }
    let sb = Arc::clone(&sandbox);
    let remove_fn = lua.create_function(move |lua, path: String| {
        let result = lua.create_table()?;
        match tool_remove(&path, sb.as_ref()) {
            Ok(()) => result.set("ok", true)?,
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e)?;
            }
        }
        Ok(result)
    })?;
    orcs_table.set("remove", remove_fn)?;

    // orcs.mv(src, dst) -> { ok, error }
    let sb = Arc::clone(&sandbox);
    let mv_fn = lua.create_function(move |lua, (src, dst): (String, String)| {
        let result = lua.create_table()?;
        match tool_mv(&src, &dst, sb.as_ref()) {
            Ok(()) => result.set("ok", true)?,
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e)?;
            }
        }
        Ok(result)
    })?;
    orcs_table.set("mv", mv_fn)?;

    tracing::debug!(
        "Registered orcs tool functions: read, write, grep, glob, mkdir, remove, mv (sandbox_root={})",
        sandbox.root().display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;
    use std::fs;
    use std::path::PathBuf;

    /// Creates a ProjectSandbox backed by a temp dir.
    fn test_sandbox() -> (PathBuf, Arc<dyn SandboxPolicy>) {
        let dir = tempdir();
        let sandbox = ProjectSandbox::new(&dir).unwrap();
        (dir, Arc::new(sandbox))
    }

    // ─── tool_read ──────────────────────────────────────────────────

    #[test]
    fn read_existing_file() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("test.txt");
        fs::write(&file, "hello world").unwrap();

        let (content, size) = tool_read(file.to_str().unwrap(), sandbox.as_ref()).unwrap();
        assert_eq!(content, "hello world");
        assert_eq!(size, 11);
    }

    #[test]
    fn read_nonexistent_file() {
        let (_root, sandbox) = test_sandbox();
        let result = tool_read("nonexistent.txt", sandbox.as_ref());
        assert!(result.is_err());
    }

    #[test]
    fn read_directory_fails() {
        let (root, sandbox) = test_sandbox();
        let sub = root.join("subdir");
        fs::create_dir_all(&sub).unwrap();

        let result = tool_read(sub.to_str().unwrap(), sandbox.as_ref());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a file"));
    }

    #[test]
    fn read_outside_root_rejected() {
        let (_root, sandbox) = test_sandbox();
        let result = tool_read("/etc/hosts", sandbox.as_ref());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("access denied"));
    }

    // ─── tool_write ─────────────────────────────────────────────────

    #[test]
    fn write_new_file() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("new.txt");

        let bytes = tool_write(file.to_str().unwrap(), "new content", sandbox.as_ref()).unwrap();
        assert_eq!(bytes, 11);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new content");
    }

    #[test]
    fn write_overwrites_existing() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("existing.txt");
        fs::write(&file, "old").unwrap();

        tool_write(file.to_str().unwrap(), "new", sandbox.as_ref()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
    }

    #[test]
    fn write_creates_parent_dirs() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("sub/dir/file.txt");

        tool_write(file.to_str().unwrap(), "nested", sandbox.as_ref()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "nested");
    }

    #[test]
    fn write_atomic_no_temp_leftover() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("atomic.txt");

        tool_write(file.to_str().unwrap(), "content", sandbox.as_ref()).unwrap();

        // Temp file should not exist after successful write
        let temp = file.with_extension("tmp.orcs");
        assert!(!temp.exists());
    }

    #[test]
    fn write_outside_root_rejected() {
        let (_root, sandbox) = test_sandbox();
        let result = tool_write("/etc/evil.txt", "bad", sandbox.as_ref());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("access denied"));
    }

    // ─── tool_grep ──────────────────────────────────────────────────

    #[test]
    fn grep_finds_matches() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("search.txt");
        fs::write(&file, "line one\nline two\nthird line").unwrap();

        let matches = tool_grep("line", file.to_str().unwrap(), sandbox.as_ref()).unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].line_number, 1);
        assert_eq!(matches[0].line, "line one");
    }

    #[test]
    fn grep_regex_pattern() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("regex.txt");
        fs::write(&file, "foo123\nbar456\nfoo789").unwrap();

        let matches = tool_grep(r"foo\d+", file.to_str().unwrap(), sandbox.as_ref()).unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn grep_no_matches() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("empty.txt");
        fs::write(&file, "nothing here").unwrap();

        let matches = tool_grep("nonexistent", file.to_str().unwrap(), sandbox.as_ref()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn grep_invalid_regex() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("test.txt");
        fs::write(&file, "content").unwrap();

        let result = tool_grep("[invalid", file.to_str().unwrap(), sandbox.as_ref());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid regex"));
    }

    #[test]
    fn grep_directory_recursive() {
        let (root, sandbox) = test_sandbox();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();

        fs::write(root.join("a.txt"), "target line\nother").unwrap();
        fs::write(sub.join("b.txt"), "no match\ntarget here").unwrap();

        let matches = tool_grep("target", root.to_str().unwrap(), sandbox.as_ref()).unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn grep_outside_root_rejected() {
        let (_root, sandbox) = test_sandbox();
        let result = tool_grep("pattern", "/etc", sandbox.as_ref());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("access denied"));
    }

    // ─── tool_glob ──────────────────────────────────────────────────

    #[test]
    fn glob_finds_files() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("a.txt"), "").unwrap();
        fs::write(root.join("b.txt"), "").unwrap();
        fs::write(root.join("c.rs"), "").unwrap();

        let files = tool_glob("*.txt", Some(root.to_str().unwrap()), sandbox.as_ref()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_recursive() {
        let (root, sandbox) = test_sandbox();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join("top.rs"), "").unwrap();
        fs::write(sub.join("nested.rs"), "").unwrap();

        let files = tool_glob("**/*.rs", Some(root.to_str().unwrap()), sandbox.as_ref()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_no_matches() {
        let (root, sandbox) = test_sandbox();
        let files = tool_glob("*.xyz", Some(root.to_str().unwrap()), sandbox.as_ref()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn glob_invalid_pattern() {
        let (root, sandbox) = test_sandbox();
        let result = tool_glob("[invalid", Some(root.to_str().unwrap()), sandbox.as_ref());
        assert!(result.is_err());
    }

    #[test]
    fn glob_outside_root_rejected() {
        let (_root, sandbox) = test_sandbox();
        let result = tool_glob("*", Some("/etc"), sandbox.as_ref());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("access denied"));
    }

    #[test]
    fn glob_rejects_dotdot_in_pattern() {
        let (_root, sandbox) = test_sandbox();
        let result = tool_glob("../../**/*", None, sandbox.as_ref());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("'..'"),
            "expected dotdot rejection"
        );
    }

    // ─── tool_grep limits ───────────────────────────────────────────

    #[test]
    fn grep_respects_depth_limit() {
        let (root, sandbox) = test_sandbox();

        // Create a directory deeper than MAX_GREP_DEPTH
        let mut deep = root.clone();
        for i in 0..35 {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.txt"), "needle").unwrap();

        // Also create a shallow file
        fs::write(root.join("shallow.txt"), "needle").unwrap();

        let matches = tool_grep("needle", root.to_str().unwrap(), sandbox.as_ref()).unwrap();
        // Shallow file should be found, deep file should be skipped
        assert_eq!(matches.len(), 1);
    }

    // ─── Lua Integration ────────────────────────────────────────────

    #[test]
    fn register_tools_in_lua() {
        let (_root, sandbox) = test_sandbox();
        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();

        register_tool_functions(&lua, sandbox).unwrap();

        let orcs: Table = lua.globals().get("orcs").unwrap();
        assert!(orcs.get::<mlua::Function>("read").is_ok());
        assert!(orcs.get::<mlua::Function>("write").is_ok());
        assert!(orcs.get::<mlua::Function>("grep").is_ok());
        assert!(orcs.get::<mlua::Function>("glob").is_ok());
    }

    #[test]
    fn lua_read_file() {
        let (root, sandbox) = test_sandbox();
        let file = root.join("lua_read.txt");
        fs::write(&file, "lua content").unwrap();

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua, sandbox).unwrap();

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
        let (root, sandbox) = test_sandbox();
        let file = root.join("lua_write.txt");

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua, sandbox).unwrap();

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
        let (root, sandbox) = test_sandbox();
        let file = root.join("lua_grep.txt");
        fs::write(&file, "alpha\nbeta\nalpha_two").unwrap();

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua, sandbox).unwrap();

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
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("a.lua"), "").unwrap();
        fs::write(root.join("b.lua"), "").unwrap();

        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua, sandbox).unwrap();

        let code = format!(
            r#"return orcs.glob("*.lua", "{}")"#,
            root.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().unwrap();
        assert!(result.get::<bool>("ok").unwrap());
        assert_eq!(result.get::<usize>("count").unwrap(), 2);
    }

    #[test]
    fn lua_read_nonexistent_returns_error() {
        let (_root, sandbox) = test_sandbox();
        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua, sandbox).unwrap();

        let result: Table = lua
            .load(r#"return orcs.read("nonexistent_file_xyz.txt")"#)
            .eval()
            .unwrap();
        assert!(!result.get::<bool>("ok").unwrap());
        assert!(result.get::<String>("error").is_ok());
    }

    #[test]
    fn lua_read_outside_sandbox_returns_error() {
        let (_root, sandbox) = test_sandbox();
        let lua = Lua::new();
        let orcs = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs).unwrap();
        register_tool_functions(&lua, sandbox).unwrap();

        let result: Table = lua
            .load(r#"return orcs.read("/etc/hosts")"#)
            .eval()
            .unwrap();
        assert!(!result.get::<bool>("ok").unwrap());
        let error = result.get::<String>("error").unwrap();
        assert!(
            error.contains("access denied"),
            "expected 'access denied', got: {error}"
        );
    }

    // ─── Symlink Attack Tests ──────────────────────────────────────

    #[cfg(unix)]
    mod symlink_tests {
        use super::*;
        use std::os::unix::fs::symlink;

        #[test]
        fn glob_skips_symlink_outside_sandbox() {
            let (root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().unwrap();
            let outside_canon = outside.path().canonicalize().unwrap();
            fs::write(outside_canon.join("leaked.txt"), "secret").unwrap();
            symlink(&outside_canon, root.join("escape")).unwrap();
            fs::write(root.join("ok.txt"), "safe").unwrap();

            let files = tool_glob("**/*.txt", None, sandbox.as_ref()).unwrap();
            for f in &files {
                assert!(!f.contains("leaked"), "leaked file found: {f}");
            }
            assert_eq!(files.len(), 1, "only ok.txt should be found");
        }

        #[test]
        fn grep_dir_skips_symlink_outside_sandbox() {
            let (root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().unwrap();
            let outside_canon = outside.path().canonicalize().unwrap();
            fs::write(outside_canon.join("secret.txt"), "password123").unwrap();
            symlink(&outside_canon, root.join("escape")).unwrap();
            fs::write(root.join("ok.txt"), "password123").unwrap();

            let matches = tool_grep("password", root.to_str().unwrap(), sandbox.as_ref()).unwrap();
            // Only sandbox-internal ok.txt should match
            assert_eq!(matches.len(), 1, "symlinked outside file should be skipped");
        }

        #[test]
        fn write_via_symlink_escape_rejected() {
            let (root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().unwrap();
            let outside_canon = outside.path().canonicalize().unwrap();
            symlink(&outside_canon, root.join("escape")).unwrap();

            let result = tool_write(
                root.join("escape/evil.txt").to_str().unwrap(),
                "evil",
                sandbox.as_ref(),
            );
            assert!(
                result.is_err(),
                "write via symlink escape should be rejected"
            );
        }

        #[test]
        fn read_via_symlink_escape_rejected() {
            let (root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().unwrap();
            let outside_canon = outside.path().canonicalize().unwrap();
            fs::write(outside_canon.join("secret.txt"), "secret").unwrap();
            symlink(&outside_canon, root.join("escape")).unwrap();

            let result = tool_read(
                root.join("escape/secret.txt").to_str().unwrap(),
                sandbox.as_ref(),
            );
            assert!(
                result.is_err(),
                "read via symlink escape should be rejected"
            );
        }
    }

    // ─── Test Helpers ───────────────────────────────────────────────

    /// Creates a temp dir for unit tests.
    /// Canonicalized to resolve symlinks (e.g. /tmp -> /private/tmp on macOS).
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
        dir.canonicalize().unwrap()
    }
}

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
use mlua::{Function, Lua, LuaSerdeExt, Table};
use orcs_runtime::sandbox::SandboxPolicy;
use std::path::Path;
use std::sync::Arc;

// ─── Rust Tool Implementations ──────────────────────────────────────────

/// Reads a file and returns its contents.
pub(crate) fn tool_read(path: &str, sandbox: &dyn SandboxPolicy) -> Result<(String, u64), String> {
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
pub(crate) fn tool_write(
    path: &str,
    content: &str,
    sandbox: &dyn SandboxPolicy,
) -> Result<usize, String> {
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
pub(crate) struct GrepMatch {
    pub(crate) line_number: usize,
    pub(crate) line: String,
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
pub(crate) fn tool_grep(
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
pub(crate) fn tool_glob(
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
pub(crate) fn tool_mkdir(path: &str, sandbox: &dyn SandboxPolicy) -> Result<(), String> {
    let target = sandbox.validate_write(path).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&target).map_err(|e| format!("mkdir failed: {path} ({e})"))
}

/// Removes a file or directory under the sandbox.
///
/// Uses `remove_file` for files and `remove_dir_all` for directories.
/// Validated via `validate_write()` (destructive) + `validate_read()` (symlink resolution).
pub(crate) fn tool_remove(path: &str, sandbox: &dyn SandboxPolicy) -> Result<(), String> {
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
pub(crate) fn tool_mv(src: &str, dst: &str, sandbox: &dyn SandboxPolicy) -> Result<(), String> {
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

// ─── scan_dir ──────────────────────────────────────────────────────────

/// Entry returned by `tool_scan_dir`.
pub(crate) struct ScanEntry {
    pub path: String,
    pub relative: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u64,
}

/// Scans a directory with include/exclude glob patterns.
pub(crate) fn tool_scan_dir(
    path: &str,
    recursive: bool,
    exclude: &[String],
    include: &[String],
    max_depth: Option<usize>,
    sandbox: &dyn SandboxPolicy,
) -> Result<Vec<ScanEntry>, String> {
    let base = sandbox.validate_read(path).map_err(|e| e.to_string())?;

    if !base.is_dir() {
        return Err(format!("not a directory: {path}"));
    }

    let exclude_set = build_glob_set(exclude)?;
    let include_set = if include.is_empty() {
        None
    } else {
        Some(build_glob_set(include)?)
    };

    let mut walker = walkdir::WalkDir::new(&base);
    if !recursive {
        walker = walker.max_depth(1);
    } else if let Some(depth) = max_depth {
        walker = walker.max_depth(depth);
    }

    let mut entries = Vec::new();
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        if entry.path() == base {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(&base)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();

        if exclude_set.is_match(&relative) {
            continue;
        }

        let is_dir = entry.file_type().is_dir();

        if !is_dir {
            if let Some(ref inc) = include_set {
                if !inc.is_match(&relative) {
                    continue;
                }
            }
        }

        let metadata = entry.metadata().ok();
        let size = metadata.as_ref().map_or(0, |m| m.len());
        let modified = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());

        entries.push(ScanEntry {
            path: entry.path().to_string_lossy().to_string(),
            relative,
            is_dir,
            size,
            modified,
        });
    }

    Ok(entries)
}

fn build_glob_set(patterns: &[String]) -> Result<globset::GlobSet, String> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            globset::Glob::new(pattern).map_err(|e| format!("invalid glob '{pattern}': {e}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| format!("glob set build error: {e}"))
}

// ─── parse_frontmatter ─────────────────────────────────────────────────

/// Result of frontmatter parsing.
pub(crate) struct FrontmatterResult {
    pub frontmatter: Option<serde_json::Value>,
    pub body: String,
    pub format: Option<String>,
}

/// Parses frontmatter from string content.
///
/// Supports `---` (YAML) and `+++` (TOML) delimiters.
pub(crate) fn tool_parse_frontmatter_str(content: &str) -> Result<FrontmatterResult, String> {
    let trimmed = content.trim_start();

    if let Some(rest) = trimmed.strip_prefix("---") {
        // YAML frontmatter: find closing "---" on its own line
        if let Some(end_idx) = rest.find("\n---") {
            let yaml_str = &rest[..end_idx];
            let body_start = end_idx + 4; // skip "\n---"
            let body = rest[body_start..].trim_start_matches('\n').to_string();

            let value: serde_json::Value =
                serde_yaml::from_str(yaml_str).map_err(|e| format!("YAML parse error: {e}"))?;

            Ok(FrontmatterResult {
                frontmatter: Some(value),
                body,
                format: Some("yaml".to_string()),
            })
        } else {
            Ok(FrontmatterResult {
                frontmatter: None,
                body: content.to_string(),
                format: None,
            })
        }
    } else if let Some(rest) = trimmed.strip_prefix("+++") {
        // TOML frontmatter
        if let Some(end_idx) = rest.find("\n+++") {
            let toml_str = &rest[..end_idx];
            let body_start = end_idx + 4;
            let body = rest[body_start..].trim_start_matches('\n').to_string();

            let toml_value: toml::Value = toml_str
                .parse()
                .map_err(|e| format!("TOML parse error: {e}"))?;
            let json_value = toml_to_json(toml_value);

            Ok(FrontmatterResult {
                frontmatter: Some(json_value),
                body,
                format: Some("toml".to_string()),
            })
        } else {
            Ok(FrontmatterResult {
                frontmatter: None,
                body: content.to_string(),
                format: None,
            })
        }
    } else {
        Ok(FrontmatterResult {
            frontmatter: None,
            body: content.to_string(),
            format: None,
        })
    }
}

/// Parses frontmatter from a file path.
pub(crate) fn tool_parse_frontmatter(
    path: &str,
    sandbox: &dyn SandboxPolicy,
) -> Result<FrontmatterResult, String> {
    let canonical = sandbox.validate_read(path).map_err(|e| e.to_string())?;
    let content =
        std::fs::read_to_string(&canonical).map_err(|e| format!("read failed: {path} ({e})"))?;
    tool_parse_frontmatter_str(&content)
}

// ─── parse_toml ────────────────────────────────────────────────────────

/// Parses a TOML string into a JSON-compatible value.
pub(crate) fn tool_parse_toml(content: &str) -> Result<serde_json::Value, String> {
    let toml_value: toml::Value = content
        .parse()
        .map_err(|e| format!("TOML parse error: {e}"))?;
    Ok(toml_to_json(toml_value))
}

fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(map) => {
            let obj = map.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect();
            serde_json::Value::Object(obj)
        }
    }
}

// ─── glob_match ────────────────────────────────────────────────────────

/// Result of glob matching.
pub(crate) struct GlobMatchResult {
    pub matched: Vec<String>,
    pub unmatched: Vec<String>,
}

/// Matches paths against a set of glob patterns.
pub(crate) fn tool_glob_match(
    patterns: &[String],
    paths: &[String],
) -> Result<GlobMatchResult, String> {
    let glob_set = build_glob_set(patterns)?;

    let mut matched = Vec::new();
    let mut unmatched = Vec::new();

    for path in paths {
        if glob_set.is_match(path) {
            matched.push(path.clone());
        } else {
            unmatched.push(path.clone());
        }
    }

    Ok(GlobMatchResult { matched, unmatched })
}

/// Evaluates Lua source in a sandboxed environment.
///
/// Creates a restricted environment that excludes dangerous globals
/// (`os`, `io`, `debug`, `loadfile`, `dofile`, `require`). Only safe
/// builtins (`table`, `string`, `math`, `pairs`, `ipairs`, `type`,
/// `tostring`, `tonumber`, `select`, `unpack`, `error`, `pcall`, `xpcall`)
/// are available.
///
/// Returns the value produced by the chunk (the last expression / `return`).
pub(crate) fn tool_load_lua(
    lua: &Lua,
    content: &str,
    source_name: &str,
) -> Result<mlua::Value, String> {
    // Build restricted environment
    let env = lua
        .create_table()
        .map_err(|e| format!("env creation failed: {e}"))?;

    let globals = lua.globals();

    // Safe builtins to copy into sandbox
    let safe_globals = [
        "table",
        "string",
        "math",
        "pairs",
        "ipairs",
        "next",
        "type",
        "tostring",
        "tonumber",
        "select",
        "unpack",
        "error",
        "pcall",
        "xpcall",
        "rawget",
        "rawset",
        "rawequal",
        "rawlen",
        "setmetatable",
        "getmetatable",
    ];

    for name in &safe_globals {
        if let Ok(val) = globals.get::<mlua::Value>(*name) {
            env.set(*name, val)
                .map_err(|e| format!("env.{name}: {e}"))?;
        }
    }

    // print → safe (just outputs to tracing)
    let src = source_name.to_string();
    let print_fn = lua
        .create_function(move |_, args: mlua::MultiValue| {
            let parts: Vec<String> = args.iter().map(|v| format!("{v:?}")).collect();
            tracing::info!(source = %src, "[lua-sandbox] {}", parts.join("\t"));
            Ok(())
        })
        .map_err(|e| format!("print fn: {e}"))?;
    env.set("print", print_fn)
        .map_err(|e| format!("env.print: {e}"))?;

    // Load chunk with source name, set env
    let chunk = lua.load(content).set_name(source_name);

    chunk
        .set_environment(env)
        .eval::<mlua::Value>()
        .map_err(|e| format!("{source_name}: {e}"))
}

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

    // orcs.scan_dir(config) -> table[]
    let sb = Arc::clone(&sandbox);
    let scan_dir_fn = lua.create_function(move |lua, config: Table| {
        let path: String = config.get("path")?;
        let recursive: bool = config.get("recursive").unwrap_or(true);
        let max_depth: Option<usize> = config.get("max_depth").ok();

        let exclude: Vec<String> = config
            .get::<Table>("exclude")
            .map(|t| {
                t.sequence_values::<String>()
                    .filter_map(|v| v.ok())
                    .collect()
            })
            .unwrap_or_default();

        let include: Vec<String> = config
            .get::<Table>("include")
            .map(|t| {
                t.sequence_values::<String>()
                    .filter_map(|v| v.ok())
                    .collect()
            })
            .unwrap_or_default();

        match tool_scan_dir(&path, recursive, &exclude, &include, max_depth, sb.as_ref()) {
            Ok(entries) => {
                let result = lua.create_table()?;
                for (i, entry) in entries.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.set("path", entry.path.as_str())?;
                    t.set("relative", entry.relative.as_str())?;
                    t.set("is_dir", entry.is_dir)?;
                    t.set("size", entry.size)?;
                    t.set("modified", entry.modified)?;
                    result.set(i + 1, t)?;
                }
                Ok(result)
            }
            Err(e) => Err(mlua::Error::RuntimeError(e)),
        }
    })?;
    orcs_table.set("scan_dir", scan_dir_fn)?;

    // orcs.parse_frontmatter(path) -> { frontmatter, body, format }
    let sb = Arc::clone(&sandbox);
    let parse_fm_fn =
        lua.create_function(move |lua, path: String| {
            match tool_parse_frontmatter(&path, sb.as_ref()) {
                Ok(result) => frontmatter_result_to_lua(lua, result),
                Err(e) => {
                    let t = lua.create_table()?;
                    t.set("ok", false)?;
                    t.set("error", e)?;
                    Ok(t)
                }
            }
        })?;
    orcs_table.set("parse_frontmatter", parse_fm_fn)?;

    // orcs.parse_frontmatter_str(content) -> { frontmatter, body, format }
    let parse_fm_str_fn = lua.create_function(move |lua, content: String| {
        match tool_parse_frontmatter_str(&content) {
            Ok(result) => frontmatter_result_to_lua(lua, result),
            Err(e) => {
                let t = lua.create_table()?;
                t.set("ok", false)?;
                t.set("error", e)?;
                Ok(t)
            }
        }
    })?;
    orcs_table.set("parse_frontmatter_str", parse_fm_str_fn)?;

    // orcs.parse_toml(str) -> table
    let parse_toml_fn =
        lua.create_function(
            move |lua, content: String| match tool_parse_toml(&content) {
                Ok(value) => lua.to_value(&value).map_err(|e| {
                    mlua::Error::RuntimeError(format!("TOML to Lua conversion failed: {e}"))
                }),
                Err(e) => Err(mlua::Error::RuntimeError(e)),
            },
        )?;
    orcs_table.set("parse_toml", parse_toml_fn)?;

    // orcs.glob_match(patterns, paths) -> { matched[], unmatched[] }
    let glob_match_fn =
        lua.create_function(move |lua, (patterns_tbl, paths_tbl): (Table, Table)| {
            let patterns: Vec<String> = patterns_tbl
                .sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect();
            let paths: Vec<String> = paths_tbl
                .sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect();

            match tool_glob_match(&patterns, &paths) {
                Ok(result) => {
                    let t = lua.create_table()?;

                    let matched = lua.create_table()?;
                    for (i, m) in result.matched.iter().enumerate() {
                        matched.set(i + 1, m.as_str())?;
                    }
                    t.set("matched", matched)?;

                    let unmatched = lua.create_table()?;
                    for (i, u) in result.unmatched.iter().enumerate() {
                        unmatched.set(i + 1, u.as_str())?;
                    }
                    t.set("unmatched", unmatched)?;

                    Ok(t)
                }
                Err(e) => Err(mlua::Error::RuntimeError(e)),
            }
        })?;
    orcs_table.set("glob_match", glob_match_fn)?;

    // orcs.load_lua(content, source_name?) -> value
    let load_lua_fn = lua.create_function(
        move |lua, (content, source_name): (String, Option<String>)| {
            let name = source_name.as_deref().unwrap_or("(eval)");
            tool_load_lua(lua, &content, name).map_err(mlua::Error::RuntimeError)
        },
    )?;
    orcs_table.set("load_lua", load_lua_fn)?;

    tracing::debug!(
        "Registered orcs tool functions: read, write, grep, glob, mkdir, remove, mv, scan_dir, parse_frontmatter, parse_toml, glob_match, load_lua (sandbox_root={})",
        sandbox.root().display()
    );
    Ok(())
}

/// Converts a FrontmatterResult to a Lua table.
fn frontmatter_result_to_lua(lua: &Lua, result: FrontmatterResult) -> Result<Table, mlua::Error> {
    let t = lua.create_table()?;
    match result.frontmatter {
        Some(fm) => {
            let lua_fm = lua.to_value(&fm)?;
            t.set("frontmatter", lua_fm)?;
        }
        None => t.set("frontmatter", mlua::Value::Nil)?,
    }
    t.set("body", result.body)?;
    match result.format {
        Some(f) => t.set("format", f)?,
        None => t.set("format", mlua::Value::Nil)?,
    }
    Ok(t)
}

// ─── Tool Hook Wrapping ────────────────────────────────────────────

/// Context for tool hook dispatch, stored in Lua `app_data`.
///
/// Set by `LuaComponent::set_child_context()` when a hook registry
/// is available. The wrapper closures (installed by
/// [`wrap_tools_with_hooks`]) check this at call time.
pub(crate) struct ToolHookContext {
    pub(crate) registry: orcs_hook::SharedHookRegistry,
    pub(crate) component_id: orcs_types::ComponentId,
}

/// Names of I/O tools that receive hook wrapping.
const HOOKABLE_TOOLS: &[&str] = &[
    "read",
    "write",
    "grep",
    "glob",
    "mkdir",
    "remove",
    "mv",
    "scan_dir",
    "parse_frontmatter",
];

/// Wraps registered tool functions with `ToolPreExecute`/`ToolPostExecute`
/// hook dispatch.
///
/// For each tool in [`HOOKABLE_TOOLS`]:
/// 1. Saves the original function reference
/// 2. Replaces it with a Lua wrapper that dispatches hooks before/after
///
/// # Pre-hook semantics
///
/// - `Abort { reason }` → returns `{ ok = false, error = "blocked by hook: ..." }`
/// - `Skip(value)` → returns the skip value directly (short-circuit)
/// - `Continue` → proceeds to the original tool
///
/// # Post-hook semantics
///
/// - `Replace(value)` → returns the replacement value
/// - `Continue` → returns the original result unchanged
///
/// # Prerequisites
///
/// Must be called after `register_tool_functions()`.
/// `ToolHookContext` must be set in `lua.set_app_data()` before tools
/// are called.
///
/// # Errors
///
/// Returns error if Lua function creation or table operations fail.
pub(crate) fn wrap_tools_with_hooks(lua: &Lua) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;

    // Register the internal Rust dispatch helper.
    // Lua wrappers call this to dispatch ToolPreExecute / ToolPostExecute.
    let dispatch_fn = lua.create_function(
        |lua, (phase, tool_name, args_val): (String, String, mlua::Value)| {
            // Extract registry + component_id, then drop the Ref
            let (registry, component_id) = {
                let ctx = lua.app_data_ref::<ToolHookContext>();
                let Some(ctx) = ctx else {
                    return Ok(mlua::Value::Nil);
                };
                (
                    std::sync::Arc::clone(&ctx.registry),
                    ctx.component_id.clone(),
                )
            };

            let point = match phase.as_str() {
                "pre" => orcs_hook::HookPoint::ToolPreExecute,
                "post" => orcs_hook::HookPoint::ToolPostExecute,
                _ => return Ok(mlua::Value::Nil),
            };

            let args_json: serde_json::Value =
                lua.from_value(args_val).unwrap_or(serde_json::Value::Null);

            let payload = serde_json::json!({
                "tool": tool_name,
                "args": args_json,
            });

            // For post-hooks, clone the payload to detect Replace
            // (the registry converts Replace → Continue with modified payload)
            let original_payload = if phase == "post" {
                Some(payload.clone())
            } else {
                None
            };

            let hook_ctx = orcs_hook::HookContext::new(
                point,
                component_id.clone(),
                orcs_types::ChannelId::new(),
                orcs_types::Principal::System,
                0,
                payload,
            );

            let action = {
                let guard = registry.read().unwrap_or_else(|poisoned| {
                    tracing::warn!("hook registry lock poisoned, using inner value");
                    poisoned.into_inner()
                });
                guard.dispatch(point, &component_id, None, hook_ctx)
            };

            match action {
                orcs_hook::HookAction::Abort { reason } => {
                    let result = lua.create_table()?;
                    result.set("ok", false)?;
                    result.set("error", format!("blocked by hook: {reason}"))?;
                    Ok(mlua::Value::Table(result))
                }
                orcs_hook::HookAction::Skip(val) | orcs_hook::HookAction::Replace(val) => {
                    lua.to_value(&val)
                }
                orcs_hook::HookAction::Continue(ctx) => {
                    // For post-hooks, the registry converts Replace to
                    // Continue(modified_ctx). Detect this by comparing payloads.
                    if let Some(original) = original_payload {
                        if ctx.payload != original {
                            lua.to_value(&ctx.payload)
                        } else {
                            Ok(mlua::Value::Nil)
                        }
                    } else {
                        Ok(mlua::Value::Nil)
                    }
                }
            }
        },
    )?;
    orcs_table.set("_dispatch_tool_hook", dispatch_fn)?;

    // Wrap each hookable tool with Lua-level pre/post dispatch.
    for &name in HOOKABLE_TOOLS {
        if orcs_table.get::<Function>(name).is_err() {
            continue;
        }

        let wrap_code = format!(
            r#"
            do
                local _orig = orcs.{name}
                orcs.{name} = function(...)
                    local pre = orcs._dispatch_tool_hook("pre", "{name}", {{...}})
                    if pre ~= nil then return pre end
                    local result = _orig(...)
                    local post = orcs._dispatch_tool_hook("post", "{name}", result)
                    if post ~= nil then return post end
                    return result
                end
            end
            "#,
        );

        lua.load(&wrap_code)
            .exec()
            .map_err(|e| LuaError::InvalidScript(format!("failed to wrap tool '{name}': {e}")))?;
    }

    tracing::debug!("Wrapped tools with hook dispatch: {:?}", HOOKABLE_TOOLS);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;
    use std::fs;
    use std::path::PathBuf;

    /// Creates a ProjectSandbox backed by a temp dir.
    /// Returns (TempDir, PathBuf, Sandbox). TempDir must be held alive for the test duration.
    fn test_sandbox() -> (tempfile::TempDir, PathBuf, Arc<dyn SandboxPolicy>) {
        let td = tempfile::tempdir().expect("should create temp directory");
        let root = td
            .path()
            .canonicalize()
            .expect("should canonicalize temp dir path");
        let sandbox = ProjectSandbox::new(&root).expect("should create project sandbox");
        (td, root, Arc::new(sandbox))
    }

    // ─── tool_read ──────────────────────────────────────────────────

    #[test]
    fn read_existing_file() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("test.txt");
        fs::write(&file, "hello world").expect("should write test file");

        let (content, size) = tool_read(
            file.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        )
        .expect("should read existing file");
        assert_eq!(content, "hello world");
        assert_eq!(size, 11);
    }

    #[test]
    fn read_nonexistent_file() {
        let (_td, _root, sandbox) = test_sandbox();
        let result = tool_read("nonexistent.txt", sandbox.as_ref());
        assert!(result.is_err());
    }

    #[test]
    fn read_directory_fails() {
        let (_td, root, sandbox) = test_sandbox();
        let sub = root.join("subdir");
        fs::create_dir_all(&sub).expect("should create subdirectory");

        let result = tool_read(
            sub.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        );
        assert!(result.is_err());
        assert!(result
            .expect_err("should fail for directory")
            .contains("not a file"));
    }

    #[test]
    fn read_outside_root_rejected() {
        let (_td, _root, sandbox) = test_sandbox();
        let result = tool_read("/etc/hosts", sandbox.as_ref());
        assert!(result.is_err());
        assert!(result
            .expect_err("should deny access outside root")
            .contains("access denied"));
    }

    // ─── tool_write ─────────────────────────────────────────────────

    #[test]
    fn write_new_file() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("new.txt");

        let bytes = tool_write(
            file.to_str().expect("path should be valid UTF-8"),
            "new content",
            sandbox.as_ref(),
        )
        .expect("should write new file");
        assert_eq!(bytes, 11);
        assert_eq!(
            fs::read_to_string(&file).expect("should read written file"),
            "new content"
        );
    }

    #[test]
    fn write_overwrites_existing() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("existing.txt");
        fs::write(&file, "old").expect("should write initial file");

        tool_write(
            file.to_str().expect("path should be valid UTF-8"),
            "new",
            sandbox.as_ref(),
        )
        .expect("should overwrite existing file");
        assert_eq!(
            fs::read_to_string(&file).expect("should read overwritten file"),
            "new"
        );
    }

    #[test]
    fn write_creates_parent_dirs() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("sub/dir/file.txt");

        tool_write(
            file.to_str().expect("path should be valid UTF-8"),
            "nested",
            sandbox.as_ref(),
        )
        .expect("should write file with parent dir creation");
        assert_eq!(
            fs::read_to_string(&file).expect("should read nested file"),
            "nested"
        );
    }

    #[test]
    fn write_atomic_no_temp_leftover() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("atomic.txt");

        tool_write(
            file.to_str().expect("path should be valid UTF-8"),
            "content",
            sandbox.as_ref(),
        )
        .expect("should write file atomically");

        // Temp file should not exist after successful write
        let temp = file.with_extension("tmp.orcs");
        assert!(!temp.exists());
    }

    #[test]
    fn write_outside_root_rejected() {
        let (_td, _root, sandbox) = test_sandbox();
        let result = tool_write("/etc/evil.txt", "bad", sandbox.as_ref());
        assert!(result.is_err());
        assert!(result
            .expect_err("should deny write outside root")
            .contains("access denied"));
    }

    // ─── tool_grep ──────────────────────────────────────────────────

    #[test]
    fn grep_finds_matches() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("search.txt");
        fs::write(&file, "line one\nline two\nthird line").expect("should write search file");

        let matches = tool_grep(
            "line",
            file.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        )
        .expect("should find grep matches");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].line_number, 1);
        assert_eq!(matches[0].line, "line one");
    }

    #[test]
    fn grep_regex_pattern() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("regex.txt");
        fs::write(&file, "foo123\nbar456\nfoo789").expect("should write regex test file");

        let matches = tool_grep(
            r"foo\d+",
            file.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        )
        .expect("should find regex matches");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn grep_no_matches() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("empty.txt");
        fs::write(&file, "nothing here").expect("should write test file");

        let matches = tool_grep(
            "nonexistent",
            file.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        )
        .expect("should return empty matches without error");
        assert!(matches.is_empty());
    }

    #[test]
    fn grep_invalid_regex() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("test.txt");
        fs::write(&file, "content").expect("should write test file");

        let result = tool_grep(
            "[invalid",
            file.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        );
        assert!(result.is_err());
        assert!(result
            .expect_err("should fail for invalid regex")
            .contains("invalid regex"));
    }

    #[test]
    fn grep_directory_recursive() {
        let (_td, root, sandbox) = test_sandbox();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("should create subdirectory");

        fs::write(root.join("a.txt"), "target line\nother").expect("should write a.txt");
        fs::write(sub.join("b.txt"), "no match\ntarget here").expect("should write b.txt");

        let matches = tool_grep(
            "target",
            root.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        )
        .expect("should find recursive grep matches");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn grep_outside_root_rejected() {
        let (_td, _root, sandbox) = test_sandbox();
        let result = tool_grep("pattern", "/etc", sandbox.as_ref());
        assert!(result.is_err());
        assert!(result
            .expect_err("should deny grep outside root")
            .contains("access denied"));
    }

    // ─── tool_glob ──────────────────────────────────────────────────

    #[test]
    fn glob_finds_files() {
        let (_td, root, sandbox) = test_sandbox();
        fs::write(root.join("a.txt"), "").expect("should write a.txt");
        fs::write(root.join("b.txt"), "").expect("should write b.txt");
        fs::write(root.join("c.rs"), "").expect("should write c.rs");

        let files = tool_glob(
            "*.txt",
            Some(root.to_str().expect("path should be valid UTF-8")),
            sandbox.as_ref(),
        )
        .expect("should find txt files via glob");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_recursive() {
        let (_td, root, sandbox) = test_sandbox();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("should create subdirectory");
        fs::write(root.join("top.rs"), "").expect("should write top.rs");
        fs::write(sub.join("nested.rs"), "").expect("should write nested.rs");

        let files = tool_glob(
            "**/*.rs",
            Some(root.to_str().expect("path should be valid UTF-8")),
            sandbox.as_ref(),
        )
        .expect("should find rs files recursively");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_no_matches() {
        let (_td, root, sandbox) = test_sandbox();
        let files = tool_glob(
            "*.xyz",
            Some(root.to_str().expect("path should be valid UTF-8")),
            sandbox.as_ref(),
        )
        .expect("should return empty matches for no-match glob");
        assert!(files.is_empty());
    }

    #[test]
    fn glob_invalid_pattern() {
        let (_td, root, sandbox) = test_sandbox();
        let result = tool_glob(
            "[invalid",
            Some(root.to_str().expect("path should be valid UTF-8")),
            sandbox.as_ref(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn glob_outside_root_rejected() {
        let (_td, _root, sandbox) = test_sandbox();
        let result = tool_glob("*", Some("/etc"), sandbox.as_ref());
        assert!(result.is_err());
        assert!(result
            .expect_err("should deny glob outside root")
            .contains("access denied"));
    }

    #[test]
    fn glob_rejects_dotdot_in_pattern() {
        let (_td, _root, sandbox) = test_sandbox();
        let result = tool_glob("../../**/*", None, sandbox.as_ref());
        assert!(result.is_err());
        assert!(
            result
                .expect_err("should reject dotdot pattern")
                .contains("'..'"),
            "expected dotdot rejection"
        );
    }

    // ─── tool_grep limits ───────────────────────────────────────────

    #[test]
    fn grep_respects_depth_limit() {
        let (_td, root, sandbox) = test_sandbox();

        // Create a directory deeper than MAX_GREP_DEPTH
        let mut deep = root.clone();
        for i in 0..35 {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).expect("should create deep directory structure");
        fs::write(deep.join("deep.txt"), "needle").expect("should write deep file");

        // Also create a shallow file
        fs::write(root.join("shallow.txt"), "needle").expect("should write shallow file");

        let matches = tool_grep(
            "needle",
            root.to_str().expect("path should be valid UTF-8"),
            sandbox.as_ref(),
        )
        .expect("should grep respecting depth limit");
        // Shallow file should be found, deep file should be skipped
        assert_eq!(matches.len(), 1);
    }

    // ─── Lua Integration ────────────────────────────────────────────

    #[test]
    fn register_tools_in_lua() {
        let (_td, _root, sandbox) = test_sandbox();
        let lua = Lua::new();
        let orcs = lua.create_table().expect("should create orcs table");
        lua.globals()
            .set("orcs", orcs)
            .expect("should set orcs global");

        register_tool_functions(&lua, sandbox).expect("should register tool functions");

        let orcs: Table = lua
            .globals()
            .get("orcs")
            .expect("should get orcs table back");
        assert!(orcs.get::<mlua::Function>("read").is_ok());
        assert!(orcs.get::<mlua::Function>("write").is_ok());
        assert!(orcs.get::<mlua::Function>("grep").is_ok());
        assert!(orcs.get::<mlua::Function>("glob").is_ok());
    }

    #[test]
    fn lua_read_file() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("lua_read.txt");
        fs::write(&file, "lua content").expect("should write lua read test file");

        let lua = Lua::new();
        let orcs = lua.create_table().expect("should create orcs table");
        lua.globals()
            .set("orcs", orcs)
            .expect("should set orcs global");
        register_tool_functions(&lua, sandbox).expect("should register tool functions");

        let code = format!(
            r#"return orcs.read("{}")"#,
            file.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().expect("should eval lua read");
        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content field"),
            "lua content"
        );
        assert_eq!(
            result.get::<u64>("size").expect("should have size field"),
            11
        );
    }

    #[test]
    fn lua_write_file() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("lua_write.txt");

        let lua = Lua::new();
        let orcs = lua.create_table().expect("should create orcs table");
        lua.globals()
            .set("orcs", orcs)
            .expect("should set orcs global");
        register_tool_functions(&lua, sandbox).expect("should register tool functions");

        let code = format!(
            r#"return orcs.write("{}", "written from lua")"#,
            file.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().expect("should eval lua write");
        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            fs::read_to_string(&file).expect("should read lua-written file"),
            "written from lua"
        );
    }

    #[test]
    fn lua_grep_file() {
        let (_td, root, sandbox) = test_sandbox();
        let file = root.join("lua_grep.txt");
        fs::write(&file, "alpha\nbeta\nalpha_two").expect("should write grep test file");

        let lua = Lua::new();
        let orcs = lua.create_table().expect("should create orcs table");
        lua.globals()
            .set("orcs", orcs)
            .expect("should set orcs global");
        register_tool_functions(&lua, sandbox).expect("should register tool functions");

        let code = format!(
            r#"return orcs.grep("alpha", "{}")"#,
            file.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().expect("should eval lua grep");
        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            result
                .get::<usize>("count")
                .expect("should have count field"),
            2
        );
    }

    #[test]
    fn lua_glob_files() {
        let (_td, root, sandbox) = test_sandbox();
        fs::write(root.join("a.lua"), "").expect("should write a.lua");
        fs::write(root.join("b.lua"), "").expect("should write b.lua");

        let lua = Lua::new();
        let orcs = lua.create_table().expect("should create orcs table");
        lua.globals()
            .set("orcs", orcs)
            .expect("should set orcs global");
        register_tool_functions(&lua, sandbox).expect("should register tool functions");

        let code = format!(
            r#"return orcs.glob("*.lua", "{}")"#,
            root.display().to_string().replace('\\', "\\\\")
        );
        let result: Table = lua.load(&code).eval().expect("should eval lua glob");
        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            result
                .get::<usize>("count")
                .expect("should have count field"),
            2
        );
    }

    #[test]
    fn lua_read_nonexistent_returns_error() {
        let (_td, _root, sandbox) = test_sandbox();
        let lua = Lua::new();
        let orcs = lua.create_table().expect("should create orcs table");
        lua.globals()
            .set("orcs", orcs)
            .expect("should set orcs global");
        register_tool_functions(&lua, sandbox).expect("should register tool functions");

        let result: Table = lua
            .load(r#"return orcs.read("nonexistent_file_xyz.txt")"#)
            .eval()
            .expect("should eval lua read for nonexistent file");
        assert!(!result.get::<bool>("ok").expect("should have ok field"));
        assert!(result.get::<String>("error").is_ok());
    }

    #[test]
    fn lua_read_outside_sandbox_returns_error() {
        let (_td, _root, sandbox) = test_sandbox();
        let lua = Lua::new();
        let orcs = lua.create_table().expect("should create orcs table");
        lua.globals()
            .set("orcs", orcs)
            .expect("should set orcs global");
        register_tool_functions(&lua, sandbox).expect("should register tool functions");

        let result: Table = lua
            .load(r#"return orcs.read("/etc/hosts")"#)
            .eval()
            .expect("should eval lua read for outside sandbox");
        assert!(!result.get::<bool>("ok").expect("should have ok field"));
        let error = result
            .get::<String>("error")
            .expect("should have error field");
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
            let (_td, root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().expect("should create outside temp dir");
            let outside_canon = outside
                .path()
                .canonicalize()
                .expect("should canonicalize outside path");
            fs::write(outside_canon.join("leaked.txt"), "secret")
                .expect("should write leaked file");
            symlink(&outside_canon, root.join("escape")).expect("should create escape symlink");
            fs::write(root.join("ok.txt"), "safe").expect("should write ok file");

            let files =
                tool_glob("**/*.txt", None, sandbox.as_ref()).expect("should glob without error");
            for f in &files {
                assert!(!f.contains("leaked"), "leaked file found: {f}");
            }
            assert_eq!(files.len(), 1, "only ok.txt should be found");
        }

        #[test]
        fn grep_dir_skips_symlink_outside_sandbox() {
            let (_td, root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().expect("should create outside temp dir");
            let outside_canon = outside
                .path()
                .canonicalize()
                .expect("should canonicalize outside path");
            fs::write(outside_canon.join("secret.txt"), "password123")
                .expect("should write secret file");
            symlink(&outside_canon, root.join("escape")).expect("should create escape symlink");
            fs::write(root.join("ok.txt"), "password123").expect("should write ok file");

            let matches = tool_grep(
                "password",
                root.to_str().expect("path should be valid UTF-8"),
                sandbox.as_ref(),
            )
            .expect("should grep without error");
            // Only sandbox-internal ok.txt should match
            assert_eq!(matches.len(), 1, "symlinked outside file should be skipped");
        }

        #[test]
        fn write_via_symlink_escape_rejected() {
            let (_td, root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().expect("should create outside temp dir");
            let outside_canon = outside
                .path()
                .canonicalize()
                .expect("should canonicalize outside path");
            symlink(&outside_canon, root.join("escape")).expect("should create escape symlink");

            let result = tool_write(
                root.join("escape/evil.txt")
                    .to_str()
                    .expect("path should be valid UTF-8"),
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
            let (_td, root, sandbox) = test_sandbox();
            let outside = tempfile::tempdir().expect("should create outside temp dir");
            let outside_canon = outside
                .path()
                .canonicalize()
                .expect("should canonicalize outside path");
            fs::write(outside_canon.join("secret.txt"), "secret")
                .expect("should write secret file");
            symlink(&outside_canon, root.join("escape")).expect("should create escape symlink");

            let result = tool_read(
                root.join("escape/secret.txt")
                    .to_str()
                    .expect("path should be valid UTF-8"),
                sandbox.as_ref(),
            );
            assert!(
                result.is_err(),
                "read via symlink escape should be rejected"
            );
        }
    }

    // ─── Tool Hook Tests ────────────────────────────────────────────

    mod tool_hook_tests {
        use super::*;
        use orcs_hook::{HookPoint, HookRegistry};
        use orcs_types::ComponentId;

        fn setup_lua_with_hooks() -> (Lua, orcs_hook::SharedHookRegistry, tempfile::TempDir) {
            let td = tempfile::tempdir().expect("should create temp dir for hooks");
            let root = td
                .path()
                .canonicalize()
                .expect("should canonicalize hook test root");
            let sandbox: Arc<dyn SandboxPolicy> =
                Arc::new(ProjectSandbox::new(&root).expect("should create hook sandbox"));

            let lua = Lua::new();
            let orcs = lua.create_table().expect("should create orcs table");
            lua.globals()
                .set("orcs", orcs)
                .expect("should set orcs global");
            register_tool_functions(&lua, sandbox).expect("should register tool functions");

            let registry = std::sync::Arc::new(std::sync::RwLock::new(HookRegistry::new()));
            let comp_id = ComponentId::builtin("test");

            lua.set_app_data(ToolHookContext {
                registry: std::sync::Arc::clone(&registry),
                component_id: comp_id,
            });

            wrap_tools_with_hooks(&lua).expect("should wrap tools with hooks");

            (lua, registry, td)
        }

        #[test]
        fn dispatch_function_registered() {
            let (lua, _registry, _td) = setup_lua_with_hooks();
            let orcs: Table = lua.globals().get("orcs").expect("should get orcs table");
            assert!(orcs.get::<Function>("_dispatch_tool_hook").is_ok());
        }

        #[test]
        fn tools_work_normally_without_hooks() {
            let (lua, _registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("test.txt"), "hello").expect("should write test file");

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("test.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read without hooks");
            assert!(result.get::<bool>("ok").expect("should have ok field"));
            assert_eq!(
                result
                    .get::<String>("content")
                    .expect("should have content field"),
                "hello"
            );
        }

        #[test]
        fn pre_hook_abort_blocks_read() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("secret.txt"), "top secret").expect("should write secret file");

            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::aborter(
                    "block-read",
                    "*::*",
                    HookPoint::ToolPreExecute,
                    "access denied by policy",
                )));
            }

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("secret.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read with abort hook");
            assert!(!result.get::<bool>("ok").expect("should have ok field"));
            let error = result
                .get::<String>("error")
                .expect("should have error field");
            assert!(
                error.contains("blocked by hook"),
                "expected 'blocked by hook', got: {error}"
            );
            assert!(error.contains("access denied by policy"));
        }

        #[test]
        fn pre_hook_skip_returns_custom_value() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("real.txt"), "real content").expect("should write real file");

            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::skipper(
                    "skip-read",
                    "*::*",
                    HookPoint::ToolPreExecute,
                    serde_json::json!({"ok": true, "content": "cached", "size": 6}),
                )));
            }

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("real.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read with skip hook");
            assert!(result.get::<bool>("ok").expect("should have ok field"));
            assert_eq!(
                result
                    .get::<String>("content")
                    .expect("should have content field"),
                "cached"
            );
        }

        #[test]
        fn pre_hook_continue_allows_tool() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("allowed.txt"), "allowed content")
                .expect("should write allowed file");

            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::pass_through(
                    "pass-read",
                    "*::*",
                    HookPoint::ToolPreExecute,
                )));
            }

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("allowed.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read with continue hook");
            assert!(result.get::<bool>("ok").expect("should have ok field"));
            assert_eq!(
                result
                    .get::<String>("content")
                    .expect("should have content field"),
                "allowed content"
            );
        }

        #[test]
        fn post_hook_replace_changes_result() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("original.txt"), "original").expect("should write original file");

            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::replacer(
                    "replace-result",
                    "*::*",
                    HookPoint::ToolPostExecute,
                    serde_json::json!({"ok": true, "content": "replaced", "size": 8}),
                )));
            }

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("original.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read with replace hook");
            assert!(result.get::<bool>("ok").expect("should have ok field"));
            assert_eq!(
                result
                    .get::<String>("content")
                    .expect("should have content field"),
                "replaced"
            );
        }

        #[test]
        fn post_hook_continue_preserves_result() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("keep.txt"), "keep this").expect("should write keep file");

            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::pass_through(
                    "observe-only",
                    "*::*",
                    HookPoint::ToolPostExecute,
                )));
            }

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("keep.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read with observe hook");
            assert!(result.get::<bool>("ok").expect("should have ok field"));
            assert_eq!(
                result
                    .get::<String>("content")
                    .expect("should have content field"),
                "keep this"
            );
        }

        #[test]
        fn pre_hook_abort_blocks_write() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");

            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::aborter(
                    "block-write",
                    "*::*",
                    HookPoint::ToolPreExecute,
                    "writes disabled",
                )));
            }

            let code = format!(
                r#"return orcs.write("{}", "evil")"#,
                root.join("blocked.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval write with abort hook");
            assert!(!result.get::<bool>("ok").expect("should have ok field"));
            let error = result
                .get::<String>("error")
                .expect("should have error field");
            assert!(error.contains("writes disabled"));

            // Verify file was NOT created
            assert!(!root.join("blocked.txt").exists());
        }

        #[test]
        fn hooks_receive_tool_name_in_payload() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("check.txt"), "data").expect("should write check file");

            // Use a modifier hook that only aborts for "write" tool
            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::modifier(
                    "check-tool",
                    "*::*",
                    HookPoint::ToolPreExecute,
                    |ctx| {
                        // Verify the payload contains the tool name
                        assert!(ctx.payload.get("tool").is_some());
                        assert!(ctx.payload.get("args").is_some());
                    },
                )));
            }

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("check.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read with modifier hook");
            assert!(result.get::<bool>("ok").expect("should have ok field"));
        }

        #[test]
        fn no_context_tools_work_normally() {
            // Setup WITHOUT ToolHookContext in app_data
            let td = tempfile::tempdir().expect("should create temp dir");
            let root = td.path().canonicalize().expect("should canonicalize root");
            let sandbox: Arc<dyn SandboxPolicy> =
                Arc::new(ProjectSandbox::new(&root).expect("should create sandbox"));

            let lua = Lua::new();
            let orcs = lua.create_table().expect("should create orcs table");
            lua.globals()
                .set("orcs", orcs)
                .expect("should set orcs global");
            register_tool_functions(&lua, sandbox).expect("should register tool functions");

            // Wrap but don't set app_data — dispatch should no-op
            wrap_tools_with_hooks(&lua).expect("should wrap tools with hooks");

            fs::write(root.join("nocontext.txt"), "works").expect("should write nocontext file");

            let code = format!(
                r#"return orcs.read("{}")"#,
                root.join("nocontext.txt")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval read without hook context");
            assert!(result.get::<bool>("ok").expect("should have ok field"));
            assert_eq!(
                result
                    .get::<String>("content")
                    .expect("should have content field"),
                "works"
            );
        }

        #[test]
        fn pre_hook_abort_blocks_glob() {
            let (lua, registry, td) = setup_lua_with_hooks();
            let root = td.path().canonicalize().expect("should canonicalize root");
            fs::write(root.join("a.txt"), "").expect("should write test file");

            {
                let mut guard = registry.write().expect("should acquire write lock");
                guard.register(Box::new(orcs_hook::testing::MockHook::aborter(
                    "block-glob",
                    "*::*",
                    HookPoint::ToolPreExecute,
                    "glob not allowed",
                )));
            }

            let code = format!(
                r#"return orcs.glob("*.txt", "{}")"#,
                root.display().to_string().replace('\\', "\\\\")
            );
            let result: Table = lua
                .load(&code)
                .eval()
                .expect("should eval glob with abort hook");
            assert!(!result.get::<bool>("ok").expect("should have ok field"));
            let error = result
                .get::<String>("error")
                .expect("should have error field");
            assert!(error.contains("glob not allowed"));
        }
    }

    // ─── Test Helpers ───────────────────────────────────────────────
}

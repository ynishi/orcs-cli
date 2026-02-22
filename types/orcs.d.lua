---@meta
--- ORCS (Orchestrated Runtime Component System) Lua API
--- Type definitions for editor autocompletion (EmmyLua / lua-language-server).
---
--- Usage: Add this directory to `workspace.library` in .luarc.json:
---   { "workspace": { "library": ["types"] } }

---@class Orcs
orcs = {}

-- ═══════════════════════════════════════════════════════════════════
-- Logging
-- ═══════════════════════════════════════════════════════════════════

--- Log a message at the specified level.
---@param level "debug"|"info"|"warn"|"error"
---@param msg string
function orcs.log(level, msg) end

-- ═══════════════════════════════════════════════════════════════════
-- Shell Execution (Capability: EXECUTE)
-- ═══════════════════════════════════════════════════════════════════

---@class ExecResult
---@field ok boolean
---@field stdout string
---@field stderr string
---@field code integer Exit code (-1 if denied, nil if signal-terminated)
---@field signal_terminated? boolean True if killed by signal

--- Execute a shell command via `sh -c`. cwd = project root.
--- Requires `Capability::EXECUTE` and command permission check.
---@param cmd string Shell command string
---@return ExecResult
function orcs.exec(cmd) end

---@class ExecArgvOpts
---@field env_remove? string[] Environment variables to remove
---@field cwd? string Override working directory

--- Execute a command without shell interpolation.
--- Requires `Capability::EXECUTE` and command permission check.
---@param program string Program path or name
---@param args string[] Array of argument strings
---@param opts? ExecArgvOpts
---@return ExecResult
function orcs.exec_argv(program, args, opts) end

-- ═══════════════════════════════════════════════════════════════════
-- LLM (Capability: LLM)
-- ═══════════════════════════════════════════════════════════════════

---@class LlmOpts
---@field provider? "ollama"|"openai"|"anthropic" Default: "ollama"
---@field base_url? string Provider base URL
---@field model? string Model name
---@field api_key? string API key (falls back to env var)
---@field system_prompt? string System prompt text
---@field session_id? string Session ID for multi-turn (nil = new session)
---@field temperature? number Sampling temperature
---@field max_tokens? integer Max completion tokens
---@field timeout? integer Request timeout in seconds (default: 120)
---@field max_retries? integer Max retry attempts
---@field tools? boolean Send IntentRegistry defs as LLM tools (default: true)
---@field resolve? boolean Auto-dispatch tool_call intents in Rust (default: false)
---@field max_tool_turns? integer Max tool-loop iterations (default: 10, requires resolve=true)

---@alias IntentSource "lua"|"llm_tool_call"|"system"

---@alias Priority "low"|"normal"|"high"|"critical"

---@alias Confidence number Confidence score in [0.0, 1.0]

---@class IntentMeta
---@field source? IntentSource Who issued the intent (default: "lua")
---@field priority? Priority Scheduling priority
---@field expected_latency_ms? integer Estimated execution time in ms (UX feedback)
---@field confidence? Confidence Issuer's confidence (below threshold → Human-in-the-Loop)

---@class ActionIntent
---@field id string Correlation ID (from LLM tool_call id, or auto-generated)
---@field name string Intent/tool name (e.g. "read", "exec", "run_skill")
---@field params table Parameters as key-value table (JSON object)
---@field meta? IntentMeta Execution metadata (priority, confidence, latency hint)

---@class IntentResult
---@field intent_id string Correlation ID (matches ActionIntent.id)
---@field name string Intent name (for diagnostics)
---@field ok boolean Whether execution succeeded
---@field content any Result payload (tool output on success, error detail on failure)
---@field error? string Error message (when ok=false)
---@field duration_ms integer Execution duration in milliseconds

---@class LlmResult
---@field ok boolean
---@field content? string Response text (when ok=true)
---@field model? string Model name from response
---@field session_id? string Session ID (new or existing)
---@field stop_reason? "end_turn"|"tool_use"|"max_tokens" Why the LLM stopped generating
---@field intents? ActionIntent[] Tool-use intents (present when stop_reason="tool_use")
---@field error? string Error message (when ok=false)
---@field error_kind? "permission_denied"|"missing_api_key"|"invalid_options"|"timeout"|"dns"|"connection_refused"|"connection_reset"|"tls"|"too_large"|"network"|"parse_error"|"auth_error"|"not_found"|"rate_limit"|"server_error"|"http_error"|"request_build_error"|"tool_loop_limit"|"internal"

--- Send a chat prompt to an LLM provider.
--- Requires `Capability::LLM`.
---@param prompt string User message text
---@param opts? LlmOpts
---@return LlmResult
function orcs.llm(prompt, opts) end

---@class LlmPingResult
---@field ok boolean
---@field provider? string Provider name
---@field base_url? string Provider base URL
---@field latency_ms? integer Round-trip latency in milliseconds
---@field status? integer HTTP status code
---@field error? string Error message (when ok=false)
---@field error_kind? "permission_denied"|"invalid_options"|"timeout"|"dns"|"connection_refused"|"tls"|"network"

--- Lightweight LLM provider connectivity check (no tokens consumed).
--- Requires `Capability::LLM`.
---@param opts? LlmOpts
---@return LlmPingResult
function orcs.llm_ping(opts) end

--- Export all LLM session history as a JSON string.
---@return string JSON-serialized session map
function orcs.llm_dump_sessions() end

---@class LlmLoadResult
---@field ok boolean
---@field count integer Number of sessions loaded

--- Import LLM session history from a JSON string.
---@param json string JSON-serialized session map
---@return LlmLoadResult
function orcs.llm_load_sessions(json) end

-- ═══════════════════════════════════════════════════════════════════
-- HTTP (Capability: HTTP)
-- ═══════════════════════════════════════════════════════════════════

---@class HttpOpts
---@field headers? table<string, string> Request headers
---@field body? string Request body string
---@field timeout? integer Timeout in seconds (default: 30)

---@class HttpResult
---@field ok boolean
---@field status? integer HTTP status code
---@field headers? table<string, string> Response headers
---@field body? string Response body
---@field error? string Error message (when ok=false)
---@field error_kind? "permission_denied"|"invalid_url"|"invalid_method"|"timeout"|"dns"|"connection_refused"|"connection_reset"|"connection_aborted"|"tls"|"too_large"|"network"

--- Send an HTTP request.
--- Requires `Capability::HTTP`.
---@param method "GET"|"POST"|"PUT"|"DELETE"|"PATCH"|"HEAD"
---@param url string
---@param opts? HttpOpts
---@return HttpResult
function orcs.http(method, url, opts) end

-- ═══════════════════════════════════════════════════════════════════
-- File Operations (sandboxed)
-- ═══════════════════════════════════════════════════════════════════

---@class ReadResult
---@field ok boolean
---@field content? string File contents (when ok=true)
---@field size? integer File size in bytes
---@field error? string Error message (when ok=false)

--- Read file contents. Path is relative to project root.
---@param path string
---@return ReadResult
function orcs.read(path) end

---@class WriteResult
---@field ok boolean
---@field bytes_written? integer Number of bytes written
---@field error? string

--- Write file contents atomically. Creates parent directories.
---@param path string
---@param content string
---@return WriteResult
function orcs.write(path, content) end

---@class GrepMatch
---@field line_number integer 1-based line number
---@field line string Matching line content

---@class GrepResult
---@field ok boolean
---@field matches? GrepMatch[] Array of matches
---@field count? integer Number of matches
---@field error? string

--- Search file or directory contents with a regex pattern.
--- Directory search is recursive (max depth 32, max 10000 matches).
---@param pattern string Regex pattern
---@param path string File or directory path
---@return GrepResult
function orcs.grep(pattern, path) end

---@class GlobResult
---@field ok boolean
---@field files? string[] Array of matching file paths
---@field count? integer Number of matches
---@field error? string

--- Find files by glob pattern.
---@param pattern string Glob pattern (e.g. "**/*.lua")
---@param dir? string Base directory (default: project root)
---@return GlobResult
function orcs.glob(pattern, dir) end

---@class OkErrorResult
---@field ok boolean
---@field error? string

--- Create directory with parents.
---@param path string
---@return OkErrorResult
function orcs.mkdir(path) end

--- Remove file or directory.
---@param path string
---@return OkErrorResult
function orcs.remove(path) end

--- Move / rename file or directory.
---@param src string Source path
---@param dst string Destination path
---@return OkErrorResult
function orcs.mv(src, dst) end

---@class ScanDirConfig
---@field path string Directory path
---@field recursive? boolean Default: true
---@field max_depth? integer Max recursion depth
---@field exclude? string[] Glob patterns to exclude
---@field include? string[] Glob patterns to include (files only)

---@class ScanEntry
---@field path string Absolute path
---@field relative string Path relative to scan root
---@field is_dir boolean
---@field size integer File size in bytes
---@field modified integer Unix timestamp (seconds)

--- Scan a directory with include/exclude glob filters.
--- Raises a Lua error on failure (wrap with `pcall` to handle errors).
---@param config ScanDirConfig
---@return ScanEntry[]
function orcs.scan_dir(config) end

---@class FrontmatterResult
---@field frontmatter? table Parsed frontmatter as table (nil if none)
---@field body string Body content after frontmatter
---@field format? "yaml"|"toml" Frontmatter format (nil if none)
---@field ok? false Only present (and false) on read error; nil on success
---@field error? string Error message (only present when ok=false)

--- Parse YAML/TOML frontmatter from a file.
---@param path string
---@return FrontmatterResult
function orcs.parse_frontmatter(path) end

--- Parse YAML/TOML frontmatter from a string.
---@param content string
---@return FrontmatterResult
function orcs.parse_frontmatter_str(content) end

--- Parse a TOML string into a Lua table.
---@param content string TOML string
---@return table
function orcs.parse_toml(content) end

---@class GlobMatchResult
---@field matched string[] Paths that matched any pattern
---@field unmatched string[] Paths that matched no pattern

--- Match paths against a set of glob patterns.
---@param patterns string[] Glob patterns
---@param paths string[] Paths to test
---@return GlobMatchResult
function orcs.glob_match(patterns, paths) end

--- Evaluate Lua source in a sandboxed environment (no io/os/debug/require).
---@param content string Lua source code
---@param source_name? string Name for error messages (default: "(eval)")
---@return any
function orcs.load_lua(content, source_name) end

-- ═══════════════════════════════════════════════════════════════════
-- Serialization
-- ═══════════════════════════════════════════════════════════════════

--- Parse a JSON string into a Lua value.
---@param str string JSON string
---@return any Lua value (table, string, number, boolean, nil)
function orcs.json_parse(str) end

--- Encode a Lua value as a JSON string.
---@param value any Lua value (table, string, number, boolean)
---@return string
function orcs.json_encode(value) end

--- Parse a TOML string into a Lua table.
---@param str string TOML string
---@return table
function orcs.toml_parse(str) end

--- Encode a Lua table as a TOML string.
---@param value table
---@return string
function orcs.toml_encode(value) end

-- ═══════════════════════════════════════════════════════════════════
-- Input Sanitization
-- ═══════════════════════════════════════════════════════════════════

---@class SanitizeResult
---@field ok boolean
---@field value? string Sanitized value (when ok=true)
---@field error? string Error message (when ok=false)
---@field violations? string[] List of violated rules

--- Validate argument string (rejects control chars, NUL).
---@param s string
---@return SanitizeResult
function orcs.sanitize_arg(s) end

--- Validate relative path (rejects traversal, absolute paths, control chars).
---@param s string
---@return SanitizeResult
function orcs.sanitize_path(s) end

--- Validate with all rules (shell meta, env expansion, glob, traversal, control).
---@param s string
---@return SanitizeResult
function orcs.sanitize_strict(s) end

-- ═══════════════════════════════════════════════════════════════════
-- Git Info
-- ═══════════════════════════════════════════════════════════════════

---@class GitInfoResult
---@field ok boolean False if not in a git repo
---@field branch? string Current branch name
---@field commit_short? string Short commit hash
---@field dirty? boolean True if uncommitted changes exist

--- Get git repository info for the project root.
---@return GitInfoResult
function orcs.git_info() end

-- ═══════════════════════════════════════════════════════════════════
-- Output & Events (requires Emitter)
-- ═══════════════════════════════════════════════════════════════════

--- Emit an output event (displayed to user).
---@param msg string
function orcs.output(msg) end

--- Emit an output event with explicit level.
---@param msg string
---@param level string Output level
function orcs.output_with_level(msg, level) end

--- Broadcast an Extension event via EventBus.
---@param category string Event category
---@param operation string Event operation
---@param payload table Event payload
function orcs.emit_event(category, operation, payload) end

---@class BoardEntry
---@field [string] any Board entry fields (schema varies)

--- Query recent entries from the shared Board.
---@param n integer Number of recent entries
---@return BoardEntry[]
function orcs.board_recent(n) end

-- ═══════════════════════════════════════════════════════════════════
-- Component-to-Component RPC
-- ═══════════════════════════════════════════════════════════════════

---@class RequestOpts
---@field timeout_ms? integer RPC timeout in milliseconds

---@class RequestResult
---@field success boolean
---@field data? any Response data (when success=true)
---@field error? string Error message (when success=false)

--- Send an RPC request to another component.
---@param target string Component FQN (e.g. "builtin::shell")
---@param operation string Operation name
---@param payload any Request payload
---@param opts? RequestOpts
---@return RequestResult
function orcs.request(target, operation, payload, opts) end

---@class BatchRequestEntry
---@field target string Component FQN
---@field operation string Operation name
---@field payload any Request payload
---@field timeout_ms? integer Per-request timeout

--- Send multiple RPC requests in parallel.
---@param requests BatchRequestEntry[]
---@return RequestResult[] Results in same order as requests
function orcs.request_batch(requests) end

-- ═══════════════════════════════════════════════════════════════════
-- Child Process Management (Capability: SPAWN)
-- ═══════════════════════════════════════════════════════════════════

---@class SpawnChildConfig
---@field id string Child identifier
---@field script? string Inline Lua script
---@field path? string Path to Lua script file

---@class SpawnChildResult
---@field ok boolean
---@field id? string Child ID (when ok=true)
---@field error? string

--- Spawn a child component.
--- Requires `Capability::SPAWN`.
---@param config SpawnChildConfig
---@return SpawnChildResult
function orcs.spawn_child(config) end

---@class SpawnRunnerConfig
---@field script string Lua script (required)
---@field id? string Optional runner ID

---@class SpawnRunnerResult
---@field ok boolean
---@field channel_id? string Channel ID (when ok=true)
---@field fqn? string Component FQN for use with orcs.request()
---@field error? string

--- Spawn a ChannelRunner for parallel execution.
--- The returned `fqn` can be used immediately with `orcs.request()`.
--- Requires `Capability::SPAWN`.
---@param config SpawnRunnerConfig
---@return SpawnRunnerResult
function orcs.spawn_runner(config) end

---@class SendToChildResult
---@field ok boolean
---@field result? any Child's response data
---@field error? string

--- Send a message to a child and wait for response.
---@param child_id string
---@param message any
---@return SendToChildResult
function orcs.send_to_child(child_id, message) end

--- Send a message to a child asynchronously (fire-and-forget).
---@param child_id string
---@param message any
---@return OkErrorResult
function orcs.send_to_child_async(child_id, message) end

--- Send messages to multiple children in parallel.
--- Arrays must have same length.
---@param ids string[] Child IDs
---@param inputs any[] Input values
---@return SendToChildResult[] Results in same order
function orcs.send_to_children_batch(ids, inputs) end

--- Get the current number of active children.
---@return integer
function orcs.child_count() end

--- Get the maximum allowed children.
---@return integer
function orcs.max_children() end

-- ═══════════════════════════════════════════════════════════════════
-- Command Permission
-- ═══════════════════════════════════════════════════════════════════

---@class CheckCommandResult
---@field status "allowed"|"denied"|"requires_approval"
---@field reason? string Denial reason (when denied)
---@field grant_pattern? string Pattern to grant (when requires_approval)
---@field description? string Human-readable description

--- Check if a command is permitted without executing it.
---@param cmd string
---@return CheckCommandResult
function orcs.check_command(cmd) end

--- Grant a command pattern for future execution.
---@param pattern string Glob or exact command pattern
function orcs.grant_command(pattern) end

--- Request Human-in-the-Loop approval for an operation.
---@param operation string Operation name
---@param description string Human-readable description
---@return string approval_id Unique approval request ID
function orcs.request_approval(operation, description) end

-- ═══════════════════════════════════════════════════════════════════
-- Hook System
-- ═══════════════════════════════════════════════════════════════════

---@class HookContext
---@field hook_point string
---@field component_id table
---@field channel_id string
---@field principal string
---@field depth integer
---@field max_depth integer
---@field payload table
---@field metadata? table<string, any>

---@class HookActionSkip
---@field action "skip"
---@field result any Value to return instead

---@class HookActionAbort
---@field action "abort"
---@field reason? string Abort reason (default: "aborted by lua hook")

---@class HookActionReplace
---@field action "replace"
---@field result any Replacement value

---@class HookActionContinue
---@field action "continue"
---@field ctx? HookContext Modified context

---@alias HookReturn nil|HookContext|HookActionSkip|HookActionAbort|HookActionReplace|HookActionContinue

---@alias HookHandler fun(ctx: HookContext): HookReturn

--- Register a hook (shorthand: "fql:hook_point", handler).
---@param descriptor string Hook descriptor (e.g. "builtin::llm:request.pre_dispatch")
---@param handler HookHandler
---@overload fun(config: HookTableConfig)
function orcs.hook(descriptor, handler) end

---@class HookTableConfig
---@field fql string FQL pattern (e.g. "builtin::llm", "*::*")
---@field point string Hook point (e.g. "request.pre_dispatch")
---@field handler HookHandler
---@field priority? integer Hook priority (default: 100, lower = earlier)
---@field id? string Hook ID for later removal

--- Remove a previously registered hook by ID.
---@param id string Hook ID
---@return boolean removed True if found and removed
function orcs.unhook(id) end

-- ═══════════════════════════════════════════════════════════════════
-- Tool Dispatch
-- ═══════════════════════════════════════════════════════════════════

---@class ToolArgSchema
---@field name string
---@field type string
---@field required boolean
---@field description string

---@class ToolSchema
---@field name string Tool name
---@field description string Tool description
---@field args ToolArgSchema[]

--- Dispatch a tool call by name with structured arguments.
---@param name string Tool name (e.g. "read", "write", "grep")
---@param args table Tool arguments
---@return table Result from the dispatched tool
function orcs.dispatch(name, args) end

--- Get structured schemas for all registered tools (legacy format).
---@return ToolSchema[]
function orcs.tool_schemas() end

--- Get formatted tool reference text for prompt embedding.
---@return string
function orcs.tool_descriptions() end

---@class IntentDef
---@field name string Intent name
---@field description string Human-readable description
---@field parameters table JSON Schema for parameters (native Lua table)

--- Get all registered intent definitions in JSON Schema format.
--- Returns native Lua tables (no cjson dependency).
---@return IntentDef[]
function orcs.intent_defs() end

---@class RegisterIntentOpts
---@field name string Intent name (must be unique)
---@field description string Human-readable description
---@field component string Component FQN that handles this intent
---@field operation? string Operation name (default: "execute")
---@field params? table<string, {type: string, description: string, required: boolean}> Parameter definitions

--- Register a new intent definition (Component resolver).
---@param def RegisterIntentOpts
---@return OkErrorResult
function orcs.register_intent(def) end

-- ═══════════════════════════════════════════════════════════════════
-- Properties
-- ═══════════════════════════════════════════════════════════════════

--- Project root path (string property, not a function).
---@type string
orcs.pwd = ""

-- ═══════════════════════════════════════════════════════════════════
-- Internal (do not call directly)
-- ═══════════════════════════════════════════════════════════════════

--- Internal hook dispatch helper for tool pre/post hooks.
--- Installed by `wrap_tools_with_hooks()`. Do not call directly.
---@private
---@param phase "pre"|"post"
---@param tool_name string
---@param args any
---@return any
function orcs._dispatch_tool_hook(phase, tool_name, args) end

return orcs

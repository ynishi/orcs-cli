# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-03-02

Agentic collaboration hardening: delegation, supervision, logging, and MCP integration.

### Added

- **Delegate task system**: `orcs.resolve_loop` for provider-agnostic tool resolution, event-driven agent_mgr dispatch (#1)
- **Concierge agent**: LLM tool-use integration with concierge component (#4)
- **RustTool dispatch**: Direct Rust dispatch for file tools bypassing Lua overhead (#3)
- **CommonAgent framework**: `spawn_runner` globals API for cross-VM data injection (#6)
- **Sandbox eval tool**: `lua_eval` for LLM tool_call execution in sandboxed Lua VM (#7)
- **MCP client**: External tool server integration via `orcs-mcp` crate (#9)
- **Runner supervision**: Liveness check API with automatic restart (#10)
- **mlua-lspec test framework**: Lua component unit testing with describe/it/expect (#11, #13)
- **Concierge lspec tests**: 24 it-blocks covering edge cases (#14)
- **Configurable turn budget**: Dynamic escalation, reminders, and MaxTokens recovery (#16)
- **Parallel delegation**: Per-worker spawning with self-termination (#17)
- **Configurable Lua timeouts**: `delegate_timeout_ms`, `concierge_ms`, `overall_timeout` (#20)
- **Supervisor completion**: Erlang-inspired supervision for managed runners (#21)
- **Independent file/terminal logging**: Dual tracing layers with per-target `EnvFilter`, `[logging]` config section, `--log-file` / `--log-level` CLI args (#24)

### Changed

- **LLM provider refactor**: Unified to OpenAICompat with WireFormat abstraction, migrated ureq → reqwest (#5)
- **mlua upgrade**: 0.10 → 0.11 (#11)
- **WorkDir RAII**: Replaced leaking TempDir with auto-cleaning lifecycle management (#8)
- **Non-blocking printer**: Structured log levels for tracing output (#22)

### Fixed

- **IME input**: Multi-character composition stalling on macOS (#2)
- **HIL approval flow**: Suspended as tool_result, intent-level HIL, session consistency (#12)
- **Session resume**: Preserve session history across HIL re-dispatch instead of rollback (#23)
- **Restart race condition**: Prevent dead channel handle accumulation in supervisor (#21)
- **MaxTokens truncation**: Recovery in LLM resolve loop (#16)

### Removed

- Unused `spawn_runner_full` / `spawn_runner_with_emitter` methods (#19)

## [0.1.0] - 2026-02-21

Initial release as a Lua-extensible Agentic Shell skeleton.

### Added

- **Component system**: Lua-based components with `init` / `on_request` / `shutdown` lifecycle
- **7 builtin components**: agent_mgr, skill_manager, profile_manager, foundation_manager, console_metrics, shell, tool
- **Child spawning**: Parent components can spawn Lua children with capability inheritance
- **30+ Lua API functions**: file I/O, exec, LLM, HTTP, JSON/TOML, sanitization, info
- **Multi-provider LLM**: Ollama, OpenAI, Anthropic support via config.toml
- **HTTP client**: `orcs.http()` with ureq blocking client
- **Capability gating**: READ, WRITE, DELETE, EXECUTE, SPAWN, LLM, HTTP -- each API requires explicit grant
- **Sandbox isolation**: Path traversal prevention, symlink attack defense, Lua stdlib restrictions
- **Input sanitization**: `orcs.sanitize_arg/path/strict` with shell injection prevention
- **Config system**: config.toml with per-component settings, global config, LLM provider config
- **Session persistence**: Pause/resume with component state snapshots
- **Hook system**: Lifecycle hooks for pre/post processing
- **Interactive prompt**: rustyline-based with history
- **`--sandbox` flag**: Isolated clean-install testing mode
- **Architecture lint**: orcs-lint with OL002 (unwrap ban) enforcement
- **100% safe Rust**: Zero `unsafe` blocks in production code

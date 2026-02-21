# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

# ORCS CLI

[![CI](https://github.com/ynishi/orcs-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/ynishi/orcs-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/orcs-cli.svg)](https://crates.io/crates/orcs-cli)
[![docs.rs](https://docs.rs/orcs-cli/badge.svg)](https://docs.rs/orcs-cli)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![MSRV: 1.80.0](https://img.shields.io/badge/MSRV-1.80.0-brightgreen.svg)](https://releases.rs/docs/1.80.0/)
[![Rust](https://img.shields.io/badge/rust-%23000000.svg?logo=rust&logoColor=white)](https://www.rust-lang.org/)

Lua-extensible Agentic Shell for building AI-powered development tools.

ORCS (Orchestrated Runtime Component System) provides a component-based architecture where every behavior is implemented as a Lua script, capability-gated, and sandboxed.

Part of the [ORCS](https://github.com/ynishi/orcs) family — a suite of multi-agent AI tools built around orchestrated reasoning and collaboration. See also [ORCS Desktop](https://github.com/ynishi/orcs) for the GUI workspace application.

## Prerequisites

- Rust 1.80.0 or later

## Features

- **Lua-first extensibility** -- All components (agents, skills, tools) are Lua scripts
- **Capability-gated APIs** -- LLM, HTTP, exec, file I/O each require explicit capability grants
- **Sandbox isolation** -- Path traversal prevention, Lua stdlib restrictions, input sanitization
- **Multi-provider LLM** -- Ollama, OpenAI, Anthropic via config.toml
- **ActionIntent system** -- Unified tool dispatch with IntentRegistry, LLM tool_use auto-resolution, and dynamic intent registration
- **Skill recommendation** -- LLM-based or keyword-based skill matching with automatic tool registration
- **Session persistence** -- Pause/resume with component state snapshot
- **Hook system** -- FQL-based lifecycle hooks for pre/post processing
- **Human-in-the-loop** -- Approval workflows for destructive operations

## Install

```bash
cargo install orcs-cli
```

Or from source:

```bash
cargo build --release
# Binary at target/release/orcs
```

## Quick Start

### Run with default builtins

```bash
orcs
```

### Run with verbose output

```bash
orcs --verbose
```

### Run in sandbox mode (isolated environment)

```bash
orcs --sandbox
```

### Resume a previous session

```bash
orcs --resume <SESSION_ID>
```

### Create a Lua component

```lua
-- ~/.orcs/components/hello.lua
return {
    id = "hello",
    namespace = "custom",
    subscriptions = {"Hello"},

    init = function(cfg)
        orcs.log("info", "Hello component initialized")
    end,

    on_request = function(request)
        if request.operation == "greet" then
            local name = request.payload or "world"
            return { success = true, data = "Hello, " .. name .. "!" }
        end
        return { success = false, error = "unknown operation" }
    end,

    on_signal = function(sig)
        return "Handled"
    end,

    shutdown = function()
        orcs.log("info", "Goodbye")
    end,
}
```

Add it to `config.toml`:

```toml
[components]
load = ["agent_mgr", "skill_manager", "hello"]
```

## CLI

```
orcs [OPTIONS] [COMMAND]...
```

| Flag | Description |
|------|-------------|
| `-d`, `--debug` | Enable debug logging |
| `-v`, `--verbose` | Verbose output |
| `-C`, `--project <PATH>` | Project root directory |
| `--resume <ID>` | Resume an existing session |
| `--profile <NAME>` | Profile name (`ORCS_PROFILE`) |
| `--experimental` | Enable experimental components (`ORCS_EXPERIMENTAL`) |
| `--sandbox [DIR]` | Sandbox mode (optional dir, defaults to tempdir) |
| `--builtins-dir <PATH>` | Override builtins directory (`ORCS_BUILTINS_DIR`) |
| `--install-builtins` | Install/update builtin components and exit |

## Lua API

### File I/O (Sandbox-gated)

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.read(path)` | `{ok, content, size}` | Read file contents |
| `orcs.write(path, content)` | `{ok, bytes_written}` | Write file (atomic) |
| `orcs.grep(pattern, path)` | `{ok, matches[], count}` | Regex search |
| `orcs.glob(pattern, dir?)` | `{ok, files[], count}` | Glob pattern search |
| `orcs.mkdir(path)` | `{ok}` | Create directory with parents |
| `orcs.remove(path)` | `{ok}` | Remove file or directory |
| `orcs.mv(src, dst)` | `{ok}` | Move / rename |
| `orcs.scan_dir(config)` | table[] | Directory scan with include/exclude |
| `orcs.parse_frontmatter(path)` | `{frontmatter, body, format}` | Parse file frontmatter |

### Execution (Capability::EXECUTE)

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.exec(cmd)` | `{ok, stdout, stderr, code}` | Shell command execution |
| `orcs.exec_argv(program, args, opts?)` | `{ok, stdout, stderr, code}` | Direct execution (no shell) |

### LLM (Capability::LLM)

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.llm(prompt, opts?)` | `{ok, content, model, session_id, stop_reason, intents}` | LLM chat completion |
| `orcs.llm_ping(opts?)` | `{ok, provider, base_url, latency_ms}` | Provider health check |
| `orcs.llm_dump_sessions()` | string | Export session history as JSON |
| `orcs.llm_load_sessions(json)` | `{ok, count}` | Restore session history from JSON |

LLM opts support `tools` (default true), `resolve` (default false), and `max_tool_turns` (default 10) for automatic tool_use dispatch.

### Intent Dispatch

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.dispatch(name, args)` | table | Unified intent dispatcher (8 builtins + dynamic) |
| `orcs.intent_defs()` | table[] | Intent definitions in JSON Schema format |
| `orcs.register_intent(def)` | `{ok, error}` | Register a Component-backed intent at runtime |
| `orcs.tool_schemas()` | table[] | Legacy tool schema format (backward compat) |
| `orcs.tool_descriptions()` | string | Formatted tool descriptions for prompts |

### HTTP (Capability::HTTP)

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.http(method, url, opts?)` | `{ok, status, headers, body}` | HTTP request |

### Component Communication

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.request(target, op, payload)` | table | Component-to-component RPC |
| `orcs.request_batch(requests)` | table[] | Parallel RPC batch |
| `orcs.output(msg)` | nil | Emit output event |
| `orcs.output_with_level(msg, level)` | nil | Emit leveled output event |
| `orcs.emit_event(category, op, payload)` | nil | Broadcast extension event |
| `orcs.board_recent(n)` | table[] | Query shared board |

### Child Management

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.spawn_child(config)` | `{ok, id}` | Spawn child entity |
| `orcs.send_to_child(id, msg)` | `{ok, result}` | Send message to child |
| `orcs.send_to_children_batch(ids, inputs)` | table[] | Parallel batch send |
| `orcs.spawn_runner(config)` | `{ok, fqn, channel_id}` | Spawn a ChannelRunner |
| `orcs.child_count()` | number | Current child count |
| `orcs.max_children()` | number | Max allowed children |

### Serialization & Utilities

| Function | Returns | Description |
|----------|---------|-------------|
| `orcs.json_parse(str)` | value | Parse JSON string |
| `orcs.json_encode(value)` | string | Encode to JSON |
| `orcs.toml_parse(str)` | value | Parse TOML string |
| `orcs.toml_encode(value)` | string | Encode to TOML |
| `orcs.sanitize_arg(s)` | `{ok, value, violations}` | Command argument validation |
| `orcs.sanitize_path(s)` | `{ok, value, violations}` | Path validation |
| `orcs.sanitize_strict(s)` | `{ok, value, violations}` | Strict validation (all rules) |
| `orcs.log(level, msg)` | nil | Structured logging |
| `orcs.pwd` | string | Sandbox root path |
| `orcs.git_info()` | `{ok, branch, commit_short, dirty}` | Git repository info |
| `orcs.load_lua(content, name?)` | value | Evaluate Lua in sandbox |
| `orcs.check_command(cmd)` | `{status, reason?}` | Check command permission |
| `orcs.grant_command(pattern)` | nil | Grant command pattern |
| `orcs.request_approval(op, desc)` | approval_id | HIL approval request |

## Configuration

Create `config.toml` in your project root (or `~/.orcs/config.toml` for global):

```toml
[components]
load = ["agent_mgr", "skill_manager", "profile_manager", "foundation_manager", "console_metrics", "shell", "tool"]

[components.settings.agent_mgr]
llm_provider = "ollama"
llm_model = "llama3.2:latest"
llm_base_url = "http://localhost:11434"
# llm_api_key = ""         # for openai/anthropic
prompt_placement = "both"   # "top" | "both" | "bottom"

[components.settings.skill_manager]
recommend_skill = true
# recommend_llm_provider = "ollama"
# recommend_llm_model = "llama3.2:latest"

[hil]
auto_approve = false
timeout_ms = 30000

[ui]
verbose = false
color = true

[[hooks.hooks]]
id = "audit"
fql = "builtin::*"
point = "request.pre_dispatch"
script = "hooks/audit.lua"
```

## Builtin Components

| Component | Description |
|-----------|-------------|
| `agent_mgr` | Agent manager with LLM worker spawning and skill intent registration |
| `skill_manager` | Skill catalog, recommendation, and execution |
| `profile_manager` | User profile and preferences |
| `foundation_manager` | Foundation/system prompt management |
| `console_metrics` | Console output metrics |
| `shell` | Interactive shell commands |
| `tool` | Tool execution bridge |
| `life_game` | Conway's Game of Life (experimental, `--experimental`) |

## Architecture

```
Layer 4: Frontend
  orcs-cli --- main.rs, Args, CliConfigResolver, --sandbox

Layer 3: Application
  orcs-app --- OrcsApp, OrcsAppBuilder, builtins expansion

Layer 2: Runtime (Internal)
  orcs-runtime --- Engine, Channel, World, Config, Session, Sandbox, IO

Layer 1.5: Hooks
  orcs-hook --- HookDef, HooksConfig, FQL-based lifecycle hooks

Layer 1: Plugin SDK
  orcs-component --- Component, Child, Agent, Skill traits
  orcs-event --- Signal, Request, Response
  orcs-types --- ComponentId, ActionIntent, IntentDef, IntentRegistry
  orcs-auth --- Capability, Permission, SandboxPolicy

Plugin Implementation:
  orcs-lua --- LuaComponent, LuaChild, orcs.* API, IntentRegistry, ScriptLoader

Dev Tools:
  orcs-lint --- Architecture lint (OL002: no unwrap, layer dependency checks)
```

## Documentation

API documentation is generated from source via rustdoc. Each crate has module-level documentation with architecture diagrams, usage examples, and security notes.

```bash
cargo doc --workspace --no-deps --open
```

## Contributing

Bug reports, feature requests, and questions are welcome via [GitHub Issues](https://github.com/ynishi/orcs-cli/issues).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

# ORCS CLI

Lua-extensible Agentic Shell for building AI-powered development tools.

ORCS (Orchestrated Runtime Component System) provides a component-based architecture where every behavior is implemented as a Lua script, capability-gated, and sandboxed.

## Features

- **Lua-first extensibility** -- All components (agents, skills, tools) are Lua scripts
- **Capability-gated APIs** -- LLM, HTTP, exec, file I/O each require explicit capability grants
- **Sandbox isolation** -- Path traversal prevention, Lua stdlib restrictions, input sanitization
- **Multi-provider LLM** -- Ollama, OpenAI, Anthropic via config.toml
- **Session persistence** -- Pause/resume with component state snapshot
- **Hook system** -- Lifecycle hooks for pre/post processing

## Install

```bash
cargo install --path crates/orcs-cli
```

Or build from source:

```bash
cargo build --release
# Binary at target/release/orcs
```

## Quick Start

### Run with default builtins

```bash
orcs
```

### Run in sandbox mode (isolated environment)

```bash
orcs --sandbox
```

### Create a Lua component

```lua
-- ~/.orcs/components/hello.lua
return {
    id = "hello",
    namespace = "custom",
    subscriptions = {"Hello"},

    init = function(cfg)
        orcs.log("Hello component initialized")
    end,

    on_request = function(request)
        if request.operation == "greet" then
            local name = request.payload or "world"
            return { success = true, data = "Hello, " .. name .. "!" }
        end
        return { success = false, error = "unknown operation" }
    end,

    shutdown = function()
        orcs.log("Goodbye")
    end,
}
```

Add it to `config.toml`:

```toml
[components]
load = ["agent_mgr", "skill_manager", "hello"]
```

## Lua API

| Category | Functions | Required Capability |
|----------|-----------|-------------------|
| File I/O | `orcs.read`, `orcs.write`, `orcs.grep`, `orcs.glob`, `orcs.mkdir`, `orcs.remove`, `orcs.mv` | Sandbox |
| Exec | `orcs.exec`, `orcs.exec_argv` | EXECUTE |
| LLM | `orcs.llm`, `orcs.llm_ping`, `orcs.llm_dump_sessions`, `orcs.llm_load_sessions` | LLM |
| HTTP | `orcs.http` | HTTP |
| Serialization | `orcs.json_parse`, `orcs.json_encode`, `orcs.toml_parse`, `orcs.toml_encode` | None |
| Sanitize | `orcs.sanitize_arg`, `orcs.sanitize_path`, `orcs.sanitize_strict` | None |
| Info | `orcs.log`, `orcs.pwd`, `orcs.git_info`, `orcs.tool_descriptions` | None |

## Configuration

Create `config.toml` in your project root (or `~/.orcs/config.toml` for global):

```toml
[components]
load = ["agent_mgr", "skill_manager", "profile_manager", "foundation_manager", "console_metrics", "shell", "tool"]

[components.settings.agent_mgr]
max_workers = 3

[llm]
provider = "ollama"       # ollama | openai | anthropic
model = "llama3.2:latest"
base_url = "http://localhost:11434"

[hooks]
# See docs for hook configuration
```

## Architecture

```
Layer 4: Frontend
  orcs-cli --- main.rs, Args, CliConfigResolver, --sandbox

Layer 3: Application
  orcs-app --- OrcsApp, OrcsAppBuilder, builtins expansion

Layer 2: Runtime (Internal)
  orcs-runtime --- Engine, Channel, World, Config, Session, Sandbox, IO, Auth

Layer 1.5: Hooks
  orcs-hook --- HookDef, HooksConfig, lifecycle hooks

Layer 1: Plugin SDK
  orcs-component --- Component, Child, Agent, Skill traits
  orcs-event --- Signal, Request, Response
  orcs-types --- ComponentId, Principal, ErrorCode
  orcs-auth --- Capability, Permission, SandboxPolicy

Plugin Implementation:
  orcs-lua --- LuaComponent, LuaChild, orcs.* API, ScriptLoader
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

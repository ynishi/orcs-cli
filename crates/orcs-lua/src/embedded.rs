//! Embedded Lua scripts.
//!
//! Scripts are embedded at compile time using `include_str!`.
//! This allows distribution without external script files.

use std::collections::HashMap;

/// Built-in echo script.
pub const ECHO: &str = include_str!("../scripts/echo.lua");

/// Built-in claude_cli script.
pub const CLAUDE_CLI: &str = include_str!("../scripts/claude_cli.lua");

/// Built-in echo_emitter script (ChannelRunner mode).
///
/// This script uses `orcs.output()` for output emission,
/// making it suitable for ChannelRunner-based execution.
pub const ECHO_EMITTER: &str = include_str!("../scripts/echo_emitter.lua");

/// Built-in subagent script.
///
/// Simple component that echoes user input with "Hello: {message}".
pub const SUBAGENT: &str = include_str!("../scripts/subagent.lua");

/// Built-in agent manager script.
///
/// Component that spawns and manages child agents.
pub const AGENT_MGR: &str = include_str!("../scripts/agent_mgr.lua");

/// Built-in shell component for auth verification.
///
/// Executes shell commands prefixed with `!` through the permission checker.
/// Displays OK/NG results to verify auth works end-to-end.
pub const SHELL: &str = include_str!("../scripts/shell.lua");

/// Built-in tool verification component.
///
/// Exercises orcs.read/write/grep/glob/mkdir/remove/mv through
/// the Capability-gated permission layer.
pub const TOOL: &str = include_str!("../scripts/tool.lua");

/// POC coding agent using orcs.llm() + orcs tools.
///
/// Multi-turn agent that calls Claude headless SDK, parses tool calls,
/// executes them, and feeds results back.
pub const CODE_AGENT: &str = include_str!("../scripts/code_agent.lua");

/// Returns all embedded scripts as a map of name -> source.
#[must_use]
pub fn all() -> HashMap<&'static str, &'static str> {
    let mut scripts = HashMap::new();
    scripts.insert("echo", ECHO);
    scripts.insert("claude_cli", CLAUDE_CLI);
    scripts.insert("echo_emitter", ECHO_EMITTER);
    scripts.insert("subagent", SUBAGENT);
    scripts.insert("agent_mgr", AGENT_MGR);
    scripts.insert("shell", SHELL);
    scripts.insert("tool", TOOL);
    scripts.insert("code_agent", CODE_AGENT);
    scripts
}

/// Gets an embedded script by name.
#[must_use]
pub fn get(name: &str) -> Option<&'static str> {
    match name {
        "echo" => Some(ECHO),
        "claude_cli" => Some(CLAUDE_CLI),
        "echo_emitter" => Some(ECHO_EMITTER),
        "subagent" => Some(SUBAGENT),
        "agent_mgr" => Some(AGENT_MGR),
        "shell" => Some(SHELL),
        "tool" => Some(TOOL),
        "code_agent" => Some(CODE_AGENT),
        _ => None,
    }
}

/// Lists all available embedded script names.
#[must_use]
pub fn list() -> Vec<&'static str> {
    vec![
        "echo",
        "claude_cli",
        "echo_emitter",
        "subagent",
        "agent_mgr",
        "shell",
        "tool",
        "code_agent",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_script_embedded() {
        assert!(ECHO.contains("id = \"echo\""));
        assert!(ECHO.contains("on_request"));
    }

    #[test]
    fn get_echo() {
        let script = get("echo").expect("echo script should exist");
        assert!(script.contains("echo"));
    }

    #[test]
    fn get_unknown_returns_none() {
        assert!(get("nonexistent").is_none());
    }

    #[test]
    fn list_contains_echo() {
        let names = list();
        assert!(names.contains(&"echo"));
    }

    #[test]
    fn claude_cli_script_embedded() {
        assert!(CLAUDE_CLI.contains("id = \"claude_cli\""));
        assert!(CLAUDE_CLI.contains("on_request"));
        assert!(CLAUDE_CLI.contains("on_signal"));
        assert!(CLAUDE_CLI.contains("orcs.exec"));
    }

    #[test]
    fn get_claude_cli() {
        let script = get("claude_cli").expect("claude_cli script should exist");
        assert!(script.contains("claude_cli"));
    }

    #[test]
    fn list_contains_claude_cli() {
        let names = list();
        assert!(names.contains(&"claude_cli"));
    }

    #[test]
    fn echo_emitter_script_embedded() {
        assert!(ECHO_EMITTER.contains("id = \"echo_emitter\""));
        assert!(ECHO_EMITTER.contains("on_request"));
        assert!(ECHO_EMITTER.contains("on_signal"));
        assert!(ECHO_EMITTER.contains("orcs.output"));
    }

    #[test]
    fn get_echo_emitter() {
        let script = get("echo_emitter").expect("echo_emitter script should exist");
        assert!(script.contains("echo_emitter"));
    }

    #[test]
    fn list_contains_echo_emitter() {
        let names = list();
        assert!(names.contains(&"echo_emitter"));
    }

    #[test]
    fn all_contains_all_scripts() {
        let scripts = all();
        assert_eq!(scripts.len(), 8);
        assert!(scripts.contains_key("echo"));
        assert!(scripts.contains_key("claude_cli"));
        assert!(scripts.contains_key("echo_emitter"));
        assert!(scripts.contains_key("subagent"));
        assert!(scripts.contains_key("agent_mgr"));
        assert!(scripts.contains_key("shell"));
        assert!(scripts.contains_key("tool"));
        assert!(scripts.contains_key("code_agent"));
    }

    #[test]
    fn agent_mgr_script_embedded() {
        assert!(AGENT_MGR.contains("id = \"agent_mgr\""));
        assert!(AGENT_MGR.contains("on_request"));
        assert!(AGENT_MGR.contains("spawn_child"));
    }

    #[test]
    fn get_agent_mgr() {
        let script = get("agent_mgr").expect("agent_mgr script should exist");
        assert!(script.contains("agent_mgr"));
    }

    #[test]
    fn list_contains_agent_mgr() {
        let names = list();
        assert!(names.contains(&"agent_mgr"));
    }

    #[test]
    fn subagent_script_embedded() {
        assert!(SUBAGENT.contains("id = \"subagent\""));
        assert!(SUBAGENT.contains("on_request"));
        assert!(SUBAGENT.contains("orcs.output"));
    }

    #[test]
    fn get_subagent() {
        let script = get("subagent").expect("subagent script should exist");
        assert!(script.contains("subagent"));
    }

    #[test]
    fn list_contains_subagent() {
        let names = list();
        assert!(names.contains(&"subagent"));
    }

    #[test]
    fn shell_script_embedded() {
        assert!(SHELL.contains("id = \"shell\""));
        assert!(SHELL.contains("on_request"));
        assert!(SHELL.contains("orcs.exec"));
        assert!(SHELL.contains("orcs.output"));
    }

    #[test]
    fn get_shell() {
        let script = get("shell").expect("shell script should exist");
        assert!(script.contains("shell"));
    }

    #[test]
    fn list_contains_shell() {
        let names = list();
        assert!(names.contains(&"shell"));
    }
}

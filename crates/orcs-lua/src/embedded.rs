//! Embedded Lua scripts.
//!
//! Scripts are embedded at compile time using `include_str!`.
//! This allows distribution without external script files.

use std::collections::HashMap;

/// Built-in echo script.
pub const ECHO: &str = include_str!("../scripts/echo.lua");

/// Built-in claude_cli script.
pub const CLAUDE_CLI: &str = include_str!("../scripts/claude_cli.lua");

/// Returns all embedded scripts as a map of name -> source.
#[must_use]
pub fn all() -> HashMap<&'static str, &'static str> {
    let mut scripts = HashMap::new();
    scripts.insert("echo", ECHO);
    scripts.insert("claude_cli", CLAUDE_CLI);
    scripts
}

/// Gets an embedded script by name.
#[must_use]
pub fn get(name: &str) -> Option<&'static str> {
    match name {
        "echo" => Some(ECHO),
        "claude_cli" => Some(CLAUDE_CLI),
        _ => None,
    }
}

/// Lists all available embedded script names.
#[must_use]
pub fn list() -> Vec<&'static str> {
    vec!["echo", "claude_cli"]
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
    fn all_contains_both_scripts() {
        let scripts = all();
        assert_eq!(scripts.len(), 2);
        assert!(scripts.contains_key("echo"));
        assert!(scripts.contains_key("claude_cli"));
    }
}

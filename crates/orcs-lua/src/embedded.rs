//! Embedded Lua scripts.
//!
//! Scripts are embedded at compile time using `include_str!`.
//! This allows distribution without external script files.

use std::collections::HashMap;

/// Built-in echo script.
pub const ECHO: &str = include_str!("../scripts/echo.lua");

/// Returns all embedded scripts as a map of name -> source.
#[must_use]
pub fn all() -> HashMap<&'static str, &'static str> {
    let mut scripts = HashMap::new();
    scripts.insert("echo", ECHO);
    scripts
}

/// Gets an embedded script by name.
#[must_use]
pub fn get(name: &str) -> Option<&'static str> {
    match name {
        "echo" => Some(ECHO),
        _ => None,
    }
}

/// Lists all available embedded script names.
#[must_use]
pub fn list() -> Vec<&'static str> {
    vec!["echo"]
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
}

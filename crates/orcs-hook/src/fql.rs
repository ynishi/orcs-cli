//! FQL (Fully Qualified Locator) pattern matching.
//!
//! FQL patterns address components in the ORCS hierarchy:
//!
//! ```text
//! FQL := <scope>::<target> [ "/" <child_path> ] [ "#" <instance> ]
//! ```
//!
//! Matches against `ComponentId::fqn()` which returns `"namespace::name"`.

use crate::HookError;
use orcs_types::ComponentId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A single segment in an FQL pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatternSegment {
    /// Matches exactly the given string.
    Exact(String),
    /// Matches any string.
    Wildcard,
}

impl PatternSegment {
    /// Returns `true` if this segment matches the given value.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Exact(s) => s == value,
            Self::Wildcard => true,
        }
    }
}

impl fmt::Display for PatternSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(s) => f.write_str(s),
            Self::Wildcard => f.write_str("*"),
        }
    }
}

/// A parsed FQL pattern for matching components and children.
///
/// # Examples
///
/// ```text
/// "builtin::llm"        → scope=Exact("builtin"), target=Exact("llm")
/// "*::*"                 → scope=Wildcard, target=Wildcard
/// "builtin::llm/agent-1" → + child_path=Exact("agent-1")
/// "builtin::llm/*"      → + child_path=Wildcard
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FqlPattern {
    /// Namespace scope (e.g., "builtin", "plugin", "*").
    pub scope: PatternSegment,
    /// Component name (e.g., "llm", "hil", "*").
    pub target: PatternSegment,
    /// Optional child path (e.g., "agent-1", "*").
    pub child_path: Option<PatternSegment>,
    /// Optional instance qualifier (e.g., "primary", "0").
    pub instance: Option<PatternSegment>,
}

impl FqlPattern {
    /// Parses an FQL pattern string.
    ///
    /// # Format
    ///
    /// ```text
    /// <scope>::<target>[/<child_path>][#<instance>]
    /// ```
    ///
    /// # Errors
    ///
    /// Returns `HookError::InvalidFql` if the string cannot be parsed.
    pub fn parse(fql: &str) -> Result<Self, HookError> {
        if fql.is_empty() {
            return Err(HookError::InvalidFql("empty FQL pattern".into()));
        }

        // Split instance qualifier first: "foo::bar#inst" → "foo::bar", "inst"
        let (main_part, instance) = if let Some(hash_pos) = fql.rfind('#') {
            let inst = &fql[hash_pos + 1..];
            if inst.is_empty() {
                return Err(HookError::InvalidFql("empty instance qualifier".into()));
            }
            (&fql[..hash_pos], Some(parse_segment(inst)))
        } else {
            (fql, None)
        };

        // Split child path: "foo::bar/child" → "foo::bar", "child"
        let (component_part, child_path) = if let Some(slash_pos) = main_part.find('/') {
            let scope_target = &main_part[..slash_pos];
            let child = &main_part[slash_pos + 1..];
            if child.is_empty() {
                return Err(HookError::InvalidFql("empty child path".into()));
            }
            (scope_target, Some(parse_segment(child)))
        } else {
            (main_part, None)
        };

        // Split scope::target
        let sep_pos = component_part
            .find("::")
            .ok_or_else(|| HookError::InvalidFql(format!("missing '::' separator in '{fql}'")))?;

        let scope_str = &component_part[..sep_pos];
        let target_str = &component_part[sep_pos + 2..];

        if scope_str.is_empty() {
            return Err(HookError::InvalidFql("empty scope".into()));
        }
        if target_str.is_empty() {
            return Err(HookError::InvalidFql("empty target".into()));
        }

        Ok(Self {
            scope: parse_segment(scope_str),
            target: parse_segment(target_str),
            child_path,
            instance,
        })
    }

    /// Returns `true` if this pattern matches the given component and optional child.
    ///
    /// Matches against `component_id.fqn()` which returns `"namespace::name"`.
    #[must_use]
    pub fn matches(&self, component_id: &ComponentId, child_id: Option<&str>) -> bool {
        let fqn = component_id.fqn();

        // Parse fqn into namespace::name
        let (namespace, name) = match fqn.find("::") {
            Some(pos) => (&fqn[..pos], &fqn[pos + 2..]),
            None => return false,
        };

        // Check scope and target
        if !self.scope.matches(namespace) || !self.target.matches(name) {
            return false;
        }

        // Check child path
        match (&self.child_path, child_id) {
            (Some(pattern), Some(id)) => pattern.matches(id),
            (Some(_), None) => false, // Pattern requires child but none given
            (None, _) => true,        // No child pattern → matches regardless
        }
    }
}

impl fmt::Display for FqlPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.scope, self.target)?;
        if let Some(ref child) = self.child_path {
            write!(f, "/{child}")?;
        }
        if let Some(ref inst) = self.instance {
            write!(f, "#{inst}")?;
        }
        Ok(())
    }
}

/// Parses a single pattern segment: `"*"` → Wildcard, anything else → Exact.
fn parse_segment(s: &str) -> PatternSegment {
    if s == "*" {
        PatternSegment::Wildcard
    } else {
        PatternSegment::Exact(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parsing ──────────────────────────────────────────────

    #[test]
    fn parse_exact_match() {
        let p = FqlPattern::parse("builtin::llm")
            .expect("exact FQL 'builtin::llm' should parse successfully");
        assert_eq!(p.scope, PatternSegment::Exact("builtin".into()));
        assert_eq!(p.target, PatternSegment::Exact("llm".into()));
        assert_eq!(p.child_path, None);
        assert_eq!(p.instance, None);
    }

    #[test]
    fn parse_full_wildcard() {
        let p = FqlPattern::parse("*::*").expect("wildcard FQL '*::*' should parse successfully");
        assert_eq!(p.scope, PatternSegment::Wildcard);
        assert_eq!(p.target, PatternSegment::Wildcard);
    }

    #[test]
    fn parse_scope_wildcard() {
        let p = FqlPattern::parse("*::llm")
            .expect("scope-wildcard FQL '*::llm' should parse successfully");
        assert_eq!(p.scope, PatternSegment::Wildcard);
        assert_eq!(p.target, PatternSegment::Exact("llm".into()));
    }

    #[test]
    fn parse_target_wildcard() {
        let p = FqlPattern::parse("builtin::*")
            .expect("target-wildcard FQL 'builtin::*' should parse successfully");
        assert_eq!(p.scope, PatternSegment::Exact("builtin".into()));
        assert_eq!(p.target, PatternSegment::Wildcard);
    }

    #[test]
    fn parse_with_child_path() {
        let p = FqlPattern::parse("builtin::llm/agent-1")
            .expect("FQL with child path should parse successfully");
        assert_eq!(p.target, PatternSegment::Exact("llm".into()));
        assert_eq!(p.child_path, Some(PatternSegment::Exact("agent-1".into())));
    }

    #[test]
    fn parse_with_child_wildcard() {
        let p = FqlPattern::parse("builtin::llm/*")
            .expect("FQL with child wildcard should parse successfully");
        assert_eq!(p.child_path, Some(PatternSegment::Wildcard));
    }

    #[test]
    fn parse_with_instance() {
        let p = FqlPattern::parse("lua::custom#primary")
            .expect("FQL with instance qualifier should parse successfully");
        assert_eq!(p.instance, Some(PatternSegment::Exact("primary".into())));
    }

    #[test]
    fn parse_full_pattern() {
        let p = FqlPattern::parse("builtin::llm/agent-1#0")
            .expect("full FQL pattern with child and instance should parse successfully");
        assert_eq!(p.scope, PatternSegment::Exact("builtin".into()));
        assert_eq!(p.target, PatternSegment::Exact("llm".into()));
        assert_eq!(p.child_path, Some(PatternSegment::Exact("agent-1".into())));
        assert_eq!(p.instance, Some(PatternSegment::Exact("0".into())));
    }

    // ── Parse errors ─────────────────────────────────────────

    #[test]
    fn parse_empty_string() {
        assert!(FqlPattern::parse("").is_err());
    }

    #[test]
    fn parse_missing_separator() {
        assert!(FqlPattern::parse("builtin_llm").is_err());
    }

    #[test]
    fn parse_empty_scope() {
        assert!(FqlPattern::parse("::llm").is_err());
    }

    #[test]
    fn parse_empty_target() {
        assert!(FqlPattern::parse("builtin::").is_err());
    }

    #[test]
    fn parse_empty_child_path() {
        assert!(FqlPattern::parse("builtin::llm/").is_err());
    }

    #[test]
    fn parse_empty_instance() {
        assert!(FqlPattern::parse("builtin::llm#").is_err());
    }

    // ── Matching ─────────────────────────────────────────────

    #[test]
    fn match_exact() {
        let p = FqlPattern::parse("builtin::llm")
            .expect("FQL 'builtin::llm' should parse for matching test");
        let id = ComponentId::builtin("llm");
        assert!(p.matches(&id, None));
    }

    #[test]
    fn match_exact_no_match() {
        let p = FqlPattern::parse("builtin::hil")
            .expect("FQL 'builtin::hil' should parse for non-match test");
        let id = ComponentId::builtin("llm");
        assert!(!p.matches(&id, None));
    }

    #[test]
    fn match_wildcard_scope() {
        let p =
            FqlPattern::parse("*::llm").expect("scope-wildcard FQL should parse for matching test");
        let id = ComponentId::builtin("llm");
        assert!(p.matches(&id, None));
    }

    #[test]
    fn match_wildcard_target() {
        let p = FqlPattern::parse("builtin::*")
            .expect("target-wildcard FQL should parse for matching test");
        let id_llm = ComponentId::builtin("llm");
        let id_hil = ComponentId::builtin("hil");
        assert!(p.matches(&id_llm, None));
        assert!(p.matches(&id_hil, None));
    }

    #[test]
    fn match_full_wildcard() {
        let p =
            FqlPattern::parse("*::*").expect("full wildcard FQL should parse for matching test");
        let id = ComponentId::builtin("anything");
        assert!(p.matches(&id, None));
    }

    #[test]
    fn match_child_exact() {
        let p = FqlPattern::parse("builtin::llm/agent-1")
            .expect("FQL with exact child path should parse for matching test");
        let id = ComponentId::builtin("llm");
        assert!(p.matches(&id, Some("agent-1")));
        assert!(!p.matches(&id, Some("agent-2")));
        assert!(!p.matches(&id, None));
    }

    #[test]
    fn match_child_wildcard() {
        let p = FqlPattern::parse("builtin::llm/*")
            .expect("FQL with child wildcard should parse for matching test");
        let id = ComponentId::builtin("llm");
        assert!(p.matches(&id, Some("agent-1")));
        assert!(p.matches(&id, Some("any-child")));
        assert!(!p.matches(&id, None));
    }

    #[test]
    fn match_no_child_pattern_accepts_any_child() {
        let p = FqlPattern::parse("builtin::llm")
            .expect("FQL without child path should parse for any-child matching test");
        let id = ComponentId::builtin("llm");
        assert!(p.matches(&id, None));
        assert!(p.matches(&id, Some("agent-1")));
    }

    #[test]
    fn match_plugin_namespace() {
        let p = FqlPattern::parse("plugin::my-tool")
            .expect("FQL with plugin namespace should parse for matching test");
        let id = ComponentId::new("plugin", "my-tool");
        assert!(p.matches(&id, None));
        assert!(!p.matches(&ComponentId::builtin("my-tool"), None));
    }

    // ── Display ──────────────────────────────────────────────

    #[test]
    fn display_roundtrip() {
        let patterns = [
            "builtin::llm",
            "*::*",
            "builtin::*",
            "*::llm",
            "builtin::llm/agent-1",
            "builtin::llm/*",
            "lua::custom#primary",
            "builtin::llm/agent-1#0",
        ];
        for &s in &patterns {
            let p =
                FqlPattern::parse(s).expect("FQL pattern should parse for display roundtrip test");
            assert_eq!(p.to_string(), s, "display roundtrip failed for {s}");
        }
    }

    // ── Serde ────────────────────────────────────────────────

    #[test]
    fn serde_roundtrip() {
        let p = FqlPattern::parse("builtin::llm/agent-1#0")
            .expect("full FQL pattern should parse for serde roundtrip");
        let json = serde_json::to_string(&p).expect("FqlPattern should serialize to JSON");
        let restored: FqlPattern =
            serde_json::from_str(&json).expect("FqlPattern should deserialize from JSON");
        assert_eq!(p, restored);
    }
}

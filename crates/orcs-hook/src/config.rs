//! Hook configuration — declarative hook definitions.
//!
//! Defines the TOML-serializable configuration for hooks.
//! These types are used in `OrcsConfig` to declare hooks that are
//! loaded at engine startup.
//!
//! # Example TOML
//!
//! ```toml
//! [[hooks]]
//! id = "audit-requests"
//! fql = "builtin::*"
//! point = "request.pre_dispatch"
//! script = "hooks/audit.lua"
//! priority = 50
//!
//! [[hooks]]
//! id = "tool-metrics"
//! fql = "*::*"
//! point = "tool.post_execute"
//! handler_inline = """
//! function(ctx)
//!     return { action = "continue", ctx = ctx }
//! end
//! """
//! priority = 200
//! ```

use crate::{FqlPattern, HookError, HookPoint};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;

/// Top-level hooks configuration.
///
/// Contains a list of declarative hook definitions that are
/// loaded and registered at engine startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HooksConfig {
    /// Declarative hook definitions.
    pub hooks: Vec<HookDef>,
}

/// A single declarative hook definition.
///
/// Either `script` or `handler_inline` must be specified (but not both).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookDef {
    /// Unique hook ID. Auto-generated if not specified.
    pub id: Option<String>,

    /// FQL pattern: which components this hook targets.
    pub fql: String,

    /// Hook point: when this hook fires (e.g., "request.pre_dispatch").
    pub point: String,

    /// Path to Lua script handler (relative to `scripts.dirs`).
    pub script: Option<String>,

    /// Inline Lua handler (for simple hooks).
    pub handler_inline: Option<String>,

    /// Priority (lower = earlier). Default: 100.
    #[serde(default = "default_priority")]
    pub priority: i32,

    /// Whether the hook is enabled. Default: true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_priority() -> i32 {
    100
}

fn default_enabled() -> bool {
    true
}

/// Errors from validating a `HookDef`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HookDefValidationError {
    /// Neither `script` nor `handler_inline` is specified.
    #[error("hook '{label}': neither 'script' nor 'handler_inline' specified")]
    NoHandler { label: String },

    /// Both `script` and `handler_inline` are specified.
    #[error("hook '{label}': both 'script' and 'handler_inline' specified (use one)")]
    BothHandlers { label: String },

    /// Invalid FQL pattern.
    #[error("hook '{label}': {source}")]
    InvalidFql { label: String, source: HookError },

    /// Invalid hook point string.
    #[error("hook '{label}': {source}")]
    InvalidPoint { label: String, source: HookError },
}

impl HookDef {
    /// Validates this hook definition.
    ///
    /// Checks:
    /// - Exactly one of `script` or `handler_inline` is specified
    /// - `fql` is a valid FQL pattern
    /// - `point` is a valid HookPoint string
    pub fn validate(&self) -> Result<(), HookDefValidationError> {
        let label = self.id.as_deref().unwrap_or("<anonymous>").to_string();

        // Handler exclusivity check
        match (&self.script, &self.handler_inline) {
            (None, None) => {
                return Err(HookDefValidationError::NoHandler { label });
            }
            (Some(_), Some(_)) => {
                return Err(HookDefValidationError::BothHandlers { label });
            }
            _ => {}
        }

        // Validate FQL pattern
        FqlPattern::parse(&self.fql).map_err(|e| HookDefValidationError::InvalidFql {
            label: label.clone(),
            source: e,
        })?;

        // Validate hook point
        HookPoint::from_str(&self.point)
            .map_err(|e| HookDefValidationError::InvalidPoint { label, source: e })?;

        Ok(())
    }
}

impl HooksConfig {
    /// Merges another config into this one.
    ///
    /// Hook definitions accumulate across config layers.
    /// If a hook in `other` has an `id` that matches an existing hook,
    /// the existing hook is replaced (override semantics).
    /// New hooks (or anonymous hooks without `id`) are appended.
    pub fn merge(&mut self, other: &Self) {
        for hook in &other.hooks {
            if let Some(id) = &hook.id {
                // Override existing hook with same ID
                self.hooks.retain(|h| h.id.as_deref() != Some(id));
            }
            self.hooks.push(hook.clone());
        }
    }

    /// Validates all hook definitions in this config.
    ///
    /// Returns all validation errors (not just the first one).
    pub fn validate_all(&self) -> Vec<HookDefValidationError> {
        self.hooks
            .iter()
            .filter_map(|h| h.validate().err())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook_def(id: &str, fql: &str, point: &str, script: Option<&str>) -> HookDef {
        HookDef {
            id: Some(id.to_string()),
            fql: fql.to_string(),
            point: point.to_string(),
            script: script.map(|s| s.to_string()),
            handler_inline: None,
            priority: default_priority(),
            enabled: default_enabled(),
        }
    }

    // ── Defaults ────────────────────────────────────────────

    #[test]
    fn default_priority_is_100() {
        assert_eq!(default_priority(), 100);
    }

    #[test]
    fn default_enabled_is_true() {
        assert!(default_enabled());
    }

    #[test]
    fn hooks_config_default_is_empty() {
        let cfg = HooksConfig::default();
        assert!(cfg.hooks.is_empty());
    }

    // ── Validation ──────────────────────────────────────────

    #[test]
    fn validate_valid_script_hook() {
        let hook = make_hook_def(
            "audit",
            "builtin::*",
            "request.pre_dispatch",
            Some("hooks/audit.lua"),
        );
        assert!(hook.validate().is_ok());
    }

    #[test]
    fn validate_valid_inline_hook() {
        let hook = HookDef {
            id: Some("inline".into()),
            fql: "*::*".into(),
            point: "tool.post_execute".into(),
            script: None,
            handler_inline: Some("function(ctx) return ctx end".into()),
            priority: 100,
            enabled: true,
        };
        assert!(hook.validate().is_ok());
    }

    #[test]
    fn validate_no_handler_error() {
        let hook = HookDef {
            id: Some("bad".into()),
            fql: "*::*".into(),
            point: "request.pre_dispatch".into(),
            script: None,
            handler_inline: None,
            priority: 100,
            enabled: true,
        };
        let err = hook.validate().unwrap_err();
        assert!(matches!(err, HookDefValidationError::NoHandler { .. }));
        assert!(err.to_string().contains("neither"));
    }

    #[test]
    fn validate_both_handlers_error() {
        let hook = HookDef {
            id: Some("bad".into()),
            fql: "*::*".into(),
            point: "request.pre_dispatch".into(),
            script: Some("hooks/foo.lua".into()),
            handler_inline: Some("function(ctx) return ctx end".into()),
            priority: 100,
            enabled: true,
        };
        let err = hook.validate().unwrap_err();
        assert!(matches!(err, HookDefValidationError::BothHandlers { .. }));
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn validate_invalid_fql() {
        let hook = HookDef {
            id: Some("bad-fql".into()),
            fql: "not-valid".into(),
            point: "request.pre_dispatch".into(),
            script: Some("hooks/x.lua".into()),
            handler_inline: None,
            priority: 100,
            enabled: true,
        };
        let err = hook.validate().unwrap_err();
        assert!(matches!(err, HookDefValidationError::InvalidFql { .. }));
    }

    #[test]
    fn validate_invalid_point() {
        let hook = HookDef {
            id: Some("bad-point".into()),
            fql: "*::*".into(),
            point: "not.a.real.point".into(),
            script: Some("hooks/x.lua".into()),
            handler_inline: None,
            priority: 100,
            enabled: true,
        };
        let err = hook.validate().unwrap_err();
        assert!(matches!(err, HookDefValidationError::InvalidPoint { .. }));
    }

    #[test]
    fn validate_anonymous_hook() {
        let hook = HookDef {
            id: None,
            fql: "*::*".into(),
            point: "request.pre_dispatch".into(),
            script: Some("hooks/x.lua".into()),
            handler_inline: None,
            priority: 100,
            enabled: true,
        };
        assert!(hook.validate().is_ok());
    }

    #[test]
    fn validate_anonymous_error_display() {
        let hook = HookDef {
            id: None,
            fql: "*::*".into(),
            point: "request.pre_dispatch".into(),
            script: None,
            handler_inline: None,
            priority: 100,
            enabled: true,
        };
        let err = hook.validate().unwrap_err();
        assert!(err.to_string().contains("<anonymous>"));
    }

    // ── Merge ───────────────────────────────────────────────

    #[test]
    fn merge_appends_new_hooks() {
        let mut base = HooksConfig {
            hooks: vec![make_hook_def(
                "h1",
                "*::*",
                "request.pre_dispatch",
                Some("a.lua"),
            )],
        };
        let overlay = HooksConfig {
            hooks: vec![make_hook_def(
                "h2",
                "*::*",
                "tool.pre_execute",
                Some("b.lua"),
            )],
        };

        base.merge(&overlay);
        assert_eq!(base.hooks.len(), 2);
        assert_eq!(base.hooks[0].id.as_deref(), Some("h1"));
        assert_eq!(base.hooks[1].id.as_deref(), Some("h2"));
    }

    #[test]
    fn merge_overrides_same_id() {
        let mut base = HooksConfig {
            hooks: vec![make_hook_def(
                "h1",
                "*::*",
                "request.pre_dispatch",
                Some("old.lua"),
            )],
        };
        let overlay = HooksConfig {
            hooks: vec![make_hook_def(
                "h1",
                "builtin::llm",
                "tool.pre_execute",
                Some("new.lua"),
            )],
        };

        base.merge(&overlay);
        assert_eq!(base.hooks.len(), 1);
        assert_eq!(base.hooks[0].fql, "builtin::llm");
        assert_eq!(base.hooks[0].script.as_deref(), Some("new.lua"));
    }

    #[test]
    fn merge_anonymous_hooks_always_append() {
        let mut base = HooksConfig {
            hooks: vec![{
                let mut h = make_hook_def("", "*::*", "request.pre_dispatch", Some("a.lua"));
                h.id = None;
                h
            }],
        };
        let overlay = HooksConfig {
            hooks: vec![{
                let mut h = make_hook_def("", "*::*", "request.pre_dispatch", Some("b.lua"));
                h.id = None;
                h
            }],
        };

        base.merge(&overlay);
        // Both anonymous hooks should be present (no dedup on None id)
        assert_eq!(base.hooks.len(), 2);
    }

    #[test]
    fn merge_mixed_override_and_append() {
        let mut base = HooksConfig {
            hooks: vec![
                make_hook_def("h1", "*::*", "request.pre_dispatch", Some("a.lua")),
                make_hook_def("h2", "*::*", "signal.pre_dispatch", Some("b.lua")),
            ],
        };
        let overlay = HooksConfig {
            hooks: vec![
                make_hook_def(
                    "h1",
                    "builtin::*",
                    "request.pre_dispatch",
                    Some("new-a.lua"),
                ),
                make_hook_def("h3", "*::*", "child.pre_spawn", Some("c.lua")),
            ],
        };

        base.merge(&overlay);
        assert_eq!(base.hooks.len(), 3);
        // h2 remains, h1 replaced, h3 appended
        assert_eq!(base.hooks[0].id.as_deref(), Some("h2"));
        assert_eq!(base.hooks[1].id.as_deref(), Some("h1"));
        assert_eq!(base.hooks[1].fql, "builtin::*");
        assert_eq!(base.hooks[2].id.as_deref(), Some("h3"));
    }

    // ── validate_all ────────────────────────────────────────

    #[test]
    fn validate_all_collects_all_errors() {
        let cfg = HooksConfig {
            hooks: vec![
                // Valid
                make_hook_def("ok", "*::*", "request.pre_dispatch", Some("ok.lua")),
                // No handler
                HookDef {
                    id: Some("bad1".into()),
                    fql: "*::*".into(),
                    point: "request.pre_dispatch".into(),
                    script: None,
                    handler_inline: None,
                    priority: 100,
                    enabled: true,
                },
                // Invalid FQL
                HookDef {
                    id: Some("bad2".into()),
                    fql: "broken".into(),
                    point: "request.pre_dispatch".into(),
                    script: Some("x.lua".into()),
                    handler_inline: None,
                    priority: 100,
                    enabled: true,
                },
            ],
        };

        let errors = cfg.validate_all();
        assert_eq!(errors.len(), 2);
    }

    // ── Serde (JSON roundtrip) ──────────────────────────────

    #[test]
    fn serde_json_roundtrip() {
        let cfg = HooksConfig {
            hooks: vec![
                make_hook_def(
                    "h1",
                    "builtin::*",
                    "request.pre_dispatch",
                    Some("hooks/audit.lua"),
                ),
                HookDef {
                    id: Some("h2".into()),
                    fql: "*::*".into(),
                    point: "tool.post_execute".into(),
                    script: None,
                    handler_inline: Some("function(ctx) return ctx end".into()),
                    priority: 200,
                    enabled: false,
                },
            ],
        };

        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let restored: HooksConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
    }

    #[test]
    fn serde_json_defaults_applied() {
        // Minimal JSON with only required fields
        let json = r#"{
            "hooks": [{
                "fql": "*::*",
                "point": "request.pre_dispatch",
                "script": "test.lua"
            }]
        }"#;

        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.hooks.len(), 1);
        assert_eq!(cfg.hooks[0].priority, 100);
        assert!(cfg.hooks[0].enabled);
        assert!(cfg.hooks[0].id.is_none());
    }

    // ── TOML roundtrip ──────────────────────────────────────

    #[test]
    fn toml_roundtrip() {
        let toml_str = r#"
[[hooks]]
id = "audit-requests"
fql = "builtin::*"
point = "request.pre_dispatch"
script = "hooks/audit.lua"
priority = 50
enabled = true

[[hooks]]
id = "tool-metrics"
fql = "*::*"
point = "tool.post_execute"
handler_inline = "function(ctx) return ctx end"
priority = 200
enabled = true
"#;

        let cfg: HooksConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.hooks.len(), 2);
        assert_eq!(cfg.hooks[0].id.as_deref(), Some("audit-requests"));
        assert_eq!(cfg.hooks[0].priority, 50);
        assert_eq!(cfg.hooks[1].id.as_deref(), Some("tool-metrics"));
        assert!(cfg.hooks[1].handler_inline.is_some());

        // Serialize back and re-parse
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let restored: HooksConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg, restored);
    }

    #[test]
    fn toml_minimal_with_defaults() {
        let toml_str = r#"
[[hooks]]
fql = "*::*"
point = "request.pre_dispatch"
script = "test.lua"
"#;

        let cfg: HooksConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.hooks.len(), 1);
        assert_eq!(cfg.hooks[0].priority, 100);
        assert!(cfg.hooks[0].enabled);
    }

    #[test]
    fn toml_empty_hooks() {
        let toml_str = "";
        let cfg: HooksConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.hooks.is_empty());
    }
}

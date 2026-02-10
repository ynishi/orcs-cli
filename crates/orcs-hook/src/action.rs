//! Hook action — return type from hook handlers.
//!
//! Determines what the runtime does after a hook executes.
//! `Default` is intentionally NOT implemented to prevent
//! accidental context destruction.

use crate::HookContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the hook wants the runtime to do after execution.
///
/// # Pre-hooks (can modify/abort)
///
/// - `Continue` — pass (possibly modified) context downstream
/// - `Skip` — skip the operation, return the given value as result
/// - `Abort` — abort the operation with an error
///
/// # Post-hooks (observe/replace)
///
/// - `Continue` — pass context to next post-hook
/// - `Replace` — replace the result payload, continue chain
///
/// # Intentional omission
///
/// `Default` is NOT implemented. `Continue(HookContext::empty())` would
/// destroy the original context silently. Handlers must return explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookAction {
    /// Continue with (possibly modified) context.
    Continue(Box<HookContext>),

    /// Skip the operation entirely (pre-hooks only).
    /// The `Value` is returned as the operation's result.
    Skip(Value),

    /// Abort the operation with an error (pre-hooks only).
    Abort {
        /// Reason for aborting.
        reason: String,
    },

    /// Replace the result with a different value (post-hooks only).
    /// In a post-hook chain, the replaced value becomes the new payload
    /// for subsequent hooks.
    Replace(Value),
}

impl HookAction {
    /// Returns `true` if this is a `Continue` variant.
    #[must_use]
    pub fn is_continue(&self) -> bool {
        matches!(self, Self::Continue(_))
    }

    /// Returns `true` if this is a `Skip` variant.
    #[must_use]
    pub fn is_skip(&self) -> bool {
        matches!(self, Self::Skip(_))
    }

    /// Returns `true` if this is an `Abort` variant.
    #[must_use]
    pub fn is_abort(&self) -> bool {
        matches!(self, Self::Abort { .. })
    }

    /// Returns `true` if this is a `Replace` variant.
    #[must_use]
    pub fn is_replace(&self) -> bool {
        matches!(self, Self::Replace(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookPoint;
    use orcs_types::{ChannelId, ComponentId, Principal};
    use serde_json::json;

    fn dummy_ctx() -> HookContext {
        HookContext::new(
            HookPoint::RequestPreDispatch,
            ComponentId::builtin("test"),
            ChannelId::new(),
            Principal::System,
            0,
            json!(null),
        )
    }

    #[test]
    fn continue_variant() {
        let action = HookAction::Continue(Box::new(dummy_ctx()));
        assert!(action.is_continue());
        assert!(!action.is_skip());
        assert!(!action.is_abort());
        assert!(!action.is_replace());
    }

    #[test]
    fn skip_variant() {
        let action = HookAction::Skip(json!({"skipped": true}));
        assert!(action.is_skip());
        assert!(!action.is_continue());
    }

    #[test]
    fn abort_variant() {
        let action = HookAction::Abort {
            reason: "policy".into(),
        };
        assert!(action.is_abort());
        assert!(!action.is_continue());
    }

    #[test]
    fn replace_variant() {
        let action = HookAction::Replace(json!({"new": "value"}));
        assert!(action.is_replace());
        assert!(!action.is_continue());
    }
}

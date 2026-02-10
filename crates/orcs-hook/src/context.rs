//! Hook context — data passed to hook handlers.

use crate::HookPoint;
use orcs_types::{ChannelId, ComponentId, Principal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Default maximum hook chain recursion depth.
pub const DEFAULT_MAX_DEPTH: u8 = 4;

/// Context passed to hook handlers.
///
/// Pre-hooks can modify `payload` to alter the downstream operation.
/// Post-hooks receive the final result in `payload`.
/// `metadata` carries cross-hook state from pre → post for the same operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    /// Which hook point triggered this.
    pub hook_point: HookPoint,

    /// The component targeted by this hook (from FQL match).
    pub component_id: ComponentId,

    /// The channel where this is happening.
    pub channel_id: ChannelId,

    /// Who initiated the action.
    pub principal: Principal,

    /// Monotonic timestamp (ms since engine start).
    pub timestamp_ms: u64,

    /// Hook-specific payload (varies by HookPoint).
    ///
    /// Pre hooks: the input data (request, config, args, etc.)
    /// Post hooks: the result data (response, child_result, etc.)
    pub payload: Value,

    /// Mutable metadata bag — hooks can store cross-hook state here.
    /// Carried from pre → post for the same operation.
    pub metadata: HashMap<String, Value>,

    /// Recursion depth counter. Incremented each time hooks re-enter
    /// (e.g., hook calls orcs.exec() which triggers ToolPreExecute hooks).
    pub depth: u8,

    /// Maximum recursion depth (default: 4).
    pub max_depth: u8,
}

impl HookContext {
    /// Creates a new HookContext with all fields specified.
    #[must_use]
    pub fn new(
        hook_point: HookPoint,
        component_id: ComponentId,
        channel_id: ChannelId,
        principal: Principal,
        timestamp_ms: u64,
        payload: Value,
    ) -> Self {
        Self {
            hook_point,
            component_id,
            channel_id,
            principal,
            timestamp_ms,
            payload,
            metadata: HashMap::new(),
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    /// Returns a new context with `depth` incremented by 1.
    #[must_use]
    pub fn with_incremented_depth(&self) -> Self {
        let mut ctx = self.clone();
        ctx.depth = ctx.depth.saturating_add(1);
        ctx
    }

    /// Returns `true` if the current depth has reached or exceeded `max_depth`.
    #[must_use]
    pub fn is_depth_exceeded(&self) -> bool {
        self.depth >= self.max_depth
    }

    /// Sets the maximum recursion depth.
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: u8) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Adds a metadata entry.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::PrincipalId;
    use serde_json::json;

    fn test_ctx() -> HookContext {
        HookContext::new(
            HookPoint::RequestPreDispatch,
            ComponentId::builtin("llm"),
            ChannelId::new(),
            Principal::User(PrincipalId::new()),
            12345,
            json!({"operation": "chat"}),
        )
    }

    #[test]
    fn new_has_correct_defaults() {
        let ctx = test_ctx();
        assert_eq!(ctx.depth, 0);
        assert_eq!(ctx.max_depth, DEFAULT_MAX_DEPTH);
        assert!(ctx.metadata.is_empty());
    }

    #[test]
    fn depth_increment() {
        let ctx = test_ctx();
        let incremented = ctx.with_incremented_depth();
        assert_eq!(incremented.depth, 1);
        assert_eq!(ctx.depth, 0); // original unchanged
    }

    #[test]
    fn depth_saturation() {
        let mut ctx = test_ctx();
        ctx.depth = u8::MAX;
        let incremented = ctx.with_incremented_depth();
        assert_eq!(incremented.depth, u8::MAX);
    }

    #[test]
    fn depth_exceeded() {
        let mut ctx = test_ctx();
        ctx.max_depth = 3;
        ctx.depth = 2;
        assert!(!ctx.is_depth_exceeded());
        ctx.depth = 3;
        assert!(ctx.is_depth_exceeded());
        ctx.depth = 4;
        assert!(ctx.is_depth_exceeded());
    }

    #[test]
    fn with_max_depth() {
        let ctx = test_ctx().with_max_depth(8);
        assert_eq!(ctx.max_depth, 8);
    }

    #[test]
    fn with_metadata() {
        let ctx = test_ctx().with_metadata("audit_id", json!("abc-123"));
        assert_eq!(ctx.metadata.get("audit_id"), Some(&json!("abc-123")));
    }

    #[test]
    fn serde_roundtrip() {
        let ctx = test_ctx().with_metadata("key", json!(42)).with_max_depth(8);
        let json = serde_json::to_string(&ctx).unwrap();
        let restored: HookContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.hook_point, ctx.hook_point);
        assert_eq!(restored.depth, ctx.depth);
        assert_eq!(restored.max_depth, ctx.max_depth);
        assert_eq!(restored.payload, ctx.payload);
        assert_eq!(restored.metadata, ctx.metadata);
    }

    #[test]
    fn clone_is_independent() {
        let mut ctx = test_ctx();
        let cloned = ctx.clone();
        ctx.payload = json!({"modified": true});
        assert_ne!(ctx.payload, cloned.payload);
    }
}

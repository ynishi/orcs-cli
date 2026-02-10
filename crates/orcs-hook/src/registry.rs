//! Hook registry — central dispatch for all hooks.
//!
//! Thread-safe: wrapped in `Arc<std::sync::RwLock<>>` at the engine level,
//! following the same pattern as `SharedChannelHandles`.

use crate::{Hook, HookAction, HookContext, HookPoint};
use orcs_types::ComponentId;
use std::collections::HashMap;

/// A registered hook with metadata.
struct RegisteredHook {
    hook: Box<dyn Hook>,
    enabled: bool,
    /// The Component that owns this hook (for auto-unregister on shutdown).
    /// Config-derived hooks have `owner: None`.
    owner: Option<ComponentId>,
}

/// Central registry for all hooks.
///
/// Hooks are indexed by [`HookPoint`] for O(1) lookup.
/// Within each point, hooks are sorted by priority (ascending).
///
/// # Concurrency
///
/// Use `Arc<std::sync::RwLock<HookRegistry>>` for concurrent access:
/// - `dispatch()` takes `&self` (read lock)
/// - `register()` / `unregister()` take `&mut self` (write lock)
pub struct HookRegistry {
    hooks: HashMap<HookPoint, Vec<RegisteredHook>>,
}

impl HookRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hooks: HashMap::new(),
        }
    }

    /// Registers a hook. Returns the hook's ID.
    ///
    /// The hook is inserted in priority order (ascending).
    pub fn register(&mut self, hook: Box<dyn Hook>) -> String {
        self.register_inner(hook, None)
    }

    /// Registers a hook owned by a component.
    ///
    /// Owned hooks are automatically unregistered when
    /// `unregister_by_owner()` is called (e.g., on component shutdown).
    pub fn register_owned(&mut self, hook: Box<dyn Hook>, owner: ComponentId) -> String {
        self.register_inner(hook, Some(owner))
    }

    fn register_inner(&mut self, hook: Box<dyn Hook>, owner: Option<ComponentId>) -> String {
        let id = hook.id().to_string();
        let point = hook.hook_point();
        let priority = hook.priority();

        let entry = self.hooks.entry(point).or_default();

        let rh = RegisteredHook {
            hook,
            enabled: true,
            owner,
        };

        // Insert in priority order (stable: FIFO for same priority)
        let pos = entry
            .iter()
            .position(|h| h.hook.priority() > priority)
            .unwrap_or(entry.len());
        entry.insert(pos, rh);

        id
    }

    /// Unregisters a hook by ID. Returns `true` if found and removed.
    pub fn unregister(&mut self, id: &str) -> bool {
        let mut found = false;
        for hooks in self.hooks.values_mut() {
            let before = hooks.len();
            hooks.retain(|rh| rh.hook.id() != id);
            if hooks.len() < before {
                found = true;
            }
        }
        found
    }

    /// Unregisters all hooks owned by the given component.
    ///
    /// Returns the number of hooks removed.
    pub fn unregister_by_owner(&mut self, owner: &ComponentId) -> usize {
        let mut count = 0;
        for hooks in self.hooks.values_mut() {
            let before = hooks.len();
            hooks.retain(|rh| rh.owner.as_ref() != Some(owner));
            count += before - hooks.len();
        }
        count
    }

    /// Enables or disables a hook by ID.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        for hooks in self.hooks.values_mut() {
            for rh in hooks.iter_mut() {
                if rh.hook.id() == id {
                    rh.enabled = enabled;
                    return;
                }
            }
        }
    }

    /// Returns the number of registered hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks.values().map(|v| v.len()).sum()
    }

    /// Returns `true` if no hooks are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Dispatches hooks for the given point and target.
    ///
    /// Hooks are executed in priority order (ascending).
    /// Chain semantics:
    ///
    /// - **Pre-hook**: `Skip` or `Abort` → stop chain immediately
    /// - **Post-hook**: `Replace` → update payload, continue chain
    /// - Disabled hooks and FQL non-matches are skipped
    /// - Depth exceeded → break with warning
    pub fn dispatch(
        &self,
        point: HookPoint,
        component_id: &ComponentId,
        child_id: Option<&str>,
        ctx: HookContext,
    ) -> HookAction {
        let Some(hooks) = self.hooks.get(&point) else {
            return HookAction::Continue(Box::new(ctx));
        };

        let mut current_ctx = ctx;

        for rh in hooks.iter().filter(|rh| rh.enabled) {
            if !rh.hook.fql_pattern().matches(component_id, child_id) {
                continue;
            }

            // Depth check (recursion prevention)
            if current_ctx.is_depth_exceeded() {
                tracing::warn!(
                    hook_id = rh.hook.id(),
                    depth = current_ctx.depth,
                    max_depth = current_ctx.max_depth,
                    "hook chain depth exceeded, stopping chain"
                );
                break;
            }

            match rh.hook.execute(current_ctx.clone()) {
                HookAction::Continue(new_ctx) => {
                    current_ctx = *new_ctx;
                }
                action @ (HookAction::Skip(_) | HookAction::Abort { .. }) => {
                    // Pre-hook aborted the chain
                    return action;
                }
                HookAction::Replace(value) => {
                    if point.is_post() {
                        // Post-hook: replace payload, continue chain
                        current_ctx.payload = value;
                    } else {
                        // Pre-hook: Replace is invalid → ignore with warning
                        tracing::warn!(
                            hook_id = rh.hook.id(),
                            point = %point,
                            "Replace returned from non-post hook, ignoring"
                        );
                    }
                }
            }
        }

        HookAction::Continue(Box::new(current_ctx))
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::testing::MockHook;
    use orcs_types::{ChannelId, Principal};
    use serde_json::json;

    fn test_ctx(point: HookPoint) -> HookContext {
        HookContext::new(
            point,
            ComponentId::builtin("llm"),
            ChannelId::new(),
            Principal::System,
            0,
            json!({"op": "test"}),
        )
    }

    // ── Basic dispatch ───────────────────────────────────────

    #[test]
    fn dispatch_no_hooks_returns_continue() {
        let reg = HookRegistry::new();
        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx.clone(),
        );
        assert!(action.is_continue());
        if let HookAction::Continue(result) = action {
            assert_eq!(result.payload, ctx.payload);
        }
    }

    #[test]
    fn dispatch_pass_through_hook() {
        let mut reg = HookRegistry::new();
        let hook = MockHook::pass_through("h1", "*::*", HookPoint::RequestPreDispatch);
        let counter = hook.call_count.clone();
        reg.register(Box::new(hook));

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx.clone(),
        );

        assert!(action.is_continue());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn dispatch_modifying_hook() {
        let mut reg = HookRegistry::new();
        let hook = MockHook::modifier("mod", "*::*", HookPoint::RequestPreDispatch, |ctx| {
            ctx.payload = json!({"modified": true});
        });
        reg.register(Box::new(hook));

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        if let HookAction::Continue(result) = action {
            assert_eq!(result.payload, json!({"modified": true}));
        } else {
            panic!("expected Continue");
        }
    }

    // ── Skip & Abort ─────────────────────────────────────────

    #[test]
    fn dispatch_skip_stops_chain() {
        let mut reg = HookRegistry::new();
        let skip = MockHook::skipper(
            "skip",
            "*::*",
            HookPoint::RequestPreDispatch,
            json!({"skipped": true}),
        )
        .with_priority(10);
        let after = MockHook::pass_through("after", "*::*", HookPoint::RequestPreDispatch)
            .with_priority(20);
        let after_counter = after.call_count.clone();

        reg.register(Box::new(skip));
        reg.register(Box::new(after));

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        assert!(action.is_skip());
        // "after" hook should not have been called
        assert_eq!(after_counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_abort_stops_chain() {
        let mut reg = HookRegistry::new();
        let abort = MockHook::aborter("abort", "*::*", HookPoint::RequestPreDispatch, "policy");
        reg.register(Box::new(abort));

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        assert!(action.is_abort());
        if let HookAction::Abort { reason } = action {
            assert_eq!(reason, "policy");
        }
    }

    // ── Priority ordering ────────────────────────────────────

    #[test]
    fn priority_ordering() {
        let mut reg = HookRegistry::new();

        // Register in reverse priority order
        let h100 = MockHook::modifier("h100", "*::*", HookPoint::RequestPreDispatch, |ctx| {
            let arr = ctx.payload.as_array_mut().unwrap();
            arr.push(json!("h100"));
        })
        .with_priority(100);

        let h10 = MockHook::modifier("h10", "*::*", HookPoint::RequestPreDispatch, |ctx| {
            let arr = ctx.payload.as_array_mut().unwrap();
            arr.push(json!("h10"));
        })
        .with_priority(10);

        let h50 = MockHook::modifier("h50", "*::*", HookPoint::RequestPreDispatch, |ctx| {
            let arr = ctx.payload.as_array_mut().unwrap();
            arr.push(json!("h50"));
        })
        .with_priority(50);

        reg.register(Box::new(h100));
        reg.register(Box::new(h10));
        reg.register(Box::new(h50));

        let mut ctx = test_ctx(HookPoint::RequestPreDispatch);
        ctx.payload = json!([]);

        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        if let HookAction::Continue(result) = action {
            // Should be ordered by priority: 10, 50, 100
            assert_eq!(result.payload, json!(["h10", "h50", "h100"]));
        } else {
            panic!("expected Continue");
        }
    }

    // ── FQL filtering ────────────────────────────────────────

    #[test]
    fn fql_filtering() {
        let mut reg = HookRegistry::new();

        let llm_only =
            MockHook::pass_through("llm-hook", "builtin::llm", HookPoint::RequestPreDispatch);
        let llm_counter = llm_only.call_count.clone();
        reg.register(Box::new(llm_only));

        let ctx = test_ctx(HookPoint::RequestPreDispatch);

        // Dispatch for LLM → should match
        reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx.clone(),
        );
        assert_eq!(llm_counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Dispatch for HIL → should NOT match
        reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("hil"),
            None,
            ctx,
        );
        assert_eq!(llm_counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ── Enabled/disabled ─────────────────────────────────────

    #[test]
    fn disabled_hook_skipped() {
        let mut reg = HookRegistry::new();
        let hook = MockHook::pass_through("h1", "*::*", HookPoint::RequestPreDispatch);
        let counter = hook.call_count.clone();
        reg.register(Box::new(hook));

        reg.set_enabled("h1", false);

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn re_enable_hook() {
        let mut reg = HookRegistry::new();
        let hook = MockHook::pass_through("h1", "*::*", HookPoint::RequestPreDispatch);
        let counter = hook.call_count.clone();
        reg.register(Box::new(hook));

        reg.set_enabled("h1", false);
        reg.set_enabled("h1", true);

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ── Depth exceeded ───────────────────────────────────────

    #[test]
    fn depth_exceeded_breaks_chain() {
        let mut reg = HookRegistry::new();
        let hook = MockHook::pass_through("h1", "*::*", HookPoint::RequestPreDispatch);
        let counter = hook.call_count.clone();
        reg.register(Box::new(hook));

        let mut ctx = test_ctx(HookPoint::RequestPreDispatch);
        ctx.depth = 4;
        ctx.max_depth = 4;

        reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        // Hook should NOT have been called
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // ── Unregister ───────────────────────────────────────────

    #[test]
    fn unregister_by_id() {
        let mut reg = HookRegistry::new();
        reg.register(Box::new(MockHook::pass_through(
            "h1",
            "*::*",
            HookPoint::RequestPreDispatch,
        )));
        assert_eq!(reg.len(), 1);

        assert!(reg.unregister("h1"));
        assert_eq!(reg.len(), 0);

        assert!(!reg.unregister("h1")); // Already gone
    }

    #[test]
    fn unregister_by_owner() {
        let mut reg = HookRegistry::new();
        let owner = ComponentId::builtin("llm");

        reg.register_owned(
            Box::new(MockHook::pass_through(
                "h1",
                "*::*",
                HookPoint::RequestPreDispatch,
            )),
            owner.clone(),
        );
        reg.register_owned(
            Box::new(MockHook::pass_through(
                "h2",
                "*::*",
                HookPoint::SignalPreDispatch,
            )),
            owner.clone(),
        );
        reg.register(Box::new(MockHook::pass_through(
            "h3",
            "*::*",
            HookPoint::RequestPreDispatch,
        )));

        assert_eq!(reg.len(), 3);

        let removed = reg.unregister_by_owner(&owner);
        assert_eq!(removed, 2);
        assert_eq!(reg.len(), 1); // h3 remains (no owner)
    }

    // ── Post-hook Replace ────────────────────────────────────

    #[test]
    fn post_hook_replace_updates_payload_and_continues_chain() {
        let mut reg = HookRegistry::new();

        let replacer = MockHook::replacer(
            "replacer",
            "*::*",
            HookPoint::RequestPostDispatch,
            json!({"replaced": true}),
        )
        .with_priority(10);

        let observer = MockHook::pass_through("observer", "*::*", HookPoint::RequestPostDispatch)
            .with_priority(20);
        let observer_counter = observer.call_count.clone();

        reg.register(Box::new(replacer));
        reg.register(Box::new(observer));

        let ctx = test_ctx(HookPoint::RequestPostDispatch);
        let action = reg.dispatch(
            HookPoint::RequestPostDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        // Chain should continue (not stop at Replace)
        assert_eq!(
            observer_counter.load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        // Final payload should be the replaced value
        if let HookAction::Continue(result) = action {
            assert_eq!(result.payload, json!({"replaced": true}));
        } else {
            panic!("expected Continue");
        }
    }

    #[test]
    fn pre_hook_replace_is_ignored() {
        let mut reg = HookRegistry::new();

        // Replace in a pre-hook should be ignored (treated as Continue)
        let replacer = MockHook::replacer(
            "bad-replacer",
            "*::*",
            HookPoint::RequestPreDispatch,
            json!({"should_not_replace": true}),
        );
        reg.register(Box::new(replacer));

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        let original_payload = ctx.payload.clone();
        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        // Should continue with original payload (Replace ignored)
        if let HookAction::Continue(result) = action {
            assert_eq!(result.payload, original_payload);
        } else {
            panic!("expected Continue");
        }
    }

    // ── Chain: multiple hooks modify sequentially ─────────────

    #[test]
    fn chain_hooks_modify_sequentially() {
        let mut reg = HookRegistry::new();

        let h1 = MockHook::modifier("h1", "*::*", HookPoint::RequestPreDispatch, |ctx| {
            if let Some(obj) = ctx.payload.as_object_mut() {
                obj.insert("h1".into(), json!(true));
            }
        })
        .with_priority(10);

        let h2 = MockHook::modifier("h2", "*::*", HookPoint::RequestPreDispatch, |ctx| {
            if let Some(obj) = ctx.payload.as_object_mut() {
                obj.insert("h2".into(), json!(true));
            }
        })
        .with_priority(20);

        reg.register(Box::new(h1));
        reg.register(Box::new(h2));

        let ctx = test_ctx(HookPoint::RequestPreDispatch);
        let action = reg.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        if let HookAction::Continue(result) = action {
            // Both hooks should have added their keys
            assert_eq!(result.payload["h1"], json!(true));
            assert_eq!(result.payload["h2"], json!(true));
            // Original key should still be there
            assert_eq!(result.payload["op"], json!("test"));
        } else {
            panic!("expected Continue");
        }
    }

    // ── Misc ─────────────────────────────────────────────────

    #[test]
    fn empty_registry() {
        let reg = HookRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn len_counts_across_points() {
        let mut reg = HookRegistry::new();
        reg.register(Box::new(MockHook::pass_through(
            "h1",
            "*::*",
            HookPoint::RequestPreDispatch,
        )));
        reg.register(Box::new(MockHook::pass_through(
            "h2",
            "*::*",
            HookPoint::SignalPreDispatch,
        )));
        assert_eq!(reg.len(), 2);
        assert!(!reg.is_empty());
    }
}

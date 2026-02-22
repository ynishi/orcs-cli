//! Hook trait and testing utilities.

use crate::{FqlPattern, HookAction, HookContext, HookPoint};

/// A single hook handler.
///
/// Hooks are registered with the [`HookRegistry`](crate::HookRegistry) and
/// invoked at specific lifecycle points. Each hook declares:
///
/// - An FQL pattern (which components it targets)
/// - A hook point (when it fires)
/// - A priority (execution order within the same point)
///
/// # Thread Safety
///
/// Hooks must be `Send + Sync` for concurrent access from multiple
/// channel runners.
pub trait Hook: Send + Sync {
    /// Unique identifier for this hook.
    fn id(&self) -> &str;

    /// FQL pattern this hook matches.
    fn fql_pattern(&self) -> &FqlPattern;

    /// Which lifecycle point this hook fires on.
    fn hook_point(&self) -> HookPoint;

    /// Priority (lower = earlier). Default: 100.
    fn priority(&self) -> i32 {
        100
    }

    /// Execute the hook with the given context.
    ///
    /// # Returns
    ///
    /// - `Continue(ctx)` — pass modified context to next hook / operation
    /// - `Skip(value)` — skip the operation (pre-hooks only)
    /// - `Abort { reason }` — abort the operation (pre-hooks only)
    /// - `Replace(value)` — replace result payload (post-hooks only)
    fn execute(&self, ctx: HookContext) -> HookAction;
}

/// Test utilities for the hook system.
#[cfg(any(test, feature = "test-utils"))]
pub mod testing {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A mock hook for testing.
    ///
    /// Returns a fixed `HookAction` on every `execute()` call.
    /// Tracks invocation count via `call_count`.
    pub struct MockHook {
        /// Hook ID.
        pub id: String,
        /// FQL pattern.
        pub fql: FqlPattern,
        /// Hook point.
        pub point: HookPoint,
        /// Priority.
        pub priority: i32,
        /// The action to return on every execute() call.
        pub action_fn: Box<dyn Fn(HookContext) -> HookAction + Send + Sync>,
        /// Number of times execute() has been called.
        pub call_count: Arc<AtomicUsize>,
    }

    impl MockHook {
        /// Creates a pass-through mock that returns `Continue(ctx)`.
        pub fn pass_through(id: &str, fql: &str, point: HookPoint) -> Self {
            Self {
                id: id.to_string(),
                fql: FqlPattern::parse(fql).expect("valid FQL for MockHook"),
                point,
                priority: 100,
                action_fn: Box::new(|ctx| HookAction::Continue(Box::new(ctx))),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a mock that modifies the payload via the given function.
        pub fn modifier(
            id: &str,
            fql: &str,
            point: HookPoint,
            modifier: impl Fn(&mut HookContext) + Send + Sync + 'static,
        ) -> Self {
            Self {
                id: id.to_string(),
                fql: FqlPattern::parse(fql).expect("valid FQL for MockHook"),
                point,
                priority: 100,
                action_fn: Box::new(move |mut ctx| {
                    modifier(&mut ctx);
                    HookAction::Continue(Box::new(ctx))
                }),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a mock that aborts with the given reason.
        pub fn aborter(id: &str, fql: &str, point: HookPoint, reason: &str) -> Self {
            let reason = reason.to_string();
            Self {
                id: id.to_string(),
                fql: FqlPattern::parse(fql).expect("valid FQL for MockHook"),
                point,
                priority: 100,
                action_fn: Box::new(move |_ctx| HookAction::Abort {
                    reason: reason.clone(),
                }),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a mock that skips with the given value.
        pub fn skipper(id: &str, fql: &str, point: HookPoint, value: serde_json::Value) -> Self {
            Self {
                id: id.to_string(),
                fql: FqlPattern::parse(fql).expect("valid FQL for MockHook"),
                point,
                priority: 100,
                action_fn: Box::new(move |_ctx| HookAction::Skip(value.clone())),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a mock that replaces with the given value.
        pub fn replacer(id: &str, fql: &str, point: HookPoint, value: serde_json::Value) -> Self {
            Self {
                id: id.to_string(),
                fql: FqlPattern::parse(fql).expect("valid FQL for MockHook"),
                point,
                priority: 100,
                action_fn: Box::new(move |_ctx| HookAction::Replace(value.clone())),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Sets the priority.
        #[must_use]
        pub fn with_priority(mut self, priority: i32) -> Self {
            self.priority = priority;
            self
        }

        /// Returns the number of times this hook has been executed.
        pub fn calls(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl Hook for MockHook {
        fn id(&self) -> &str {
            &self.id
        }

        fn fql_pattern(&self) -> &FqlPattern {
            &self.fql
        }

        fn hook_point(&self) -> HookPoint {
            self.point
        }

        fn priority(&self) -> i32 {
            self.priority
        }

        fn execute(&self, ctx: HookContext) -> HookAction {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            (self.action_fn)(ctx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::MockHook;
    use super::*;
    use orcs_types::{ChannelId, ComponentId, Principal};
    use serde_json::json;

    fn test_ctx() -> HookContext {
        HookContext::new(
            HookPoint::RequestPreDispatch,
            ComponentId::builtin("llm"),
            ChannelId::new(),
            Principal::System,
            0,
            json!({"op": "test"}),
        )
    }

    #[test]
    fn mock_pass_through() {
        let hook = MockHook::pass_through("test", "*::*", HookPoint::RequestPreDispatch);
        let ctx = test_ctx();
        let action = hook.execute(ctx.clone());
        assert!(action.is_continue());
        assert_eq!(hook.calls(), 1);
    }

    #[test]
    fn mock_aborter() {
        let hook = MockHook::aborter("test", "*::*", HookPoint::RequestPreDispatch, "blocked");
        let action = hook.execute(test_ctx());
        assert!(action.is_abort());
        if let HookAction::Abort { reason } = action {
            assert_eq!(reason, "blocked");
        }
    }

    #[test]
    fn mock_modifier() {
        let hook = MockHook::modifier("test", "*::*", HookPoint::RequestPreDispatch, |ctx| {
            ctx.payload = json!({"modified": true});
        });
        let action = hook.execute(test_ctx());
        if let HookAction::Continue(ctx) = action {
            assert_eq!(ctx.payload, json!({"modified": true}));
        } else {
            panic!("expected Continue");
        }
    }

    #[test]
    fn mock_priority() {
        let hook =
            MockHook::pass_through("test", "*::*", HookPoint::RequestPreDispatch).with_priority(50);
        assert_eq!(hook.priority(), 50);
    }

    #[test]
    fn mock_call_count_increments() {
        let hook = MockHook::pass_through("test", "*::*", HookPoint::RequestPreDispatch);
        hook.execute(test_ctx());
        hook.execute(test_ctx());
        hook.execute(test_ctx());
        assert_eq!(hook.calls(), 3);
    }

    #[test]
    fn hook_default_priority() {
        let hook = MockHook::pass_through("test", "*::*", HookPoint::RequestPreDispatch);
        assert_eq!(hook.priority(), 100);
    }
}

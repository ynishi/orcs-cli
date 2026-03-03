//! Cooperative kill flag for Lua execution cancellation.
//!
//! Two complementary kill mechanisms:
//!
//! 1. **Instruction hook** (`KillFlag` / `AtomicBool`):
//!    Terminates pure Lua code every ~128 instructions.
//!
//! 2. **Cancel channel** (`CancelToken` / `watch`):
//!    Cancels blocking FFI calls (`exec`, `llm`, `http`) via
//!    `tokio::select!` inside `block_in_place`.
//!
//! Both are triggered simultaneously on Veto signal.

use mlua::{HookTriggers, Lua, VmState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

/// Instruction check interval for the kill flag hook.
///
/// Lower values = faster cancellation but higher overhead.
/// 128 instructions is a good balance (~microseconds of Lua execution).
const HOOK_INTERVAL: u32 = 128;

// ─── KillFlag (instruction hook) ─────────────────────────────────────

/// Cooperative kill flag for Lua execution cancellation.
///
/// Clone is cheap (Arc clone). Multiple clones share the same flag.
#[derive(Clone)]
pub(crate) struct KillFlag(Arc<AtomicBool>);

impl KillFlag {
    /// Creates a new kill flag in the unset state.
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Sets the kill flag, causing the instruction hook to terminate
    /// Lua execution on its next check.
    pub(crate) fn set(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Resets the kill flag to allow new executions.
    pub(crate) fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// Returns whether the flag is currently set.
    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Returns a clone of the inner `Arc<AtomicBool>` for use in closures.
    pub(crate) fn inner(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }
}

// ─── CancelToken (watch channel for FFI cancel) ──────────────────────

/// Cancel token for cooperative cancellation of blocking FFI calls.
///
/// Wraps a `tokio::sync::watch` channel. The sender fires on Veto,
/// and FFI functions clone the receiver to `select!` against their
/// blocking operations.
#[derive(Clone)]
pub(crate) struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl CancelToken {
    /// Creates a new cancel token in the uncancelled state.
    pub(crate) fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// Fires the cancel signal, unblocking all FFI `select!` waiters.
    pub(crate) fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    /// Resets the cancel token for reuse.
    ///
    /// Creates a new watch channel so that stale receivers from
    /// previous requests don't interfere.
    pub(crate) fn reset(&mut self) {
        let (tx, rx) = watch::channel(false);
        self.tx = Arc::new(tx);
        self.rx = rx;
    }

    /// Returns a cloneable receiver for FFI functions.
    pub(crate) fn receiver(&self) -> watch::Receiver<bool> {
        self.rx.clone()
    }

    /// Returns a clone of the sender Arc for the interrupt bridge.
    ///
    /// The bridge task fires this sender when an out-of-band interrupt
    /// arrives, cancelling in-flight FFI calls without the Component Mutex.
    pub(crate) fn sender(&self) -> Arc<watch::Sender<bool>> {
        Arc::clone(&self.tx)
    }
}

/// Wrapper for storing the cancel receiver in Lua `app_data`.
///
/// FFI functions retrieve this to participate in cooperative cancellation.
pub(crate) struct CancelReceiverAppData(pub(crate) watch::Receiver<bool>);

/// Stores the cancel receiver in Lua `app_data`.
pub(crate) fn install_cancel_receiver(lua: &Lua, token: &CancelToken) {
    lua.set_app_data(CancelReceiverAppData(token.receiver()));
}

/// Retrieves a clone of the cancel receiver from Lua `app_data`.
///
/// Returns `None` if no cancel token is installed (e.g. in tests
/// that don't use LuaComponent).
pub(crate) fn get_cancel_receiver(lua: &Lua) -> Option<watch::Receiver<bool>> {
    lua.app_data_ref::<CancelReceiverAppData>()
        .map(|d| d.0.clone())
}

/// Checks whether the kill flag is currently set for the given Lua VM.
///
/// Used by the resolve loop to bail out early when a Veto signal
/// has been received but the in-flight LLM response arrived before
/// the cancel receiver could race it.
///
/// Returns `false` if no kill flag is installed (e.g. in tests).
pub(crate) fn is_killed(lua: &Lua) -> bool {
    lua.app_data_ref::<KillFlagAppData>()
        .map_or(false, |d| d.0.load(Ordering::Acquire))
}

// ─── Instruction hook installation ───────────────────────────────────

/// Installs the kill-flag instruction hook on the given Lua VM.
///
/// The hook fires every [`HOOK_INTERVAL`] instructions and checks
/// the `kill_flag`. If set, it raises `RuntimeError("killed by veto")`.
///
/// Also stores the `Arc<AtomicBool>` in Lua `app_data` so that
/// [`sandbox_eval`](crate::sandbox_eval) can restore the hook after
/// its own hook runs.
pub(crate) fn install_hook(lua: &Lua, kill_flag: &KillFlag) -> Result<(), mlua::Error> {
    let flag = kill_flag.inner();

    // Store in app_data for hook restoration (e.g. after sandbox_eval).
    lua.set_app_data(KillFlagAppData(Arc::clone(&flag)));

    lua.set_hook(
        HookTriggers::default().every_nth_instruction(HOOK_INTERVAL),
        move |_lua, _debug| {
            if flag.load(Ordering::Acquire) {
                Err(mlua::Error::RuntimeError("killed by veto".into()))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
}

/// Re-installs the kill-flag instruction hook from Lua `app_data`.
///
/// Called after code that replaces or removes the hook (e.g. `sandbox_eval`).
/// No-op if no `KillFlagAppData` is stored.
pub(crate) fn restore_hook(lua: &Lua) -> Result<(), mlua::Error> {
    let Some(app_data) = lua.app_data_ref::<KillFlagAppData>() else {
        return Ok(());
    };
    let flag = Arc::clone(&app_data.0);
    drop(app_data); // Release borrow before set_hook

    lua.set_hook(
        HookTriggers::default().every_nth_instruction(HOOK_INTERVAL),
        move |_lua, _debug| {
            if flag.load(Ordering::Acquire) {
                Err(mlua::Error::RuntimeError("killed by veto".into()))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
}

/// Wrapper for storing the kill flag in Lua `app_data`.
pub(crate) struct KillFlagAppData(Arc<AtomicBool>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_flag_is_unset() {
        let flag = KillFlag::new();
        assert!(!flag.is_set());
    }

    #[test]
    fn set_and_reset() {
        let flag = KillFlag::new();
        flag.set();
        assert!(flag.is_set());
        flag.reset();
        assert!(!flag.is_set());
    }

    #[test]
    fn clone_shares_state() {
        let flag1 = KillFlag::new();
        let flag2 = flag1.clone();
        flag1.set();
        assert!(flag2.is_set());
    }

    #[tokio::test]
    async fn hook_kills_infinite_loop() {
        let lua = Lua::new();
        let flag = KillFlag::new();
        install_hook(&lua, &flag).expect("install hook");

        let flag_clone = flag.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag_clone.set();
        });

        let start = std::time::Instant::now();
        let result =
            tokio::task::spawn_blocking(move || lua.load("while true do end").exec()).await;

        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "should kill quickly"
        );

        match result {
            Ok(Err(mlua::Error::RuntimeError(msg))) => {
                assert!(msg.contains("killed by veto"), "got: {msg}");
            }
            other => panic!("expected RuntimeError, got: {other:?}"),
        }
    }

    #[test]
    fn hook_does_not_kill_when_unset() {
        let lua = Lua::new();
        let flag = KillFlag::new();
        install_hook(&lua, &flag).expect("install hook");

        // Short computation should succeed
        let result: i64 = lua
            .load("local s = 0; for i = 1, 1000 do s = s + i end; return s")
            .eval()
            .expect("should succeed");
        assert_eq!(result, 500_500);
    }

    #[test]
    fn restore_hook_after_removal() {
        let lua = Lua::new();
        let flag = KillFlag::new();
        install_hook(&lua, &flag).expect("install hook");

        // Simulate sandbox_eval removing the hook
        lua.remove_hook();

        // Restore
        restore_hook(&lua).expect("restore hook");

        // Flag is set — next Lua execution should be killed
        flag.set();
        let result = lua
            .load("local s = 0; for i = 1, 10000 do s = s + i end; return s")
            .exec();
        assert!(result.is_err(), "should be killed after hook restore");
    }

    #[tokio::test]
    async fn cancel_token_fires() {
        let token = CancelToken::new();
        let mut rx = token.receiver();

        token.cancel();

        // Receiver should see the change
        rx.changed().await.expect("should receive cancel");
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn cancel_token_reset_isolates() {
        let mut token = CancelToken::new();
        let mut old_rx = token.receiver();

        token.reset();
        let mut new_rx = token.receiver();

        // Cancel after reset should only affect new receivers
        token.cancel();

        // Old receiver's sender is dropped → changed() returns Err
        assert!(old_rx.changed().await.is_err());
        // New receiver sees the cancel
        new_rx
            .changed()
            .await
            .expect("new rx should receive cancel");
        assert!(*new_rx.borrow());
    }

    #[tokio::test]
    async fn cancel_receiver_stored_in_app_data() {
        let lua = Lua::new();
        let token = CancelToken::new();
        install_cancel_receiver(&lua, &token);

        let rx = get_cancel_receiver(&lua);
        assert!(
            rx.is_some(),
            "should retrieve cancel receiver from app_data"
        );

        token.cancel();
        let mut rx = rx.expect("checked above");
        rx.changed()
            .await
            .expect("should receive cancel via app_data");
    }
}

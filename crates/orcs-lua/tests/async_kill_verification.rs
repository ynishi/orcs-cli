//! Verification tests for mlua AsyncThread + create_async_function kill mechanism.
//!
//! Key findings:
//! 1. `create_async_function` + `AsyncThread` enables Lua to yield at FFI boundaries
//! 2. `AsyncThread::drop` with `recycle=false` (default from `into_async()`) does NOT
//!    drop the inner Future — the coroutine is merely abandoned for Lua GC.
//!    `set_recyclable(true)` is `pub(crate)` and inaccessible from outside mlua.
//! 3. **Cooperative cancellation** (cancel signal INSIDE async fn) correctly cancels
//!    in-flight operations and drops resources including subprocesses with kill_on_drop(true).
//! 4. `set_hook` + KillFlag kills pure Lua code within ~128 instructions.
//! 5. Combined: cooperative cancel for async FFI + instruction hook for pure Lua = complete kill.

use mlua::{Lua, VmState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── Test 1: AsyncThread basic ───────────────────────────────────────

#[tokio::test]
async fn async_function_basic_roundtrip() {
    let lua = Lua::new();

    let async_add = lua
        .create_async_function(|_lua, (a, b): (i64, i64)| async move { Ok(a + b) })
        .expect("create_async_function");

    lua.globals()
        .set("async_add", async_add)
        .expect("set global");

    let thread: mlua::Thread = lua
        .load(
            r#"
            coroutine.create(function()
                return async_add(10, 32)
            end)
        "#,
        )
        .eval()
        .expect("create thread");

    let async_thread = thread.into_async::<i64>(()).expect("into_async");
    let result = async_thread.await.expect("await result");
    assert_eq!(result, 42);
}

// ─── Test 2: Cooperative cancellation drops inner Future ─────────────
//
// AsyncThread::drop alone does NOT cancel the inner Future (recycle=false).
// Instead, we pass a cancel signal INTO the async function via watch channel.
// When the signal fires, the async fn's inner select! resolves, dropping resources.

#[tokio::test]
async fn cooperative_cancel_drops_inner_future() {
    let lua = Lua::new();

    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_clone = Arc::clone(&dropped);

    // Watch channel for cooperative cancellation
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let slow_fn = lua
        .create_async_function(move |_lua, ()| {
            let dropped = Arc::clone(&dropped_clone);
            let mut rx = cancel_rx.clone();
            async move {
                let _guard = DropGuard(dropped);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(42i64),
                    _ = async { let _ = rx.changed().await; } => {
                        // Cancelled — DropGuard dropped when this async block ends
                        Err(mlua::Error::external("cancelled by veto"))
                    }
                }
            }
        })
        .expect("create_async_function");

    lua.globals().set("slow_fn", slow_fn).expect("set global");

    let thread: mlua::Thread = lua
        .load(
            r#"
            coroutine.create(function()
                return slow_fn()
            end)
        "#,
        )
        .eval()
        .expect("create thread");

    let async_thread = thread.into_async::<mlua::Value>(()).expect("into_async");

    // Send cancel signal after 50ms
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = cancel_tx.send(true);
    });

    let start = Instant::now();
    let result = async_thread.await;

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "Should cancel quickly, took {:?}",
        elapsed
    );

    // The async function returned Err (cancelled), DropGuard was dropped
    assert!(result.is_err(), "Should have been cancelled with error");
    assert!(
        dropped.load(Ordering::Acquire),
        "DropGuard should have been triggered (inner Future resources dropped)"
    );
}

struct DropGuard(Arc<AtomicBool>);
impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

// ─── Test 3: Cooperative cancel + kill_on_drop kills subprocess ──────
//
// spawn() + kill_on_drop(true) + cooperative select! inside async fn.
// When cancel signal fires, select! resolves → child dropped → SIGKILL.

#[tokio::test]
async fn cooperative_cancel_kills_subprocess() {
    let lua = Lua::new();

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let exec_fn = lua
        .create_async_function(move |_lua, ()| {
            let mut rx = cancel_rx.clone();
            async move {
                let mut child = tokio::process::Command::new("sleep")
                    .arg("60")
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(mlua::Error::external)?;

                tokio::select! {
                    status = child.wait() => {
                        let status = status.map_err(mlua::Error::external)?;
                        Ok(status.code().unwrap_or(-1))
                    },
                    _ = async { let _ = rx.changed().await; } => {
                        // child dropped here → kill_on_drop sends SIGKILL
                        Err(mlua::Error::external("cancelled"))
                    }
                }
            }
        })
        .expect("create_async_function");

    lua.globals().set("exec_fn", exec_fn).expect("set global");

    let thread: mlua::Thread = lua
        .load(
            r#"
            coroutine.create(function()
                return exec_fn()
            end)
        "#,
        )
        .eval()
        .expect("create thread");

    let async_thread = thread.into_async::<mlua::Value>(()).expect("into_async");

    // Cancel after 100ms
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cancel_tx.send(true);
    });

    let start = Instant::now();
    let result = async_thread.await;

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "Should cancel subprocess quickly, took {:?}",
        elapsed
    );
    assert!(result.is_err(), "Should have been cancelled");

    // Verify no orphan `sleep 60` processes remain
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ps_output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("pgrep -f 'sleep 60' || true")
        .output()
        .await
        .expect("pgrep");
    let remaining = String::from_utf8_lossy(&ps_output.stdout);
    assert!(
        remaining.trim().is_empty(),
        "Orphan 'sleep 60' processes found: {}",
        remaining.trim()
    );
}

// ─── Test 4: Instruction hook kills pure Lua code ───────────────────

#[tokio::test]
async fn instruction_hook_kills_infinite_lua_loop() {
    let lua = Lua::new();

    let kill_flag = Arc::new(AtomicBool::new(false));
    let flag_for_hook = Arc::clone(&kill_flag);

    lua.set_hook(
        mlua::HookTriggers::default().every_nth_instruction(128),
        move |_lua, _debug| {
            if flag_for_hook.load(Ordering::Acquire) {
                Err(mlua::Error::RuntimeError("cancelled by kill signal".into()))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .expect("set_hook");

    let chunk = lua.load("local i = 0; while true do i = i + 1 end; return i");

    let flag_for_killer = Arc::clone(&kill_flag);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        flag_for_killer.store(true, Ordering::Release);
    });

    let start = Instant::now();
    let result = tokio::task::spawn_blocking(move || chunk.exec()).await;

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "Should kill within ~128 instructions after flag set, took {:?}",
        elapsed
    );

    match result {
        Ok(Err(mlua::Error::RuntimeError(msg))) => {
            assert!(
                msg.contains("cancelled by kill signal"),
                "Expected cancel message, got: {}",
                msg
            );
        }
        other => panic!(
            "Expected RuntimeError('cancelled by kill signal'), got: {:?}",
            other
        ),
    }
}

// ─── Test 5: Combined — cooperative cancel + instruction hook ────────
//
// Complete Veto kill chain:
//   Veto signal → (1) KillFlag for instruction hook (pure Lua)
//                 (2) watch::send for cooperative cancel (async FFI)
// Lua is either in pure code (killed by hook) or in async FFI (killed by cancel).

#[tokio::test]
async fn combined_cooperative_cancel_and_instruction_hook() {
    let lua = Lua::new();

    let kill_flag = Arc::new(AtomicBool::new(false));
    let flag_for_hook = Arc::clone(&kill_flag);

    // Instruction hook for pure Lua code
    lua.set_hook(
        mlua::HookTriggers::default().every_nth_instruction(128),
        move |_lua, _debug| {
            if flag_for_hook.load(Ordering::Acquire) {
                Err(mlua::Error::RuntimeError("cancelled".into()))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .expect("set_hook");

    // Watch channel for async FFI cooperative cancel
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let async_sleep = lua
        .create_async_function(move |_lua, secs: f64| {
            let mut rx = cancel_rx.clone();
            async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs_f64(secs)) => Ok(true),
                    _ = async { let _ = rx.changed().await; } => {
                        Err(mlua::Error::external("cancelled by veto"))
                    }
                }
            }
        })
        .expect("create_async_function");

    lua.globals()
        .set("async_sleep", async_sleep)
        .expect("set global");

    let thread: mlua::Thread = lua
        .load(
            r#"
            coroutine.create(function()
                -- Pure Lua work (killable via instruction hook)
                local sum = 0
                for i = 1, 1000 do sum = sum + i end

                -- Async FFI call (killable via cooperative cancel)
                async_sleep(60)

                return sum
            end)
        "#,
        )
        .eval()
        .expect("create thread");

    let async_thread = thread.into_async::<mlua::Value>(()).expect("into_async");

    // Simulate Veto after 100ms — fire BOTH kill mechanisms
    let flag_for_veto = Arc::clone(&kill_flag);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        flag_for_veto.store(true, Ordering::Release); // for instruction hook
        let _ = cancel_tx.send(true); // for cooperative async cancel
    });

    let start = Instant::now();
    let result = async_thread.await;

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "Combined kill should be fast, took {:?}",
        elapsed
    );

    // Should have been cancelled (either by hook error or cooperative cancel)
    assert!(
        result.is_err(),
        "Should have been cancelled, got: {:?}",
        result
    );
}

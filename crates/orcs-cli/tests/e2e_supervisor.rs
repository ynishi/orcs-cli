//! E2E tests for supervisor completion feature.
//!
//! Verifies exit_reason logging and shutdown behaviour added by the
//! `feat/supervisor-completion` branch.
//!
//! Tests that require killing individual runners mid-session (concierge
//! restart, delegate-worker restart, restart intensity) cannot be
//! automated here — they require manual testing.

mod common;

use common::spawn_and_wait_for;

// ─── Exit Reason Logging ──────────────────────────────────────────

/// Verifies that ClientRunner logs `exit_reason` during clean shutdown.
///
/// Flow: user sends "q" → App calls engine.stop() → Veto → runners break
/// with exit_reason=Signal → ClientRunner logs "stopped (reason=Signal)".
///
/// Uses `spawn_and_wait_for` to gate quit on "Interactive mode started",
/// ensuring components have initialised before shutdown begins.
#[test]
fn client_runner_logs_exit_reason_on_shutdown() {
    let tmp = tempfile::tempdir().expect("create temp dir for sandbox");
    let (stdout, stderr) = spawn_and_wait_for("Interactive mode started", &["-d"], tmp.path());

    let _dump = common::dump_test_output("supervisor_exit_reason", &stdout, &stderr);

    assert!(
        stdout.contains("reason="),
        "ClientRunner should log exit_reason during shutdown.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// Verifies that shutdown completes cleanly after Veto signal propagation.
///
/// All runners should stop with Signal (not a crash or timeout).
/// "Shutting down" log confirms the engine completed shutdown_parallel().
#[test]
fn clean_shutdown_after_veto() {
    let tmp = tempfile::tempdir().expect("create temp dir for sandbox");
    let (stdout, stderr) = spawn_and_wait_for("[AgentMgr] Ready", &["-d"], tmp.path());

    let _dump = common::dump_test_output("supervisor_clean_shutdown", &stdout, &stderr);

    // Engine completed shutdown_parallel() successfully.
    assert!(
        stdout.contains("Shutting down"),
        "Engine should log shutdown completion.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // agent_mgr received shutdown callback.
    assert!(
        stdout.contains("agent_mgr shutdown"),
        "agent_mgr should execute shutdown callback.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// Verifies that both concierge and delegate-worker are spawned and their
/// FQNs appear in the Ready message — prerequisite for supervisor restart
/// to have something to track.
#[test]
fn supervisor_managed_runners_spawned() {
    let tmp = tempfile::tempdir().expect("create temp dir for sandbox");
    let (stdout, stderr) = spawn_and_wait_for(
        "[AgentMgr] Ready (backend: builtin::concierge, delegate: ok",
        &["-d"],
        tmp.path(),
    );

    let _dump = common::dump_test_output("supervisor_managed_runners", &stdout, &stderr);

    assert!(
        stdout.contains("spawned concierge agent"),
        "Concierge should be spawned.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("spawned delegate-worker agent"),
        "Delegate-worker should be spawned.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("delegate: ok"),
        "Ready message should confirm delegate-worker is running.\nstdout:\n{stdout}"
    );
}

// ─── Exit Reason NOT Leaked in Base ChannelRunner ─────────────────

/// Verifies that the base ChannelRunner's stop log does NOT contain
/// exit_reason (it only says "ChannelRunner stopped").
///
/// This is by design: exit_reason is reported by ClientRunner and the
/// engine monitor, not the base runner. Verifying absence ensures the
/// base runner wasn't accidentally modified.
#[test]
fn channel_runner_stop_log_does_not_contain_reason() {
    let tmp = tempfile::tempdir().expect("create temp dir for sandbox");
    let (stdout, stderr) = spawn_and_wait_for("Interactive mode started", &["-d"], tmp.path());

    let _dump = common::dump_test_output("supervisor_channel_runner", &stdout, &stderr);

    // Base ChannelRunner log lines should NOT contain "reason=".
    for line in stdout.lines() {
        if line.contains("ChannelRunner stopped") {
            assert!(
                !line.contains("reason="),
                "Base ChannelRunner should NOT log exit_reason.\nLine: {line}"
            );
        }
    }
}

//! E2E integration tests for the `orcs` binary.
//!
//! Tests the binary's stdin/stdout interface by spawning real subprocesses.
//! tracing output goes to stdout; eprintln (errors) goes to stderr.

mod common;

use common::orcs_cmd;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

// ─── Startup / Shutdown ────────────────────────────────────────────

#[test]
fn quit_immediately() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("Interactive mode started"))
        .stdout(contains("Session ID:"))
        .stdout(contains("Quit requested"));
}

#[test]
fn quit_long_form() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("quit\n")
        .assert()
        .success()
        .stdout(contains("Quit requested"));
}

#[test]
fn empty_stdin_exits_gracefully() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("").assert().success();
}

// ─── Component Ready Messages ──────────────────────────────────────

#[test]
fn components_initialize() {
    // Check spawn messages (synchronous in builder, before interactive mode)
    // rather than Ready messages which race with stdin processing.
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("builtin::agent_mgr"))
        .stdout(contains("builtin::shell"))
        .stdout(contains("builtin::tool"));
}

#[test]
fn profile_manager_initializes() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("profile::profile_manager"))
        .stdout(contains("ProfileManager initialized"));
}

#[test]
fn agent_mgr_ready_with_workers() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("[AgentMgr] Ready (workers: llm, skill)"));
}

// ─── Worker Spawning ─────────────────────────────────────────────

#[test]
fn agent_mgr_spawns_workers() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("spawned llm-worker"))
        .stdout(contains("spawned skill-worker"));
}

// ─── Component Routes ───────────────────────────────────────────

#[test]
fn all_component_routes_registered() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("Component routes registered"))
        .stdout(contains("agent_mgr"))
        .stdout(contains("profile_manager"))
        .stdout(contains("skill_manager"))
        .stdout(contains("shell"))
        .stdout(contains("tool"));
}

// ─── Shutdown ───────────────────────────────────────────────────

#[test]
fn clean_shutdown_logs() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("agent_mgr shutdown"))
        .stdout(contains("ProfileManager shutdown"))
        .stdout(contains("Shutting down"));
}

// ─── Help Command ──────────────────────────────────────────────────

#[test]
fn help_shows_commands() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("help\nq\n")
        .assert()
        .success()
        .stdout(contains("Commands:"))
        .stdout(contains("y [id]"))
        .stdout(contains("q / quit"));
}

// ─── Component Routing (@target) ───────────────────────────────────

#[test]
fn at_shell_routes_to_shell() {
    // With -d, debug log "Routed @shell to channel" confirms routing.
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("@shell echo hello_e2e\nq\n")
        .assert()
        .success()
        .stdout(contains("Routed @shell to channel"));
}

#[test]
fn at_profile_manager_routes() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("@profile_manager list\nq\n")
        .assert()
        .success()
        .stdout(contains("Routed @profile_manager to channel"));
}

#[test]
fn at_unknown_component_shows_error() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("@nonexistent test\nq\n")
        .assert()
        .success()
        .stderr(contains("Unknown component: @nonexistent"));
}

// ─── Skill Discovery ──────────────────────────────────────────────

#[test]
fn skill_manager_discovers_skills() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("SkillManager initialized"));
}

// ─── Pause / Session Save ──────────────────────────────────────────

#[test]
fn pause_saves_session() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("p\n")
        .assert()
        .success()
        .stdout(contains("Saving session").or(contains("Pause requested")));
}

// ─── Resume Session ────────────────────────────────────────────────

#[test]
fn resume_nonexistent_session_returns_error() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.args(["--resume", "00000000-0000-0000-0000-000000000000"])
        .write_stdin("q\n")
        .assert()
        .failure()
        .stderr(contains("session not found"));
}

// ─── Blank Input ───────────────────────────────────────────────────

#[test]
fn blank_lines_are_ignored() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("\n\n\nq\n").assert().success();
}

// ─── User Message → dispatch_llm → orcs.output ──────────────────────

/// Sends a plain message (no @prefix) so agent_mgr routes to dispatch_llm.
/// dispatch_llm runs the llm-worker → orcs.llm() → claude CLI.
///
/// Uses a **mock claude script** that returns instantly with a known response,
/// eliminating external dependency on a real LLM and making the test fast
/// (~3 seconds instead of minutes).
///
/// Verifies the full success pipeline:
///   user input → agent_mgr → dispatch_llm → llm-worker → orcs.llm()
///   → mock claude → orcs.output(response) → Emitter → output_tx
///   → ConsoleRenderer → stdout
#[test]
fn dispatch_llm_output_reaches_stdout() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc as std_mpsc;
    use std::thread;
    use std::time::Duration;

    let tmp = tempfile::tempdir().expect("create temp dir");
    let bin = assert_cmd::cargo::cargo_bin!("orcs");

    // Create a mock `claude` script that returns a known response instantly.
    let mock_claude = tmp.path().join("claude");
    std::fs::write(&mock_claude, "#!/bin/sh\necho \"MOCK_LLM_RESPONSE_42\"\n")
        .expect("write mock claude script");
    std::fs::set_permissions(&mock_claude, std::fs::Permissions::from_mode(0o755))
        .expect("set mock claude executable");

    // Prepend tmpdir to PATH so orcs finds our mock claude first.
    let original_path = std::env::var("PATH").unwrap_or_default();
    let mock_path = format!("{}:{}", tmp.path().display(), original_path);

    let mut child = Command::new(&bin)
        .arg("-d")
        .arg("--builtins-dir")
        .arg(tmp.path())
        .env("PATH", &mock_path)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_CODE_SSE_PORT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orcs process");

    let mut stdin = child.stdin.take().expect("open stdin pipe");
    let child_stdout = child.stdout.take().expect("open stdout pipe");
    let child_stderr = child.stderr.take().expect("open stderr pipe");

    // Read stdout in a background thread, signalling when dispatch completes.
    let (done_tx, done_rx) = std_mpsc::channel::<()>();
    let stdout_thread = thread::spawn(move || {
        let mut lines = Vec::new();
        let reader = BufReader::new(child_stdout);
        let mut notified = false;
        for line in reader.lines() {
            let line = line.expect("read stdout line");
            // Detect dispatch_llm completion:
            //   "component returned success" (base.rs DEBUG log)
            //   "component returned error"   (base.rs WARN log)
            if !notified
                && (line.contains("component returned success")
                    || line.contains("component returned error"))
            {
                // Small delay to let ConsoleRenderer flush.
                thread::sleep(Duration::from_millis(200));
                let _ = done_tx.send(());
                notified = true;
            }
            lines.push(line);
        }
        if !notified {
            let _ = done_tx.send(());
        }
        lines.join("\n")
    });

    let stderr_thread = thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(child_stderr);
        reader
            .read_to_string(&mut buf)
            .expect("read stderr to string");
        buf
    });

    // Send user message — agent_mgr will dispatch to llm-worker → mock claude.
    writeln!(stdin, "test_msg_e2e").expect("write user message");
    stdin.flush().expect("flush message to stdin");

    // Wait for dispatch_llm to complete. Mock claude returns instantly,
    // so this should complete in a few seconds.
    let max_wait = Duration::from_secs(30);
    let dispatch_completed = done_rx.recv_timeout(max_wait).is_ok();

    // Send quit AFTER dispatch completes — IOPort channel is still open.
    writeln!(stdin, "q").expect("write quit command");
    drop(stdin);

    let stdout = stdout_thread.join().expect("join stdout thread");
    let stderr = stderr_thread.join().expect("join stderr thread");

    // Dump output for post-mortem inspection.
    let dump_dir = std::env::temp_dir().join("orcs_e2e_dump");
    std::fs::create_dir_all(&dump_dir).expect("create dump dir");
    std::fs::write(dump_dir.join("stdout.txt"), stdout.as_bytes()).expect("write stdout dump");
    std::fs::write(dump_dir.join("stderr.txt"), stderr.as_bytes()).expect("write stderr dump");

    eprintln!("=== DISPATCH_LLM DEBUG ===");
    eprintln!("dispatch_completed={dispatch_completed}");
    eprintln!("dump_dir={}", dump_dir.display());
    eprintln!("stdout_len={} stderr_len={}", stdout.len(), stderr.len());
    eprintln!("=== END DEBUG ===");

    // 1. Verify agent_mgr received the message.
    assert!(
        stdout.contains("AgentMgr received: test_msg_e2e"),
        "agent_mgr should log receipt of message.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // 2. Verify dispatch_llm completed (not timed out).
    assert!(
        dispatch_completed,
        "dispatch_llm should have completed within {max_wait:?}.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // 3. Verify the mock LLM response reached stdout via IOPort.
    //    orcs.output(response) → Emitter → output_tx → ConsoleRenderer → stdout.
    let has_mock_response = stdout.contains("MOCK_LLM_RESPONSE_42");
    let has_error_on_stderr = stderr.contains("[AgentMgr] Error:");
    let output_channel_closed = stdout.contains("output_tx send failed");

    eprintln!("has_mock_response={has_mock_response}");
    eprintln!("has_error_on_stderr={has_error_on_stderr}");
    eprintln!("output_channel_closed={output_channel_closed}");

    // The IOPort channel must NOT have been closed before output arrived.
    assert!(
        !output_channel_closed,
        "BUG: IOPort channel closed before orcs.output() could deliver.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The mock LLM response must appear in stdout (success path).
    assert!(
        has_mock_response,
        "Expected mock LLM response 'MOCK_LLM_RESPONSE_42' in stdout.\n\
         This verifies the full pipeline: dispatch_llm → orcs.output() → IOPort → stdout.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

// ─── Version / Help Flags ──────────────────────────────────────────

#[test]
fn version_flag() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(contains("orcs"));
}

#[test]
fn help_flag() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(contains("ORCS CLI"));
}

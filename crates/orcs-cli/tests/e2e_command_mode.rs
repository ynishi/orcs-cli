//! E2E tests for non-interactive command mode (`orcs "command"`).
//!
//! Tests the `args.command` path in main.rs where the binary
//! receives a command via trailing arguments instead of stdin.

mod common;

use common::orcs_cmd;
use predicates::str::contains;

// ─── Basic Command Mode ──────────────────────────────────────────

#[test]
fn command_mode_empty_args_enters_interactive() {
    // No trailing args → interactive mode (exits on empty stdin).
    let (mut cmd, _guard) = orcs_cmd();
    cmd.write_stdin("").assert().success();
}

#[test]
fn command_mode_at_unknown_component_fails() {
    // @nonexistent should produce an error on stderr.
    let (mut cmd, _guard) = orcs_cmd();
    cmd.args(["@nonexistent", "test"])
        .assert()
        .stderr(contains("Unknown component: @nonexistent"));
}

#[test]
fn command_mode_at_shell_routes_but_needs_approval() {
    // @shell routes to the shell component, but shell requires HIL
    // approval. Command mode cannot provide approval → exit 1.
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .args(["@shell", "echo", "hello_cmd_mode"])
        .assert()
        .failure()
        .stdout(contains("Routed @shell to channel"))
        .stderr(contains("interactive approval"));
}

// ─── Mock LLM Pipeline (full command mode) ──────────────────────

/// Sends a plain message in command mode (no @prefix).
/// agent_mgr dispatches to llm-worker → mock claude → stdout.
///
/// Uses a mock `claude` script that returns instantly.
/// Verifies: user input → agent_mgr → dispatch_llm → mock claude
///   → orcs.output(response) → Emitter → IOPort → stdout
#[test]
fn command_mode_llm_pipeline() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let tmp = tempfile::tempdir().expect("create temp dir");
    let bin = assert_cmd::cargo::cargo_bin!("orcs");

    // Create a mock `claude` script that returns a known response.
    let mock_claude = tmp.path().join("claude");
    std::fs::write(&mock_claude, "#!/bin/sh\necho \"CMD_MODE_RESPONSE_99\"\n")
        .expect("write mock claude script");
    std::fs::set_permissions(&mock_claude, std::fs::Permissions::from_mode(0o755))
        .expect("set mock claude executable");

    // Prepend tmpdir to PATH so orcs finds our mock claude first.
    let original_path = std::env::var("PATH").unwrap_or_default();
    let mock_path = format!("{}:{}", tmp.path().display(), original_path);

    let child = Command::new(&bin)
        .arg("-d")
        .arg("--builtins-dir")
        .arg(tmp.path())
        // The trailing args form the command
        .arg("test_cmd_mode_msg")
        .env("PATH", &mock_path)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_CODE_SSE_PORT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orcs process");

    // Command mode should exit on its own (no stdin needed).
    let output = child.wait_with_output().expect("wait for orcs to complete");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Dump for debugging
    let dump_dir = std::env::temp_dir().join("orcs_cmd_mode_dump");
    std::fs::create_dir_all(&dump_dir).expect("create dump dir");
    std::fs::write(dump_dir.join("stdout.txt"), stdout.as_bytes()).expect("write stdout dump");
    std::fs::write(dump_dir.join("stderr.txt"), stderr.as_bytes()).expect("write stderr dump");

    eprintln!("=== CMD_MODE DEBUG ===");
    eprintln!("exit_status={:?}", output.status);
    eprintln!("stdout_len={} stderr_len={}", stdout.len(), stderr.len());
    eprintln!("=== END DEBUG ===");

    // 1. Verify agent_mgr received the message.
    assert!(
        stdout.contains("AgentMgr received: test_cmd_mode_msg"),
        "agent_mgr should log receipt of command.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // 2. Verify the mock LLM response reached stdout.
    let has_mock_response = stdout.contains("CMD_MODE_RESPONSE_99");

    eprintln!("has_mock_response={has_mock_response}");

    assert!(
        has_mock_response,
        "Expected mock LLM response 'CMD_MODE_RESPONSE_99' in stdout.\n\
         This verifies the full command-mode pipeline.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

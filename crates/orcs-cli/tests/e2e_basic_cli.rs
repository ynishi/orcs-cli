//! E2E integration tests for the `orcs` binary.
//!
//! Tests the binary's stdin/stdout interface by spawning real subprocesses.
//! tracing output goes to stdout; eprintln (errors) goes to stderr.

mod common;

use common::{orcs_cmd, spawn_and_wait_for};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

// ─── Startup / Shutdown ────────────────────────────────────────────

#[test]
fn quit_immediately() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("Interactive mode started"))
        .stdout(contains("Session ID:"))
        .stdout(contains("Quit requested"));
}

#[test]
fn quit_long_form() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("quit\n")
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
    // Requires -d (debug mode) because spawn logs are at info! level.
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
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

/// Verifies AgentMgr emits Ready message with backend and delegate info.
///
/// Uses [`spawn_and_wait_for`] instead of `write_stdin("q\n")` to avoid
/// the stdin-vs-init race condition. See `common/mod.rs` module doc for
/// the full explanation of why `write_stdin` is racy for async output.
#[test]
fn agent_mgr_ready_with_workers() {
    let tmp = tempfile::tempdir().expect("create temp dir for sandbox");
    let (stdout, _stderr) = spawn_and_wait_for(
        "[AgentMgr] Ready (backend: builtin::concierge, delegate: ok, agents: none)",
        &["-d"],
        tmp.path(),
    );
    assert!(
        stdout
            .contains("[AgentMgr] Ready (backend: builtin::concierge, delegate: ok, agents: none)"),
        "Expected Ready message with delegate status in stdout.\nstdout:\n{stdout}"
    );
}

// ─── Worker Spawning ─────────────────────────────────────────────

#[test]
fn agent_mgr_spawns_workers() {
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("spawned concierge agent"));
}

/// Verifies that both concierge and delegate-worker agents are spawned
/// via the builtin resolution mechanism (spawn_runner({ builtin = "..." })).
#[test]
fn agent_mgr_spawns_both_agents() {
    let tmp = tempfile::tempdir().expect("create temp dir for sandbox");
    let (stdout, _stderr) = spawn_and_wait_for("[AgentMgr] Ready", &["-d"], tmp.path());
    assert!(
        stdout.contains("spawned concierge agent"),
        "Expected concierge agent spawn message.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("spawned delegate-worker agent"),
        "Expected delegate-worker agent spawn message.\nstdout:\n{stdout}"
    );
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
    // Requires -d so that component init completes before quit is processed.
    let (mut cmd, _guard) = orcs_cmd();
    cmd.arg("-d")
        .write_stdin("q\n")
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

/// Sends a plain message (no @prefix) so agent_mgr emits an AgentTask event.
/// The concierge (subscribed to AgentTask) processes the event asynchronously,
/// calling orcs.llm() → HTTP LLM API.
///
/// Uses a **mock HTTP server** that returns an Ollama-format response instantly,
/// eliminating external dependency on a real LLM and making the test fast
/// (~3 seconds instead of minutes).
///
/// Verifies the full event-driven pipeline:
///   user input → agent_mgr → emit_event(AgentTask) → concierge (subscriber)
///   → orcs.llm() → mock HTTP server → orcs.output(response) → Emitter
///   → output_tx → ConsoleRenderer → stdout
#[test]
fn dispatch_llm_output_reaches_stdout() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::sync::mpsc as std_mpsc;
    use std::thread;
    use std::time::Duration;

    let tmp = tempfile::tempdir().expect("create temp dir");
    let bin = assert_cmd::cargo::cargo_bin!("orcs");

    // Start a mock HTTP server that mimics Ollama's POST /api/chat response.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock HTTP server");
    let mock_port = listener.local_addr().expect("get mock port").port();
    let mock_base_url = format!("http://127.0.0.1:{}", mock_port);

    // Mock server thread: accepts one connection, returns Ollama-format JSON.
    let server_thread = thread::spawn(move || {
        // Accept connections until the test ends.
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };

            // Read request headers (we don't need to parse them carefully).
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                // Check if we've received the full request (headers + body).
                // Simple heuristic: look for double CRLF marking end of headers,
                // then read Content-Length bytes of body.
                let req_str = String::from_utf8_lossy(&request);
                if let Some(header_end) = req_str.find("\r\n\r\n") {
                    // Check Content-Length
                    let headers = &req_str[..header_end];
                    let content_length: usize = headers
                        .lines()
                        .find(|l| l.to_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    if request.len() >= body_start + content_length {
                        break;
                    }
                }
            }

            // Return Ollama-format JSON response.
            let ollama_response = r#"{"model":"mock-model","message":{"role":"assistant","content":"MOCK_LLM_RESPONSE_42"},"done":true,"done_reason":"stop"}"#;
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                ollama_response.len(),
                ollama_response
            );
            let _ = stream.write_all(http_response.as_bytes());
            let _ = stream.flush();
        }
    });

    let mut child = Command::new(&bin)
        .arg("-d")
        .arg("--sandbox")
        .arg(tmp.path())
        .env("ORCS_LLM_BASE_URL", &mock_base_url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orcs process");

    let mut stdin = child.stdin.take().expect("open stdin pipe");
    let child_stdout = child.stdout.take().expect("open stdout pipe");
    let child_stderr = child.stderr.take().expect("open stderr pipe");

    // Read stdout in a background thread, signalling when the concierge completes.
    //
    // With event-driven dispatch, agent_mgr returns immediately after emitting
    // the AgentTask event. The concierge processes asynchronously and outputs
    // the response via orcs.output(). We detect completion by observing:
    //   - The mock LLM response text (success path)
    //   - "concierge: completed" debug log (success path, if response is empty)
    //   - "[concierge] Error:" prefix (failure path)
    let (done_tx, done_rx) = std_mpsc::channel::<()>();
    let stdout_thread = thread::spawn(move || {
        let mut lines = Vec::new();
        let reader = BufReader::new(child_stdout);
        let mut notified = false;
        for line in reader.lines() {
            let line = line.expect("read stdout line");
            if !notified
                && (line.contains("MOCK_LLM_RESPONSE_42")
                    || line.contains("concierge: completed")
                    || line.contains("[concierge] Error:"))
            {
                // Small delay to let ConsoleRenderer flush remaining output.
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

    // Send user message — agent_mgr emits AgentTask event → concierge → mock HTTP server.
    writeln!(stdin, "test_msg_e2e").expect("write user message");
    stdin.flush().expect("flush message to stdin");

    // Wait for concierge to complete. Mock server returns instantly,
    // so this should complete in a few seconds.
    let max_wait = Duration::from_secs(30);
    let worker_completed = done_rx.recv_timeout(max_wait).is_ok();

    // Send quit AFTER worker completes — IOPort channel is still open.
    writeln!(stdin, "q").expect("write quit command");
    drop(stdin);

    let stdout = stdout_thread.join().expect("join stdout thread");
    let stderr = stderr_thread.join().expect("join stderr thread");
    drop(server_thread);

    // Dump output for post-mortem inspection.
    let dump_dir = std::env::temp_dir().join("orcs_e2e_dump");
    std::fs::create_dir_all(&dump_dir).expect("create dump dir");
    std::fs::write(dump_dir.join("stdout.txt"), stdout.as_bytes()).expect("write stdout dump");
    std::fs::write(dump_dir.join("stderr.txt"), stderr.as_bytes()).expect("write stderr dump");

    eprintln!("=== DISPATCH_LLM DEBUG ===");
    eprintln!("worker_completed={worker_completed}");
    eprintln!("dump_dir={}", dump_dir.display());
    eprintln!("stdout_len={} stderr_len={}", stdout.len(), stderr.len());
    eprintln!("=== END DEBUG ===");

    // 1. Verify agent_mgr received the message.
    assert!(
        stdout.contains("AgentMgr received: test_msg_e2e"),
        "agent_mgr should log receipt of message.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // 2. Verify concierge completed (not timed out).
    assert!(
        worker_completed,
        "concierge should have completed within {max_wait:?}.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // 3. Verify the mock LLM response reached stdout via IOPort.
    //    concierge → orcs.output(response) → Emitter → output_tx → ConsoleRenderer → stdout.
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

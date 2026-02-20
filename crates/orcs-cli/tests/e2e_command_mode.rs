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
/// agent_mgr dispatches to llm-worker → mock HTTP LLM server → stdout.
///
/// Uses a mock HTTP server returning Ollama-format JSON instantly.
/// Verifies: user input → agent_mgr → dispatch_llm → mock HTTP
///   → orcs.output(response) → Emitter → IOPort → stdout
#[test]
fn command_mode_llm_pipeline() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::thread;

    let tmp = tempfile::tempdir().expect("create temp dir");
    let bin = assert_cmd::cargo::cargo_bin!("orcs");

    // Start a mock HTTP server that mimics Ollama's POST /api/chat response.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock HTTP server");
    let mock_port = listener.local_addr().expect("get mock port").port();
    let mock_base_url = format!("http://127.0.0.1:{}", mock_port);

    let server_thread = thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };

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
                let req_str = String::from_utf8_lossy(&request);
                if let Some(header_end) = req_str.find("\r\n\r\n") {
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

            let ollama_response = r#"{"model":"mock-model","message":{"role":"assistant","content":"CMD_MODE_RESPONSE_99"},"done":true,"done_reason":"stop"}"#;
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                ollama_response.len(),
                ollama_response
            );
            let _ = stream.write_all(http_response.as_bytes());
            let _ = stream.flush();
        }
    });

    let child = Command::new(&bin)
        .arg("-d")
        .arg("--builtins-dir")
        .arg(tmp.path())
        // The trailing args form the command
        .arg("test_cmd_mode_msg")
        .env("ORCS_LLM_BASE_URL", &mock_base_url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orcs process");

    // Command mode should exit on its own (no stdin needed).
    let output = child.wait_with_output().expect("wait for orcs to complete");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    drop(server_thread);

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

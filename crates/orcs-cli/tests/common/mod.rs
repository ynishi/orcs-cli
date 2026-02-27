#![allow(dead_code)]
//! Shared E2E test helpers for `orcs` binary tests.
//!
//! # Test Isolation: stdin Timing & Component Init
//!
//! `assert_cmd::write_stdin()` sets stdin content **before** the process starts,
//! so piped data (e.g. `"q\n"`) is available the instant the binary reads stdin.
//!
//! `run_interactive()` uses `tokio::select! { biased; }` to prioritize IOOutput
//! over stdin, but this only helps when IOOutput messages are **already queued**.
//! If component init (Lua VM startup, file I/O) hasn't completed yet, the
//! IOOutput channel is empty and stdin wins the race.
//!
//! **Consequence:** Any test that checks async component output (e.g. Ready
//! messages) AND uses `write_stdin("q\n")` is inherently racy — it works in
//! isolation but fails under parallel execution due to CPU/IO contention.
//!
//! **Solution:** Tests that need to observe async output before quitting MUST
//! use [`spawn_and_wait_for`] — piped stdin with explicit output-gate — instead
//! of `write_stdin`. This gives a structural guarantee: quit is sent only after
//! the expected output has been observed.

use assert_cmd::cargo::cargo_bin_cmd;
use std::time::Duration;

/// Default timeout for basic CLI tests.
pub const TIMEOUT_BASIC: Duration = Duration::from_secs(10);

/// Extended timeout for snapshot/resume tests (heavier I/O).
pub const TIMEOUT_SNAPSHOT: Duration = Duration::from_secs(15);

/// Build a bare Command without `--builtins-dir` pre-set.
///
/// Use this only for tests that explicitly provide their own `--builtins-dir`.
pub fn orcs_cmd_raw() -> assert_cmd::Command {
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_BASIC);

    cmd
}

/// Build a Command for the `orcs` binary in sandbox mode.
///
/// Uses `--sandbox` to skip global config (`~/.orcs/config.toml`) and isolate
/// all state (sessions, builtins, history) inside a fresh temp directory.
/// Returns (command, _guard) — keep the guard alive for the test's duration.
pub fn orcs_cmd() -> (assert_cmd::Command, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create temp dir for test isolation");
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_BASIC);

    cmd.args(["--sandbox", tmp.path().to_str().expect("valid utf8")]);
    (cmd, tmp)
}

/// Build a Command with fresh builtins and the extended snapshot timeout.
pub fn orcs_cmd_snapshot() -> (assert_cmd::Command, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_SNAPSHOT);

    cmd.args(["--builtins-dir", tmp.path().to_str().expect("valid utf8")]);
    (cmd, tmp)
}

/// Build a Command using a shared builtins dir (for multi-step tests).
pub fn orcs_cmd_with_builtins(dir: &str) -> assert_cmd::Command {
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_SNAPSHOT);

    cmd.args(["--builtins-dir", dir]);
    cmd
}

/// Dump stdout/stderr to a directory for post-mortem debugging.
///
/// By default, output is written to a [`tempfile::TempDir`] that is
/// automatically cleaned up when the returned guard is dropped.
///
/// Set `ORCS_TEST_DUMP_DIR` to a path to persist dumps there instead:
///
/// ```sh
/// ORCS_TEST_DUMP_DIR=/tmp/orcs-debug cargo test -- dispatch_llm
/// ```
///
/// Returns a guard that keeps the dump directory alive.  Bind it to `_guard`
/// in the caller so the directory survives until the test function ends.
pub fn dump_test_output(label: &str, stdout: &str, stderr: &str) -> DumpGuard {
    let (dir_path, _guard) = match std::env::var("ORCS_TEST_DUMP_DIR") {
        Ok(base) => {
            let dir = std::path::PathBuf::from(base).join(label);
            std::fs::create_dir_all(&dir).expect("create persistent dump dir");
            (dir, None)
        }
        Err(_) => {
            let td = tempfile::tempdir().expect("create temp dump dir");
            let dir = td.path().to_path_buf();
            (dir, Some(td))
        }
    };

    std::fs::write(dir_path.join("stdout.txt"), stdout.as_bytes()).expect("write stdout dump");
    std::fs::write(dir_path.join("stderr.txt"), stderr.as_bytes()).expect("write stderr dump");

    eprintln!("dump_dir={}", dir_path.display());

    DumpGuard {
        _guard,
        path: dir_path,
    }
}

/// RAII guard that keeps a temporary dump directory alive.
///
/// When `ORCS_TEST_DUMP_DIR` is set, `_guard` is `None` and the directory
/// persists after the test.  Otherwise the [`tempfile::TempDir`] inside
/// `_guard` deletes the directory on drop.
pub struct DumpGuard {
    _guard: Option<tempfile::TempDir>,
    #[allow(dead_code)]
    path: std::path::PathBuf,
}

/// Extract session ID from stdout containing "Session saved: <UUID>".
pub fn extract_session_id(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(pos) = line.find("Session saved: ") {
            let id_start = pos + "Session saved: ".len();
            let id = line[id_start..].trim();
            // Validate UUID format (8-4-4-4-12)
            if id.len() >= 36 {
                return Some(id[..36].to_string());
            }
        }
    }
    None
}

/// Spawn the `orcs` binary with piped stdin/stdout and wait for a specific
/// output line before sending quit.
///
/// This solves the stdin-vs-component-init race condition:
/// `write_stdin("q\n")` is available instantly, so quit can be processed
/// before async component init completes. This helper gates the quit on
/// observing the expected output first, giving a structural guarantee.
///
/// # Arguments
/// * `gate` - Substring to wait for in stdout before sending quit.
/// * `extra_args` - Additional CLI arguments (e.g. `&["-d"]`).
/// * `sandbox_dir` - Path to sandbox directory (isolates state and skips global config).
///
/// # Returns
/// `(stdout, stderr)` collected from the process.
///
/// # Panics
/// Panics if the gate string is not observed within [`TIMEOUT_BASIC`].
pub fn spawn_and_wait_for(
    gate: &str,
    extra_args: &[&str],
    sandbox_dir: &std::path::Path,
) -> (String, String) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc as std_mpsc;
    use std::thread;

    let bin = assert_cmd::cargo::cargo_bin!("orcs");

    let mut cmd = Command::new(bin);
    cmd.arg("--sandbox").arg(sandbox_dir).args(extra_args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn orcs process");

    let mut stdin = child.stdin.take().expect("open stdin pipe");
    let child_stdout = child.stdout.take().expect("open stdout pipe");
    let child_stderr = child.stderr.take().expect("open stderr pipe");

    let gate_owned = gate.to_string();
    let (gate_tx, gate_rx) = std_mpsc::channel::<()>();

    let stdout_thread = thread::spawn(move || {
        let mut lines = Vec::new();
        let reader = BufReader::new(child_stdout);
        let mut notified = false;
        for line in reader.lines() {
            let line = line.expect("read stdout line");
            if !notified && line.contains(&gate_owned) {
                let _ = gate_tx.send(());
                notified = true;
            }
            lines.push(line);
        }
        if !notified {
            let _ = gate_tx.send(());
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

    // Wait for the gate output, then send quit.
    let found = gate_rx.recv_timeout(TIMEOUT_BASIC).is_ok();
    writeln!(stdin, "q").expect("send quit after gate");
    drop(stdin);

    let stdout = stdout_thread.join().expect("join stdout thread");
    let stderr = stderr_thread.join().expect("join stderr thread");
    let _ = child.wait();

    assert!(
        found && stdout.contains(gate),
        "Gate string {gate:?} not observed within {TIMEOUT_BASIC:?}.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    (stdout, stderr)
}

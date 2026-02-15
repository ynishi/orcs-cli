//! Shared E2E test helpers for `orcs` binary tests.

use assert_cmd::cargo::cargo_bin_cmd;
use std::time::Duration;

/// Default timeout for basic CLI tests.
pub const TIMEOUT_BASIC: Duration = Duration::from_secs(10);

/// Extended timeout for snapshot/resume tests (heavier I/O).
pub const TIMEOUT_SNAPSHOT: Duration = Duration::from_secs(15);

/// Environment variables that prevent the claude CLI from launching
/// inside another Claude Code session (nested session guard).
/// Removed so that `orcs` → `claude` calls work when tests run
/// inside a Claude Code terminal.
const CLAUDE_GUARD_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
];

/// Remove Claude Code nested-session guard variables from a command.
fn strip_claude_guard(cmd: &mut assert_cmd::Command) {
    for var in CLAUDE_GUARD_VARS {
        cmd.env_remove(var);
    }
}

/// Build a bare Command without `--builtins-dir` pre-set.
///
/// Use this only for tests that explicitly provide their own `--builtins-dir`.
pub fn orcs_cmd_raw() -> assert_cmd::Command {
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_BASIC);
    strip_claude_guard(&mut cmd);
    cmd
}

/// Build a Command for the `orcs` binary with fresh builtins in a tempdir.
///
/// Always uses a fresh temp directory to avoid stale builtin cache issues.
/// Returns (command, _guard) — keep the guard alive for the test's duration.
pub fn orcs_cmd() -> (assert_cmd::Command, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_BASIC);
    strip_claude_guard(&mut cmd);
    cmd.args(["--builtins-dir", tmp.path().to_str().expect("valid utf8")]);
    (cmd, tmp)
}

/// Build a Command with fresh builtins and the extended snapshot timeout.
pub fn orcs_cmd_snapshot() -> (assert_cmd::Command, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_SNAPSHOT);
    strip_claude_guard(&mut cmd);
    cmd.args(["--builtins-dir", tmp.path().to_str().expect("valid utf8")]);
    (cmd, tmp)
}

/// Build a Command using a shared builtins dir (for multi-step tests).
pub fn orcs_cmd_with_builtins(dir: &str) -> assert_cmd::Command {
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(TIMEOUT_SNAPSHOT);
    strip_claude_guard(&mut cmd);
    cmd.args(["--builtins-dir", dir]);
    cmd
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

//! E2E tests for component snapshot/restore via session pause/resume.
//!
//! Tests the full cycle: pause (snapshot collection + save) → resume (restore).
//! Requires builtins to be expanded with snapshot support.

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::time::Duration;

/// Helper: build a Command for the `orcs` binary with a default timeout.
fn orcs_cmd() -> assert_cmd::Command {
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(Duration::from_secs(15));
    cmd
}

/// Helper: build a Command with fresh builtins in a tempdir.
/// Returns (command, _guard) — keep the guard alive for the test's duration.
fn orcs_cmd_fresh() -> (assert_cmd::Command, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(Duration::from_secs(15));
    cmd.args(["--builtins-dir", tmp.path().to_str().expect("valid utf8")]);
    (cmd, tmp)
}

/// Helper: build a Command using a shared builtins dir (for multi-step tests).
fn orcs_cmd_with_builtins(dir: &str) -> assert_cmd::Command {
    let mut cmd: assert_cmd::Command = cargo_bin_cmd!("orcs");
    cmd.timeout(Duration::from_secs(15));
    cmd.args(["--builtins-dir", dir]);
    cmd
}

/// Extract session ID from stdout containing "Session saved: <UUID>".
fn extract_session_id(stdout: &str) -> Option<String> {
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

// ─── --install-builtins ──────────────────────────────────────────

#[test]
fn install_builtins_creates_files() {
    let tmp = tempfile::tempdir().expect("create temp dir");

    orcs_cmd()
        .args(["--builtins-dir", tmp.path().to_str().expect("valid utf8")])
        .arg("--install-builtins")
        .assert()
        .success()
        .stdout(contains("Installed").and(contains("file(s)")));
}

#[test]
fn install_builtins_skips_existing() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir_str = tmp.path().to_str().expect("valid utf8");

    // First install
    orcs_cmd()
        .args(["--builtins-dir", dir_str])
        .arg("--install-builtins")
        .assert()
        .success();

    // Second install should skip
    orcs_cmd()
        .args(["--builtins-dir", dir_str])
        .arg("--install-builtins")
        .assert()
        .success()
        .stdout(contains("already installed"));
}

#[test]
fn install_builtins_force_overwrites() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir_str = tmp.path().to_str().expect("valid utf8");

    // First install
    orcs_cmd()
        .args(["--builtins-dir", dir_str])
        .arg("--install-builtins")
        .assert()
        .success();

    // Force reinstall should overwrite
    orcs_cmd()
        .args(["--builtins-dir", dir_str])
        .arg("--install-builtins")
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("Force-installing"))
        .stdout(contains("Installed").and(contains("file(s)")));
}

#[test]
fn force_requires_install_builtins() {
    orcs_cmd()
        .arg("--force")
        .assert()
        .failure()
        .stderr(contains("--install-builtins"));
}

// ─── Pause → Snapshot Collection ─────────────────────────────────

#[test]
fn pause_collects_snapshots() {
    let (mut cmd, _guard) = orcs_cmd_fresh();
    cmd.arg("-d")
        .write_stdin("p\n")
        .assert()
        .success()
        .stdout(contains("collected 2 snapshots from runners"))
        .stdout(contains("Session saved:"))
        .stdout(contains("Resume with: orcs --resume"));
}

#[test]
fn pause_captures_skill_manager_snapshot() {
    let (mut cmd, _guard) = orcs_cmd_fresh();
    cmd.arg("-d").write_stdin("p\n").assert().success().stdout(
        contains("skill::skill_manager").and(contains("has_snapshot").and(contains("true"))),
    );
}

#[test]
fn pause_captures_profile_manager_snapshot() {
    let (mut cmd, _guard) = orcs_cmd_fresh();
    cmd.arg("-d").write_stdin("p\n").assert().success().stdout(
        contains("profile::profile_manager").and(contains("has_snapshot").and(contains("true"))),
    );
}

// ─── Pause → Resume Round-Trip ───────────────────────────────────

#[test]
fn pause_resume_round_trip() {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let dir_str = tmp.path().to_str().expect("valid utf8");

    // Step 1: Pause to get a session with snapshots
    let pause_output = orcs_cmd_with_builtins(dir_str)
        .write_stdin("p\n")
        .output()
        .expect("pause command should succeed");

    assert!(pause_output.status.success(), "pause should exit 0");

    let stdout = String::from_utf8_lossy(&pause_output.stdout);
    let session_id = extract_session_id(&stdout).expect("should find session ID in pause output");

    // Step 2: Resume that session
    orcs_cmd_with_builtins(dir_str)
        .args(["--resume", &session_id])
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("Resumed session:"))
        .stdout(contains("2 component(s) restored"));
}

#[test]
fn resumed_session_restores_skill_manager() {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let dir_str = tmp.path().to_str().expect("valid utf8");

    // Step 1: Pause
    let pause_output = orcs_cmd_with_builtins(dir_str)
        .arg("-d")
        .write_stdin("p\n")
        .output()
        .expect("pause command should succeed");

    assert!(pause_output.status.success(), "pause should exit 0");

    let stdout = String::from_utf8_lossy(&pause_output.stdout);
    let session_id = extract_session_id(&stdout).expect("should find session ID in pause output");

    // Step 2: Resume with debug to see restore details
    orcs_cmd_with_builtins(dir_str)
        .arg("-d")
        .args(["--resume", &session_id])
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("Restoring snapshot for component"))
        .stdout(contains("skill::skill_manager"))
        .stdout(contains("SkillManager restored"));
}

#[test]
fn resumed_session_restores_profile_manager() {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let dir_str = tmp.path().to_str().expect("valid utf8");

    // Step 1: Pause
    let pause_output = orcs_cmd_with_builtins(dir_str)
        .arg("-d")
        .write_stdin("p\n")
        .output()
        .expect("pause command should succeed");

    assert!(pause_output.status.success(), "pause should exit 0");

    let stdout = String::from_utf8_lossy(&pause_output.stdout);
    let session_id = extract_session_id(&stdout).expect("should find session ID in pause output");

    // Step 2: Resume with debug to see profile_manager restore
    orcs_cmd_with_builtins(dir_str)
        .arg("-d")
        .args(["--resume", &session_id])
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("Restoring snapshot for component"))
        .stdout(contains("profile::profile_manager"));
}

#[test]
fn resumed_session_skips_skill_manager_reinit() {
    let tmp = tempfile::tempdir().expect("create temp dir for builtins");
    let dir_str = tmp.path().to_str().expect("valid utf8");

    // Step 1: Pause
    let pause_output = orcs_cmd_with_builtins(dir_str)
        .arg("-d")
        .write_stdin("p\n")
        .output()
        .expect("pause command should succeed");

    assert!(pause_output.status.success(), "pause should exit 0");

    let stdout = String::from_utf8_lossy(&pause_output.stdout);
    let session_id = extract_session_id(&stdout).expect("should find session ID in pause output");

    // Step 2: Resume - init() should detect restored state and skip re-init
    orcs_cmd_with_builtins(dir_str)
        .arg("-d")
        .args(["--resume", &session_id])
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("restored session"));
}

// ─── --builtins-dir Override ─────────────────────────────────────

#[test]
fn builtins_dir_override_is_used() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir_str = tmp.path().to_str().expect("valid utf8");

    // Install builtins to custom dir
    orcs_cmd()
        .args(["--builtins-dir", dir_str])
        .arg("--install-builtins")
        .assert()
        .success()
        .stdout(contains(dir_str));
}

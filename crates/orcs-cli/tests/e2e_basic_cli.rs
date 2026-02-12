//! E2E integration tests for the `orcs` binary.
//!
//! Tests the binary's stdin/stdout interface by spawning real subprocesses.
//! tracing output goes to stdout; eprintln (errors) goes to stderr.

mod common;

use common::{orcs_cmd, orcs_cmd_fresh};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

// ─── Startup / Shutdown ────────────────────────────────────────────

#[test]
fn quit_immediately() {
    orcs_cmd()
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("Interactive mode started"))
        .stdout(contains("Session ID:"))
        .stdout(contains("Quit requested"));
}

#[test]
fn quit_long_form() {
    orcs_cmd()
        .write_stdin("quit\n")
        .assert()
        .success()
        .stdout(contains("Quit requested"));
}

#[test]
fn empty_stdin_exits_gracefully() {
    orcs_cmd().write_stdin("").assert().success();
}

// ─── Component Ready Messages ──────────────────────────────────────

#[test]
fn components_initialize() {
    // Check spawn messages (synchronous in builder, before interactive mode)
    // rather than Ready messages which race with stdin processing.
    orcs_cmd()
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("builtin::agent_mgr"))
        .stdout(contains("builtin::shell"))
        .stdout(contains("builtin::tool"));
}

#[test]
fn profile_manager_initializes() {
    let (mut cmd, _guard) = orcs_cmd_fresh();
    cmd.arg("-d")
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("profile::profile_manager"))
        .stdout(contains("ProfileManager initialized"));
}

#[test]
fn agent_mgr_ready_with_workers() {
    let (mut cmd, _guard) = orcs_cmd_fresh();
    cmd.write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("[AgentMgr] Ready (workers: llm, skill)"));
}

// ─── Worker Spawning ─────────────────────────────────────────────

#[test]
fn agent_mgr_spawns_workers() {
    let (mut cmd, _guard) = orcs_cmd_fresh();
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
    let (mut cmd, _guard) = orcs_cmd_fresh();
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
    let (mut cmd, _guard) = orcs_cmd_fresh();
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
    orcs_cmd()
        .write_stdin("help\nq\n")
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
    orcs_cmd()
        .arg("-d")
        .write_stdin("@shell echo hello_e2e\nq\n")
        .assert()
        .success()
        .stdout(contains("Routed @shell to channel"));
}

#[test]
fn at_profile_manager_routes() {
    let (mut cmd, _guard) = orcs_cmd_fresh();
    cmd.arg("-d")
        .write_stdin("@profile_manager list\nq\n")
        .assert()
        .success()
        .stdout(contains("Routed @profile_manager to channel"));
}

#[test]
fn at_unknown_component_shows_error() {
    orcs_cmd()
        .write_stdin("@nonexistent test\nq\n")
        .assert()
        .success()
        .stdout(contains("Unknown component: @nonexistent"));
}

// ─── Skill Discovery ──────────────────────────────────────────────

#[test]
fn skill_manager_discovers_skills() {
    orcs_cmd()
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(contains("SkillManager initialized"));
}

// ─── Pause / Session Save ──────────────────────────────────────────

#[test]
fn pause_saves_session() {
    orcs_cmd()
        .write_stdin("p\n")
        .assert()
        .success()
        .stdout(contains("Saving session").or(contains("Pause requested")));
}

// ─── Resume Session ────────────────────────────────────────────────

#[test]
fn resume_nonexistent_session_returns_error() {
    orcs_cmd()
        .args(["--resume", "00000000-0000-0000-0000-000000000000"])
        .write_stdin("q\n")
        .assert()
        .failure()
        .stderr(contains("session not found"));
}

// ─── Blank Input ───────────────────────────────────────────────────

#[test]
fn blank_lines_are_ignored() {
    orcs_cmd().write_stdin("\n\n\nq\n").assert().success();
}

// ─── Version / Help Flags ──────────────────────────────────────────

#[test]
fn version_flag() {
    orcs_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("orcs"));
}

#[test]
fn help_flag() {
    orcs_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("ORCS CLI"));
}

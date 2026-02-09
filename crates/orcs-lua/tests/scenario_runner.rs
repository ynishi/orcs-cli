//! Lua Scenario Test Runner.
//!
//! Discovers and runs all `.lua` scenario files in `tests/scenarios/`.
//! Each file defines a suite of test cases that exercise the ORCS runtime
//! through Lua scripts.
//!
//! Run with: `cargo test --test scenario_runner`

use orcs_lua::scenario::{assert_all_pass, ScenarioRunner};
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use std::path::Path;
use std::sync::Arc;

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

fn runner() -> ScenarioRunner {
    ScenarioRunner::new(test_sandbox())
}

fn scenarios_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scenarios")
}

// =============================================================================
// File-based scenarios
// =============================================================================

#[test]
fn scenario_echo_basic() {
    let result = runner()
        .run_file(&scenarios_dir().join("echo_basic.lua"))
        .expect("failed to run scenario");
    eprintln!("{}", result.report());
    assert_all_pass(&result);
}

#[test]
fn scenario_hil_approval() {
    let result = runner()
        .run_file(&scenarios_dir().join("hil_approval.lua"))
        .expect("failed to run scenario");
    eprintln!("{}", result.report());
    assert_all_pass(&result);
}

#[test]
fn scenario_embedded_echo() {
    let result = runner()
        .run_file(&scenarios_dir().join("embedded_echo.lua"))
        .expect("failed to run scenario");
    eprintln!("{}", result.report());
    assert_all_pass(&result);
}

// =============================================================================
// Discovery: run all .lua files in scenarios/
// =============================================================================

#[test]
fn all_scenarios_pass() {
    let dir = scenarios_dir();
    if !dir.exists() {
        panic!("scenarios directory not found: {}", dir.display());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read scenarios dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "lua"))
        .collect();

    entries.sort_by_key(|e| e.path());

    assert!(
        !entries.is_empty(),
        "no .lua scenario files found in {}",
        dir.display()
    );

    let runner = runner();
    let mut all_passed = true;
    let mut reports = Vec::new();

    for entry in &entries {
        let path = entry.path();
        match runner.run_file(&path) {
            Ok(result) => {
                reports.push(result.report());
                if !result.all_passed() {
                    all_passed = false;
                }
            }
            Err(e) => {
                reports.push(format!(
                    "━━ {} ━━  LOAD ERROR\n  {e}",
                    path.file_name().unwrap().to_string_lossy()
                ));
                all_passed = false;
            }
        }
    }

    let full_report = reports.join("\n\n");
    eprintln!("\n{full_report}\n");

    if !all_passed {
        panic!("Some scenarios failed. See report above.");
    }
}

//! Integration tests for SkillManagementComponent.

use orcs_component::EventCategory;
use orcs_event::SignalResponse;
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::sync::Arc;

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    let dir = std::env::temp_dir().join(format!(
        "orcs-skill-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let canon = dir.canonicalize().unwrap();
    Arc::new(ProjectSandbox::new(&canon).expect("test sandbox"))
}

fn skill_harness() -> LuaTestHarness {
    LuaTestHarness::from_script(orcs_lua::embedded::SKILL_MANAGER, test_sandbox()).unwrap()
}

fn ext_cat() -> EventCategory {
    EventCategory::Extension {
        namespace: "skill".to_string(),
        kind: "request".to_string(),
    }
}

/// Helper: register a sample skill and return the harness
fn register_sample(harness: &mut LuaTestHarness) -> serde_json::Value {
    harness
        .request(
            ext_cat(),
            "register",
            json!({
                "skill": {
                    "name": "test-skill",
                    "description": "A test skill",
                    "source": { "format": "agent-skills", "path": "/tmp/test-skill" },
                    "frontmatter": {},
                    "metadata": { "tags": ["test"], "categories": ["debug"] },
                    "state": "discovered"
                }
            }),
        )
        .unwrap()
}

// =============================================================================
// Basic Lifecycle
// =============================================================================

mod lifecycle {
    use super::*;

    #[test]
    fn load_skill_manager_component() {
        let harness = skill_harness();
        assert_eq!(harness.id().name, "skill_manager");
    }

    #[test]
    fn init_and_shutdown() {
        let mut harness = skill_harness();
        let _ = harness.init();
        // After a request completes, status returns to Idle
        // init itself does not change status to Running permanently
        harness.shutdown();
    }

    #[test]
    fn veto_aborts() {
        let mut harness = skill_harness();
        let _ = harness.init();
        let response = harness.veto();
        assert_eq!(response, SignalResponse::Abort);
    }
}

// =============================================================================
// Status Operation
// =============================================================================

mod status {
    use super::*;

    #[test]
    fn status_returns_initial_state() {
        let mut harness = skill_harness();
        let _ = harness.init();

        // on_request returns { success = true, data = { total, frozen, active, formats } }
        // The harness extracts `data` on success.
        let result = harness.request(ext_cat(), "status", json!({})).unwrap();

        assert_eq!(result["total"], 0);
        assert_eq!(result["frozen"], false);
        assert_eq!(result["active"], 0);
    }
}

// =============================================================================
// Registry Operations
// =============================================================================

mod registry {
    use super::*;

    #[test]
    fn register_and_list() {
        let mut harness = skill_harness();
        let _ = harness.init();

        // Register returns { success = true, data = { count = 1 } }
        let result = register_sample(&mut harness);
        assert_eq!(result["count"], 1);

        // List returns { success = true, data = entries, count = #entries }
        let result = harness.request(ext_cat(), "list", json!({})).unwrap();
        // data is the entries array
        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(result[0]["name"], "test-skill");
    }

    #[test]
    fn register_duplicate_fails() {
        let mut harness = skill_harness();
        let _ = harness.init();

        register_sample(&mut harness);

        // Second register should return Err
        let err = harness
            .request(
                ext_cat(),
                "register",
                json!({
                    "skill": {
                        "name": "test-skill",
                        "description": "duplicate",
                        "source": { "format": "agent-skills", "path": "/tmp/dup" },
                        "frontmatter": {},
                        "metadata": { "tags": [], "categories": [] },
                        "state": "discovered"
                    }
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn get_skill() {
        let mut harness = skill_harness();
        let _ = harness.init();
        register_sample(&mut harness);

        let result = harness
            .request(ext_cat(), "get", json!({ "name": "test-skill" }))
            .unwrap();
        assert_eq!(result["name"], "test-skill");
        assert_eq!(result["description"], "A test skill");
    }

    #[test]
    fn get_nonexistent_returns_error() {
        let mut harness = skill_harness();
        let _ = harness.init();

        let err = harness
            .request(ext_cat(), "get", json!({ "name": "nonexistent" }))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn unregister_skill() {
        let mut harness = skill_harness();
        let _ = harness.init();
        register_sample(&mut harness);

        // Unregister
        let _ = harness
            .request(ext_cat(), "unregister", json!({ "name": "test-skill" }))
            .unwrap();

        // Verify it's gone via status
        let status = harness.request(ext_cat(), "status", json!({})).unwrap();
        assert_eq!(status["total"], 0);
    }

    #[test]
    fn search_by_name() {
        let mut harness = skill_harness();
        let _ = harness.init();

        harness
            .request(
                ext_cat(),
                "register",
                json!({
                    "skill": {
                        "name": "deploy-prod",
                        "description": "Deploy to production",
                        "source": { "format": "agent-skills", "path": "/tmp/deploy" },
                        "frontmatter": {},
                        "metadata": { "tags": ["deploy"], "categories": ["execute"] },
                        "state": "discovered"
                    }
                }),
            )
            .unwrap();

        harness
            .request(
                ext_cat(),
                "register",
                json!({
                    "skill": {
                        "name": "code-review",
                        "description": "Review code changes",
                        "source": { "format": "agent-skills", "path": "/tmp/review" },
                        "frontmatter": {},
                        "metadata": { "tags": ["review"], "categories": ["review"] },
                        "state": "discovered"
                    }
                }),
            )
            .unwrap();

        // Search by name
        let result = harness
            .request(ext_cat(), "search", json!({ "query": "deploy" }))
            .unwrap();
        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(result[0]["name"], "deploy-prod");
    }
}

// =============================================================================
// Catalog Operations
// =============================================================================

mod catalog {
    use super::*;

    fn register_n_skills(harness: &mut LuaTestHarness, n: usize) {
        for i in 0..n {
            harness
                .request(
                    ext_cat(),
                    "register",
                    json!({
                        "skill": {
                            "name": format!("skill-{i}"),
                            "description": format!("Skill number {i}"),
                            "source": { "format": "agent-skills", "path": format!("/tmp/s{i}") },
                            "frontmatter": {},
                            "metadata": { "tags": [], "categories": [] },
                            "state": "discovered"
                        }
                    }),
                )
                .unwrap();
        }
    }

    #[test]
    fn render_catalog_empty() {
        let mut harness = skill_harness();
        let _ = harness.init();

        let result = harness.request(ext_cat(), "catalog", json!({})).unwrap();
        // catalog returns { success = true, data = { catalog = text, stats = {...} } }
        assert!(result["catalog"].is_string());
        assert!(result["stats"].is_object());
    }

    #[test]
    fn render_catalog_with_skills() {
        let mut harness = skill_harness();
        let _ = harness.init();
        register_n_skills(&mut harness, 3);

        let result = harness.request(ext_cat(), "catalog", json!({})).unwrap();
        assert!(result["catalog"].is_string());
        let catalog_text = result["catalog"].as_str().unwrap();
        // Should contain at least one skill name
        assert!(catalog_text.contains("skill-0") || catalog_text.contains("skill-1"));
    }
}

// =============================================================================
// Unknown Operation
// =============================================================================

mod errors {
    use super::*;

    #[test]
    fn unknown_operation_returns_error() {
        let mut harness = skill_harness();
        let _ = harness.init();

        let err = harness
            .request(ext_cat(), "nonexistent_op", json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("unknown operation"));
    }

    #[test]
    fn missing_required_name() {
        let mut harness = skill_harness();
        let _ = harness.init();

        let err = harness.request(ext_cat(), "get", json!({})).unwrap_err();
        assert!(err.to_string().contains("required"));
    }
}

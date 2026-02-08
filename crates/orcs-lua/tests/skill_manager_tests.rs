//! Integration tests for SkillManagementComponent.

use orcs_component::EventCategory;
use orcs_event::SignalResponse;
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// Creates a unique temp dir and returns (sandbox, root_path).
fn test_sandbox_with_root() -> (Arc<dyn SandboxPolicy>, PathBuf) {
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
    let sandbox = Arc::new(ProjectSandbox::new(&canon).expect("test sandbox"));
    (sandbox, canon)
}

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    test_sandbox_with_root().0
}

fn skill_harness() -> LuaTestHarness {
    LuaTestHarness::from_script(orcs_lua::embedded::SKILL_MANAGER, test_sandbox()).unwrap()
}

fn skill_harness_with_root() -> (LuaTestHarness, PathBuf) {
    let (sandbox, root) = test_sandbox_with_root();
    let harness = LuaTestHarness::from_script(orcs_lua::embedded::SKILL_MANAGER, sandbox).unwrap();
    (harness, root)
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

// =============================================================================
// File I/O: Discover skills from real directories
// =============================================================================

mod file_io {
    use super::*;
    use std::fs;

    /// Create an Agent Skills Standard skill (SKILL.md + YAML frontmatter)
    fn create_agent_skill(root: &PathBuf, name: &str, description: &str) {
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\n{description}\n"
        );
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    /// Create a Lua DSL skill (skill.lua returning a table)
    fn create_lua_skill(root: &PathBuf, name: &str, description: &str) {
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = format!(
            "return {{\n  name = \"{name}\",\n  description = \"{description}\",\n  body = \"# {name}\\n\\n{description}\",\n  tags = {{\"test\"}},\n  categories = {{\"execute\"}},\n}}\n"
        );
        fs::write(skill_dir.join("skill.lua"), content).unwrap();
    }

    #[test]
    fn discover_agent_skills_from_directory() {
        let (mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        // Create sample skills in the sandbox
        let skills_dir = root.join("skills");
        create_agent_skill(&skills_dir, "deploy-prod", "Deploy to production");
        create_agent_skill(&skills_dir, "code-review", "Review code changes");

        // Discover
        let result = harness
            .request(
                ext_cat(),
                "discover",
                json!({ "path": skills_dir.to_str().unwrap() }),
            )
            .unwrap();

        assert_eq!(result["discovered"], 2);
        assert_eq!(result["registered"], 2);

        // Verify via list
        let list = harness.request(ext_cat(), "list", json!({})).unwrap();
        assert!(list.is_array());
        assert_eq!(list.as_array().unwrap().len(), 2);
    }

    #[test]
    fn discover_lua_dsl_skill() {
        let (mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        let skills_dir = root.join("skills");
        create_lua_skill(&skills_dir, "my-tool", "A custom Lua tool");

        let result = harness
            .request(
                ext_cat(),
                "discover",
                json!({ "path": skills_dir.to_str().unwrap() }),
            )
            .unwrap();

        assert_eq!(result["discovered"], 1);
        assert_eq!(result["registered"], 1);

        // Verify via get
        let skill = harness
            .request(ext_cat(), "get", json!({ "name": "my-tool" }))
            .unwrap();
        assert_eq!(skill["name"], "my-tool");
        assert_eq!(skill["description"], "A custom Lua tool");
        assert_eq!(skill["source"]["format"], "lua-dsl");
    }

    #[test]
    fn discover_mixed_formats() {
        let (mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        let skills_dir = root.join("skills");
        create_agent_skill(&skills_dir, "agent-skill", "An agent skill");
        create_lua_skill(&skills_dir, "lua-skill", "A lua skill");

        let result = harness
            .request(
                ext_cat(),
                "discover",
                json!({ "path": skills_dir.to_str().unwrap() }),
            )
            .unwrap();

        assert_eq!(result["discovered"], 2);
        assert_eq!(result["registered"], 2);

        // Verify both exist
        let status = harness.request(ext_cat(), "status", json!({})).unwrap();
        assert_eq!(status["total"], 2);
    }

    #[test]
    fn discover_empty_directory() {
        let (mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        let empty_dir = root.join("empty-skills");
        fs::create_dir_all(&empty_dir).unwrap();

        let result = harness
            .request(
                ext_cat(),
                "discover",
                json!({ "path": empty_dir.to_str().unwrap() }),
            )
            .unwrap();

        assert_eq!(result["discovered"], 0);
        assert_eq!(result["registered"], 0);
    }

    #[test]
    fn discover_then_catalog() {
        let (mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        let skills_dir = root.join("skills");
        create_agent_skill(&skills_dir, "deploy", "Deploy to production");
        create_agent_skill(&skills_dir, "review", "Code review assistant");
        create_lua_skill(&skills_dir, "lint", "Run linters");

        // Discover
        harness
            .request(
                ext_cat(),
                "discover",
                json!({ "path": skills_dir.to_str().unwrap() }),
            )
            .unwrap();

        // Catalog should include all discovered skills
        let catalog = harness.request(ext_cat(), "catalog", json!({})).unwrap();
        let text = catalog["catalog"].as_str().unwrap();

        assert!(text.contains("deploy"), "catalog should contain 'deploy'");
        assert!(text.contains("review"), "catalog should contain 'review'");
        assert!(text.contains("lint"), "catalog should contain 'lint'");

        let stats = &catalog["stats"];
        assert_eq!(stats["total"], 3);
        assert_eq!(stats["shown"], 3);
    }

    #[test]
    fn detect_format_agent_skills() {
        let (mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        let skill_dir = root.join("my-skill");
        create_agent_skill(&root, "my-skill", "test");

        let result = harness
            .request(
                ext_cat(),
                "detect_format",
                json!({ "path": skill_dir.to_str().unwrap() }),
            )
            .unwrap();
        assert_eq!(result["format"], "agent-skills");
    }

    #[test]
    fn detect_format_lua_dsl() {
        let (mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        let skill_dir = root.join("lua-skill");
        create_lua_skill(&root, "lua-skill", "test");

        let result = harness
            .request(
                ext_cat(),
                "detect_format",
                json!({ "path": skill_dir.to_str().unwrap() }),
            )
            .unwrap();
        assert_eq!(result["format"], "lua-dsl");
    }
}

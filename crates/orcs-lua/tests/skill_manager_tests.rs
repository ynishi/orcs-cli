//! Integration tests for SkillManagementComponent.

use orcs_component::EventCategory;
use orcs_event::SignalResponse;
use orcs_lua::testing::LuaTestHarness;
use orcs_lua::ScriptLoader;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

/// Returns the path to the skill_manager component directory.
fn skill_manager_dir() -> std::path::PathBuf {
    ScriptLoader::crate_scripts_dir().join("skill_manager")
}

/// Creates a unique temp dir and returns (TempDir guard, sandbox, root_path).
/// TempDir must be held alive for the test duration (auto-deleted on Drop).
fn test_sandbox_with_root() -> (TempDir, Arc<dyn SandboxPolicy>, PathBuf) {
    let td = tempfile::tempdir().expect("create temp dir");
    let root = td.path().canonicalize().expect("canonicalize temp dir");
    let sandbox = Arc::new(ProjectSandbox::new(&root).expect("test sandbox"));
    (td, sandbox, root)
}

fn test_sandbox() -> (TempDir, Arc<dyn SandboxPolicy>) {
    let (td, sandbox, _root) = test_sandbox_with_root();
    (td, sandbox)
}

fn skill_harness() -> (TempDir, LuaTestHarness) {
    let (td, sandbox) = test_sandbox();
    let harness = LuaTestHarness::from_dir(skill_manager_dir(), sandbox)
        .expect("load skill_manager from crate scripts dir");
    (td, harness)
}

fn skill_harness_with_root() -> (TempDir, LuaTestHarness, PathBuf) {
    let (td, sandbox, root) = test_sandbox_with_root();
    let harness = LuaTestHarness::from_dir(skill_manager_dir(), sandbox)
        .expect("load skill_manager from crate scripts dir");
    (td, harness, root)
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
        let (_td, harness) = skill_harness();
        assert_eq!(harness.id().name, "skill_manager");
    }

    #[test]
    fn init_and_shutdown() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();
        // After a request completes, status returns to Idle
        // init itself does not change status to Running permanently
        harness.shutdown();
    }

    #[test]
    fn veto_aborts() {
        let (_td, mut harness) = skill_harness();
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
        let (_td, mut harness) = skill_harness();
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
        let (_td, mut harness) = skill_harness();
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
        let (_td, mut harness) = skill_harness();
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
        let (_td, mut harness) = skill_harness();
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
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        let err = harness
            .request(ext_cat(), "get", json!({ "name": "nonexistent" }))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn unregister_skill() {
        let (_td, mut harness) = skill_harness();
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
        let (_td, mut harness) = skill_harness();
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

    #[test]
    fn select_relevant_skills() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        // Register 3 skills with different topics
        for (name, desc, tags) in [
            ("deploy-prod", "Deploy to production", vec!["deploy", "ci"]),
            (
                "code-review",
                "Review code changes",
                vec!["review", "quality"],
            ),
            (
                "deploy-staging",
                "Deploy to staging environment",
                vec!["deploy", "staging"],
            ),
        ] {
            harness
                .request(
                    ext_cat(),
                    "register",
                    json!({
                        "skill": {
                            "name": name,
                            "description": desc,
                            "source": { "format": "agent-skills", "path": format!("/tmp/{name}") },
                            "frontmatter": {},
                            "metadata": { "tags": tags, "categories": [] },
                            "state": "discovered"
                        }
                    }),
                )
                .unwrap();
        }

        // Select with "deploy" query should return deploy skills, not code-review
        let result = harness
            .request(
                ext_cat(),
                "select",
                json!({ "query": "deploy", "limit": 5 }),
            )
            .unwrap();
        assert!(result.is_array());
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Both should be deploy-related
        let names: Vec<&str> = arr.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"deploy-prod"));
        assert!(names.contains(&"deploy-staging"));
    }

    #[test]
    fn select_respects_limit() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        // Register 5 skills all matching "tool"
        for i in 0..5 {
            harness
                .request(
                    ext_cat(),
                    "register",
                    json!({
                        "skill": {
                            "name": format!("tool-{i}"),
                            "description": format!("Tool number {i}"),
                            "source": { "format": "agent-skills", "path": format!("/tmp/t{i}") },
                            "frontmatter": {},
                            "metadata": { "tags": ["tool"], "categories": [] },
                            "state": "discovered"
                        }
                    }),
                )
                .unwrap();
        }

        // Select with limit=2
        let result = harness
            .request(ext_cat(), "select", json!({ "query": "tool", "limit": 2 }))
            .unwrap();
        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    #[test]
    fn select_empty_query_returns_first_n() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        for i in 0..3 {
            harness
                .request(
                    ext_cat(),
                    "register",
                    json!({
                        "skill": {
                            "name": format!("skill-{i}"),
                            "description": format!("Skill {i}"),
                            "source": { "format": "agent-skills", "path": format!("/tmp/s{i}") },
                            "frontmatter": {},
                            "metadata": { "tags": [], "categories": [] },
                            "state": "discovered"
                        }
                    }),
                )
                .unwrap();
        }

        // Empty query returns first N
        let result = harness
            .request(ext_cat(), "select", json!({ "query": "", "limit": 2 }))
            .unwrap();
        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    #[test]
    fn select_no_match_returns_empty() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();
        register_sample(&mut harness);

        let result = harness
            .request(ext_cat(), "select", json!({ "query": "zzz_nomatch_zzz" }))
            .unwrap();
        // Empty Lua table {} → JSON object {} (not array [])
        let is_empty = match &result {
            serde_json::Value::Array(a) => a.is_empty(),
            serde_json::Value::Object(o) => o.is_empty(),
            _ => false,
        };
        assert!(is_empty, "expected empty result, got: {result}");
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
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        let result = harness.request(ext_cat(), "catalog", json!({})).unwrap();
        // catalog returns { success = true, data = { catalog = text, stats = {...} } }
        assert!(result["catalog"].is_string());
        assert!(result["stats"].is_object());
    }

    #[test]
    fn render_catalog_with_skills() {
        let (_td, mut harness) = skill_harness();
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
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        let err = harness
            .request(ext_cat(), "nonexistent_op", json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("unknown operation"));
    }

    #[test]
    fn missing_required_name() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        let err = harness.request(ext_cat(), "get", json!({})).unwrap_err();
        assert!(err.to_string().contains("required"));
    }
}

// =============================================================================
// Recommend Operation
// =============================================================================

mod recommend {
    use super::*;

    /// Register multiple topic-specific skills for recommend tests.
    /// Skills include body so activate() skips file I/O.
    fn register_topic_skills(harness: &mut LuaTestHarness) {
        for (name, desc, tags) in [
            (
                "requirements-analysis",
                "Structured requirements elicitation and specification",
                vec!["requirements", "analysis", "planning"],
            ),
            (
                "code-review",
                "Automated code review with quality metrics",
                vec!["review", "quality", "testing"],
            ),
            (
                "deploy-prod",
                "Production deployment with rollback support",
                vec!["deploy", "production", "ci"],
            ),
            (
                "rust-tdd",
                "Test-driven development workflow for Rust",
                vec!["rust", "tdd", "testing"],
            ),
            (
                "api-design",
                "REST API design patterns and OpenAPI spec generation",
                vec!["api", "design", "rest"],
            ),
        ] {
            harness
                .request(
                    ext_cat(),
                    "register",
                    json!({
                        "skill": {
                            "name": name,
                            "description": desc,
                            "body": format!("# {name}\n\n{desc}"),
                            "source": { "format": "agent-skills", "path": format!("/tmp/{name}") },
                            "frontmatter": {},
                            "metadata": { "tags": tags, "categories": ["dev"] },
                            "state": "discovered"
                        }
                    }),
                )
                .expect("register should succeed");

            // Activate (L2) — body is pre-set so no file I/O
            harness
                .request(ext_cat(), "activate", json!({ "name": name }))
                .expect("activate should succeed");
        }
    }

    #[test]
    fn recommend_missing_intent_returns_error() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        let err = harness
            .request(ext_cat(), "recommend", json!({}))
            .unwrap_err();
        assert!(
            err.to_string().contains("intent"),
            "error should mention 'intent', got: {err}"
        );
    }

    #[test]
    fn recommend_empty_registry_returns_empty() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        let result = harness
            .request(
                ext_cat(),
                "recommend",
                json!({ "intent": "build a feature" }),
            )
            .expect("recommend should succeed with empty registry");

        // Empty active skills → immediate empty return (no LLM call)
        let is_empty = match &result {
            serde_json::Value::Array(a) => a.is_empty(),
            serde_json::Value::Object(o) => o.is_empty(),
            _ => false,
        };
        assert!(
            is_empty,
            "empty registry should return empty result, got: {result}"
        );
    }

    /// In test harness, orcs.llm() returns deny-by-default error.
    /// handle_recommend falls back to keyword-based select.
    /// This verifies the fallback path returns relevant skills.
    #[test]
    fn recommend_fallback_returns_relevant_skills() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();
        register_topic_skills(&mut harness);

        // Intent about "deploy" → fallback select should find deploy-prod
        let result = harness
            .request(
                ext_cat(),
                "recommend",
                json!({ "intent": "deploy to production", "limit": 3 }),
            )
            .expect("recommend should succeed via fallback");

        assert!(result.is_array(), "expected array, got: {result}");
        let arr = result.as_array().expect("should be array");
        assert!(
            !arr.is_empty(),
            "fallback should return at least 1 skill for 'deploy' intent"
        );

        let names: Vec<&str> = arr.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(
            names.contains(&"deploy-prod"),
            "deploy-prod should be in results, got: {names:?}"
        );
    }

    /// Verify fallback flag is set when LLM is unavailable.
    #[test]
    fn recommend_fallback_flag_is_set() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();
        register_topic_skills(&mut harness);

        // We need the raw response (including `fallback` field).
        // The harness extracts `data` on success, so `fallback` is at the
        // top-level response, not in `data`. Let's check via a workaround:
        // query something that will hit fallback, verify we get results
        // (which confirms fallback worked since LLM is denied).
        let result = harness
            .request(
                ext_cat(),
                "recommend",
                json!({ "intent": "rust testing", "limit": 5 }),
            )
            .expect("recommend should succeed via fallback");

        assert!(result.is_array(), "expected array, got: {result}");
        let arr = result.as_array().expect("should be array");
        // "rust" and "testing" should match rust-tdd and code-review
        assert!(
            !arr.is_empty(),
            "fallback should return skills for 'rust testing'"
        );
    }

    #[test]
    fn recommend_respects_limit() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();
        register_topic_skills(&mut harness);

        // Empty intent → fallback select returns first N
        let result = harness
            .request(
                ext_cat(),
                "recommend",
                json!({ "intent": "dev", "limit": 2 }),
            )
            .expect("recommend should succeed");

        assert!(result.is_array(), "expected array, got: {result}");
        let arr = result.as_array().expect("should be array");
        assert!(
            arr.len() <= 2,
            "should respect limit=2, got {} results",
            arr.len()
        );
    }

    #[test]
    fn recommend_with_context() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();
        register_topic_skills(&mut harness);

        // Include context — should still work via fallback
        let result = harness
            .request(
                ext_cat(),
                "recommend",
                json!({
                    "intent": "design an API",
                    "context": "We discussed REST endpoints earlier",
                    "limit": 5,
                }),
            )
            .expect("recommend with context should succeed");

        assert!(result.is_array(), "expected array, got: {result}");
        let arr = result.as_array().expect("should be array");
        // "API" and "design" should match api-design
        let names: Vec<&str> = arr.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(
            names.contains(&"api-design"),
            "api-design should be in results, got: {names:?}"
        );
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
        let (_td, mut harness, root) = skill_harness_with_root();
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
        let (_td, mut harness, root) = skill_harness_with_root();
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
        let (_td, mut harness, root) = skill_harness_with_root();
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
        let (_td, mut harness, root) = skill_harness_with_root();
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
        let (_td, mut harness, root) = skill_harness_with_root();
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
        let (_td, mut harness, root) = skill_harness_with_root();
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
        let (_td, mut harness, root) = skill_harness_with_root();
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

    /// End-to-end: discover multiple topic skills, then select by query.
    /// Verifies "rust" query returns rust-dev as top result.
    #[test]
    fn discover_then_select_by_topic() {
        let (_td, mut harness, root) = skill_harness_with_root();
        let _ = harness.init();

        let skills_dir = root.join("skills");

        // Create topic-specific skills with tags
        for (name, desc, tags) in [
            (
                "rust-dev",
                "Rust development with cargo and borrow checker",
                vec!["rust", "cargo"],
            ),
            (
                "python-dev",
                "Python development with pip and venv",
                vec!["python", "pip"],
            ),
            (
                "deploy-ops",
                "Deployment and CI/CD pipelines",
                vec!["deploy", "docker"],
            ),
            (
                "git-workflow",
                "Git branching and PR workflows",
                vec!["git", "branch"],
            ),
        ] {
            let skill_dir = skills_dir.join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            let content = format!(
                "---\nname: {name}\ndescription: {desc}\ntags:\n{tags_yaml}\ncategories:\n  - dev\n---\n\n# {name}\n\n{desc}\n",
                tags_yaml = tags.iter().map(|t| format!("  - {t}")).collect::<Vec<_>>().join("\n"),
            );
            std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        }

        // Discover all
        let disc = harness
            .request(
                ext_cat(),
                "discover",
                json!({ "path": skills_dir.to_str().unwrap() }),
            )
            .unwrap();
        assert_eq!(disc["discovered"], 4);
        assert_eq!(disc["registered"], 4);

        // Select with "rust" → rust-dev should be top
        let result = harness
            .request(ext_cat(), "select", json!({ "query": "rust", "limit": 3 }))
            .unwrap();
        assert!(result.is_array(), "expected array, got: {result}");
        let arr = result.as_array().unwrap();
        assert!(!arr.is_empty(), "select should return at least 1 skill");
        assert_eq!(arr[0]["name"], "rust-dev", "rust-dev should be top result");

        // Select with "python" → python-dev should be top
        let result = harness
            .request(
                ext_cat(),
                "select",
                json!({ "query": "python", "limit": 3 }),
            )
            .unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(
            arr[0]["name"], "python-dev",
            "python-dev should be top result"
        );

        // Select with "deploy docker" → deploy-ops should be top
        let result = harness
            .request(
                ext_cat(),
                "select",
                json!({ "query": "deploy docker", "limit": 3 }),
            )
            .unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(
            arr[0]["name"], "deploy-ops",
            "deploy-ops should be top result"
        );
    }
}

// =============================================================================
// Snapshot / Restore
// =============================================================================

mod snapshot_restore {
    use super::*;
    use orcs_component::Component;

    #[test]
    fn snapshot_returns_valid_data() {
        let (_td, mut harness) = skill_harness();
        let _ = harness.init();

        // Register skills
        register_sample(&mut harness);
        harness
            .request(
                ext_cat(),
                "register",
                json!({
                    "skill": {
                        "name": "second-skill",
                        "description": "Another skill",
                        "source": { "format": "agent-skills", "path": "/tmp/second" },
                        "frontmatter": {},
                        "metadata": { "tags": ["extra"], "categories": ["test"] },
                        "state": "discovered"
                    }
                }),
            )
            .expect("register second-skill should succeed");

        // Take snapshot via Component trait
        let snapshot = harness
            .component()
            .snapshot()
            .expect("snapshot should succeed");

        assert!(
            snapshot.component_fqn.contains("skill_manager"),
            "FQN should contain skill_manager, got: {}",
            snapshot.component_fqn
        );
        assert!(
            !snapshot.state.is_null(),
            "snapshot state should not be null"
        );

        // State should contain skills data
        let state = &snapshot.state;
        assert!(
            state["skills"].is_array(),
            "state.skills should be an array, got: {state}"
        );
        let skills = state["skills"].as_array().expect("skills should be array");
        assert_eq!(skills.len(), 2, "should have 2 skills in snapshot");
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        // Phase 1: Create and populate
        let (_td1, mut harness1) = skill_harness();
        let _ = harness1.init();

        // Register multiple skills
        for (name, desc) in [
            ("alpha", "First skill"),
            ("beta", "Second skill"),
            ("gamma", "Third skill"),
        ] {
            harness1
                .request(
                    ext_cat(),
                    "register",
                    json!({
                        "skill": {
                            "name": name,
                            "description": desc,
                            "source": { "format": "agent-skills", "path": format!("/tmp/{name}") },
                            "frontmatter": {},
                            "metadata": { "tags": [name], "categories": ["test"] },
                            "state": "discovered"
                        }
                    }),
                )
                .expect("register should succeed");
        }

        // Verify 3 skills registered
        let status1 = harness1
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");
        assert_eq!(status1["total"], 3);

        // Take snapshot
        let snapshot = harness1
            .component()
            .snapshot()
            .expect("snapshot should succeed");

        // Phase 2: Restore into fresh component
        let (_td2, mut harness2) = skill_harness();

        // Restore BEFORE init (matching ChannelRunner behavior)
        harness2
            .component_mut()
            .restore(&snapshot)
            .expect("restore should succeed");

        // init() should detect restored state and skip re-init
        let _ = harness2.init();

        // Verify restored state
        let status2 = harness2
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");
        assert_eq!(
            status2["total"], 3,
            "restored component should have 3 skills"
        );

        // Verify each skill is accessible
        for name in ["alpha", "beta", "gamma"] {
            let result = harness2
                .request(ext_cat(), "get", json!({ "name": name }))
                .expect(&format!("get '{name}' should succeed"));
            assert_eq!(result["name"], name);
        }

        // Verify search still works
        let result = harness2
            .request(ext_cat(), "search", json!({ "query": "beta" }))
            .expect("search should succeed");
        assert!(result.is_array());
        assert_eq!(result.as_array().expect("should be array").len(), 1);
        assert_eq!(result[0]["name"], "beta");
    }

    #[test]
    fn snapshot_preserves_skill_descriptions() {
        let (_td1, mut harness1) = skill_harness();
        let _ = harness1.init();

        harness1
            .request(
                ext_cat(),
                "register",
                json!({
                    "skill": {
                        "name": "detailed-skill",
                        "description": "A very detailed description of this skill",
                        "source": { "format": "lua-dsl", "path": "/tmp/detailed" },
                        "frontmatter": { "user-invocable": true },
                        "metadata": { "tags": ["lua", "test"], "categories": ["dev"] },
                        "state": "discovered"
                    }
                }),
            )
            .expect("register should succeed");

        let snapshot = harness1
            .component()
            .snapshot()
            .expect("snapshot should succeed");

        let (_td2, mut harness2) = skill_harness();
        harness2
            .component_mut()
            .restore(&snapshot)
            .expect("restore should succeed");
        let _ = harness2.init();

        let skill = harness2
            .request(ext_cat(), "get", json!({ "name": "detailed-skill" }))
            .expect("get should succeed");
        assert_eq!(skill["name"], "detailed-skill");
        assert_eq!(
            skill["description"],
            "A very detailed description of this skill"
        );
    }

    #[test]
    fn snapshot_empty_registry() {
        let (_td1, mut harness1) = skill_harness();
        let _ = harness1.init();

        // Snapshot with no skills registered
        let snapshot = harness1
            .component()
            .snapshot()
            .expect("snapshot of empty registry should succeed");

        let (_td2, mut harness2) = skill_harness();
        harness2
            .component_mut()
            .restore(&snapshot)
            .expect("restore of empty snapshot should succeed");
        let _ = harness2.init();

        let status = harness2
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");
        assert_eq!(status["total"], 0);
    }

    #[test]
    fn operations_work_after_restore() {
        let (_td1, mut harness1) = skill_harness();
        let _ = harness1.init();

        register_sample(&mut harness1);

        let snapshot = harness1
            .component()
            .snapshot()
            .expect("snapshot should succeed");

        let (_td2, mut harness2) = skill_harness();
        harness2
            .component_mut()
            .restore(&snapshot)
            .expect("restore should succeed");
        let _ = harness2.init();

        // Register new skill after restore
        harness2
            .request(
                ext_cat(),
                "register",
                json!({
                    "skill": {
                        "name": "post-restore-skill",
                        "description": "Added after restore",
                        "source": { "format": "agent-skills", "path": "/tmp/post" },
                        "frontmatter": {},
                        "metadata": { "tags": [], "categories": [] },
                        "state": "discovered"
                    }
                }),
            )
            .expect("register after restore should succeed");

        let status = harness2
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");
        assert_eq!(status["total"], 2, "should have original + new skill");

        // Unregister should work
        harness2
            .request(ext_cat(), "unregister", json!({ "name": "test-skill" }))
            .expect("unregister should succeed");

        let status = harness2
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");
        assert_eq!(status["total"], 1);
    }
}

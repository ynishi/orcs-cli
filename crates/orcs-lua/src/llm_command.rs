//! Claude CLI command builder for `orcs.llm()`.
//!
//! Builds `std::process::Command` for the `claude` CLI with session
//! management support (`--session-id` for new sessions, `--resume`
//! for continuation).

use std::path::Path;
use std::process::Command;

/// Session mode for the LLM call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmSessionMode {
    /// Single-shot call (no session tracking). Backward-compatible default.
    SingleShot,
    /// Start a new tracked session with the given UUID.
    /// Maps to `claude -p --session-id <uuid> <prompt>`.
    NewSession(String),
    /// Resume an existing session by UUID.
    /// Maps to `claude -p --resume <uuid> <prompt>`.
    Resume(String),
}

/// Result of an LLM invocation.
#[derive(Debug)]
pub struct LlmResult {
    /// Whether the call succeeded.
    pub ok: bool,
    /// Response content (on success).
    pub content: Option<String>,
    /// Error message (on failure).
    pub error: Option<String>,
    /// Session ID (only for `NewSession` mode).
    pub session_id: Option<String>,
}

/// Environment variables that prevent the claude CLI from launching
/// inside another Claude Code session (nested session guard).
const CLAUDE_GUARD_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
];

/// Builds a `Command` for the claude CLI.
fn build_command(prompt: &str, mode: &LlmSessionMode, model: Option<&str>, cwd: &Path) -> Command {
    let mut cmd = Command::new("claude");

    // Remove nested-session guard variables so the child claude process
    // doesn't refuse to start when orcs itself runs inside Claude Code.
    for var in CLAUDE_GUARD_VARS {
        cmd.env_remove(var);
    }

    cmd.arg("-p");

    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }

    match mode {
        LlmSessionMode::SingleShot => {}
        LlmSessionMode::NewSession(uuid) => {
            cmd.arg("--session-id").arg(uuid);
        }
        LlmSessionMode::Resume(uuid) => {
            cmd.arg("--resume").arg(uuid);
        }
    }

    cmd.arg(prompt);
    cmd.current_dir(cwd);
    cmd
}

/// Executes a claude CLI call and returns the result.
pub fn execute_llm(
    prompt: &str,
    mode: &LlmSessionMode,
    model: Option<&str>,
    cwd: &Path,
) -> LlmResult {
    let mut cmd = build_command(prompt, mode, model, cwd);

    match cmd.output() {
        Ok(out) if out.status.success() => {
            let content = String::from_utf8_lossy(&out.stdout).to_string();
            // claude CLI の `--session-id` は指定した UUID をそのまま使用する仕様。
            // CLI が別 ID を振り直すことはない。
            let session_id = match mode {
                LlmSessionMode::NewSession(uuid) => Some(uuid.clone()),
                _ => None,
            };
            LlmResult {
                ok: true,
                content: Some(content),
                error: None,
                session_id,
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            LlmResult {
                ok: false,
                content: None,
                error: Some(if stderr.is_empty() { stdout } else { stderr }),
                session_id: None,
            }
        }
        Err(e) => LlmResult {
            ok: false,
            content: None,
            error: Some(format!("failed to spawn claude: {e}")),
            session_id: None,
        },
    }
}

/// Parses the model from an optional Lua options table.
///
/// Returns `None` if no model is specified (uses CLI default).
pub fn parse_model(opts: Option<&mlua::Table>) -> Result<Option<String>, mlua::Error> {
    let Some(opts) = opts else {
        return Ok(None);
    };
    match opts.get::<String>("model") {
        Ok(m) if !m.is_empty() => Ok(Some(m)),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// Parses the session mode from an optional Lua options table.
///
/// Supports:
/// - No opts → `SingleShot`
/// - `{ new_session = true }` → `NewSession(generated UUID)`
/// - `{ resume = "<uuid>" }` → `Resume(uuid)`
pub fn parse_session_mode(opts: Option<&mlua::Table>) -> Result<LlmSessionMode, mlua::Error> {
    let Some(opts) = opts else {
        return Ok(LlmSessionMode::SingleShot);
    };

    // Check resume first (takes priority)
    if let Ok(resume_id) = opts.get::<String>("resume") {
        if resume_id.is_empty() {
            return Err(mlua::Error::RuntimeError(
                "opts.resume must be a non-empty session ID string".into(),
            ));
        }
        return Ok(LlmSessionMode::Resume(resume_id));
    }

    // Check new_session
    if let Ok(true) = opts.get::<bool>("new_session") {
        let uuid = uuid::Uuid::new_v4().to_string();
        return Ok(LlmSessionMode::NewSession(uuid));
    }

    // No recognized session opts → single shot
    Ok(LlmSessionMode::SingleShot)
}

/// Writes the LLM result into a Lua table.
pub fn result_to_lua_table(lua: &mlua::Lua, result: &LlmResult) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("ok", result.ok)?;

    if let Some(content) = &result.content {
        table.set("content", content.as_str())?;
    }
    if let Some(error) = &result.error {
        table.set("error", error.as_str())?;
    }
    if let Some(session_id) = &result.session_id {
        table.set("session_id", session_id.as_str())?;
    }

    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_single_shot() {
        let cmd = build_command(
            "hello",
            &LlmSessionMode::SingleShot,
            None,
            Path::new("/tmp"),
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-p", "hello"]);
    }

    #[test]
    fn build_command_with_model() {
        let cmd = build_command(
            "hello",
            &LlmSessionMode::SingleShot,
            Some("claude-haiku-4-5-20251001"),
            Path::new("/tmp"),
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec!["-p", "--model", "claude-haiku-4-5-20251001", "hello"]
        );
    }

    #[test]
    fn build_command_removes_nested_guard_vars() {
        let cmd = build_command("x", &LlmSessionMode::SingleShot, None, Path::new("/tmp"));
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().to_string(), v.map(|s| s.to_owned())))
            .collect();
        for var in CLAUDE_GUARD_VARS {
            assert_eq!(
                envs.get(*var),
                Some(&None),
                "{var} should be removed from child env"
            );
        }
    }

    #[test]
    fn build_command_new_session() {
        let uuid = "d87f45df-6c28-4b27-ac04-12033f0c7f8b".to_string();
        let cmd = build_command(
            "test",
            &LlmSessionMode::NewSession(uuid.clone()),
            None,
            Path::new("/tmp"),
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-p", "--session-id", &uuid, "test"]);
    }

    #[test]
    fn build_command_resume() {
        let uuid = "d87f45df-6c28-4b27-ac04-12033f0c7f8b".to_string();
        let cmd = build_command(
            "test",
            &LlmSessionMode::Resume(uuid.clone()),
            None,
            Path::new("/tmp"),
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-p", "--resume", &uuid, "test"]);
    }

    #[test]
    fn parse_mode_none() {
        let mode = parse_session_mode(None).expect("should parse None opts");
        assert_eq!(mode, LlmSessionMode::SingleShot);
    }

    #[test]
    fn parse_mode_new_session() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        opts.set("new_session", true).expect("set new_session");
        let mode = parse_session_mode(Some(&opts)).expect("should parse new_session");
        match mode {
            LlmSessionMode::NewSession(uuid) => {
                assert!(!uuid.is_empty());
                // Verify it's a valid UUID format
                uuid::Uuid::parse_str(&uuid).expect("should be valid UUID");
            }
            other => panic!("expected NewSession, got {:?}", other),
        }
    }

    #[test]
    fn parse_mode_resume() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        let test_uuid = "d87f45df-6c28-4b27-ac04-12033f0c7f8b";
        opts.set("resume", test_uuid).expect("set resume");
        let mode = parse_session_mode(Some(&opts)).expect("should parse resume");
        assert_eq!(mode, LlmSessionMode::Resume(test_uuid.to_string()));
    }

    #[test]
    fn parse_mode_resume_empty_errors() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        opts.set("resume", "").expect("set resume");
        let err = parse_session_mode(Some(&opts));
        assert!(err.is_err());
    }

    #[test]
    fn parse_mode_resume_takes_priority_over_new_session() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        opts.set("new_session", true).expect("set new_session");
        opts.set("resume", "abc-123").expect("set resume");
        let mode = parse_session_mode(Some(&opts)).expect("should parse");
        assert_eq!(mode, LlmSessionMode::Resume("abc-123".to_string()));
    }

    #[test]
    fn parse_model_none() {
        let model = parse_model(None).expect("should parse None");
        assert_eq!(model, None);
    }

    #[test]
    fn parse_model_with_value() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        opts.set("model", "claude-haiku-4-5-20251001")
            .expect("set model");
        let model = parse_model(Some(&opts)).expect("should parse model");
        assert_eq!(model, Some("claude-haiku-4-5-20251001".to_string()));
    }

    #[test]
    fn parse_model_empty_string_returns_none() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        opts.set("model", "").expect("set model");
        let model = parse_model(Some(&opts)).expect("should parse empty model");
        assert_eq!(model, None);
    }

    #[test]
    fn parse_model_missing_key_returns_none() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        opts.set("new_session", true).expect("set other key");
        let model = parse_model(Some(&opts)).expect("should parse without model key");
        assert_eq!(model, None);
    }

    #[test]
    fn parse_mode_empty_table_is_single_shot() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("create table");
        let mode = parse_session_mode(Some(&opts)).expect("should parse empty table");
        assert_eq!(mode, LlmSessionMode::SingleShot);
    }

    #[test]
    fn result_to_lua_success() {
        let lua = mlua::Lua::new();
        let result = LlmResult {
            ok: true,
            content: Some("hello".into()),
            error: None,
            session_id: Some("uuid-123".into()),
        };
        let table = result_to_lua_table(&lua, &result).expect("should create table");
        assert_eq!(table.get::<bool>("ok").expect("get ok"), true);
        assert_eq!(
            table.get::<String>("content").expect("get content"),
            "hello"
        );
        assert_eq!(
            table.get::<String>("session_id").expect("get session_id"),
            "uuid-123"
        );
    }

    /// E2E test: new_session → resume → verify conversation memory.
    ///
    /// Requires `claude` CLI installed and authenticated.
    /// Run with: cargo test -p orcs-lua --lib llm_command::tests::e2e_session -- --ignored
    #[test]
    #[ignore]
    fn e2e_session_new_and_resume() {
        let cwd = std::env::current_dir().expect("get cwd");

        // Step 1: new session — introduce name
        let uuid = uuid::Uuid::new_v4().to_string();
        let mode_new = LlmSessionMode::NewSession(uuid.clone());
        let r1 = execute_llm(
            "My name is ORCSTest42. Reply only: OK",
            &mode_new,
            None,
            &cwd,
        );
        assert!(r1.ok, "Step 1 failed: {:?}", r1.error);
        assert_eq!(r1.session_id.as_deref(), Some(uuid.as_str()));

        // Step 2: resume — ask for name back
        let mode_resume = LlmSessionMode::Resume(uuid.clone());
        let r2 = execute_llm(
            "What is my name? Reply only the name.",
            &mode_resume,
            None,
            &cwd,
        );
        assert!(r2.ok, "Step 2 failed: {:?}", r2.error);
        let content = r2.content.expect("should have content");

        assert!(
            content.contains("ORCSTest42"),
            "Expected 'ORCSTest42' in response, got: {}",
            content.chars().take(200).collect::<String>()
        );
    }

    #[test]
    fn result_to_lua_error() {
        let lua = mlua::Lua::new();
        let result = LlmResult {
            ok: false,
            content: None,
            error: Some("boom".into()),
            session_id: None,
        };
        let table = result_to_lua_table(&lua, &result).expect("should create table");
        assert_eq!(table.get::<bool>("ok").expect("get ok"), false);
        assert_eq!(table.get::<String>("error").expect("get error"), "boom");
    }
}

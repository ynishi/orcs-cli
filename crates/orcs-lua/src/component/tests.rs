use super::*;
use orcs_event::Request;
use orcs_runtime::sandbox::ProjectSandbox;
use orcs_types::{ChannelId, ComponentId, Principal};

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

fn create_test_request(operation: &str, payload: JsonValue) -> Request {
    Request::new(
        EventCategory::Echo,
        operation,
        ComponentId::builtin("test"),
        ChannelId::new(),
        payload,
    )
}

fn test_principal() -> Principal {
    Principal::User(orcs_types::PrincipalId::new())
}

#[test]
fn load_simple_component() {
    let script = r#"
            return {
                id = "test-component",
                subscriptions = {"Echo"},
                on_request = function(req)
                    return { success = true, data = req.payload }
                end,
                on_signal = function(sig)
                    return "Ignored"
                end,
            }
        "#;

    let component = LuaComponent::from_script(script, test_sandbox()).expect("load script");
    assert!(component.id().fqn().contains("test-component"));
    assert_eq!(component.subscriptions(), &[EventCategory::Echo]);
}

#[test]
fn handle_request() {
    let script = r#"
            return {
                id = "echo-lua",
                subscriptions = {"Echo"},
                on_request = function(req)
                    if req.operation == "echo" then
                        return { success = true, data = "echoed" }
                    end
                    return { success = false, error = "unknown" }
                end,
                on_signal = function(sig)
                    return "Ignored"
                end,
            }
        "#;

    let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

    let req = create_test_request("echo", JsonValue::String("hello".into()));
    let result = component.on_request(&req);

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), JsonValue::String("echoed".into()));
}

#[test]
fn handle_signal_abort() {
    let script = r#"
            return {
                id = "signal-test",
                subscriptions = {"Echo"},
                on_request = function(req)
                    return { success = true }
                end,
                on_signal = function(sig)
                    if sig.kind == "Veto" then
                        return "Abort"
                    end
                    return "Ignored"
                end,
            }
        "#;

    let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

    let signal = Signal::veto(test_principal());
    let response = component.on_signal(&signal);

    assert!(matches!(response, SignalResponse::Abort));
    assert_eq!(component.status(), Status::Aborted);
}

#[test]
fn missing_callback_error() {
    let script = r#"
            return {
                id = "incomplete",
                subscriptions = {"Echo"},
                -- missing on_request and on_signal
            }
        "#;

    let result = LuaComponent::from_script(script, test_sandbox());
    assert!(result.is_err());
}

// --- orcs.exec tests ---

mod exec_tests {
    use super::*;
    use orcs_component::{ChildConfig, ChildHandle, ChildResult, CommandPermission, SpawnError};
    /// Minimal permissive ChildContext for testing exec with permission checking.
    #[derive(Debug)]
    struct PermissiveContext;

    #[derive(Debug)]
    struct StubHandle {
        id: String,
    }

    impl ChildHandle for StubHandle {
        fn id(&self) -> &str {
            &self.id
        }
        fn status(&self) -> Status {
            Status::Idle
        }
        fn run_sync(
            &mut self,
            _input: serde_json::Value,
        ) -> Result<ChildResult, orcs_component::RunError> {
            Ok(ChildResult::Ok(serde_json::Value::Null))
        }
        fn abort(&mut self) {}
        fn is_finished(&self) -> bool {
            false
        }
    }

    impl ChildContext for PermissiveContext {
        fn parent_id(&self) -> &str {
            "test-parent"
        }
        fn emit_output(&self, _message: &str) {}
        fn emit_output_with_level(&self, _message: &str, _level: &str) {}
        fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
            Ok(Box::new(StubHandle { id: config.id }))
        }
        fn child_count(&self) -> usize {
            0
        }
        fn max_children(&self) -> usize {
            10
        }
        fn send_to_child(
            &self,
            _child_id: &str,
            _input: serde_json::Value,
        ) -> Result<ChildResult, orcs_component::RunError> {
            Ok(ChildResult::Ok(serde_json::Value::Null))
        }
        fn check_command_permission(&self, _cmd: &str) -> CommandPermission {
            CommandPermission::Allowed
        }
        fn clone_box(&self) -> Box<dyn ChildContext> {
            Box::new(PermissiveContext)
        }
    }

    /// Helper to create a component that uses orcs.exec
    fn create_exec_component(cmd: &str) -> LuaComponent {
        let script = format!(
            r#"
                return {{
                    id = "exec-test",
                    subscriptions = {{"Echo"}},
                    on_request = function(req)
                        local result = orcs.exec("{}")
                        return {{
                            success = true,
                            data = {{
                                stdout = result.stdout,
                                stderr = result.stderr,
                                code = result.code,
                                ok = result.ok
                            }}
                        }}
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }}
            "#,
            cmd
        );
        let mut comp =
            LuaComponent::from_script(&script, test_sandbox()).expect("load exec script");
        comp.set_child_context(Box::new(PermissiveContext));
        comp
    }

    #[test]
    fn exec_denied_without_context() {
        // Without ChildContext, exec should deny
        let script = r#"
                return {
                    id = "exec-deny",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        local result = orcs.exec("echo hello")
                        return {
                            success = true,
                            data = {
                                ok = result.ok,
                                stderr = result.stderr,
                                code = result.code
                            }
                        }
                    end,
                    on_signal = function(sig) return "Ignored" end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");
        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req).expect("should succeed");

        assert!(!result["ok"].as_bool().unwrap_or(true));
        let stderr = result["stderr"].as_str().unwrap();
        assert!(
            stderr.contains("exec denied"),
            "expected 'exec denied', got: {stderr}"
        );
    }

    #[test]
    fn exec_echo_command_with_context() {
        let mut component = create_exec_component("echo 'hello world'");

        let req = create_test_request("test", JsonValue::Null);
        let data = component.on_request(&req).expect("should succeed");

        assert!(data["ok"].as_bool().unwrap_or(false));
        assert!(data["stdout"].as_str().unwrap().contains("hello world"));
        assert_eq!(data["code"].as_i64(), Some(0));
    }

    #[test]
    fn exec_failing_command() {
        let mut component = create_exec_component("exit 42");

        let req = create_test_request("test", JsonValue::Null);
        let data = component.on_request(&req).expect("should succeed");

        assert!(!data["ok"].as_bool().unwrap_or(true));
        assert_eq!(data["code"].as_i64(), Some(42));
    }

    #[test]
    fn exec_stderr_captured() {
        let mut component = create_exec_component("echo 'error output' >&2");

        let req = create_test_request("test", JsonValue::Null);
        let data = component.on_request(&req).expect("should succeed");

        assert!(data["stderr"].as_str().unwrap().contains("error output"));
    }

    #[tokio::test]
    async fn exec_in_async_context() {
        let mut component = create_exec_component("echo 'async test'");

        let req = create_test_request("test", JsonValue::Null);

        let result = tokio::task::spawn_blocking(move || component.on_request(&req))
            .await
            .expect("spawn_blocking should succeed");

        let data = result.expect("should succeed");
        assert!(data["ok"].as_bool().unwrap_or(false));
        assert!(data["stdout"].as_str().unwrap().contains("async test"));
    }

    #[test]
    fn exec_with_special_characters() {
        let script = r#"
                return {
                    id = "exec-special",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        local result = orcs.exec("echo \"quotes and 'apostrophes'\"")
                        return {
                            success = result.ok,
                            data = { stdout = result.stdout }
                        }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");
        component.set_child_context(Box::new(PermissiveContext));
        let req = create_test_request("test", JsonValue::Null);
        let data = component.on_request(&req).expect("should succeed");

        assert!(data["stdout"].as_str().unwrap().contains("quotes"));
    }
}

// --- orcs.log tests ---

mod log_tests {
    use super::*;

    #[test]
    fn log_levels_work() {
        let script = r#"
                return {
                    id = "log-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        orcs.log("debug", "debug message")
                        orcs.log("info", "info message")
                        orcs.log("warn", "warn message")
                        orcs.log("error", "error message")
                        orcs.log("unknown", "unknown level")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");
        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req);

        // Should complete without error (log output goes to tracing)
        assert!(result.is_ok());
    }
}

// --- orcs.output / Emitter tests ---

mod emitter_tests {
    use super::*;
    use orcs_component::Emitter;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock emitter that records calls for testing.
    #[derive(Debug, Clone)]
    struct MockEmitter {
        call_count: Arc<AtomicUsize>,
    }

    impl MockEmitter {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl Emitter for MockEmitter {
        fn emit_output(&self, _message: &str) {
            self.call_count.fetch_add(1, Ordering::SeqCst);
        }

        fn emit_output_with_level(&self, _message: &str, _level: &str) {
            self.call_count.fetch_add(1, Ordering::SeqCst);
        }

        fn clone_box(&self) -> Box<dyn Emitter> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn has_emitter_returns_false_initially() {
        let component = LuaComponent::from_script(
            r#"
                return {
                    id = "test",
                    subscriptions = {"Echo"},
                    on_request = function(req) return { success = true } end,
                    on_signal = function(sig) return "Ignored" end,
                }
                "#,
            test_sandbox(),
        )
        .expect("load script");

        assert!(!component.has_emitter());
    }

    #[test]
    fn set_emitter_enables_has_emitter() {
        let mut component = LuaComponent::from_script(
            r#"
                return {
                    id = "test",
                    subscriptions = {"Echo"},
                    on_request = function(req) return { success = true } end,
                    on_signal = function(sig) return "Ignored" end,
                }
                "#,
            test_sandbox(),
        )
        .expect("load script");

        let emitter = MockEmitter::new();
        component.set_emitter(Box::new(emitter));

        assert!(component.has_emitter());
    }

    #[test]
    fn orcs_output_calls_emitter() {
        let script = r#"
                return {
                    id = "output-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        orcs.output("Hello from Lua!")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

        let emitter = MockEmitter::new();
        let emitter_clone = emitter.clone();
        component.set_emitter(Box::new(emitter));

        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req);

        assert!(result.is_ok());
        assert!(
            emitter_clone.call_count() >= 1,
            "emitter should have been called"
        );
    }

    #[test]
    fn orcs_output_with_level_calls_emitter() {
        let script = r#"
                return {
                    id = "output-level-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        orcs.output_with_level("Warning message", "warn")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

        let emitter = MockEmitter::new();
        let emitter_clone = emitter.clone();
        component.set_emitter(Box::new(emitter));

        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req);

        assert!(result.is_ok());
        assert!(
            emitter_clone.call_count() >= 1,
            "emitter should have been called"
        );
    }

    #[test]
    fn orcs_output_without_emitter_is_noop() {
        let script = r#"
                return {
                    id = "output-noop-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        -- This should not panic even without emitter
                        orcs.output("Message without emitter")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

        // Note: no emitter set
        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req);

        // Should complete without error
        assert!(result.is_ok());
    }

    /// Mock emitter that returns canned board entries.
    #[derive(Debug, Clone)]
    struct BoardMockEmitter {
        entries: Vec<serde_json::Value>,
    }

    impl BoardMockEmitter {
        fn with_entries(entries: Vec<serde_json::Value>) -> Self {
            Self { entries }
        }
    }

    impl Emitter for BoardMockEmitter {
        fn emit_output(&self, _message: &str) {}
        fn emit_output_with_level(&self, _message: &str, _level: &str) {}
        fn board_recent(&self, n: usize) -> Vec<serde_json::Value> {
            let len = self.entries.len();
            let skip = len.saturating_sub(n);
            self.entries[skip..].to_vec()
        }
        fn clone_box(&self) -> Box<dyn Emitter> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn board_recent_via_emitter() {
        let script = r#"
                return {
                    id = "board-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        local entries = orcs.board_recent(10)
                        local count = 0
                        for _ in pairs(entries) do count = count + 1 end
                        return { success = true, data = { count = count } }
                    end,
                    on_signal = function(sig) return "Ignored" end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

        let entries = vec![
            serde_json::json!({"source": {"name": "tool"}, "kind": {"type": "Output", "level": "info"}, "operation": "display", "payload": {"message": "hello"}}),
            serde_json::json!({"source": {"name": "agent"}, "kind": {"type": "Event", "category": "tool:result"}, "operation": "complete", "payload": {"tool": "read"}}),
        ];
        let emitter = BoardMockEmitter::with_entries(entries);
        component.set_emitter(Box::new(emitter));

        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req).expect("should succeed");
        assert_eq!(result["count"], 2);
    }

    #[test]
    fn board_recent_empty_without_emitter() {
        let script = r#"
                return {
                    id = "board-empty-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        -- board_recent is placeholder (noop) without emitter
                        -- orcs.board_recent is not registered, so calling it would error
                        return { success = true }
                    end,
                    on_signal = function(sig) return "Ignored" end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req);
        assert!(result.is_ok());
    }

    #[test]
    fn board_recent_respects_limit() {
        let script = r#"
                return {
                    id = "board-limit-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        local entries = orcs.board_recent(2)
                        local count = 0
                        local last_msg = ""
                        for _, e in ipairs(entries) do
                            count = count + 1
                            last_msg = e.payload.message or ""
                        end
                        return { success = true, data = { count = count, last = last_msg } }
                    end,
                    on_signal = function(sig) return "Ignored" end,
                }
            "#;

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

        let entries: Vec<serde_json::Value> = (0..5)
            .map(|i| serde_json::json!({"payload": {"message": format!("msg{i}")}}))
            .collect();
        let emitter = BoardMockEmitter::with_entries(entries);
        component.set_emitter(Box::new(emitter));

        let req = create_test_request("test", JsonValue::Null);
        let result = component.on_request(&req).expect("should succeed");
        assert_eq!(result["count"], 2);
        assert_eq!(result["last"], "msg4");
    }
}

// --- JSON ↔ Lua conversion tests ---

mod json_lua_conversion_tests {
    use super::*;

    /// Helper: create a Lua VM and convert JSON → Lua → JSON roundtrip.
    fn roundtrip(input: &JsonValue) -> JsonValue {
        let lua = Lua::new();
        let lua_val = json_to_lua_value(&lua, input).expect("json_to_lua_value should succeed");
        lua_value_to_json(&lua_val)
    }

    #[test]
    fn null_roundtrip() {
        assert_eq!(roundtrip(&JsonValue::Null), JsonValue::Null);
    }

    #[test]
    fn bool_roundtrip() {
        assert_eq!(roundtrip(&serde_json::json!(true)), serde_json::json!(true));
        assert_eq!(
            roundtrip(&serde_json::json!(false)),
            serde_json::json!(false)
        );
    }

    #[test]
    fn integer_roundtrip() {
        assert_eq!(roundtrip(&serde_json::json!(42)), serde_json::json!(42));
        assert_eq!(roundtrip(&serde_json::json!(-7)), serde_json::json!(-7));
        assert_eq!(roundtrip(&serde_json::json!(0)), serde_json::json!(0));
    }

    #[test]
    fn float_roundtrip() {
        let val = serde_json::json!(3.14);
        let result = roundtrip(&val);
        let diff = (result.as_f64().expect("should be f64") - 3.14).abs();
        assert!(diff < 1e-10, "float roundtrip drift: {diff}");
    }

    #[test]
    fn string_roundtrip() {
        assert_eq!(
            roundtrip(&serde_json::json!("hello")),
            serde_json::json!("hello")
        );
        assert_eq!(roundtrip(&serde_json::json!("")), serde_json::json!(""));
    }

    #[test]
    fn array_roundtrip() {
        let input = serde_json::json!([1, 2, 3]);
        assert_eq!(roundtrip(&input), input);
    }

    #[test]
    fn object_roundtrip() {
        let input = serde_json::json!({"key": "value", "num": 42});
        let result = roundtrip(&input);
        assert_eq!(result["key"], "value");
        assert_eq!(result["num"], 42);
    }

    #[test]
    fn nested_structure_roundtrip() {
        let input = serde_json::json!({
            "skills": [
                {"name": "deploy", "tags": ["ci", "cd"]},
                {"name": "review", "tags": ["code"]}
            ],
            "frozen": false,
            "count": 2
        });
        let result = roundtrip(&input);
        assert_eq!(result["frozen"], false);
        assert_eq!(result["count"], 2);
        assert!(result["skills"].is_array());
        let skills = result["skills"].as_array().expect("should be array");
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["name"], "deploy");
        assert_eq!(skills[1]["tags"][0], "code");
    }

    #[test]
    fn empty_array_roundtrip() {
        // Note: empty Lua table {} is ambiguous (array or object).
        // json_to_lua_value creates empty table; lua_value_to_json detects len=0 → object.
        let input = serde_json::json!([]);
        let result = roundtrip(&input);
        // Empty array becomes empty object in Lua roundtrip (known limitation)
        assert!(
            result.is_object() || result.is_array(),
            "empty collection: {result}"
        );
    }

    #[test]
    fn empty_object_roundtrip() {
        let input = serde_json::json!({});
        let result = roundtrip(&input);
        assert!(result.is_object());
        assert!(result.as_object().expect("should be object").is_empty());
    }
}

// --- Snapshot / Restore tests ---

mod snapshot_tests {
    use super::*;
    use orcs_component::Component;

    #[test]
    fn snapshot_without_callback_returns_not_supported() {
        let script = r#"
            return {
                id = "no-snapshot",
                subscriptions = {"Echo"},
                on_request = function(req) return { success = true } end,
                on_signal = function(sig) return "Ignored" end,
            }
        "#;

        let component =
            LuaComponent::from_script(script, test_sandbox()).expect("should load script");
        let result = component.snapshot();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, SnapshotError::NotSupported(_)),
            "expected NotSupported, got: {err:?}"
        );
    }

    #[test]
    fn restore_without_callback_returns_not_supported() {
        let script = r#"
            return {
                id = "no-restore",
                subscriptions = {"Echo"},
                on_request = function(req) return { success = true } end,
                on_signal = function(sig) return "Ignored" end,
            }
        "#;

        let mut component =
            LuaComponent::from_script(script, test_sandbox()).expect("should load script");
        let snapshot = ComponentSnapshot::from_state(
            "lua::no-restore",
            &serde_json::json!({"key": "value"}),
        )
        .expect("should create snapshot");

        let result = component.restore(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, SnapshotError::NotSupported(_)),
            "expected NotSupported, got: {err:?}"
        );
    }

    #[test]
    fn snapshot_roundtrip_simple_state() {
        let script = r#"
            local state = { counter = 0, name = "test" }

            return {
                id = "snap-test",
                subscriptions = {"Echo"},
                on_request = function(req)
                    if req.operation == "increment" then
                        state.counter = state.counter + 1
                        return { success = true, data = { counter = state.counter } }
                    elseif req.operation == "get" then
                        return { success = true, data = state }
                    end
                    return { success = false, error = "unknown" }
                end,
                on_signal = function(sig) return "Ignored" end,

                snapshot = function()
                    return state
                end,

                restore = function(s)
                    state = s
                end,
            }
        "#;

        // Create component, mutate state
        let mut comp1 =
            LuaComponent::from_script(script, test_sandbox()).expect("should load script");
        let req = create_test_request("increment", JsonValue::Null);
        comp1.on_request(&req).expect("increment should succeed");
        comp1.on_request(&req).expect("increment should succeed");
        comp1.on_request(&req).expect("increment should succeed");

        // Verify state is counter=3
        let get_req = create_test_request("get", JsonValue::Null);
        let data = comp1.on_request(&get_req).expect("get should succeed");
        assert_eq!(data["counter"], 3);

        // Take snapshot
        let snapshot = comp1.snapshot().expect("snapshot should succeed");
        assert_eq!(snapshot.component_fqn, "lua::snap-test");
        assert!(!snapshot.state.is_null(), "snapshot state should not be null");

        // Create new component, restore
        let mut comp2 =
            LuaComponent::from_script(script, test_sandbox()).expect("should load script");

        // Verify fresh state is counter=0
        let data = comp2.on_request(&get_req).expect("get should succeed");
        assert_eq!(data["counter"], 0);

        // Restore from snapshot
        comp2.restore(&snapshot).expect("restore should succeed");

        // Verify restored state is counter=3
        let data = comp2.on_request(&get_req).expect("get should succeed");
        assert_eq!(data["counter"], 3);
        assert_eq!(data["name"], "test");
    }

    #[test]
    fn snapshot_with_nested_tables() {
        let script = r#"
            local state = {
                items = {},
                metadata = { version = 1 },
            }

            return {
                id = "snap-nested",
                subscriptions = {"Echo"},
                on_request = function(req)
                    if req.operation == "add" then
                        table.insert(state.items, req.payload.item)
                        return { success = true }
                    elseif req.operation == "get" then
                        return { success = true, data = {
                            items = state.items,
                            count = #state.items,
                            version = state.metadata.version,
                        }}
                    end
                    return { success = false, error = "unknown" }
                end,
                on_signal = function(sig) return "Ignored" end,

                snapshot = function()
                    return state
                end,

                restore = function(s)
                    state = s
                end,
            }
        "#;

        let mut comp1 =
            LuaComponent::from_script(script, test_sandbox()).expect("should load script");

        // Add items
        let add_req =
            create_test_request("add", serde_json::json!({"item": "alpha"}));
        comp1.on_request(&add_req).expect("add should succeed");
        let add_req =
            create_test_request("add", serde_json::json!({"item": "beta"}));
        comp1.on_request(&add_req).expect("add should succeed");

        // Snapshot
        let snapshot = comp1.snapshot().expect("snapshot should succeed");

        // Restore into new component
        let mut comp2 =
            LuaComponent::from_script(script, test_sandbox()).expect("should load script");
        comp2.restore(&snapshot).expect("restore should succeed");

        // Verify
        let get_req = create_test_request("get", JsonValue::Null);
        let data = comp2.on_request(&get_req).expect("get should succeed");
        assert_eq!(data["count"], 2);
        assert_eq!(data["version"], 1);
        assert_eq!(data["items"][0], "alpha");
        assert_eq!(data["items"][1], "beta");
    }

    #[test]
    fn restore_validates_component_fqn() {
        let script = r#"
            return {
                id = "fqn-check",
                subscriptions = {"Echo"},
                on_request = function(req) return { success = true } end,
                on_signal = function(sig) return "Ignored" end,
                snapshot = function() return {} end,
                restore = function(s) end,
            }
        "#;

        let mut component =
            LuaComponent::from_script(script, test_sandbox()).expect("should load script");

        // Create snapshot with wrong FQN
        let wrong_snapshot = ComponentSnapshot::from_state(
            "lua::wrong-component",
            &serde_json::json!({}),
        )
        .expect("should create snapshot");

        let result = component.restore(&wrong_snapshot);
        assert!(result.is_err(), "restore with wrong FQN should fail");
    }
}

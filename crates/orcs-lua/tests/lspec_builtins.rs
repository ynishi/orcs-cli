//! Lua component unit tests using mlua-lspec.
//!
//! Tests pure Lua logic in builtins without ORCS runtime dependencies.
//! These tests are also executable from the lua-debugger MCP via `test_launch`.
//!
//! # Test strategies
//!
//! Two patterns are used depending on what is being tested:
//!
//! - **Pure function extraction** — Copy a small pure function from the Lua source
//!   into the test code and test it in isolation (e.g. `concierge_build_llm_opts`,
//!   `console_metrics_helpers`). Useful when the function has no side effects.
//!
//! - **Mock injection** — Load the real `.lua` file via `dofile()` after replacing
//!   the `orcs` global with a recording mock. Each call to `fresh_concierge()`
//!   creates an isolated component instance with independent module state, so
//!   tests do not interfere with each other. The mock captures all `orcs.*` calls
//!   (`log`, `output`, `request`, `llm`, etc.) for assertion. See
//!   [`concierge_mock_setup`] for the factory that generates the mock harness.
//!
//! # Run
//!
//! ```bash
//! cargo test --test lspec_builtins
//! ```

use std::path::Path;

/// Resolve absolute path to a builtin Lua file.
fn builtin_path(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("orcs-lua parent dir")
        .join("orcs-app")
        .join("builtins")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Helper: run lspec tests and assert all pass.
fn run_lspec(code: &str, chunk_name: &str) -> mlua_lspec::TestSummary {
    let summary = mlua_lspec::run_tests(code, chunk_name)
        .unwrap_or_else(|e| panic!("{chunk_name} failed to run: {e}"));
    assert_eq!(
        summary.failed, 0,
        "{chunk_name}: {} passed, {} failed\n{:#?}",
        summary.passed, summary.failed, summary.tests
    );
    summary
}

// =============================================================================
// Echo Component
// =============================================================================

/// Echo component: structure, on_request routing, on_signal dispatch.
#[test]
fn echo_component() {
    let echo_path = builtin_path("echo.lua");
    let code = format!(
        r#"
        local echo = dofile("{echo_path}")
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('echo component structure', function()
            it('has required id', function()
                expect(echo.id).to.equal('echo')
            end)
            it('has namespace', function()
                expect(echo.namespace).to.equal('builtin')
            end)
            it('subscribes to Echo events', function()
                expect(#echo.subscriptions).to.equal(1)
                expect(echo.subscriptions[1]).to.equal('Echo')
            end)
            it('has on_request handler', function()
                expect(type(echo.on_request)).to.equal('function')
            end)
            it('has on_signal handler', function()
                expect(type(echo.on_signal)).to.equal('function')
            end)
            it('has init callback', function()
                expect(type(echo.init)).to.equal('function')
            end)
            it('has shutdown callback', function()
                expect(type(echo.shutdown)).to.equal('function')
            end)
        end)

        describe('echo on_request', function()
            it('echoes payload on echo operation', function()
                local result = echo.on_request({{
                    operation = 'echo',
                    payload = {{ message = 'hello' }},
                }})
                expect(result.success).to.equal(true)
                expect(result.data.message).to.equal('hello')
            end)
            it('returns error for unknown operation', function()
                local result = echo.on_request({{
                    operation = 'unknown_op',
                    payload = {{}},
                }})
                expect(result.success).to.equal(false)
                expect(result.error).to.exist()
            end)
            it('error message includes operation name', function()
                local result = echo.on_request({{
                    operation = 'bogus',
                    payload = {{}},
                }})
                expect(result.error).to.match('bogus')
            end)
        end)

        describe('echo on_signal', function()
            it('aborts on Veto', function()
                expect(echo.on_signal({{ kind = 'Veto' }})).to.equal('Abort')
            end)
            it('ignores Cancel', function()
                expect(echo.on_signal({{ kind = 'Cancel' }})).to.equal('Ignored')
            end)
            it('ignores unknown signals', function()
                expect(echo.on_signal({{ kind = 'SomethingElse' }})).to.equal('Ignored')
            end)
        end)
        "#
    );
    run_lspec(&code, "@echo_test.lua");
}

// =============================================================================
// Concierge Component (mock-injected full component test)
// =============================================================================

/// Generate Lua source that bootstraps a mock `orcs` global for concierge tests.
///
/// The returned code defines two Lua-side factories:
///
/// - `create_mock(overrides)` — builds a recording mock table `m` and installs
///   it as the `orcs` global. All `orcs.*` calls (`log`, `output`, `request`,
///   `llm`, `llm_ping`, `register_intent`, `emit_event`) are recorded into `m`
///   for later assertion. `overrides` allows customising mock responses:
///   - `llm_response` — return value of `orcs.llm()` (default: success with
///     `content = "mock reply"`, `session_id = "sess-1"`, `cost = 0.0042`).
///   - `foundation` — foundation segments returned by the `get_all` RPC.
///   - `metrics_formatted` — console metrics string.
///   - `skills` — skill recommendations returned by the `recommend` RPC.
///
/// - `fresh_concierge(overrides)` — calls `create_mock`, then `dofile()` on the
///   real `concierge.lua`. Returns `(component, mock)` where `component` is the
///   table returned by `concierge.lua` (with `on_request` / `on_signal`) and
///   `mock` is the recording table for assertions.
///
/// Because `dofile()` re-executes the script from scratch, each call produces a
/// component with fresh module-local state (`busy`, `session_id`, `turn_count`,
/// etc.), guaranteeing test isolation.
fn concierge_mock_setup() -> String {
    format!(
        r#"
        local function create_mock(overrides)
            overrides = overrides or {{}}
            local m = {{
                log_calls = {{}},
                output_calls = {{}},
                output_level_calls = {{}},
                request_calls = {{}},
                intent_calls = {{}},
                event_calls = {{}},
                last_prompt = nil,
                last_llm_opts = nil,
                llm_response = overrides.llm_response,
                foundation = overrides.foundation or {{ system = "[SYS]", task = "[TASK]", guard = "[GUARD]" }},
                metrics_formatted = overrides.metrics_formatted or "[METRICS]",
                skills = overrides.skills or {{ {{ name = "test-skill", description = "A test" }} }},
            }}

            orcs = {{
                log = function(level, msg)
                    m.log_calls[#m.log_calls + 1] = {{ level = level, msg = msg }}
                end,
                output = function(msg)
                    m.output_calls[#m.output_calls + 1] = msg
                end,
                output_with_level = function(msg, level)
                    m.output_level_calls[#m.output_level_calls + 1] = {{ msg = msg, level = level }}
                end,
                request = function(target, op, payload, opts)
                    m.request_calls[#m.request_calls + 1] = {{ target = target, op = op, payload = payload }}
                    if target == "foundation::foundation_manager" and op == "get_all" then
                        return {{ success = true, data = m.foundation }}
                    end
                    if target == "metrics::console_metrics" and op == "get_all" then
                        return {{ success = true, formatted = m.metrics_formatted }}
                    end
                    if target == "skill::skill_manager" and op == "recommend" then
                        return {{ success = true, data = m.skills }}
                    end
                    return {{ success = false }}
                end,
                register_intent = function(def)
                    m.intent_calls[#m.intent_calls + 1] = def
                    return {{ ok = true }}
                end,
                emit_event = function(cat, op, payload)
                    m.event_calls[#m.event_calls + 1] = {{ cat = cat, op = op, payload = payload }}
                end,
                llm = function(prompt, opts)
                    m.last_prompt = prompt
                    m.last_llm_opts = opts
                    return m.llm_response or {{
                        ok = true, content = "mock reply", session_id = "sess-1", cost = 0.0042,
                    }}
                end,
                llm_ping = function(opts)
                    return {{ ok = true, provider = opts.provider or "unknown" }}
                end,
            }}

            return m
        end

        local CONCIERGE_PATH = "{concierge_path}"

        local function fresh_concierge(overrides)
            local m = create_mock(overrides)
            local c = dofile(CONCIERGE_PATH)
            return c, m
        end
        "#,
        concierge_path = builtin_path("concierge.lua")
    )
}

/// Concierge component: full integration test via mock injection.
/// Loads real concierge.lua with mocked orcs.* APIs.
#[test]
fn concierge_component() {
    let setup = concierge_mock_setup();
    let code = format!(
        r#"
        {setup}
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('concierge status', function()
            it('returns initial idle state', function()
                local c, m = fresh_concierge()
                local r = c.on_request({{ operation = "status", payload = {{}} }})
                expect(r.success).to.equal(true)
                expect(r.data.busy).to.equal(false)
                expect(r.data.turn_count).to.equal(0)
            end)

            it('reflects state after successful process', function()
                local c, m = fresh_concierge()
                c.on_request({{ operation = "process", payload = {{ message = "hi" }} }})
                local r = c.on_request({{ operation = "status", payload = {{}} }})
                expect(r.data.turn_count).to.equal(1)
                expect(r.data.session_id).to.equal("sess-1")
                expect(r.data.busy).to.equal(false)
            end)
        end)

        describe('concierge ping', function()
            it('returns success with provider config', function()
                local c, m = fresh_concierge()
                local r = c.on_request({{
                    operation = "ping",
                    payload = {{ llm_config = {{ provider = "ollama" }} }}
                }})
                expect(r.success).to.equal(true)
                expect(r.data.ok).to.equal(true)
            end)

            it('returns failure when llm_ping fails', function()
                local c, m = fresh_concierge()
                orcs.llm_ping = function(opts)
                    return {{ ok = false, error = "connection refused" }}
                end
                local r = c.on_request({{
                    operation = "ping",
                    payload = {{ llm_config = {{ provider = "bad" }} }}
                }})
                expect(r.success).to.equal(false)
                expect(r.data.error).to.equal("connection refused")
            end)
        end)

        describe('concierge process (initial)', function()
            it('succeeds and returns response data', function()
                local c, m = fresh_concierge()
                local r = c.on_request({{
                    operation = "process",
                    payload = {{ message = "hello" }}
                }})
                expect(r.success).to.equal(true)
                expect(r.data.response).to.equal("mock reply")
                expect(r.data.session_id).to.equal("sess-1")
                expect(r.data.cost).to.equal(0.0042)
            end)

            it('fetches foundation, metrics, skills via RPC', function()
                local c, m = fresh_concierge()
                c.on_request({{ operation = "process", payload = {{ message = "test" }} }})
                local targets = {{}}
                for _, call in ipairs(m.request_calls) do
                    targets[call.target] = call.op
                end
                expect(targets["foundation::foundation_manager"]).to.equal("get_all")
                expect(targets["metrics::console_metrics"]).to.equal("get_all")
                expect(targets["skill::skill_manager"]).to.equal("recommend")
            end)

            it('registers skill and delegate intents', function()
                local c, m = fresh_concierge()
                c.on_request({{ operation = "process", payload = {{ message = "test" }} }})
                local names = {{}}
                for _, def in ipairs(m.intent_calls) do
                    names[def.name] = true
                end
                expect(names["test-skill"]).to.equal(true)
                expect(names["delegate_task"]).to.equal(true)
            end)

            it('assembles prompt with "both" placement by default', function()
                local c, m = fresh_concierge()
                c.on_request({{ operation = "process", payload = {{ message = "user msg" }} }})
                local p = m.last_prompt
                -- "both" placement: [SYS] appears twice (top + bottom anchor)
                local _, count = p:gsub("%[SYS%]", "")
                expect(count).to.equal(2)
                -- Message at the end
                expect(p:find("user msg")).to.exist()
                -- Guard before message
                local guard_pos = p:find("%[GUARD%]")
                local msg_pos = p:find("user msg")
                expect(guard_pos < msg_pos).to.equal(true)
            end)

            it('passes llm_config to llm opts', function()
                local c, m = fresh_concierge()
                c.on_request({{
                    operation = "process",
                    payload = {{
                        message = "hi",
                        llm_config = {{ provider = "anthropic", model = "claude-4", temperature = 0.3 }}
                    }}
                }})
                expect(m.last_llm_opts.provider).to.equal("anthropic")
                expect(m.last_llm_opts.model).to.equal("claude-4")
                expect(m.last_llm_opts.temperature).to.equal(0.3)
                expect(m.last_llm_opts.resolve).to.equal(true)
            end)

            it('outputs Thinking indicator', function()
                local c, m = fresh_concierge()
                c.on_request({{ operation = "process", payload = {{ message = "x" }} }})
                local found = false
                for _, msg in ipairs(m.output_calls) do
                    if msg:find("Thinking") then found = true end
                end
                expect(found).to.equal(true)
            end)

            it('emits llm_response event on success', function()
                local c, m = fresh_concierge()
                c.on_request({{ operation = "process", payload = {{ message = "x" }} }})
                local found = false
                for _, ev in ipairs(m.event_calls) do
                    if ev.op == "llm_response" and ev.payload.source == "concierge" then
                        found = true
                    end
                end
                expect(found).to.equal(true)
            end)
        end)

        describe('concierge process (session resume)', function()
            it('skips prompt assembly on second call', function()
                local c, m = fresh_concierge()
                c.on_request({{ operation = "process", payload = {{ message = "first" }} }})
                local first_request_count = #m.request_calls
                c.on_request({{ operation = "process", payload = {{ message = "second" }} }})
                -- No new RPC calls (session resume path)
                expect(#m.request_calls).to.equal(first_request_count)
                expect(m.last_llm_opts.session_id).to.equal("sess-1")
            end)
        end)

        describe('concierge process (empty message)', function()
            it('returns success without processing', function()
                local c, m = fresh_concierge()
                local r = c.on_request({{ operation = "process", payload = {{ message = "" }} }})
                expect(r.success).to.equal(true)
                expect(m.last_prompt).to.equal(nil)
            end)
        end)

        describe('concierge process (LLM error)', function()
            it('returns error when llm fails', function()
                local c, m = fresh_concierge({{
                    llm_response = {{ ok = false, error = "rate limit exceeded" }}
                }})
                local r = c.on_request({{ operation = "process", payload = {{ message = "hi" }} }})
                expect(r.success).to.equal(false)
                expect(r.error).to.match("rate limit")
            end)

            it('resets busy after LLM error', function()
                local c, m = fresh_concierge({{
                    llm_response = {{ ok = false, error = "fail" }}
                }})
                c.on_request({{ operation = "process", payload = {{ message = "hi" }} }})
                local status = c.on_request({{ operation = "status", payload = {{}} }})
                expect(status.data.busy).to.equal(false)
            end)
        end)

        describe('concierge process (busy rejection)', function()
            it('rejects process while another is in-flight', function()
                local c, m = fresh_concierge()
                local inner_result = nil
                -- Override llm to attempt a recursive process call while busy=true
                orcs.llm = function(prompt, opts)
                    inner_result = c.on_request({{ operation = "process", payload = {{ message = "concurrent" }} }})
                    return {{ ok = true, content = "reply", session_id = "sess-1", cost = 0.001 }}
                end
                c.on_request({{ operation = "process", payload = {{ message = "outer" }} }})
                expect(inner_result).to.exist()
                expect(inner_result.success).to.equal(false)
                expect(inner_result.error).to.match("busy")
            end)
        end)

        describe('concierge process (prompt placement)', function()
            it('uses "top" placement: system context appears once', function()
                local c, m = fresh_concierge()
                c.on_request({{
                    operation = "process",
                    payload = {{ message = "msg", prompt_placement = "top" }}
                }})
                local _, count = m.last_prompt:gsub("%[SYS%]", "")
                expect(count).to.equal(1)
            end)

            it('uses "bottom" placement: system context appears once', function()
                local c, m = fresh_concierge()
                c.on_request({{
                    operation = "process",
                    payload = {{ message = "msg", prompt_placement = "bottom" }}
                }})
                local _, count = m.last_prompt:gsub("%[SYS%]", "")
                expect(count).to.equal(1)
            end)

            it('"top" places system before user message', function()
                local c, m = fresh_concierge()
                c.on_request({{
                    operation = "process",
                    payload = {{ message = "USR_MSG", prompt_placement = "top" }}
                }})
                local sys_pos = m.last_prompt:find("%[SYS%]")
                local msg_pos = m.last_prompt:find("USR_MSG")
                expect(sys_pos < msg_pos).to.equal(true)
            end)

            it('"bottom" places task before system', function()
                local c, m = fresh_concierge()
                c.on_request({{
                    operation = "process",
                    payload = {{ message = "USR_MSG", prompt_placement = "bottom" }}
                }})
                -- In bottom placement, [TASK] comes before [SYS]
                local task_pos = m.last_prompt:find("%[TASK%]")
                local sys_pos = m.last_prompt:find("%[SYS%]")
                expect(task_pos < sys_pos).to.equal(true)
            end)
        end)

        describe('concierge tracking preservation (H1 fix)', function()
            it('preserves provider when config absent on resume', function()
                local c, m = fresh_concierge()
                c.on_request({{
                    operation = "process",
                    payload = {{ message = "a", llm_config = {{ provider = "anthropic", model = "claude" }} }}
                }})
                c.on_request({{ operation = "process", payload = {{ message = "b" }} }})
                local status = c.on_request({{ operation = "status", payload = {{}} }})
                expect(status.data.provider).to.equal("anthropic")
                expect(status.data.model).to.equal("claude")
            end)
        end)

        describe('concierge unknown operation', function()
            it('returns error with operation name', function()
                local c, m = fresh_concierge()
                local r = c.on_request({{ operation = "nonexistent", payload = {{}} }})
                expect(r.success).to.equal(false)
                expect(r.error).to.match("unknown operation")
                expect(r.error).to.match("nonexistent")
            end)
        end)

        describe('concierge on_signal', function()
            it('aborts on Veto', function()
                local c = fresh_concierge()
                expect(c.on_signal({{ kind = "Veto" }})).to.equal("Abort")
            end)
            it('handles other signals', function()
                local c = fresh_concierge()
                expect(c.on_signal({{ kind = "Other" }})).to.equal("Handled")
            end)
        end)
        "#
    );
    let summary = run_lspec(&code, "@concierge_component_test.lua");
    assert!(
        summary.passed >= 24,
        "expected at least 24 concierge component tests, got {}",
        summary.passed
    );
}

// =============================================================================
// Concierge Helpers (pure function unit tests)
// =============================================================================

/// Concierge: build_llm_opts pure function.
#[test]
fn concierge_build_llm_opts() {
    run_lspec(
        r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        local function build_llm_opts(input)
            local opts = {}
            local llm_cfg = input.llm_config or {}
            if llm_cfg.provider    then opts.provider    = llm_cfg.provider end
            if llm_cfg.model       then opts.model       = llm_cfg.model end
            if llm_cfg.base_url    then opts.base_url    = llm_cfg.base_url end
            if llm_cfg.api_key     then opts.api_key     = llm_cfg.api_key end
            if llm_cfg.temperature then opts.temperature  = llm_cfg.temperature end
            if llm_cfg.max_tokens  then opts.max_tokens   = llm_cfg.max_tokens end
            if llm_cfg.timeout     then opts.timeout      = llm_cfg.timeout end
            return opts
        end

        describe('build_llm_opts', function()
            it('returns empty table when no llm_config', function()
                local opts = build_llm_opts({})
                expect(next(opts)).to.equal(nil)
            end)
            it('passes through provider and model', function()
                local opts = build_llm_opts({
                    llm_config = { provider = 'openai', model = 'gpt-4' }
                })
                expect(opts.provider).to.equal('openai')
                expect(opts.model).to.equal('gpt-4')
            end)
            it('passes through all supported fields', function()
                local opts = build_llm_opts({
                    llm_config = {
                        provider = 'anthropic', model = 'claude-3',
                        base_url = 'https://api.example.com', api_key = 'sk-test',
                        temperature = 0.7, max_tokens = 1024, timeout = 30,
                    }
                })
                expect(opts.provider).to.equal('anthropic')
                expect(opts.model).to.equal('claude-3')
                expect(opts.base_url).to.equal('https://api.example.com')
                expect(opts.api_key).to.equal('sk-test')
                expect(opts.temperature).to.equal(0.7)
                expect(opts.max_tokens).to.equal(1024)
                expect(opts.timeout).to.equal(30)
            end)
            it('ignores unknown config fields', function()
                local opts = build_llm_opts({
                    llm_config = { provider = 'openai', unknown_field = 'value' }
                })
                expect(opts.provider).to.equal('openai')
                expect(opts.unknown_field).to.equal(nil)
            end)
            it('handles empty llm_config', function()
                local opts = build_llm_opts({ llm_config = {} })
                expect(next(opts)).to.equal(nil)
            end)
        end)
        "#,
        "@concierge_helpers_test.lua",
    );
}

// =============================================================================
// FormatAdapter
// =============================================================================

/// FormatAdapter: adapter registration, helper functions, to_internal conversion.
#[test]
fn format_adapter() {
    let fa_path = builtin_path("skill_manager/format_adapter.lua");
    let code = format!(
        r#"
        local FA = dofile("{fa_path}")
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('FormatAdapter registration', function()
            it('has agent-skills adapter', function()
                expect(FA.get('agent-skills')).to.exist()
            end)
            it('has cursor-mdc adapter', function()
                expect(FA.get('cursor-mdc')).to.exist()
            end)
            it('has lua-dsl adapter', function()
                expect(FA.get('lua-dsl')).to.exist()
            end)
            it('returns nil for unknown format', function()
                expect(FA.get('unknown-format')).to.equal(nil)
            end)
            it('lists supported formats sorted', function()
                local formats = FA.supported_formats()
                expect(formats[1]).to.equal('agent-skills')
                expect(formats[2]).to.equal('cursor-mdc')
                expect(formats[3]).to.equal('lua-dsl')
            end)
        end)

        describe('agent-skills to_internal', function()
            local adapter = FA.get('agent-skills')

            it('uses frontmatter name', function()
                local skill = adapter.to_internal({{
                    frontmatter = {{ name = 'my-skill', description = 'desc' }},
                    body = 'body text',
                }}, '/path/to/skill')
                expect(skill.name).to.equal('my-skill')
                expect(skill.description).to.equal('desc')
                expect(skill.body).to.equal('body text')
                expect(skill.source.format).to.equal('agent-skills')
                expect(skill.state).to.equal('discovered')
            end)

            it('falls back to dir name when no frontmatter name', function()
                local skill = adapter.to_internal({{
                    frontmatter = {{}},
                    body = 'hello',
                }}, '/skills/code-review')
                expect(skill.name).to.equal('code-review')
            end)

            it('extracts metadata fields', function()
                local skill = adapter.to_internal({{
                    frontmatter = {{
                        name = 'test',
                        author = 'alice',
                        version = '1.0',
                        tags = {{'lua', 'test'}},
                        categories = {{'dev'}},
                    }},
                    body = '',
                }}, '/p')
                expect(skill.metadata.author).to.equal('alice')
                expect(skill.metadata.version).to.equal('1.0')
                expect(skill.metadata.tags[1]).to.equal('lua')
                expect(skill.metadata.categories[1]).to.equal('dev')
            end)
        end)

        describe('cursor-mdc to_internal', function()
            local adapter = FA.get('cursor-mdc')

            it('uses file stem as name', function()
                local skill = adapter.to_internal({{
                    frontmatter = {{}},
                    body = 'rule body',
                }}, '/rules/no-console.mdc')
                expect(skill.name).to.equal('no-console')
                expect(skill.source.format).to.equal('cursor-mdc')
            end)

            it('sets user-invocable based on alwaysApply', function()
                local skill = adapter.to_internal({{
                    frontmatter = {{ alwaysApply = true }},
                    body = '',
                }}, '/r/auto.mdc')
                expect(skill.frontmatter['user-invocable']).to.equal(false)
                expect(skill.frontmatter['disable-model-invocation']).to.equal(true)
            end)
        end)

        describe('lua-dsl to_internal', function()
            local adapter = FA.get('lua-dsl')

            it('uses def.name directly', function()
                local skill = adapter.to_internal({{
                    name = 'lua-skill',
                    description = 'a lua skill',
                    body = 'do stuff',
                }}, '/skills/lua-skill')
                expect(skill.name).to.equal('lua-skill')
                expect(skill.body).to.equal('do stuff')
                expect(skill.source.format).to.equal('lua-dsl')
            end)

            it('falls back to prompt when body is nil', function()
                local skill = adapter.to_internal({{
                    name = 'test',
                    prompt = 'prompt text',
                }}, '/p')
                expect(skill.body).to.equal('prompt text')
            end)

            it('returns empty string when no body or prompt', function()
                local skill = adapter.to_internal({{
                    name = 'empty',
                }}, '/p')
                expect(skill.body).to.equal('')
            end)
        end)
        "#
    );
    run_lspec(&code, "@format_adapter_test.lua");
}

// =============================================================================
// SkillRegistry
// =============================================================================

/// SkillRegistry: register, get, list, search, select, freeze, serialize.
#[test]
fn skill_registry() {
    let sr_path = builtin_path("skill_manager/skill_registry.lua");
    let code = format!(
        r#"
        local SR = dofile("{sr_path}")
        local describe, it, expect = lust.describe, lust.it, lust.expect

        local function make_skill(name, desc, tags)
            return {{
                name = name,
                description = desc or '',
                source = {{ format = 'agent-skills', path = '/skills/' .. name }},
                metadata = {{ tags = tags or {{}}, categories = {{}} }},
                frontmatter = {{}},
                state = 'discovered',
            }}
        end

        describe('SkillRegistry.new', function()
            it('creates empty registry', function()
                local reg = SR.new()
                expect(reg:count()).to.equal(0)
            end)
        end)

        describe('register', function()
            it('registers a valid skill', function()
                local reg = SR.new()
                local ok = reg:register(make_skill('test-skill', 'desc'))
                expect(ok).to.equal(true)
                expect(reg:count()).to.equal(1)
            end)

            it('rejects skill without name', function()
                local reg = SR.new()
                local ok, err = reg:register({{ source = {{ format = 'x' }} }})
                expect(ok).to.equal(false)
                expect(err).to.match('name')
            end)

            it('rejects skill with name > 64 chars', function()
                local reg = SR.new()
                local long_name = string.rep('a', 65)
                local ok, err = reg:register({{
                    name = long_name,
                    source = {{ format = 'x' }},
                }})
                expect(ok).to.equal(false)
                expect(err).to.match('64')
            end)

            it('rejects skill without source.format', function()
                local reg = SR.new()
                local ok, err = reg:register({{ name = 'test', source = {{}} }})
                expect(ok).to.equal(false)
                expect(err).to.match('source.format')
            end)

            it('rejects duplicate registration', function()
                local reg = SR.new()
                reg:register(make_skill('dup'))
                local ok, err = reg:register(make_skill('dup'))
                expect(ok).to.equal(false)
                expect(err).to.match('already registered')
            end)
        end)

        describe('get', function()
            it('returns registered skill', function()
                local reg = SR.new()
                reg:register(make_skill('find-me', 'found'))
                local s = reg:get('find-me')
                expect(s).to.exist()
                expect(s.description).to.equal('found')
            end)
            it('returns nil for missing skill', function()
                local reg = SR.new()
                expect(reg:get('nonexistent')).to.equal(nil)
            end)
        end)

        describe('list', function()
            it('returns catalog entries in registration order', function()
                local reg = SR.new()
                reg:register(make_skill('alpha'))
                reg:register(make_skill('beta'))
                reg:register(make_skill('gamma'))
                local entries = reg:list()
                expect(#entries).to.equal(3)
                expect(entries[1].name).to.equal('alpha')
                expect(entries[2].name).to.equal('beta')
                expect(entries[3].name).to.equal('gamma')
            end)
            it('applies filter function', function()
                local reg = SR.new()
                reg:register(make_skill('keep', 'yes'))
                reg:register(make_skill('drop', 'no'))
                local entries = reg:list(function(s) return s.description == 'yes' end)
                expect(#entries).to.equal(1)
                expect(entries[1].name).to.equal('keep')
            end)
        end)

        describe('search', function()
            it('matches by name', function()
                local reg = SR.new()
                reg:register(make_skill('code-review', 'review code'))
                reg:register(make_skill('test-runner', 'run tests'))
                local results = reg:search('code')
                expect(#results).to.equal(1)
                expect(results[1].name).to.equal('code-review')
            end)
            it('matches by description', function()
                local reg = SR.new()
                reg:register(make_skill('alpha', 'find bugs'))
                local results = reg:search('bugs')
                expect(#results).to.equal(1)
            end)
            it('matches by tag', function()
                local reg = SR.new()
                reg:register(make_skill('tagtest', 'desc', {{'security', 'audit'}}))
                local results = reg:search('security')
                expect(#results).to.equal(1)
            end)
            it('returns all when query is empty', function()
                local reg = SR.new()
                reg:register(make_skill('a'))
                reg:register(make_skill('b'))
                local results = reg:search('')
                expect(#results).to.equal(2)
            end)
            it('is case insensitive', function()
                local reg = SR.new()
                reg:register(make_skill('MySkill', 'Does Things'))
                local results = reg:search('myskill')
                expect(#results).to.equal(1)
            end)
        end)

        describe('select (scored ranking)', function()
            it('returns top-N results', function()
                local reg = SR.new()
                for i = 1, 10 do
                    reg:register(make_skill('skill-' .. i, 'task ' .. i))
                end
                local results = reg:select('task', 3)
                expect(#results).to.equal(3)
            end)
            it('ranks exact name match highest', function()
                local reg = SR.new()
                reg:register(make_skill('deploy', 'deploy app'))
                reg:register(make_skill('deployer', 'deployment tool'))
                local results = reg:select('deploy', 5)
                expect(results[1].name).to.equal('deploy')
            end)
            it('returns first N when no query', function()
                local reg = SR.new()
                reg:register(make_skill('a'))
                reg:register(make_skill('b'))
                reg:register(make_skill('c'))
                local results = reg:select(nil, 2)
                expect(#results).to.equal(2)
            end)
        end)

        describe('freeze', function()
            it('blocks register after freeze', function()
                local reg = SR.new()
                reg:freeze()
                local ok, err = reg:register(make_skill('blocked'))
                expect(ok).to.equal(false)
                expect(err).to.match('frozen')
            end)
            it('blocks unregister after freeze', function()
                local reg = SR.new()
                reg:register(make_skill('existing'))
                reg:freeze()
                local ok, err = reg:unregister('existing')
                expect(ok).to.equal(false)
                expect(err).to.match('frozen')
            end)
        end)

        describe('unregister', function()
            it('removes a skill', function()
                local reg = SR.new()
                reg:register(make_skill('gone'))
                local ok = reg:unregister('gone')
                expect(ok).to.equal(true)
                expect(reg:count()).to.equal(0)
                expect(reg:get('gone')).to.equal(nil)
            end)
            it('fails for missing skill', function()
                local reg = SR.new()
                local ok, err = reg:unregister('nope')
                expect(ok).to.equal(false)
                expect(err).to.match('not found')
            end)
        end)

        describe('serialize / deserialize', function()
            it('roundtrips registry state', function()
                local reg = SR.new()
                reg:register(make_skill('s1', 'one'))
                reg:register(make_skill('s2', 'two'))
                reg:freeze()

                local snapshot = reg:serialize()
                expect(#snapshot.skills).to.equal(2)
                expect(snapshot.frozen).to.equal(true)

                local reg2 = SR.new()
                reg2:deserialize(snapshot)
                expect(reg2:count()).to.equal(2)
                expect(reg2:get('s1').description).to.equal('one')
                expect(reg2:get('s2').description).to.equal('two')
            end)
            it('handles empty snapshot', function()
                local reg = SR.new()
                reg:register(make_skill('old'))
                reg:deserialize({{ skills = {{}} }})
                expect(reg:count()).to.equal(0)
            end)
        end)

        describe('reload', function()
            it('adds new skills and removes stale ones', function()
                local reg = SR.new()
                reg:register(make_skill('keep', 'kept'))
                reg:register(make_skill('remove', 'removed'))

                local result = reg:reload({{
                    make_skill('keep', 'updated'),
                    make_skill('new-one', 'fresh'),
                }})
                expect(result.updated).to.equal(1)
                expect(result.registered).to.equal(1)
                expect(result.removed).to.equal(1)
                expect(reg:get('keep').description).to.equal('updated')
                expect(reg:get('new-one')).to.exist()
                expect(reg:get('remove')).to.equal(nil)
            end)
            it('respects frozen state after reload', function()
                local reg = SR.new()
                reg:register(make_skill('a'))
                reg:freeze()
                reg:reload({{ make_skill('a', 'refreshed') }})
                -- Should re-freeze after reload
                local ok, err = reg:register(make_skill('blocked'))
                expect(ok).to.equal(false)
                expect(err).to.match('frozen')
            end)
        end)
        "#
    );
    run_lspec(&code, "@skill_registry_test.lua");
}

// =============================================================================
// SkillCatalog (pure subset)
// =============================================================================

/// SkillCatalog: estimate_tokens, render_catalog, active tracking.
#[test]
fn skill_catalog() {
    let sr_path = builtin_path("skill_manager/skill_registry.lua");
    let sc_path = builtin_path("skill_manager/skill_catalog.lua");
    let code = format!(
        r#"
        local SR = dofile("{sr_path}")
        local SC = dofile("{sc_path}")
        local describe, it, expect = lust.describe, lust.it, lust.expect

        local function make_skill(name, desc)
            return {{
                name = name,
                description = desc or '',
                source = {{ format = 'agent-skills', path = '/skills/' .. name }},
                metadata = {{ tags = {{}}, categories = {{}} }},
                frontmatter = {{}},
                state = 'discovered',
            }}
        end

        local function setup()
            local reg = SR.new()
            reg:register(make_skill('alpha', 'first skill'))
            reg:register(make_skill('beta', 'second skill'))
            reg:register(make_skill('gamma', 'third skill'))
            return SC.new(reg), reg
        end

        describe('SkillCatalog.new', function()
            it('creates catalog with registry', function()
                local cat = setup()
                expect(cat).to.exist()
                expect(cat.budget_chars).to.equal(16000)
            end)
        end)

        describe('discover', function()
            it('returns all skills as catalog entries', function()
                local cat = setup()
                local entries = cat:discover()
                expect(#entries).to.equal(3)
                expect(entries[1].name).to.equal('alpha')
            end)
            it('applies filter', function()
                local cat = setup()
                local entries = cat:discover(function(s)
                    return s.name == 'beta'
                end)
                expect(#entries).to.equal(1)
            end)
        end)

        describe('render_catalog', function()
            it('renders markdown list', function()
                local cat = setup()
                local text, stats = cat:render_catalog()
                expect(text).to.match('alpha')
                expect(text).to.match('beta')
                expect(stats.total).to.equal(3)
                expect(stats.shown).to.equal(3)
            end)
            it('respects budget limit', function()
                local cat = setup()
                local text, stats = cat:render_catalog({{ budget = 30 }})
                -- Budget of 30 chars should truncate after 1 entry
                expect(stats.shown < stats.total).to.equal(true)
            end)
        end)

        describe('active tracking', function()
            it('starts with zero active', function()
                local cat = setup()
                expect(cat:active_count()).to.equal(0)
                expect(#cat:active_skills()).to.equal(0)
            end)
        end)
        "#
    );
    run_lspec(&code, "@skill_catalog_test.lua");
}

// =============================================================================
// Console Metrics Helpers (extracted pure functions)
// =============================================================================

/// Console metrics: basename, format_for_prompt (pure helpers).
#[test]
fn console_metrics_helpers() {
    run_lspec(
        r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        -- Extracted from console_metrics.lua (local pure functions)
        local function basename(path)
            return path:match("([^/]+)$") or path
        end

        local function format_for_prompt(m)
            local parts = {}
            parts[#parts + 1] = "[Console Metrics]"
            if m.project_name and m.project_name ~= "" then
                parts[#parts + 1] = "Project: " .. m.project_name
            end
            if m.cwd and m.cwd ~= "" then
                parts[#parts + 1] = "Working Directory: " .. m.cwd
            end
            if m.git then
                local git = m.git
                if git.ok then
                    local git_line = "Git: " .. (git.branch or "?")
                    if git.commit_short and git.commit_short ~= "" then
                        git_line = git_line .. " (" .. git.commit_short .. ")"
                    end
                    if git.dirty then
                        git_line = git_line .. " [dirty]"
                    end
                    parts[#parts + 1] = git_line
                end
            end
            if m.timestamp and m.timestamp ~= "" then
                parts[#parts + 1] = "Time: " .. m.timestamp
            end
            return table.concat(parts, "\n")
        end

        describe('basename', function()
            it('extracts last path component', function()
                expect(basename('/home/user/project')).to.equal('project')
            end)
            it('handles single component', function()
                expect(basename('project')).to.equal('project')
            end)
            it('falls back to full path on trailing slash', function()
                -- pattern ([^/]+)$ cannot match when path ends with /
                expect(basename('/home/user/project/')).to.equal('/home/user/project/')
            end)
            it('handles root', function()
                expect(basename('/')).to.equal('/')
            end)
        end)

        describe('format_for_prompt', function()
            it('includes header', function()
                local result = format_for_prompt({})
                expect(result).to.match('%[Console Metrics%]')
            end)
            it('includes project name', function()
                local result = format_for_prompt({ project_name = 'myapp' })
                expect(result).to.match('Project: myapp')
            end)
            it('includes cwd', function()
                local result = format_for_prompt({ cwd = '/home/user/myapp' })
                expect(result).to.match('Working Directory: /home/user/myapp')
            end)
            it('formats git info with branch and commit', function()
                local result = format_for_prompt({
                    git = { ok = true, branch = 'main', commit_short = 'abc123', dirty = false }
                })
                expect(result).to.match('Git: main %(abc123%)')
            end)
            it('shows dirty flag', function()
                local result = format_for_prompt({
                    git = { ok = true, branch = 'dev', dirty = true }
                })
                expect(result).to.match('%[dirty%]')
            end)
            it('skips git when not ok', function()
                local result = format_for_prompt({ git = { ok = false } })
                expect(result:find('Git:')).to.equal(nil)
            end)
            it('includes timestamp', function()
                local result = format_for_prompt({ timestamp = '2026-02-28 12:00:00' })
                expect(result).to.match('Time: 2026%-02%-28')
            end)
            it('combines all fields', function()
                local result = format_for_prompt({
                    project_name = 'orcs',
                    cwd = '/home/user/orcs',
                    git = { ok = true, branch = 'main', dirty = false },
                    timestamp = '2026-02-28 12:00:00',
                })
                expect(result).to.match('Project: orcs')
                expect(result).to.match('Git: main')
                expect(result).to.match('Time:')
            end)
        end)
        "#,
        "@console_metrics_helpers_test.lua",
    );
}

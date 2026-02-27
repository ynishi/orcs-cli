//! Lua component unit tests using mlua-lspec.
//!
//! Tests pure Lua logic in builtins without ORCS runtime dependencies.
//! These tests are also executable from the lua-debugger MCP via `test_launch`.
//!
//! Run with: `cargo test --test lspec_builtins`

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

/// Echo component: on_request routing and on_signal dispatch.
#[test]
fn echo_component() {
    let echo_path = builtin_path("echo.lua");
    let code = format!(
        r#"
        -- Load the echo component table
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
                local response = echo.on_signal({{ kind = 'Veto' }})
                expect(response).to.equal('Abort')
            end)

            it('ignores Cancel', function()
                local response = echo.on_signal({{ kind = 'Cancel' }})
                expect(response).to.equal('Ignored')
            end)

            it('ignores unknown signals', function()
                local response = echo.on_signal({{ kind = 'SomethingElse' }})
                expect(response).to.equal('Ignored')
            end)
        end)
        "#
    );

    let summary =
        mlua_lspec::run_tests(&code, "@echo_test.lua").expect("lspec echo tests should run");

    assert_eq!(
        summary.failed, 0,
        "echo tests: {} passed, {} failed\n{:?}",
        summary.passed, summary.failed, summary.tests
    );
}

/// Concierge component: pure helper functions (build_llm_opts).
#[test]
fn concierge_helpers() {
    let summary = mlua_lspec::run_tests(
        r#"
        -- concierge.lua uses orcs.* globals, so we can't dofile it directly.
        -- Instead, extract and test the pure helper function in isolation.

        local describe, it, expect = lust.describe, lust.it, lust.expect

        -- Replicate build_llm_opts (pure function from concierge.lua)
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
                        provider = 'anthropic',
                        model = 'claude-3',
                        base_url = 'https://api.example.com',
                        api_key = 'sk-test',
                        temperature = 0.7,
                        max_tokens = 1024,
                        timeout = 30,
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
    )
    .expect("lspec concierge helper tests should run");

    assert_eq!(
        summary.failed, 0,
        "concierge helper tests: {} passed, {} failed\n{:?}",
        summary.passed, summary.failed, summary.tests
    );
}
